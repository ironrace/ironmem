//! Local process evidence for Codex invocations. Missing evidence means unknown.
//!
//! Restart adoption verifies the native Codex root's exact invocation marker.
//! A launcher wrapper whose marker appears only on a different native child
//! conservatively holds until trusted supervisor completion. Process polling
//! and authenticated tree cleanup cover ordinary Codex tools, including setsid
//! children; they do not contain deliberate double-fork escapes between polls.
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::autopilot::codex_dispatch::InvocationArtifacts;
use crate::autopilot::registry::Liveness;
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub stat: String,
    pub args: String,
}

impl ProcessRow {
    pub fn active(&self) -> bool {
        !self.stat.starts_with('Z')
    }

    /// ps does not preserve argv boundaries. Only the verified option prefix
    /// before the first positional prompt can supply an invocation marker.
    ///
    /// An unparseable prefix is ambiguous only when the row could carry an
    /// Autopilot marker at all. Every marker is a whitespace-free absolute
    /// path to a `schema.json` (artifact paths refuse whitespace), so a row
    /// with no such word is someone else's `codex exec`, and holding every
    /// Codex dispatch on it would let any unrelated session stall Autopilot.
    pub fn marker(&self) -> Result<Option<&str>, String> {
        self.parse_marker().or_else(|reason| {
            if self.args.split_whitespace().any(|word| {
                Path::new(word)
                    .file_name()
                    .is_some_and(|f| f == "schema.json")
            }) {
                Err(reason)
            } else {
                Ok(None)
            }
        })
    }

    fn parse_marker(&self) -> Result<Option<&str>, String> {
        let mut args = self.args.split_whitespace();
        if args
            .next()
            .and_then(|a| Path::new(a).file_name())
            .and_then(|a| a.to_str())
            != Some("codex")
            || args.next() != Some("exec")
        {
            return Ok(None);
        }
        if !args
            .clone()
            .any(|a| a == "--output-schema" || a.starts_with("--output-schema="))
        {
            return Ok(None);
        }
        let mut words = args.peekable();
        if words.peek() == Some(&"resume") {
            words.next();
        }
        let mut marker = None;
        while let Some(word) = words.next() {
            match word {
                "--output-schema" => {
                    let path = words
                        .next()
                        .ok_or("Codex output-schema option has no path")?;
                    if !Path::new(path).is_absolute() || marker.replace(path).is_some() {
                        return Err("ambiguous Codex output-schema marker".into());
                    }
                }
                "--json"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
                | "--skip-git-repo-check"
                | "--full-auto"
                | "--ephemeral" => {}
                "-o"
                | "--output-last-message"
                | "-C"
                | "--cd"
                | "-m"
                | "--model"
                | "-c"
                | "--config"
                | "-s"
                | "--sandbox"
                | "--color"
                | "--add-dir"
                | "--image"
                | "-i" => {
                    words.next().ok_or("Codex option has no value")?;
                }
                "--" => break,
                other if other.starts_with('-') => {
                    return Err("unrecognized Codex option before prompt".into())
                }
                _ => break,
            }
        }
        Ok(marker)
    }
}

/// Any same-user process may carry non-UTF-8 argv, and ps prints it raw.
/// Lossy decoding keeps that row from failing every observation; artifact
/// paths refuse U+FFFD, so a replaced byte can never forge a marker match.
fn decode_process_table(output: &[u8]) -> Result<Vec<ProcessRow>, String> {
    let text = String::from_utf8_lossy(output);
    if text.trim().is_empty() {
        return Err("ps returned no process table".into());
    }
    parse_processes(&text)
}

/// The issue slug an Autopilot marker names: `.../autopilot/<slug>/<uuid>/schema.json`.
pub(crate) fn marker_issue_slug(marker: &str) -> Option<&str> {
    Path::new(marker)
        .parent()
        .and_then(|invocation| invocation.parent())
        .and_then(|issue| issue.file_name())
        .and_then(|name| name.to_str())
}

