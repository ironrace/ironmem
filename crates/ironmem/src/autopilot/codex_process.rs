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

use crate::autopilot::codex_dispatch::CodexArtifacts;
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
    pub fn marker(&self) -> Result<Option<&str>, String> {
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

pub fn parse_codex_processes(text: &str) -> Result<Vec<ProcessRow>, String> {
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
pub enum CodexProcessSnapshot {
    Available(Vec<ProcessRow>),
    Unavailable { reason: String },
}

pub trait CodexProcessSource {
    fn list(&mut self) -> Result<Vec<ProcessRow>, String>;
}

pub struct UnixCodexProcesses;
impl CodexProcessSource for UnixCodexProcesses {
    fn list(&mut self) -> Result<Vec<ProcessRow>, String> {
        #[cfg(unix)]
        {
            let output = Command::new("ps")
                .args(["-axww", "-o", "pid=,ppid=,pgid=,stat=,args="])
                .output()
                .map_err(|e| format!("cannot execute ps: {e}"))?;
            if !output.status.success() {
                return Err("ps exited unsuccessfully".into());
            }
            let text = std::str::from_utf8(&output.stdout).map_err(|_| "ps output is not UTF-8")?;
            if text.trim().is_empty() {
                return Err("ps returned no process table".into());
            }
            parse_codex_processes(text)
        }
        #[cfg(not(unix))]
        {
            Err("Codex process observation is unsupported on this platform".into())
        }
    }
}

pub fn snapshot(source: &mut dyn CodexProcessSource) -> CodexProcessSnapshot {
    match source.list() {
        Ok(rows) => CodexProcessSnapshot::Available(rows),
        Err(reason) => CodexProcessSnapshot::Unavailable { reason },
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

fn identity_path(a: &CodexArtifacts) -> std::path::PathBuf {
    Path::new(&a.schema_path).with_file_name("process.json")
}

pub(crate) fn persist_identity(
    a: &CodexArtifacts,
    identity: &ProcessIdentity,
) -> Result<(), MemoryError> {
    let path = identity_path(a);
    let tmp = path.with_file_name(format!("process-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec(identity)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(
            path.parent()
                .ok_or_else(|| MemoryError::Validation("missing process manifest parent".into()))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub(crate) fn read_identity(a: &CodexArtifacts) -> Result<ProcessIdentity, String> {
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

impl CodexProcessSnapshot {
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
    pub fn invocation_reason(&self, artifacts: Option<&CodexArtifacts>) -> String {
        let Self::Available(rows) = self else {
            return self.reason();
        };
        let Some(a) = artifacts else {
            return "Codex invocation has no persisted artifacts".into();
        };
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
        if rows.iter().any(|r| {
            r.active()
                && (r.pid == identity.root_pid
                    || r.pgid == identity.root_pgid
                    || identity.descendants.iter().any(|d| d.pid == r.pid))
        }) {
            return "Codex root or tracked descendant is present; process identity/completion requires verification".into();
        }
        if !super::codex_dispatch::has_valid_exit_record(a) {
            return "Codex invocation has no trusted supervisor cleanup/completion record; historical descendants cannot be ruled out".into();
        }
        "Codex supervisor completion verified".into()
    }
    pub fn liveness(&self, artifacts: Option<&CodexArtifacts>) -> Liveness {
        let Self::Available(rows) = self else {
            return Liveness::Unknown;
        };
        let Some(a) = artifacts else {
            return Liveness::Unknown;
        };
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
        if !matches.is_empty()
            || rows.iter().any(|r| {
                r.active()
                    && (r.pid == identity.root_pid
                        || r.pgid == identity.root_pgid
                        || identity.descendants.iter().any(|d| d.pid == r.pid))
            })
        {
            return Liveness::Unknown;
        }
        if super::codex_dispatch::has_valid_exit_record(a) {
            Liveness::NotListed
        } else {
            Liveness::Unknown
        }
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
fn birth(pid: u32) -> Result<Option<Birth>, String> {
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
        Ok(Some(Birth(info.pbi_start_tvsec, info.pbi_start_tvusec)))
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
        Ok(Some(Birth(start, 0)))
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
        UnixCodexProcesses.list().map_err(MemoryError::Validation)
    }

    pub(crate) fn observe(&mut self) -> Result<Vec<ProcessRow>, MemoryError> {
        let rows = Self::rows()?;
        // PID ownership is reserved until Child::try_wait/wait reaps the root.
        // Known descendants are revalidated against their kernel birth token.
        let mut seeds = std::collections::HashSet::new();
        for (old, expected) in &self.known {
            match birth(old.pid).map_err(MemoryError::Validation)? {
                Some(actual) if actual == *expected => {
                    seeds.insert(old.pid);
                }
                Some(_) => {
                    return Err(MemoryError::Validation(
                        "tracked Codex PID was reused; cleanup cannot be authenticated".into(),
                    ))
                }
                None => {}
            }
        }
        if let Some(root) = rows
            .iter()
            .find(|r| !self.root_reaped && r.pid == self.root_pid && r.active())
        {
            self.observed_root = true;
            seeds.insert(root.pid);
        }
        loop {
            let before = seeds.len();
            for row in rows.iter().filter(|r| r.active()) {
                if seeds.contains(&row.ppid) {
                    seeds.insert(row.pid);
                }
            }
            if before == seeds.len() {
                break;
            }
        }
        for row in rows.iter().filter(|r| r.active() && seeds.contains(&r.pid)) {
            if !self.known.iter().any(|(old, _)| old.pid == row.pid) {
                if let Some(token) = birth(row.pid).map_err(MemoryError::Validation)? {
                    self.known.push((row.clone(), token));
                }
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
    pub(crate) fn invocation(dir: &Path) -> (CodexArtifacts, ProcessRow) {
        let path = |name: &str| dir.join(name).to_str().unwrap().to_owned();
        let a = CodexArtifacts {
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
    pub(crate) fn complete(a: &CodexArtifacts) {
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
        let empty = CodexProcessSnapshot::Available(Vec::new());
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        assert_eq!(
            CodexProcessSnapshot::Available(vec![row.clone()]).liveness(Some(&a)),
            Liveness::Alive
        );
        testing::complete(&a);
        assert_eq!(empty.liveness(Some(&a)), Liveness::NotListed);
        assert_eq!(
            CodexProcessSnapshot::Available(vec![row.clone()]).liveness(Some(&a)),
            Liveness::Alive,
            "live process beats an exit record"
        );
        let mut zombie = row.clone();
        zombie.stat = "Z".into();
        assert_eq!(
            CodexProcessSnapshot::Available(vec![zombie]).liveness(Some(&a)),
            Liveness::NotListed
        );
        let mut mismatched = row.clone();
        mismatched.pid += 1;
        mismatched.pgid += 1;
        assert!(CodexProcessSnapshot::Available(vec![mismatched])
            .invocation_reason(Some(&a))
            .contains("marker"));
        let mut reused = row;
        reused.args = "unrelated".into();
        assert_eq!(
            CodexProcessSnapshot::Available(vec![reused]).liveness(Some(&a)),
            Liveness::Unknown
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
        impl CodexProcessSource for Failed {
            fn list(&mut self) -> Result<Vec<ProcessRow>, String> {
                Err("source failed".into())
            }
        }
        assert_eq!(snapshot(&mut Failed).liveness(Some(&a)), Liveness::Unknown);
        assert!(parse_codex_processes("partial row").is_err());
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
        testing::complete(&a);
        assert_eq!(
            CodexProcessSnapshot::Available(vec![child]).liveness(Some(&a)),
            Liveness::Unknown
        );
        std::fs::remove_file(&a.exit_path).unwrap();
        assert_eq!(
            CodexProcessSnapshot::Available(vec![]).liveness(Some(&a)),
            Liveness::Unknown
        );
    }
    #[test]
    fn orphan_detection_requires_the_autopilot_invocation_layout() {
        let rows = parse_codex_processes("10 1 10 S codex exec --output-schema /tmp/ironmem/autopilot/schema.json prompt\n11 1 11 S codex exec --output-schema /tmp/reviewer-schema.json prompt\n12 1 12 S codex exec --output-schema /tmp/.git/ironmem/autopilot/owner-repo-42/11111111-2222-3333-4444-555555555555/schema.json prompt\n").unwrap();
        assert_eq!(CodexProcessSnapshot::Available(rows).orphan_markers(), vec!["/tmp/.git/ironmem/autopilot/owner-repo-42/11111111-2222-3333-4444-555555555555/schema.json"]);
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
            CodexProcessSnapshot::Available(Vec::new()).liveness(Some(&a)),
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
        let rows = UnixCodexProcesses.list().unwrap();
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
            CodexProcessSnapshot::Available(rows).liveness(Some(&a)),
            Liveness::Unknown
        );
        drop(cleanup);
        // Even a later empty process view cannot supply missing historical
        // supervisor completion evidence after the controller disappeared.
        assert_eq!(
            CodexProcessSnapshot::Available(Vec::new()).liveness(Some(&a)),
            Liveness::Unknown
        );
    }
}