pub fn parse_processes(text: &str) -> Result<Vec<ProcessRow>, String> {
    text.lines()
        .filter(|s| !s.trim().is_empty())
        .map(|line| {
            // Whitespace columns have variable padding, so consume fields explicitly.
            let mut fields = Vec::new();
            let mut remaining = line.trim();
            for _ in 0..4 {
                let end = remaining
                    .find(char::is_whitespace)
                    .ok_or("malformed ps row")?;
                fields.push(&remaining[..end]);
                remaining = remaining[end..].trim_start();
            }
            let number = |n: usize| {
                fields[n]
                    .parse::<u32>()
                    .map_err(|_| "invalid ps identity".to_string())
            };
            if remaining.is_empty() {
                return Err("ps row has no executable".into());
            }
            Ok(ProcessRow {
                pid: number(0)?,
                ppid: number(1)?,
                pgid: number(2)?,
                stat: fields[3].into(),
                args: remaining.into(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessSnapshot {
    Available(Vec<ProcessRow>),
    Unavailable { reason: String },
}

pub trait ProcessSource {
    fn list(&mut self) -> Result<Vec<ProcessRow>, String>;
}

pub struct UnixProcesses;
impl ProcessSource for UnixProcesses {
    fn list(&mut self) -> Result<Vec<ProcessRow>, String> {
        #[cfg(unix)]
        {
            decode_process_table(&bounded_process_table()?)
        }
        #[cfg(not(unix))]
        {
            Err("Codex process observation is unsupported on this platform".into())
        }
    }
}

/// Keep process observation inside the dispatch's finite supervision bound.
/// Nonblocking reads also bound a pipe left open after ps itself exits.
#[cfg(unix)]
fn bounded_process_table() -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    const TIMEOUT: Duration = Duration::from_secs(2);
    const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
    let mut command = Command::new("ps");
    command
        .args(["-axww", "-o", "pid=,ppid=,pgid=,stat=,args="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = crate::autopilot::dispatch::retry_on_etxtbsy(|| command.spawn())
        .map_err(|e| format!("cannot execute ps: {e}"))?;
    let started = Instant::now();
    let mut status = None;
    let result = (|| {
        let mut pipe = child.stdout.take().ok_or("ps stdout is unavailable")?;
        let fd = pipe.as_raw_fd();
        // SAFETY: the descriptor is our owned stdout pipe; preserve its flags
        // and add nonblocking mode so neither EOF nor a reader join can hang.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(format!(
                "cannot bound ps output: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut output = Vec::new();
        let mut eof = false;
        let mut buffer = [0; 8192];
        loop {
            if started.elapsed() >= TIMEOUT {
                return Err("ps exceeded its 2s process observation bound".into());
            }
            while !eof {
                if started.elapsed() >= TIMEOUT {
                    return Err("ps exceeded its 2s process observation bound".into());
                }
                match pipe.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        if output.len() + count > OUTPUT_LIMIT {
                            return Err("ps exceeded its process-table output limit".into());
                        }
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(format!("cannot read ps output: {e}")),
                }
                if started.elapsed() >= TIMEOUT {
                    return Err("ps exceeded its 2s process observation bound".into());
                }
            }
            if status.is_none() {
                status = child
                    .try_wait()
                    .map_err(|e| format!("cannot observe ps completion: {e}"))?;
            }
            if let Some(status) = status {
                if eof {
                    return if status.success() {
                        Ok(output)
                    } else {
                        Err("ps exited unsuccessfully".into())
                    };
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() && status.is_none() {
        // Only signal the group while the spawned ps PID is still reserved
        // by our unreaped Child. Never signal a stale group after try_wait.
        crate::autopilot::dispatch::kill_process_group(&mut child);
        let _ = child.wait();
    }
    result
}

pub fn snapshot(source: &mut dyn ProcessSource) -> ProcessSnapshot {
    match source.list() {
        Ok(rows) => ProcessSnapshot::Available(rows),
        Err(reason) => ProcessSnapshot::Unavailable { reason },
    }
}

/// Written immediately after spawn. A terminal record is separate evidence
/// that the supervising controller finished cleanup, not merely that PID died.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessIdentity {
    pub version: u32,
    pub root_pid: u32,
    pub root_pgid: u32,
    pub schema_path: String,
    pub invocation_id: String,
    pub descendants: Vec<ProcessRow>,
}

fn identity_path(a: &InvocationArtifacts) -> std::path::PathBuf {
    Path::new(&a.schema_path).with_file_name("process.json")
}

pub(crate) fn persist_identity(
    a: &InvocationArtifacts,
    identity: &ProcessIdentity,
) -> Result<(), MemoryError> {
    super::codex_dispatch::persist_private_atomic(
        &identity_path(a),
        "process",
        &serde_json::to_vec(identity)?,
    )
}

pub(crate) fn read_identity(a: &InvocationArtifacts) -> Result<ProcessIdentity, String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(identity_path(a))
        .map_err(|e| format!("missing Codex process identity: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Codex identity is not a regular file".into());
    }
    let mut data = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > 1024 * 1024 {
        return Err("Codex identity exceeds size limit".into());
    }
    let identity: ProcessIdentity = serde_json::from_slice(&data)
        .map_err(|e| format!("invalid Codex process identity: {e}"))?;
    if identity.version != 1
        || identity.root_pid == 0
        || identity.root_pgid != identity.root_pid
        || identity.schema_path != a.schema_path
        || identity.invocation_id != a.invocation_id
    {
        return Err("inconsistent Codex process identity".into());
    }
    Ok(identity)
}

/// The first active row that could still be this invocation's process tree.
///
/// Before a trusted exit record, a recorded PID or the root's process group
/// is evidence. After one, cleanup has already proven every recorded process
/// gone by kernel birth identity, so a recorded PID is a reuse and no longer
/// counts. The group still does: an untracked member keeps it alive, and the
/// kernel never hands out a live group's ID as a new PID. Only a live process
/// holding the root's PID shows the ID was freed and reused, making the group
/// someone else's.
fn recorded_tree_evidence(
    rows: &[ProcessRow],
    identity: &ProcessIdentity,
    completed: bool,
) -> Option<String> {
    let active = || rows.iter().filter(|r| r.active());
    if !completed {
        if let Some(r) = active().find(|r| {
            r.pid == identity.root_pid || identity.descendants.iter().any(|d| d.pid == r.pid)
        }) {
            return Some(format!(
                "process {} has a PID this invocation recorded",
                r.pid
            ));
        }
    }
    let group_reused = completed && active().any(|r| r.pid == identity.root_pid);
    if group_reused {
        return None;
    }
    active().find(|r| r.pgid == identity.root_pgid).map(|r| {
        format!(
            "process {} is in this invocation's process group {}",
            r.pid, r.pgid
        )
    })
}

impl ProcessSnapshot {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
    pub fn reason(&self) -> String {
        match self {
            Self::Unavailable { reason } => reason.clone(),
            Self::Available(_) => {
                "Codex invocation identity or completion evidence is unavailable or inconsistent"
                    .into()
            }
        }
    }
    pub fn invocation_reason(&self, artifacts: Option<&InvocationArtifacts>) -> String {
        let Self::Available(rows) = self else {
            return self.reason();
        };
        let Some(a) = artifacts else {
            return "Codex invocation has no persisted artifacts".into();
        };
        if super::codex_dispatch::has_launch_failure_record(a) {
            return "Codex never launched, but an active process carries its marker or an ambiguous one; holding for verification".into();
        }
        let identity = match read_identity(a) {
            Ok(identity) => identity,
            Err(reason) => return reason,
        };
        if let Some(reason) = rows
            .iter()
            .filter(|r| r.active())
            .find_map(|r| r.marker().err())
        {
            return reason;
        }
        let markers: Vec<_> = rows
            .iter()
            .filter(|r| r.active() && r.marker().ok().flatten() == Some(a.schema_path.as_str()))
            .collect();
        if !markers.is_empty()
            && (markers.len() != 1
                || markers[0].pid != identity.root_pid
                || markers[0].pgid != identity.root_pgid)
        {
            return "Codex schema marker matched an unexpected or ambiguous process identity; holding for verification".into();
        }
        let completed = super::codex_dispatch::has_valid_exit_record(a);
        if let Some(evidence) = recorded_tree_evidence(rows, &identity, completed) {
            return format!("{evidence}; process identity/completion requires verification");
        }
        if !completed {
            return "Codex invocation has no trusted supervisor cleanup/completion record; historical descendants cannot be ruled out".into();
        }
        "Codex supervisor completion verified".into()
    }
    pub fn liveness(&self, artifacts: Option<&InvocationArtifacts>) -> Liveness {
        let Self::Available(rows) = self else {
            return Liveness::Unknown;
        };
        let Some(a) = artifacts else {
            return Liveness::Unknown;
        };
        // A failed spawn writes no process identity, only the supervisor's
        // proof that no child existed. An identity file or any active marker
        // contradicts it.
        if super::codex_dispatch::has_launch_failure_record(a) {
            let contradicted = identity_path(a).exists()
                || rows.iter().any(|r| {
                    r.active()
                        && r.marker()
                            .map_or(true, |m| m == Some(a.schema_path.as_str()))
                });
            return if contradicted {
                Liveness::Unknown
            } else {
                Liveness::NotListed
            };
        }
        let Ok(identity) = read_identity(a) else {
            return Liveness::Unknown;
        };
        // Any ambiguous actual Codex command can hide our invocation.
        if rows.iter().any(|r| r.active() && r.marker().is_err()) {
            return Liveness::Unknown;
        }
        let matches: Vec<_> = rows
            .iter()
            .filter(|r| r.active() && r.marker().ok().flatten() == Some(a.schema_path.as_str()))
            .collect();
        if matches.len() == 1
            && matches[0].pid == identity.root_pid
            && matches[0].pgid == identity.root_pgid
        {
            return Liveness::Alive;
        }
        let completed = super::codex_dispatch::has_valid_exit_record(a);
        if !matches.is_empty() || recorded_tree_evidence(rows, &identity, completed).is_some() {
            return Liveness::Unknown;
        }
        if completed {
            Liveness::NotListed
        } else {
            Liveness::Unknown
        }
    }
    /// Fresh evidence that an issue's Codex work may still be running, or
    /// `None` when an available table shows nothing that could be it: its
    /// marker, another Autopilot marker for the issue, an ambiguous Codex row,
    /// or the recorded tree as [`recorded_tree_evidence`] judges it (recorded
    /// PIDs count only before a trusted completion record; the group after).
    pub fn live_evidence(
        &self,
        issue_slug: &str,
        artifacts: Option<&InvocationArtifacts>,
    ) -> Option<String> {
        let Self::Available(rows) = self else {
            return Some(self.reason());
        };
        for r in rows.iter().filter(|r| r.active()) {
            match r.marker() {
                Err(reason) => {
                    return Some(format!(
                        "process {} is an ambiguous Codex command: {reason}",
                        r.pid
                    ))
                }
                Ok(Some(marker)) if artifacts.is_some_and(|a| a.schema_path == marker) => {
                    return Some(format!(
                        "process {} carries this invocation's marker",
                        r.pid
                    ))
                }
                Ok(Some(marker)) if marker_issue_slug(marker) == Some(issue_slug) => {
                    return Some(format!(
                        "process {} runs an Autopilot invocation for {issue_slug}: {marker}",
                        r.pid
                    ))
                }
                _ => {}
            }
        }
        let a = artifacts?;
        let identity = read_identity(a).ok()?;
        recorded_tree_evidence(
            rows,
            &identity,
            super::codex_dispatch::has_valid_exit_record(a),
        )
    }
    pub fn orphan_markers(&self) -> Vec<&str> {
        match self {
            Self::Unavailable { .. } => Vec::new(),
            Self::Available(rows) => rows
                .iter()
                .filter(|r| r.active())
                .filter_map(|r| r.marker().ok().flatten())
                .filter(|p| {
                    let path = Path::new(p);
                    let Some(invocation) = path.parent() else {
                        return false;
                    };
                    let Some(issue) = invocation.parent() else {
                        return false;
                    };
                    let Some(autopilot) = issue.parent() else {
                        return false;
                    };
                    path.is_absolute()
                        && path.file_name().is_some_and(|s| s == "schema.json")
                        && invocation
                            .file_name()
                            .and_then(|s| s.to_str())
                            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
                        && issue
                            .file_name()
                            .and_then(|s| s.to_str())
                            .and_then(|s| s.rsplit_once('-'))
                            .is_some_and(|(_, number)| number.parse::<u64>().is_ok())
                        && autopilot.file_name().is_some_and(|s| s == "autopilot")
                        && autopilot
                            .parent()
                            .and_then(|p| p.file_name())
                            .is_some_and(|s| s == "ironmem")
                })
                .collect(),
        }
    }
}

#[cfg(unix)]
const STOP_SIGNAL: i32 = libc::SIGSTOP;
#[cfg(unix)]
const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const STOP_SIGNAL: i32 = 0;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 0;

/// Kernel birth identity prevents a recycled PID from authorizing a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Birth(u64, u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KernelProcess {
    pid: u32,
    ppid: u32,
    pgid: u32,
    birth: Birth,
}
fn birth(pid: u32) -> Result<Option<Birth>, String> {
    kernel_observation(pid).map(|process| process.map(|p| p.birth))
}

/// Ancestry and birth come from the same kernel record, never a stale ps row.
fn kernel_observation(pid: u32) -> Result<Option<KernelProcess>, String> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: the kernel writes at most the allocated proc_bsdinfo size.
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        if bytes == 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
                return Ok(None);
            }
            return Err(format!("cannot authenticate process {pid}: {error}"));
        }
        if bytes as usize != std::mem::size_of::<libc::proc_bsdinfo>() {
            return Err("incomplete process birth identity".into());
        }
        // SAFETY: a full structure was written by proc_pidinfo.
        let info = unsafe { info.assume_init() };
        if info.pbi_pid != pid {
            return Err("kernel process identity mismatch".into());
        }
        Ok(Some(KernelProcess {
            pid: info.pbi_pid,
            ppid: info.pbi_ppid,
            pgid: info.pbi_pgid,
            birth: Birth(info.pbi_start_tvsec, info.pbi_start_tvusec),
        }))
    }
    #[cfg(target_os = "linux")]
    {
        let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot authenticate process {pid}: {e}")),
        };
        let fields = text
            .rsplit_once(')')
            .ok_or("malformed proc stat")?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        let start = fields
            .get(19)
            .ok_or("missing proc birth identity")?
            .parse()
            .map_err(|_| "invalid proc birth identity")?;
        let record_pid = text
            .split_once(' ')
            .ok_or("missing proc pid")?
            .0
            .parse::<u32>()
            .map_err(|_| "invalid proc pid")?;
        if record_pid != pid {
            return Err("kernel process identity mismatch".into());
        }
        let ppid = fields
            .get(1)
            .ok_or("missing proc parent")?
            .parse()
            .map_err(|_| "invalid proc parent")?;
        let pgid = fields
            .get(2)
            .ok_or("missing proc group")?
            .parse()
            .map_err(|_| "invalid proc group")?;
        Ok(Some(KernelProcess {
            pid,
            ppid,
            pgid,
            birth: Birth(start, 0),
        }))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        Err("kernel process identity is unsupported on this platform".into())
    }
}

pub(crate) struct ProcessTracker {
    root_pid: u32,
    known: Vec<(ProcessRow, Birth)>,
    observed_root: bool,
    root_reaped: bool,
}
impl ProcessTracker {
    pub(crate) fn new(root_pid: u32) -> Self {
        Self {
            root_pid,
            known: Vec::new(),
            observed_root: false,
            root_reaped: false,
        }
    }
    fn rows() -> Result<Vec<ProcessRow>, MemoryError> {
        UnixProcesses.list().map_err(MemoryError::Validation)
    }

    pub(crate) fn observe(&mut self) -> Result<Vec<ProcessRow>, MemoryError> {
        self.observe_rows(Self::rows()?, kernel_observation)
    }

    fn observe_rows(
        &mut self,
        rows: Vec<ProcessRow>,
        mut kernel: impl FnMut(u32) -> Result<Option<KernelProcess>, String>,
    ) -> Result<Vec<ProcessRow>, MemoryError> {
        // Existing descendants remain owned only while their kernel birth
        // token matches. Keep those tokens with the ancestry seeds; a bare
        // PID from ps must never authorize enrollment of a new subtree.
        let mut seeds = std::collections::HashMap::new();
        for (old, expected) in &self.known {
            match kernel(old.pid).map_err(MemoryError::Validation)? {
                Some(actual) if actual.pid == old.pid && actual.birth == *expected => {
                    seeds.insert(old.pid, *expected);
                }
                Some(_) => {
                    return Err(MemoryError::Validation(
                        "tracked Codex PID was reused; cleanup cannot be authenticated".into(),
                    ));
                }
                None => {
                    // A shell that ran a tool and exited after the ps sample
                    // leaves a stale row for a child that is gone too; only a
                    // child the kernel still has escaped authentication.
                    for row in rows.iter().filter(|row| {
                        row.active()
                            && row.ppid == old.pid
                            && !self.known.iter().any(|(known, _)| known.pid == row.pid)
                    }) {
                        if kernel(row.pid).map_err(MemoryError::Validation)?.is_some() {
                            return Err(MemoryError::Validation(
                                "Codex ancestry seed disappeared before descendant authentication"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }
        // The spawned root's PID is reserved by our unreaped Child. Still
        // bind its ps ancestry/group to the same kernel birth observation.
        if let Some(root) = rows
            .iter()
            .find(|r| !self.root_reaped && r.pid == self.root_pid && r.active())
        {
            match kernel(root.pid).map_err(MemoryError::Validation)? {
                Some(actual) => {
                    if actual.pid != root.pid
                        || actual.ppid != root.ppid
                        || actual.pgid != root.pgid
                        || seeds
                            .get(&root.pid)
                            .is_some_and(|expected| *expected != actual.birth)
                    {
                        return Err(MemoryError::Validation(
                            "Codex root identity changed during authentication".into(),
                        ));
                    }
                    self.observed_root = true;
                    seeds.insert(root.pid, actual.birth);
                    if !self.known.iter().any(|(old, _)| old.pid == root.pid) {
                        self.known.push((root.clone(), actual.birth));
                    }
                }
                None => {
                    // The owned, unreaped root cannot be PID-reused. A stale
                    // active ps row may already have become an inert exit.
                    seeds.remove(&root.pid);
                }
            }
        }
        loop {
            let mut enrolled = false;
            for row in rows.iter().filter(|r| r.active()) {
                if self.known.iter().any(|(old, _)| old.pid == row.pid) {
                    continue;
                }
                let Some(expected_parent) = seeds.get(&row.ppid).copied() else {
                    continue;
                };
                let parent_before = kernel(row.ppid).map_err(MemoryError::Validation)?;
                if !parent_before.is_some_and(|p| p.pid == row.ppid && p.birth == expected_parent) {
                    return Err(MemoryError::Validation(
                        "Codex ancestry seed changed before descendant authentication".into(),
                    ));
                }
                let Some(actual) = kernel(row.pid).map_err(MemoryError::Validation)? else {
                    // A genuinely gone row supplies neither a signal target
                    // nor an ancestry seed. Ordinary short-lived tools race
                    // ps this way; it is distinct from an observed replacement.
                    continue;
                };
                // A fresh unrelated replacement cannot inherit an old ps
                // row's ownership: parent, group and birth are one record.
                if actual.pid != row.pid || actual.ppid != row.ppid {
                    return Err(MemoryError::Validation(
                        "Codex descendant ancestry changed during authentication".into(),
                    ));
                }
                let parent_after = kernel(row.ppid).map_err(MemoryError::Validation)?;
                if !parent_after.is_some_and(|p| p.pid == row.ppid && p.birth == expected_parent) {
                    return Err(MemoryError::Validation(
                        "Codex ancestry seed changed during descendant authentication".into(),
                    ));
                }
                let Some(candidate_after) = kernel(row.pid).map_err(MemoryError::Validation)?
                else {
                    // Completion between the two captures still supplies no
                    // signal target or seed, just as a gone first capture.
                    continue;
                };
                if candidate_after != actual {
                    return Err(MemoryError::Validation(
                        "Codex descendant kernel identity changed during authentication".into(),
                    ));
                }
                // The same owned parent bracketed a stable kernel capture.
                // setsid may have changed the group since ps: kernel PGID is
                // authoritative metadata, not an ownership input to PID kills.
                let mut authenticated = row.clone();
                authenticated.pgid = actual.pgid;
                self.known.push((authenticated, actual.birth));
                seeds.insert(row.pid, actual.birth);
                enrolled = true;
            }
            if !enrolled {
                break;
            }
        }
        Ok(rows)
    }

    fn kill_order(&self) -> impl Iterator<Item = u32> + '_ {
        self.known
            .iter()
            .rev()
            .map(|(r, _)| r.pid)
            .filter(|pid| *pid != self.root_pid)
            .chain(
                self.known
                    .iter()
                    .map(|(r, _)| r.pid)
                    .filter(|pid| *pid == self.root_pid),
            )
    }

    fn signal(&self, pid: u32, signal: i32) -> Result<(), MemoryError> {
        let Some((_, expected)) = self.known.iter().find(|(r, _)| r.pid == pid) else {
            return Err(MemoryError::Validation(
                "cannot signal unauthenticated Codex process".into(),
            ));
        };
        match birth(pid).map_err(MemoryError::Validation)? {
            None => Ok(()),
            Some(actual) if actual == *expected => {
                #[cfg(unix)]
                {
                    // SAFETY: positive PID checked against a kernel birth token.
                    if unsafe { libc::kill(pid as i32, signal) } != 0 {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() != Some(libc::ESRCH) {
                            return Err(e.into());
                        }
                    }
                    Ok(())
                }
                #[cfg(not(unix))]
                {
                    let _ = signal;
                    Err(MemoryError::Validation(
                        "Codex process signaling unsupported".into(),
                    ))
                }
            }
            Some(_) => Err(MemoryError::Validation(
                "Codex PID identity changed; refusing signal".into(),
            )),
        }
    }

    /// Freeze the owned tree before enumerating/killing it. Separate setsid
    /// groups remain children and are discovered through PPID, not PGID.
    /// Sampling is practical tool cleanup, not malicious fork containment.
    pub(crate) fn cleanup(&mut self, root_reaped: bool) -> Result<(), MemoryError> {
        self.root_reaped = root_reaped;
        let result = (|| {
            if !root_reaped {
                self.observe()?;
                self.signal(self.root_pid, STOP_SIGNAL)?;
            }
            let mut stable = false;
            for _ in 0..32 {
                let before = self.known.len();
                let rows = self.observe()?;
                for (row, _) in &self.known {
                    self.signal(row.pid, STOP_SIGNAL)?;
                }
                let stopped = rows
                    .iter()
                    .filter(|r| self.known.iter().any(|(k, _)| k.pid == r.pid) && r.active())
                    .all(|r| r.stat.starts_with('T'));
                if before == self.known.len() && stopped {
                    stable = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if !stable {
                return Err(MemoryError::Validation(
                    "Codex process tree did not quiesce".into(),
                ));
            }
            Ok(())
        })();
        // Continue best-effort authenticated cleanup after any uncertainty so
        // SIGSTOP cannot strand a known process. Never signal a stale group.
        let mut signal_error = None;
        for pid in self.kill_order() {
            if let Err(e) = self.signal(pid, KILL_SIGNAL) {
                signal_error = Some(e);
            }
        }
        result?;
        if let Some(e) = signal_error {
            return Err(e);
        }
        self.verify_gone()
    }
    pub(crate) fn verify_gone(&self) -> Result<(), MemoryError> {
        for _ in 0..100 {
            let rows = Self::rows()?;
            let mut live = false;
            for (old, expected) in &self.known {
                if rows.iter().any(|r| r.pid == old.pid && r.active()) {
                    match birth(old.pid).map_err(MemoryError::Validation)? {
                        None => {} // The ps row raced a completed process exit.
                        Some(actual) if actual == *expected => live = true,
                        Some(_) => {
                            return Err(MemoryError::Validation(
                                "Codex tracked PID changed during cleanup".into(),
                            ))
                        }
                    }
                }
            }
            if !live {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(MemoryError::Validation(
            "Codex descendants remained alive after cleanup".into(),
        ))
    }
    /// Once the root is reaped its children reparent, so PPID enrollment can
    /// no longer reach a tool it started after the last scan. The root's
    /// group still can: an active member cleanup never enrolled is
    /// unaccounted for, and completion must not be recorded over it. It gets
    /// a second's grace to finish a shutdown it began.
    pub(crate) fn verify_group_drained(&self) -> Result<(), MemoryError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut straggler = None;
        while std::time::Instant::now() < deadline {
            straggler = Self::rows()?
                .into_iter()
                .find(|r| {
                    r.active()
                        && r.pgid == self.root_pid
                        && !self.known.iter().any(|(k, _)| k.pid == r.pid)
                })
                .map(|r| r.pid);
            if straggler.is_none() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(MemoryError::Validation(format!(
            "untracked process {} remained in Codex process group {} after cleanup; \
             completion is unproven",
            straggler.unwrap_or_default(),
            self.root_pid
        )))
    }
    pub(crate) fn observed_root(&self) -> bool {
        self.observed_root
    }
    pub(crate) fn descendants(&self) -> Vec<ProcessRow> {
        self.known
            .iter()
            .filter(|(r, _)| r.pid != self.root_pid)
            .map(|(r, _)| r.clone())
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    pub(crate) fn invocation(dir: &Path) -> (InvocationArtifacts, ProcessRow) {
        let path = |name: &str| dir.join(name).to_str().unwrap().to_owned();
        let a = InvocationArtifacts {
            invocation_id: "invocation".into(),
            schema_path: path("schema.json"),
            journal_path: path("journal.jsonl"),
            final_path: path("final.json"),
            stderr_path: path("stderr.log"),
            exit_path: path("exit.json"),
        };
        persist_identity(
            &a,
            &ProcessIdentity {
                version: 1,
                root_pid: 1234,
                root_pgid: 1234,
                schema_path: a.schema_path.clone(),
                invocation_id: a.invocation_id.clone(),
                descendants: Vec::new(),
            },
        )
        .unwrap();
        let row = ProcessRow {
            pid: 1234,
            ppid: 1,
            pgid: 1234,
            stat: "S".into(),
            args: format!("codex exec --output-schema {} prompt", a.schema_path),
        };
        (a, row)
    }
    pub(crate) fn complete(a: &InvocationArtifacts) {
        std::fs::write(&a.exit_path, serde_json::to_vec(&serde_json::json!({"exit_code":0,"timed_out":false,"limit_exceeded":null,"completion":{"version":1,"invocation_id":a.invocation_id,"schema_path":a.schema_path}})).unwrap()).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_failures_malformed_identity_and_absent_completion_hold() {
        let dir = tempfile::tempdir().unwrap();
        let (a, row) = testing::invocation(dir.path());
        let empty = ProcessSnapshot::Available(Vec::new());
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        assert_eq!(
            ProcessSnapshot::Available(vec![row.clone()]).liveness(Some(&a)),
            Liveness::Alive
        );
        testing::complete(&a);
        assert_eq!(empty.liveness(Some(&a)), Liveness::NotListed);
        assert_eq!(
            ProcessSnapshot::Available(vec![row.clone()]).liveness(Some(&a)),
            Liveness::Alive,
            "live process beats an exit record"
        );
        let mut zombie = row.clone();
        zombie.stat = "Z".into();
        assert_eq!(
            ProcessSnapshot::Available(vec![zombie]).liveness(Some(&a)),
            Liveness::NotListed
        );
        let mut mismatched = row.clone();
        mismatched.pid += 1;
        mismatched.pgid += 1;
        assert!(ProcessSnapshot::Available(vec![mismatched])
            .invocation_reason(Some(&a))
            .contains("marker"));
        // The exit record proves the supervisor reaped the root, so its PID
        // on an unmarked process is a reuse.
        let mut reused = row;
        reused.args = "unrelated".into();
        assert_eq!(
            ProcessSnapshot::Available(vec![reused]).liveness(Some(&a)),
            Liveness::NotListed
        );
        assert!(empty.invocation_reason(Some(&a)).contains("completion"));
        std::fs::write(identity_path(&a), "bad-json").unwrap();
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        assert!(empty
            .invocation_reason(Some(&a))
            .contains("invalid Codex process identity"));
        std::fs::remove_file(identity_path(&a)).unwrap();
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        struct Failed;
        impl ProcessSource for Failed {
            fn list(&mut self) -> Result<Vec<ProcessRow>, String> {
                Err("source failed".into())
            }
        }
        assert_eq!(snapshot(&mut Failed).liveness(Some(&a)), Liveness::Unknown);
        assert!(parse_processes("partial row").is_err());
    }
    #[test]
    fn an_unrelated_non_utf8_argv_neither_hides_nor_fails_the_process_table() {
        // macOS ps prints argv bytes raw; any same-user process can carry
        // Latin-1. One such row must not abort a live dispatch's observation.
        let schema = "/tmp/.git/ironmem/autopilot/owner-repo-42/11111111-2222-3333-4444-555555555555/schema.json";
        let mut table = b"20 1 20 S perl -e sleep \xff\xfe\n".to_vec();
        table.extend(format!("21 1 21 S codex exec --output-schema {schema} prompt\n").bytes());
        let rows = decode_process_table(&table).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].marker().unwrap(), Some(schema));
        assert_eq!(rows[0].marker().unwrap(), None);
        assert!(decode_process_table(b"   \n").is_err());
    }
    #[test]
    fn an_unparseable_codex_command_is_ambiguous_only_if_it_could_carry_a_marker() {
        let marker = |args: &str| {
            parse_processes(&format!("30 1 30 S {args}\n")).unwrap()[0]
                .marker()
                .map(|m| m.map(str::to_owned))
        };
        // Someone else's codex exec: its own schema file, flags we do not list.
        assert_eq!(
            marker("codex exec --output-schema ./out.json --profile work prompt"),
            Ok(None)
        );
        assert_eq!(marker("codex exec --output-schema"), Ok(None));
        // Anything naming a schema.json could be hiding ours, so it still holds.
        assert!(marker(
            "codex exec --profile work --output-schema /r/.git/ironmem/autopilot/o-r-1/u/schema.json p"
        )
        .is_err());
        assert!(marker("codex exec --output-schema=/x/schema.json --profile w p").is_err());
    }
    #[test]
    fn unobserved_history_and_live_descendants_cannot_be_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut child) = testing::invocation(dir.path());
        child.pid = 2345;
        child.pgid = 2345;
        child.args = "python gate".into();
        let mut identity = read_identity(&a).unwrap();
        identity.descendants.push(child.clone());
        persist_identity(&a, &identity).unwrap();
        let live = |rows: Vec<ProcessRow>| ProcessSnapshot::Available(rows).liveness(Some(&a));
        let row = |pid, pgid| ProcessRow {
            pid,
            ppid: 1,
            pgid,
            stat: "S".into(),
            args: "unrelated".into(),
        };
        // No trusted completion: any recorded PID or the group could be it.
        assert_eq!(live(vec![child.clone()]), Liveness::Unknown);
        assert_eq!(live(vec![]), Liveness::Unknown);
        testing::complete(&a);
        // Cleanup proved every recorded process gone by birth identity, so a
        // recorded PID now is a reuse, not this invocation.
        assert_eq!(live(vec![child]), Liveness::NotListed);
        assert_eq!(
            live(vec![row(identity.root_pid, 4242)]),
            Liveness::NotListed
        );
        // A member of the root's group keeps the group, and so the
        // invocation, in question...
        assert_eq!(live(vec![row(4243, identity.root_pgid)]), Liveness::Unknown);
        // ...unless the root's PID itself was reused, which proves the old
        // group was gone first.
        assert_eq!(
            live(vec![
                row(identity.root_pid, identity.root_pgid),
                row(4244, identity.root_pgid)
            ]),
            Liveness::NotListed
        );
    }
    #[test]
    fn orphan_detection_requires_the_autopilot_invocation_layout() {
        let rows = parse_processes("10 1 10 S codex exec --output-schema /tmp/ironmem/autopilot/schema.json prompt\n11 1 11 S codex exec --output-schema /tmp/reviewer-schema.json prompt\n12 1 12 S codex exec --output-schema /tmp/.git/ironmem/autopilot/owner-repo-42/11111111-2222-3333-4444-555555555555/schema.json prompt\n").unwrap();
        assert_eq!(ProcessSnapshot::Available(rows).orphan_markers(), vec!["/tmp/.git/ironmem/autopilot/owner-repo-42/11111111-2222-3333-4444-555555555555/schema.json"]);
    }

    #[test]
    fn cleanup_orders_root_last_independent_of_ps_order() {
        let row = |pid| ProcessRow {
            pid,
            ppid: 1,
            pgid: pid,
            stat: "T".into(),
            args: "test".into(),
        };
        let tracker = ProcessTracker {
            root_pid: 10,
            known: vec![
                (row(20), Birth(1, 0)),
                (row(10), Birth(2, 0)),
                (row(30), Birth(3, 0)),
            ],
            observed_root: true,
            root_reaped: false,
        };
        let order = tracker.kill_order().collect::<Vec<_>>();
        assert_eq!(order.last(), Some(&10));
        assert_eq!(order.len(), 3);
    }
    #[test]
    fn legacy_unversioned_process_identity_does_not_authorize_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        testing::complete(&a);
        let mut wire = serde_json::to_value(read_identity(&a).unwrap()).unwrap();
        wire.as_object_mut().unwrap().remove("version");
        std::fs::write(identity_path(&a), serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(
            ProcessSnapshot::Available(Vec::new()).liveness(Some(&a)),
            Liveness::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_root_exit_with_detached_child_and_no_completion_holds() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.json");
        let code = format!("import subprocess,json; p=subprocess.Popen(['/usr/bin/python3','-c','import time; time.sleep(30)'],start_new_session=True); json.dump(dict(pid=p.pid),open({:?},'w'))", marker.to_str().unwrap());
        let mut root = Command::new("/usr/bin/python3")
            .args(["-c", &code])
            .process_group(0)
            .spawn()
            .unwrap();
        let root_pid = root.id();
        assert!(root.wait().unwrap().success());
        let child: serde_json::Value =
            serde_json::from_slice(&std::fs::read(marker).unwrap()).unwrap();
        let child_pid = child["pid"].as_u64().unwrap() as u32;
        struct Cleanup(u32, Option<Birth>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.1.is_some() && birth(self.0).ok() == Some(self.1) {
                    unsafe {
                        libc::kill(self.0 as i32, libc::SIGKILL);
                    }
                }
            }
        }
        let cleanup = Cleanup(child_pid, birth(child_pid).unwrap());
        let (a, _) = testing::invocation(dir.path());
        let rows = UnixProcesses.list().unwrap();
        let child_row = rows.iter().find(|r| r.pid == child_pid).unwrap().clone();
        assert!(child_row.active());
        assert_ne!(child_row.pgid, root_pid);
        persist_identity(
            &a,
            &ProcessIdentity {
                version: 1,
                root_pid,
                root_pgid: root_pid,
                schema_path: a.schema_path.clone(),
                invocation_id: a.invocation_id.clone(),
                descendants: vec![child_row],
            },
        )
        .unwrap();
        std::fs::write(
            &a.final_path,
            r#"{"verdict":"met","reason":"not proof of process completion"}"#,
        )
        .unwrap();
        assert_eq!(
            ProcessSnapshot::Available(rows).liveness(Some(&a)),
            Liveness::Unknown
        );
        drop(cleanup);
        // Even a later empty process view cannot supply missing historical
        // supervisor completion evidence after the controller disappeared.
        assert_eq!(
            ProcessSnapshot::Available(Vec::new()).liveness(Some(&a)),
            Liveness::Unknown
        );
    }
    fn process_row(pid: u32, ppid: u32, pgid: u32) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            pgid,
            stat: "S".into(),
            args: "fixture".into(),
        }
    }
    fn kernel_process(pid: u32, ppid: u32, pgid: u32, start: u64) -> KernelProcess {
        KernelProcess {
            pid,
            ppid,
            pgid,
            birth: Birth(start, 0),
        }
    }

    #[test]
    fn stale_descendant_row_cannot_enroll_a_reused_unrelated_pid() {
        let root = kernel_process(10, 1, 10, 1);
        for replacement in [Some(kernel_process(20, 999, 999, 2)), None] {
            let mut tracker = ProcessTracker::new(10);
            let result = tracker.observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 20)],
                |pid| Ok(if pid == 10 { Some(root) } else { replacement }),
            );
            if replacement.is_some() {
                assert!(
                    result.is_err(),
                    "stale ancestry must hold, not enroll the replacement"
                );
            } else {
                assert!(
                    result.is_ok(),
                    "a genuinely gone tool must not abort an ordinary live invocation"
                );
            }
            assert!(!tracker.known.iter().any(|(row, _)| row.pid == 20));
            assert!(tracker
                .signal(20, STOP_SIGNAL)
                .unwrap_err()
                .to_string()
                .contains("unauthenticated"));
        }
    }

    #[test]
    fn replaced_parent_cannot_authorize_a_new_subtree() {
        let root = kernel_process(10, 1, 10, 1);
        let parent = kernel_process(20, 10, 20, 2);
        let replacement_parent = kernel_process(20, 999, 999, 3);
        let child = kernel_process(30, 20, 30, 4);
        let mut tracker = ProcessTracker::new(10);
        tracker.known.push((process_row(20, 10, 20), parent.birth));
        let mut parent_reads = 0;
        let result = tracker.observe_rows(
            vec![
                process_row(10, 1, 10),
                process_row(20, 10, 20),
                process_row(30, 20, 30),
            ],
            |pid| {
                Ok(match pid {
                    10 => Some(root),
                    20 => {
                        parent_reads += 1;
                        Some(if parent_reads <= 2 {
                            parent
                        } else {
                            replacement_parent
                        })
                    }
                    30 => Some(child),
                    _ => unreachable!(),
                })
            },
        );
        assert!(result.is_err(), "parent birth changed during enrollment");
        assert!(!tracker.known.iter().any(|(row, _)| row.pid == 30));
        assert!(tracker
            .signal(30, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }

    #[test]
    fn authenticated_detached_child_is_enrolled_by_its_real_parent() {
        let root = kernel_process(10, 1, 10, 1);
        let child = kernel_process(20, 10, 20, 2);
        let mut tracker = ProcessTracker::new(10);
        tracker
            .observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 10)],
                |pid| Ok(Some(if pid == 10 { root } else { child })),
            )
            .unwrap();
        assert!(tracker.known.iter().any(|(row, token)| row.pid == 20
            && row.pgid != tracker.root_pid
            && *token == child.birth));
    }
    #[test]
    fn vanished_known_parent_cannot_authorize_a_snapshot_child() {
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::new(10);
        tracker.known.push((process_row(20, 10, 20), Birth(2, 0)));
        let result = tracker.observe_rows(
            vec![
                process_row(10, 1, 10),
                process_row(20, 10, 20),
                process_row(30, 20, 30),
            ],
            |pid| {
                Ok(match pid {
                    10 => Some(root),
                    20 => None,
                    30 => Some(kernel_process(30, 20, 30, 4)),
                    _ => unreachable!(),
                })
            },
        );
        assert!(
            result.is_err(),
            "lost ancestry evidence must hold the snapshot child"
        );
        assert!(!tracker.known.iter().any(|(row, _)| row.pid == 30));
        assert!(tracker
            .signal(30, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }

    #[test]
    fn a_tool_that_exited_with_its_vanished_parent_does_not_abort_observation() {
        // A shell ran a short-lived tool and both exited between the ps
        // sample and the kernel check: the stale child row escaped nothing.
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::new(10);
        tracker.known.push((process_row(20, 10, 20), Birth(2, 0)));
        tracker
            .observe_rows(
                vec![
                    process_row(10, 1, 10),
                    process_row(20, 10, 20),
                    process_row(30, 20, 30),
                ],
                |pid| Ok(if pid == 10 { Some(root) } else { None }),
            )
            .unwrap();
        assert!(!tracker.known.iter().any(|(row, _)| row.pid == 30));
    }

    #[test]
    fn kernel_lookup_failure_does_not_authorize_a_descendant_signal() {
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::new(10);
        let result = tracker.observe_rows(
            vec![process_row(10, 1, 10), process_row(20, 10, 20)],
            |pid| {
                if pid == 10 {
                    Ok(Some(root))
                } else {
                    Err("kernel source failed".into())
                }
            },
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("kernel source failed"));
        assert!(!tracker.known.iter().any(|(row, _)| row.pid == 20));
        assert!(tracker
            .signal(20, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }
    #[cfg(unix)]
    #[test]
    fn hung_ps_is_bounded_in_an_isolated_environment() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        const CHILD: &str = "IRONMEM_BOUNDED_PS_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let started = Instant::now();
            let error = UnixProcesses.list().unwrap_err();
            assert!(error.contains("exceeded"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(5));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ps = dir.path().join("ps");
        let pid_file = dir.path().join("ps.pid");
        std::fs::write(
            &ps,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$IRONMEM_BOUNDED_PS_PID\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "autopilot::process_tree::tests::hung_ps_is_bounded_in_an_isolated_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PATH", dir.path())
            .env("IRONMEM_BOUNDED_PS_PID", &pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut observed_ps = None;
        let finished = loop {
            if observed_ps.is_none() {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.parse::<u32>() {
                        observed_ps = Some((pid, birth(pid).unwrap()));
                    }
                }
            }
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if started.elapsed() >= Duration::from_secs(6) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if !finished {
            let _ = child.kill();
        }
        // The watchdog cleans the recorded fake ps even on red. Its birth
        // identity must still match before this test sends any signal.
        if let Some((pid, Some(expected))) = observed_ps {
            if birth(pid).unwrap() == Some(expected) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
        let output = child.wait_with_output().unwrap();
        assert!(finished, "hung ps outlived the process-source bound");
        assert!(
            output.status.success(),
            "isolated source test failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            observed_ps.is_some(),
            "the hanging ps fixture was actually invoked"
        );
    }
    #[test]
    fn candidate_kernel_identity_must_be_stable_across_parent_authentication() {
        let root = kernel_process(10, 1, 10, 1);
        let candidate = kernel_process(20, 10, 20, 2);
        for changed in [
            kernel_process(20, 10, 20, 3),
            kernel_process(20, 10, 30, 2),
            kernel_process(20, 999, 20, 2),
        ] {
            let mut tracker = ProcessTracker::new(10);
            let mut candidate_reads = 0;
            let result = tracker.observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 10)],
                |pid| {
                    Ok(Some(if pid == 10 {
                        root
                    } else {
                        candidate_reads += 1;
                        if candidate_reads == 1 {
                            candidate
                        } else {
                            changed
                        }
                    }))
                },
            );
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("kernel identity changed"));
            assert!(!tracker.known.iter().any(|(row, _)| row.pid == 20));
            assert!(tracker
                .signal(20, STOP_SIGNAL)
                .unwrap_err()
                .to_string()
                .contains("unauthenticated"));
        }
    }
    #[test]
    fn candidate_gone_before_final_capture_is_neither_enrolled_nor_signaled() {
        let root = kernel_process(10, 1, 10, 1);
        let candidate = kernel_process(20, 10, 20, 2);
        let mut tracker = ProcessTracker::new(10);
        let mut reads = 0;
        let result = tracker.observe_rows(
            vec![process_row(10, 1, 10), process_row(20, 10, 10)],
            |pid| {
                if pid == 10 {
                    Ok(Some(root))
                } else {
                    reads += 1;
                    Ok(if reads == 1 { Some(candidate) } else { None })
                }
            },
        );
        assert!(
            result.is_ok(),
            "a completed short-lived tool is not a PID replacement"
        );
        assert!(!tracker.known.iter().any(|(row, _)| row.pid == 20));
        assert!(tracker
            .signal(20, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }
}
