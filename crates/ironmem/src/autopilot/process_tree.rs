//! Local process evidence for supervised CLI invocations (Codex, Muse). Missing evidence means unknown.
//!
//! Restart adoption verifies the native harness root's exact invocation
//! marker. A launcher wrapper whose marker appears only on a different native
//! child conservatively holds until trusted supervisor completion. Process
//! polling and authenticated tree cleanup cover ordinary harness tools,
//! including setsid children; they do not contain deliberate double-fork
//! escapes between polls. A short-lived tool's orphaned background child is
//! caught at completion instead, while its working directory is still in the
//! worktree (`ProcessTracker::verify_worktree_quiet`). The operator's shared ironmem daemon on its
//! configured socket, running the controller's own executable, which a
//! harness's MCP proxy may autospawn, is never owned; a tool disguised as it
//! escapes tracking the same way.
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::autopilot::invocation::InvocationArtifacts;
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

    /// ps does not preserve argv boundaries. Only argv words the harness
    /// grammar verifies as options can supply an invocation marker: Codex's
    /// option prefix before its first positional prompt, or Muse's whole
    /// argv, which Autopilot passes with no positional prompt.
    ///
    /// An unparseable argv is ambiguous only when the row could carry an
    /// Autopilot marker at all. Every marker is a whitespace-free absolute
    /// path (artifact paths refuse whitespace) of the Autopilot invocation
    /// layout, so a row with no such word is someone else's `codex exec` or
    /// `muse exec`, and holding every dispatch on it would let any unrelated
    /// session stall Autopilot.
    pub fn marker(&self) -> Result<Option<&str>, String> {
        self.parse_marker().or_else(|reason| {
            if marker_words(&self.args).next().is_some() {
                Err(reason)
            } else {
                Ok(None)
            }
        })
    }

    fn parse_marker(&self) -> Result<Option<&str>, String> {
        fn file_name(word: &str) -> Option<&str> {
            Path::new(word).file_name().and_then(|a| a.to_str())
        }
        let mut args = self.args.split_whitespace();
        let Some(mut exe) = args.next().and_then(file_name) else {
            return Ok(None);
        };
        // `~/.local/bin/muse` is a bash script that `exec`s
        // `muse-bin-<version>` under the same PID (measured), so until that
        // exec ps shows the root as `bash …/muse exec …`. That is the root
        // itself, carrying its own marker.
        if exe == "bash" || exe == "sh" {
            let mut script = args.clone();
            if script.next().and_then(file_name) != Some("muse") {
                return Ok(None);
            }
            args = script;
            exe = "muse";
        }
        let codex = exe == "codex";
        // After that exec ps shows the versioned binary.
        let muse = exe == "muse" || exe.starts_with("muse-bin-");
        if !(codex || muse) || args.next() != Some("exec") {
            return Ok(None);
        }
        if !args
            .clone()
            .any(|a| a == "--output-schema" || a.starts_with("--output-schema="))
        {
            return Ok(None);
        }
        if codex {
            parse_codex_options(args)
        } else {
            parse_muse_options(args)
        }
    }

    /// Whether this row is the operator's shared ironmem daemon, which a
    /// harness's `ironmem serve --connect` MCP proxy may autospawn as its
    /// own child (`mcp::daemon::spawn_daemon_process`). It serves every
    /// other session, so supervision must never own it. Matches only that
    /// spawn's exact argv ([`Self::daemon_listen_socket`]) listening on
    /// `daemon.socket`, the operator's configured shared socket
    /// (`Config::daemon_socket_path`, the path the registered MCP proxies
    /// connect to). A daemon on any other socket, such as one an ironmem
    /// test suite autospawns on a temp socket, is an ordinary tool and is
    /// cleaned up.
    ///
    /// ps loses argv boundaries, so a socket or db path containing
    /// whitespace is never recognised: that daemon is tracked and killed as
    /// before, which fails safe, and [`SharedDaemon::for_current_exe`]
    /// warns about such a socket.
    ///
    /// This is only the argv half. The tracker also requires the kernel's
    /// executable path to be the controller's own canonical ironmem binary
    /// ([`SharedDaemon`]), so an agent-built `target/debug/ironmem` with the
    /// same argv is still owned. Trade-off: a tool that runs that installed
    /// binary itself, with the same argv on the configured socket in a group
    /// of its own, escapes tracking. That is the same class as the
    /// double-fork escape this module already does not contain.
    pub(crate) fn is_shared_ironmem_daemon(&self, daemon: &SharedDaemon) -> bool {
        daemon.socket.to_str().is_some_and(|socket| {
            !socket.chars().any(char::is_whitespace)
                && self.daemon_listen_socket(daemon.exe.file_name()) == Some(socket)
        })
    }

    /// The socket this row listens on when it has the shape of an ironmem
    /// daemon: `<…/exe> serve --listen <socket> [--db <path>]`, exactly, on
    /// a process leading its own group (the spawn uses `process_group(0)`),
    /// where `exe` is named `ironmem` or `exe_name` (the controller's own
    /// executable, which a versioned install names otherwise). Only the
    /// shape: the kernel's executable path is checked separately.
    pub(crate) fn daemon_listen_socket(&self, exe_name: Option<&std::ffi::OsStr>) -> Option<&str> {
        if self.pgid != self.pid {
            return None;
        }
        let words: Vec<&str> = self.args.split_whitespace().collect();
        let name = Path::new(words.first()?).file_name()?;
        if name != "ironmem" && Some(name) != exe_name {
            return None;
        }
        match words[1..] {
            ["serve", "--listen", listen] | ["serve", "--listen", listen, "--db", _] => {
                Some(listen)
            }
            _ => None,
        }
    }
}

/// Every word of `args` shaped like an Autopilot marker, including the
/// value of an `--option=<path>` word, which hides the path behind its name.
fn marker_words(args: &str) -> impl Iterator<Item = &str> {
    args.split_whitespace().filter_map(|word| {
        if is_autopilot_marker_path(word) {
            Some(word)
        } else {
            word.split_once('=')
                .map(|(_, value)| value)
                .filter(|value| is_autopilot_marker_path(value))
        }
    })
}

fn parse_codex_options<'a>(args: impl Iterator<Item = &'a str>) -> Result<Option<&'a str>, String> {
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

/// `muse exec` options measured on 1.4.13 (`muse exec --help`). Autopilot
/// passes the prompt with `--prompt-file`, so its own rows have no positional
/// word; any positional makes the rest unparseable.
const MUSE_FLAGS: &[&str] = &[
    "--json",
    "--api-key-stdin",
    "--parallel-tool-calls",
    "--no-parallel-tool-calls",
    "--allow-workspace-switch",
    "--user-input-auto-resolve",
    "--subagent-worktree-isolation",
    "--disable-web-tools",
    "--disable-reminders",
    "--disable-request-user-input",
    "--no-foreign-personal-context",
    "--no-session-log",
    "--yolo",
    "--trust-workspace",
    "--disable-approval",
    "--disable-sandbox",
    "--disable-write",
    "--disable-shell",
    "--enable-shell-tool",
];
const MUSE_VALUED: &[&str] = &[
    "--prompt-file",
    "--provider",
    "--preset",
    "--permission-profile",
    "--model",
    "--contributor-mode",
    "-c",
    "--config",
    "--reasoning-effort",
    "--effort",
    "--base-url",
    "--image",
    "--workspace",
    "--worktree-base",
    "--worktree-existing",
    "--context-compaction-strategy",
    "--context-compaction-soft-threshold",
    "--context-compaction-hard-threshold",
    "--max-model-steps",
    "--max-tool-output-bytes",
    "--session-id",
    "--approval-mode",
    "--approval-judge",
    "--sandbox-network",
];

fn parse_muse_options<'a>(
    mut words: impl Iterator<Item = &'a str>,
) -> Result<Option<&'a str>, String> {
    let mut marker = None;
    let mut take_marker = |path: &'a str| -> Result<(), String> {
        if !Path::new(path).is_absolute() || marker.replace(path).is_some() {
            return Err("ambiguous Muse output-schema marker".into());
        }
        Ok(())
    };
    while let Some(word) = words.next() {
        if word == "--output-schema" {
            let path = words
                .next()
                .ok_or("Muse output-schema option has no path")?;
            take_marker(path)?;
        } else if let Some(path) = word.strip_prefix("--output-schema=") {
            take_marker(path)?;
        } else if MUSE_FLAGS.contains(&word) {
        } else if MUSE_VALUED.contains(&word) {
            words.next().ok_or("Muse option has no value")?;
        } else if word
            .split_once('=')
            .is_some_and(|(name, _)| MUSE_VALUED.contains(&name))
        {
        } else if word == "-w" || word == "--worktree" || word.starts_with("--worktree=") {
            return Err("Muse --worktree takes an optional value; the argv is ambiguous".into());
        } else if word.starts_with('-') {
            return Err("unrecognized Muse option".into());
        } else {
            return Err("Muse argv carries a positional prompt; its options are ambiguous".into());
        }
    }
    Ok(marker)
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

/// Whether `path` has the Autopilot invocation layout:
/// `/…/ironmem/autopilot/<slug>-<number>/<uuid>/schema.json`.
pub(crate) fn is_autopilot_marker_path(path: &str) -> bool {
    let path = Path::new(path);
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
            Err("process observation is unsupported on this platform".into())
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

/// Written immediately after spawn, then again whenever the root's birth is
/// first observed or the live descendant set changes. A terminal record is
/// separate evidence that the supervising controller finished cleanup, not
/// merely that PID died.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessIdentity {
    pub version: u32,
    pub root_pid: u32,
    pub root_pgid: u32,
    /// The root's kernel birth, once observed; absent from older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_birth: Option<Birth>,
    pub schema_path: String,
    pub invocation_id: String,
    /// Descendants alive at the supervisor's last sample.
    pub descendants: Vec<RecordedProcess>,
}

/// One recorded descendant. Only its PID, group, parent and birth are read
/// back; the command is kept, bounded and scrubbed, for an operator reading
/// the file. Older records (full `ProcessRow`s, no birth) still parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedProcess {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub birth: Option<Birth>,
}

/// The longest command a recorded descendant keeps, in bytes.
const RECORDED_ARGS_LIMIT: usize = 256;

impl RecordedProcess {
    fn new(row: &ProcessRow, birth: Birth) -> Self {
        let mut args =
            crate::autopilot::scrub::scrub_and_bound(&row.args, RECORDED_ARGS_LIMIT).text;
        let mut end = args.len().min(RECORDED_ARGS_LIMIT);
        while !args.is_char_boundary(end) {
            end -= 1;
        }
        args.truncate(end);
        Self {
            pid: row.pid,
            ppid: row.ppid,
            pgid: row.pgid,
            args,
            birth: Some(birth),
        }
    }
}

pub(crate) fn identity_path(a: &InvocationArtifacts) -> std::path::PathBuf {
    Path::new(&a.schema_path).with_file_name("process.json")
}

/// The most bytes an identity file may hold: [`read_identity`] refuses
/// more, and [`persist_identity`] never writes more.
const IDENTITY_LIMIT: usize = 1024 * 1024;

/// Write `identity` within [`IDENTITY_LIMIT`]. Past it, each descendant's
/// command (kept only for an operator) is dropped, and every PID, group,
/// parent and birth recovery reads is still written; an identity too large
/// even then is refused, leaving the previous file in place, rather than
/// written unreadable.
pub(crate) fn persist_identity(
    a: &InvocationArtifacts,
    identity: &ProcessIdentity,
) -> Result<(), MemoryError> {
    let mut bytes = serde_json::to_vec(identity)?;
    if bytes.len() > IDENTITY_LIMIT {
        let mut bare = identity.clone();
        for descendant in &mut bare.descendants {
            descendant.args.clear();
        }
        bytes = serde_json::to_vec(&bare)?;
    }
    if bytes.len() > IDENTITY_LIMIT {
        return Err(MemoryError::Validation(format!(
            "invocation process identity for {} descendants exceeds its {IDENTITY_LIMIT}-byte limit",
            identity.descendants.len()
        )));
    }
    super::invocation::persist_private_atomic(&identity_path(a), "process", &bytes)
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
        .map_err(|e| format!("missing invocation process identity: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("invocation identity is not a regular file".into());
    }
    let mut data = Vec::new();
    file.take(IDENTITY_LIMIT as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > IDENTITY_LIMIT {
        return Err("invocation identity exceeds size limit".into());
    }
    let identity: ProcessIdentity = serde_json::from_slice(&data)
        .map_err(|e| format!("invalid invocation process identity: {e}"))?;
    if identity.version != 1
        || identity.root_pid == 0
        || identity.root_pgid != identity.root_pid
        || identity.schema_path != a.schema_path
        || identity.invocation_id != a.invocation_id
    {
        return Err("inconsistent invocation process identity".into());
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
///
/// A recorded descendant's PID now running the operator's shared daemon
/// (`is_daemon`) is not evidence: a sample before the daemon's exec may
/// have recorded it, and a cleanup that failed before its release pass
/// left it there.
fn recorded_tree_evidence(
    rows: &[ProcessRow],
    identity: &ProcessIdentity,
    completed: bool,
    is_daemon: &dyn Fn(&ProcessRow) -> bool,
) -> Option<String> {
    let active = || rows.iter().filter(|r| r.active());
    if !completed {
        if let Some(r) = active().find(|r| {
            r.pid == identity.root_pid
                || (identity.descendants.iter().any(|d| d.pid == r.pid) && !is_daemon(r))
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

/// The operator's shared daemon, read on demand: loading it reads the
/// configuration, which most liveness checks never need.
pub(crate) type DaemonSource<'a> = &'a dyn Fn() -> Option<SharedDaemon>;

/// Whether a row is the shared daemon `daemon` names, by argv and the
/// kernel's executable path; `daemon` is read at most once.
fn daemon_check(daemon: DaemonSource<'_>) -> impl Fn(&ProcessRow) -> bool + '_ {
    let loaded = std::cell::OnceCell::new();
    move |row| is_shared_daemon(loaded.get_or_init(daemon).as_ref(), kernel_executable, row)
}

impl ProcessSnapshot {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
    pub fn reason(&self) -> String {
        match self {
            Self::Unavailable { reason } => reason.clone(),
            Self::Available(_) => {
                "invocation identity or completion evidence is unavailable or inconsistent".into()
            }
        }
    }
    pub fn invocation_reason(&self, artifacts: Option<&InvocationArtifacts>) -> String {
        self.invocation_reason_with(artifacts, &|| None)
    }
    /// [`Self::invocation_reason`] with the operator's shared daemon
    /// (`daemon`, read only when a recorded PID is found running), whose
    /// PID a pre-exec sample may have recorded.
    pub(crate) fn invocation_reason_with(
        &self,
        artifacts: Option<&InvocationArtifacts>,
        daemon: DaemonSource<'_>,
    ) -> String {
        let Self::Available(rows) = self else {
            return self.reason();
        };
        let Some(a) = artifacts else {
            return "invocation has no persisted artifacts".into();
        };
        if super::invocation::has_launch_failure_record(a) {
            return "invocation never launched, but an active process carries its marker or an ambiguous one; holding for verification".into();
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
            return "invocation schema marker matched an unexpected or ambiguous process identity; holding for verification".into();
        }
        let completed = super::invocation::has_valid_exit_record(a);
        if let Some(evidence) =
            recorded_tree_evidence(rows, &identity, completed, &daemon_check(daemon))
        {
            return format!("{evidence}; process identity/completion requires verification");
        }
        if !completed {
            return "invocation has no trusted supervisor cleanup/completion record; historical descendants cannot be ruled out".into();
        }
        "supervisor completion verified".into()
    }
    pub fn liveness(&self, artifacts: Option<&InvocationArtifacts>) -> Liveness {
        self.liveness_with(artifacts, &|| None)
    }
    /// [`Self::liveness`] with the operator's shared daemon, as
    /// [`Self::invocation_reason_with`].
    pub(crate) fn liveness_with(
        &self,
        artifacts: Option<&InvocationArtifacts>,
        daemon: DaemonSource<'_>,
    ) -> Liveness {
        let Self::Available(rows) = self else {
            return Liveness::Unknown;
        };
        let Some(a) = artifacts else {
            return Liveness::Unknown;
        };
        // A failed spawn writes no process identity, only the supervisor's
        // proof that no child existed. An identity file or any active marker
        // contradicts it.
        if super::invocation::has_launch_failure_record(a) {
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
        // Any ambiguous actual Codex or Muse command can hide our invocation.
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
        let completed = super::invocation::has_valid_exit_record(a);
        if !matches.is_empty()
            || recorded_tree_evidence(rows, &identity, completed, &daemon_check(daemon)).is_some()
        {
            return Liveness::Unknown;
        }
        if completed {
            Liveness::NotListed
        } else {
            Liveness::Unknown
        }
    }
    /// Fresh evidence that an issue's harness work may still be running, or
    /// `None` when an available table shows nothing that could be it: its
    /// marker, another Autopilot marker for the issue, an ambiguous Codex or
    /// Muse row, or the recorded tree as [`recorded_tree_evidence`] judges it
    /// (recorded PIDs count only before a trusted completion record; the
    /// group after).
    pub fn live_evidence(
        &self,
        issue_slug: &str,
        artifacts: Option<&InvocationArtifacts>,
    ) -> Option<String> {
        self.live_evidence_with(issue_slug, artifacts, &|| None)
    }
    /// [`Self::live_evidence`] with the operator's shared daemon, as
    /// [`Self::invocation_reason_with`].
    pub(crate) fn live_evidence_with(
        &self,
        issue_slug: &str,
        artifacts: Option<&InvocationArtifacts>,
        daemon: DaemonSource<'_>,
    ) -> Option<String> {
        let Self::Available(rows) = self else {
            return Some(self.reason());
        };
        for r in rows.iter().filter(|r| r.active()) {
            match r.marker() {
                Err(reason) => {
                    return Some(format!(
                        "process {} is an ambiguous Codex or Muse command: {reason}",
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
            // Whatever runs it: a launcher shape the grammar does not parse
            // (`bash -e`, `zsh`) still carries the marker. Only this issue
            // is held by it, and only where clearing is decided.
            if let Some(marker) =
                marker_words(&r.args).find(|word| marker_issue_slug(word) == Some(issue_slug))
            {
                return Some(format!(
                    "process {} names an Autopilot invocation marker for {issue_slug}: {marker}",
                    r.pid
                ));
            }
        }
        let a = artifacts?;
        // No identity file means nothing was ever spawned into this
        // invocation. One that exists but cannot be read hides the recorded
        // tree, so it is evidence, never "nothing running".
        match std::fs::symlink_metadata(identity_path(a)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => return Some(format!("cannot stat the invocation process identity: {e}")),
            Ok(_) => {}
        }
        let identity = match read_identity(a) {
            Ok(identity) => identity,
            Err(reason) => {
                return Some(format!("unreadable invocation process identity: {reason}"))
            }
        };
        recorded_tree_evidence(
            rows,
            &identity,
            super::invocation::has_valid_exit_record(a),
            &daemon_check(daemon),
        )
    }
    pub fn orphan_markers(&self) -> Vec<&str> {
        match self {
            Self::Unavailable { .. } => Vec::new(),
            Self::Available(rows) => rows
                .iter()
                .filter(|r| r.active())
                .filter_map(|r| r.marker().ok().flatten())
                .filter(|p| is_autopilot_marker_path(p))
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
/// Ordered by start time: macOS (seconds, microseconds); Linux (clock ticks
/// since boot, 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Birth(u64, u64);
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

/// The kernel's BSD record for `pid`, or `None` once it has exited.
#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Result<Option<libc::proc_bsdinfo>, String> {
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
    Ok(Some(info))
}

/// Ancestry and birth come from the same kernel record, never a stale ps row.
fn kernel_observation(pid: u32) -> Result<Option<KernelProcess>, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(bsd_info(pid)?.map(|info| KernelProcess {
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
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(libc::ESRCH) =>
            {
                return Ok(None)
            }
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

/// The operator's shared ironmem daemon, which supervision never owns (see
/// [`ProcessRow::is_shared_ironmem_daemon`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDaemon {
    /// The configured shared-daemon socket (`Config::daemon_socket_path`).
    pub socket: PathBuf,
    /// The canonical ironmem executable the daemon must be running.
    pub exe: PathBuf,
}

impl SharedDaemon {
    /// The daemon on `socket` running the controller's own executable, the
    /// binary its MCP proxies autospawn. `None` when that executable cannot
    /// be resolved: then nothing is exempt and every descendant is owned.
    /// Either way the shared daemon would die with each invocation, so that,
    /// or a socket the process table cannot show ([`Self::unrecognisable`]),
    /// is logged once per process.
    pub fn for_current_exe(socket: PathBuf) -> Option<Self> {
        static UNRECOGNISABLE: std::sync::Once = std::sync::Once::new();
        static NO_EXE: std::sync::Once = std::sync::Once::new();
        if let Some(reason) = Self::unrecognisable(&socket) {
            UNRECOGNISABLE.call_once(|| {
                tracing::warn!(
                    socket = %socket.display(),
                    "{reason}; Autopilot cannot recognise the shared ironmem daemon there, so a \
                     daemon a harness autospawns on it is cleaned up with each invocation"
                );
            });
        }
        let exe = std::env::current_exe().and_then(|exe| exe.canonicalize());
        match exe {
            Ok(exe) => Some(Self { socket, exe }),
            Err(e) => {
                NO_EXE.call_once(|| {
                    tracing::warn!(
                        error = %e,
                        "cannot resolve this ironmem executable, so Autopilot cannot recognise \
                         the shared ironmem daemon: a daemon a harness autospawns is cleaned up \
                         with each invocation"
                    );
                });
                None
            }
        }
    }

    /// Why a daemon listening on `socket` can never be recognised from the
    /// process table, if it cannot: ps loses argv boundaries and decodes
    /// argv lossily.
    pub(crate) fn unrecognisable(socket: &Path) -> Option<String> {
        match socket.to_str() {
            None => Some("the shared ironmem daemon socket path is not UTF-8".into()),
            Some(text) if text.chars().any(char::is_whitespace) => {
                Some("the shared ironmem daemon socket path contains whitespace".into())
            }
            Some(_) => None,
        }
    }
}

/// The socket `row` listens on, when it has an ironmem daemon's shape on a
/// socket other than the configured shared one (`daemon`; any socket when
/// there is none) and outside `temp`, the system temp directory where an
/// ironmem test suite puts its own. Cleanup names such a daemon as it kills
/// it: the operator's MCP registration and Autopilot disagree about the
/// socket, which silently reintroduces the daemon kill.
fn foreign_daemon_socket(
    row: &ProcessRow,
    daemon: Option<&SharedDaemon>,
    temp: &Path,
) -> Option<String> {
    let socket = row.daemon_listen_socket(daemon.and_then(|d| d.exe.file_name()))?;
    if daemon.is_some_and(|d| d.socket.to_str() == Some(socket)) {
        return None;
    }
    let canonical_temp = temp.canonicalize().ok();
    let in_temp = |path: &Path| {
        path.starts_with(temp) || canonical_temp.as_ref().is_some_and(|t| path.starts_with(t))
    };
    let path = Path::new(socket);
    let canonical = path.parent().and_then(|p| p.canonicalize().ok());
    if in_temp(path) || canonical.as_deref().is_some_and(in_temp) {
        return None;
    }
    Some(socket.to_owned())
}

/// The executable the kernel says `pid` is running, canonicalized, or `None`
/// when it cannot be read.
fn kernel_executable(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the kernel writes at most `buffer.len()` bytes.
        let length = unsafe {
            libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32)
        };
        if length <= 0 {
            return None;
        }
        buffer.truncate(length as usize);
        Path::new(std::ffi::OsStr::from_bytes(&buffer))
            .canonicalize()
            .ok()
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()?
            .canonicalize()
            .ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// How often the completion check rescans while it waits for the worktree
/// to go quiet.
pub(crate) const QUIET_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// A process the current user owns, with its working directory, as the
/// kernel reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CwdProcess {
    pid: u32,
    uid: u32,
    birth: Birth,
    cwd: PathBuf,
}

/// The effective user ID supervision runs as, which every harness process
/// it spawns shares.
pub(crate) fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        Some(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Why a working-directory scan failed. When the cause is one in-window
/// process whose directory could not be read, `unreadable` names it by PID
/// and kernel birth: the invocation may have started it, so a hold records
/// it like a straggler and recovery waits for that very process to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanError {
    pub reason: String,
    pub unreadable: Option<(u32, Birth)>,
}

impl From<String> for ScanError {
    fn from(reason: String) -> Self {
        Self {
            reason,
            unreadable: None,
        }
    }
}

impl From<&str> for ScanError {
    fn from(reason: &str) -> Self {
        reason.to_string().into()
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The core of a working-directory scan, over injectable kernel reads.
/// `owner_birth` gives a PID's owner and kernel birth, `cwd_of` its working
/// directory; either answers `Ok(None)` for a process that has exited.
///
/// Owner and birth are read first, so a process another user owns, or one
/// born before `spawned` (it cannot be the invocation's), is skipped before
/// its directory is touched: macOS refuses some same-user system processes,
/// and those must not fail the scan. Only an in-window process whose
/// directory cannot be read fails it, since that process could be the
/// invocation's. The birth is read again afterwards: a PID reused in between
/// may have handed back a newcomer's directory.
fn scan_cwds(
    pids: impl IntoIterator<Item = u32>,
    uid: u32,
    spawned: Birth,
    mut owner_birth: impl FnMut(u32) -> Result<Option<(u32, Birth)>, String>,
    mut cwd_of: impl FnMut(u32) -> Result<Option<PathBuf>, String>,
) -> Result<Vec<CwdProcess>, ScanError> {
    let mut processes = Vec::new();
    for pid in pids {
        let Some((owner, birth)) = owner_birth(pid)? else {
            continue;
        };
        if owner != uid || birth < spawned {
            continue;
        }
        let cwd = match cwd_of(pid) {
            Ok(Some(cwd)) => cwd,
            Ok(None) => continue,
            Err(e) => {
                return Err(ScanError {
                    reason: format!(
                        "process {pid}, started during the invocation, has an unreadable \
                         working directory: {e}"
                    ),
                    unreadable: Some((pid, birth)),
                })
            }
        };
        match owner_birth(pid)? {
            None => continue,
            Some((_, after)) if after == birth => {}
            Some(_) => return Err(format!("process {pid} was replaced during the scan").into()),
        }
        processes.push(CwdProcess {
            pid,
            uid: owner,
            birth,
            cwd,
        });
    }
    Ok(processes)
}

/// The PIDs a worktree-quiet scan reads. Production scans every process
/// the current user owns ([`user_pids`]). A test passes only the processes
/// its fixture started, so no unrelated process on the machine can decide
/// it: on macOS `/bin/ps` is setuid root, so another `ps` caught between
/// the listing and its exec refuses the kernel read with EPERM (measured),
/// which fails a real scan as the fail-closed rule requires.
pub(crate) type ScanPids<'a> = &'a dyn Fn() -> Result<Vec<u32>, String>;

/// Every PID the current user owns (macOS: `proc_listpids` by user; Linux:
/// every `/proc` entry, filtered by owner when read).
pub(crate) fn user_pids() -> Result<Vec<u32>, String> {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid().ok_or("process ownership is unreadable here")?;
        /// `PROC_UID_ONLY` from `<sys/proc_info.h>`, absent from libc.
        const PROC_UID_ONLY: u32 = 4;
        const MAX_PIDS: usize = 1 << 20;
        let mut pids = vec![0i32; 4096];
        loop {
            let capacity = (pids.len() * std::mem::size_of::<i32>()) as i32;
            // SAFETY: the kernel writes at most `capacity` bytes of PIDs.
            let bytes = unsafe {
                libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), capacity)
            };
            if bytes < 0 {
                return Err(format!(
                    "cannot list processes: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let count = bytes as usize / std::mem::size_of::<i32>();
            if count < pids.len() {
                pids.truncate(count);
                break;
            }
            if pids.len() >= MAX_PIDS {
                return Err("process list exceeded its size bound".into());
            }
            pids.resize(pids.len() * 2, 0);
        }
        Ok(pids
            .into_iter()
            .filter(|pid| *pid > 0)
            .map(|pid| pid as u32)
            .collect())
    }
    #[cfg(target_os = "linux")]
    {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc").map_err(|e| format!("cannot list /proc: {e}"))? {
            let entry = entry.map_err(|e| format!("cannot list /proc: {e}"))?;
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            {
                pids.push(pid);
            }
        }
        Ok(pids)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("processes cannot be listed on this platform".into())
    }
}

/// Of `pids`, the current user's processes born at or after `spawned`,
/// with their working directories, read from the kernel ([`scan_cwds`]).
/// macOS reads `PROC_PIDTBSDINFO` and `PROC_PIDVNODEPATHINFO`: about 1.5 ms
/// for ~600 processes, measured. Linux reads `/proc`.
fn processes_with_cwd(pids: Vec<u32>, spawned: Birth) -> Result<Vec<CwdProcess>, ScanError> {
    let uid = current_uid().ok_or("process working directories are unreadable here")?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let owner_birth = |pid: u32| {
            Ok(bsd_info(pid)?.map(|info| {
                (
                    info.pbi_uid,
                    Birth(info.pbi_start_tvsec, info.pbi_start_tvusec),
                )
            }))
        };
        let cwd_of = |pid: u32| {
            let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
            // SAFETY: the kernel writes at most `size` bytes.
            let bytes = unsafe {
                libc::proc_pidinfo(
                    pid as i32,
                    libc::PROC_PIDVNODEPATHINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if bytes <= 0 {
                let error = std::io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
                    return Ok(None);
                }
                return Err(error.to_string());
            }
            if bytes != size {
                return Err("incomplete working directory record".into());
            }
            // SAFETY: a full structure was written by proc_pidinfo.
            let info = unsafe { info.assume_init() };
            // SAFETY: the kernel NUL-terminates the MAXPATHLEN path buffer.
            let path = unsafe {
                std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast::<libc::c_char>())
            };
            Ok(Some(
                Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())).to_path_buf(),
            ))
        };
        scan_cwds(pids, uid, spawned, owner_birth, cwd_of)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let gone = |e: &std::io::Error| {
            e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH)
        };
        let owner_birth = |pid: u32| {
            let owner = match std::fs::metadata(format!("/proc/{pid}")) {
                Ok(meta) => meta.uid(),
                Err(e) if gone(&e) => return Ok(None),
                Err(e) => return Err(format!("cannot read process {pid}: {e}")),
            };
            Ok(kernel_observation(pid)?.map(|process| (owner, process.birth)))
        };
        let cwd_of = |pid: u32| match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Ok(cwd) => Ok(Some(cwd)),
            Err(e) if gone(&e) => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        scan_cwds(pids, uid, spawned, owner_birth, cwd_of)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pids, uid, spawned);
        Err("process working directories are unreadable on this platform".into())
    }
}

/// [`processes_with_cwd`] of whatever `pids` lists.
fn scan_with(pids: ScanPids<'_>, spawned: Birth) -> Result<Vec<CwdProcess>, ScanError> {
    processes_with_cwd(pids()?, spawned)
}

fn is_shared_daemon(
    daemon: Option<&SharedDaemon>,
    kernel_exe: fn(u32) -> Option<PathBuf>,
    row: &ProcessRow,
) -> bool {
    daemon.is_some_and(|daemon| {
        row.is_shared_ironmem_daemon(daemon)
            && kernel_exe(row.pid).is_some_and(|exe| exe == daemon.exe)
    })
}

/// How long supervision waits for the worktree to go quiet before it
/// refuses completion. Ordinary detached helpers (git's auto-gc, say) exit
/// within it; anything longer is held and finished by recovery once gone.
pub(crate) const QUIET_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether the worktree held no process the invocation could have started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Quiet {
    Yes,
    Held(QuietHold),
}

/// Why the worktree is not quiet, as the supervisor found it: what recovery
/// must later see gone before it trusts the invocation's completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuietHold {
    /// The canonical worktree that was scanned.
    pub worktree: PathBuf,
    /// The effective user ID the scan ran as, and so could see.
    pub uid: u32,
    /// Each qualifying process, by PID and kernel birth.
    pub stragglers: Vec<(u32, Birth)>,
    /// Set when the scan itself kept failing. `stragglers` then holds only
    /// the in-window process whose unreadable directory failed it, if one did.
    pub scan_failed: Option<String>,
    /// For the operator: each straggler's PID and command, or the failure.
    pub reason: String,
}

/// One worktree-quiet test. A process qualifies, and keeps completion
/// unproven, when its working directory is the worktree or inside it, the
/// current user owns it, the kernel started it at or after the invocation's
/// root (which excludes the operator's own older shell or editor there),
/// and it is neither the checking process nor the shared daemon or one of
/// its children.
pub(crate) struct QuietCheck<'a> {
    worktree: PathBuf,
    spawned: Birth,
    uid: u32,
    daemon: Option<&'a SharedDaemon>,
    kernel_exe: fn(u32) -> Option<PathBuf>,
}

impl<'a> QuietCheck<'a> {
    fn new(
        worktree: &Path,
        spawned: Birth,
        daemon: Option<&'a SharedDaemon>,
        kernel_exe: fn(u32) -> Option<PathBuf>,
    ) -> Result<Self, String> {
        let canonical = worktree.canonicalize().map_err(|e| {
            format!(
                "cannot resolve the invocation worktree {}: {e}",
                worktree.display()
            )
        })?;
        Ok(Self {
            worktree: canonical,
            spawned,
            uid: current_uid().ok_or("process ownership is unreadable on this platform")?,
            daemon,
            kernel_exe,
        })
    }

    /// A single pass with the real kernel sources, as recovery runs it. A
    /// scan that fails is an error here: recovery simply holds and retries
    /// on its next check.
    pub(crate) fn once(
        worktree: &Path,
        spawned: Birth,
        daemon: Option<&'a SharedDaemon>,
        rows: &[ProcessRow],
        pids: ScanPids<'_>,
    ) -> Result<Quiet, String> {
        let check = Self::new(worktree, spawned, daemon, kernel_executable)?;
        let scanned = scan_with(pids, spawned).map_err(|e| e.reason)?;
        let found = check.stragglers(scanned, || Ok(rows.to_vec()))?;
        Ok(check.verdict(found))
    }

    fn verdict(&self, found: Vec<(u32, Birth, String)>) -> Quiet {
        if found.is_empty() {
            return Quiet::Yes;
        }
        Quiet::Held(QuietHold {
            worktree: self.worktree.clone(),
            uid: self.uid,
            reason: self.describe(&found),
            stragglers: found
                .into_iter()
                .map(|(pid, birth, _)| (pid, birth))
                .collect(),
            scan_failed: None,
        })
    }

    /// A scan that kept failing. A process that failed it by its unreadable
    /// directory is recorded as a straggler: it may be the invocation's, and
    /// it must be seen gone by birth even if it later leaves the worktree.
    fn scan_failed(&self, error: ScanError) -> Quiet {
        Quiet::Held(QuietHold {
            worktree: self.worktree.clone(),
            uid: self.uid,
            stragglers: error.unreadable.into_iter().collect(),
            reason: format!(
                "the processes in the invocation worktree {} could not all be checked, so it \
                 is not known to be quiet: {}",
                self.worktree.display(),
                error.reason
            ),
            scan_failed: Some(error.reason),
        })
    }

    /// The qualifying processes in `scanned`, each with its birth and named
    /// with its command. The process table is read only for a candidate.
    fn stragglers(
        &self,
        scanned: Vec<CwdProcess>,
        rows: impl FnOnce() -> Result<Vec<ProcessRow>, String>,
    ) -> Result<Vec<(u32, Birth, String)>, String> {
        let me = std::process::id();
        let candidates: Vec<CwdProcess> = scanned
            .into_iter()
            .filter(|p| {
                p.uid == self.uid
                    && p.pid != me
                    && p.birth >= self.spawned
                    && p.cwd.starts_with(&self.worktree)
            })
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let rows = rows()?;
        Ok(candidates
            .iter()
            .filter(|p| !self.in_shared_daemon_tree(p.pid, &rows))
            .map(|p| {
                let command = rows
                    .iter()
                    .find(|r| r.pid == p.pid)
                    .map_or("<command unavailable>", |r| r.args.as_str());
                let named = format!(
                    "process {} ({})",
                    p.pid,
                    crate::autopilot::scrub::scrub_and_bound(command, 120).text
                );
                (p.pid, p.birth, named)
            })
            .collect())
    }

    fn describe(&self, found: &[(u32, Birth, String)]) -> String {
        const NAMED: usize = 5;
        let more = found.len().saturating_sub(NAMED);
        let mut named = found
            .iter()
            .take(NAMED)
            .map(|(_, _, named)| named.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if more > 0 {
            named.push_str(&format!(" and {more} more"));
        }
        format!(
            "{named} started during the invocation and still runs in its worktree {}",
            self.worktree.display()
        )
    }

    /// Whether `pid` is the shared daemon or, by its process-table ancestry,
    /// one of the daemon's children, which the tracker never owns either.
    fn in_shared_daemon_tree(&self, pid: u32, rows: &[ProcessRow]) -> bool {
        let mut current = pid;
        for _ in 0..64 {
            let Some(row) = rows.iter().find(|r| r.pid == current && r.active()) else {
                return false;
            };
            if is_shared_daemon(self.daemon, self.kernel_exe, row) {
                return true;
            }
            if row.ppid <= 1 || row.ppid == current {
                return false;
            }
            current = row.ppid;
        }
        false
    }
}

/// The kernel birth of `pid`, or `None` once it has exited. A read that
/// fails is an error, never "exited": a caller deciding that a process is
/// gone must hold on it.
pub(crate) fn kernel_birth(pid: u32) -> Result<Option<Birth>, String> {
    birth(pid)
}

pub(crate) struct ProcessTracker {
    root_pid: u32,
    known: Vec<(ProcessRow, Birth)>,
    observed_root: bool,
    root_reaped: bool,
    /// A daemon on its socket, running its executable, is never owned;
    /// `None` owns every descendant.
    shared_daemon: Option<SharedDaemon>,
    /// [`kernel_executable`]; a seam so tests need no real daemon binary.
    kernel_exe: fn(u32) -> Option<PathBuf>,
    /// Descendants whose kernel birth the last observe authenticated.
    live: std::collections::HashSet<u32>,
}
impl ProcessTracker {
    /// A tracker that never owns `daemon`, the operator's shared daemon.
    pub(crate) fn exempting(root_pid: u32, daemon: &SharedDaemon) -> Self {
        Self::new(root_pid, Some(daemon))
    }

    /// A tracker that owns every descendant, the operator's shared daemon
    /// included: only for when that daemon cannot be identified
    /// ([`SharedDaemon::for_current_exe`] logs why) or a test needs none.
    pub(crate) fn owning_every_descendant(root_pid: u32) -> Self {
        Self::new(root_pid, None)
    }

    fn new(root_pid: u32, shared_daemon: Option<&SharedDaemon>) -> Self {
        Self {
            root_pid,
            known: Vec::new(),
            observed_root: false,
            root_reaped: false,
            shared_daemon: shared_daemon.cloned(),
            kernel_exe: kernel_executable,
            live: std::collections::HashSet::new(),
        }
    }
    /// The argv must match, and then the kernel's executable path must be
    /// the configured one: an agent-built `target/debug/ironmem` with the
    /// same argv is owned like any other tool.
    fn is_shared_daemon(&self, row: &ProcessRow) -> bool {
        is_shared_daemon(self.shared_daemon.as_ref(), self.kernel_exe, row)
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
        // The shared ironmem daemon is never owned (see
        // `ProcessRow::is_shared_ironmem_daemon`). A sample taken between its
        // fork and exec showed it with its spawner's argv and enrolled it.
        // It may have started children since that sample, but none was
        // enrolled: it had none then. Releasing it here, before this sample
        // enrols anything, means it is no seed, so those children are never
        // reached either. Only the same process, by kernel birth, is
        // released: a different birth is a reused PID, which the seed check
        // below refuses as it would any.
        //
        // The daemon test reads a kernel path, so it runs once per row here
        // and never per tracked descendant: `known` keeps every exited one.
        let shared_daemon: std::collections::HashSet<u32> = rows
            .iter()
            .filter(|r| r.active() && self.is_shared_daemon(r))
            .map(|r| r.pid)
            .collect();
        let mut released = Vec::new();
        for (old, expected) in &self.known {
            let shows_daemon = old.pid != self.root_pid && shared_daemon.contains(&old.pid);
            if shows_daemon
                && kernel(old.pid)
                    .map_err(MemoryError::Validation)?
                    .is_some_and(|actual| actual.pid == old.pid && actual.birth == *expected)
            {
                released.push(old.pid);
            }
        }
        self.known.retain(|(old, _)| !released.contains(&old.pid));
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
                        "tracked PID was reused; cleanup cannot be authenticated".into(),
                    ));
                }
                None => {
                    // A shell that ran a tool and exited after the ps sample
                    // leaves a stale row for a child that is gone too; only a
                    // child the kernel still has escaped authentication. The
                    // shared daemon outlives its proxy by design and was
                    // never ours to authenticate.
                    for row in rows.iter().filter(|row| {
                        row.ppid == old.pid
                            && row.active()
                            && !shared_daemon.contains(&row.pid)
                            && !self.known.iter().any(|(known, _)| known.pid == row.pid)
                    }) {
                        if kernel(row.pid).map_err(MemoryError::Validation)?.is_some() {
                            return Err(MemoryError::Validation(
                                "invocation ancestry seed disappeared before descendant authentication"
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
                            "invocation root identity changed during authentication".into(),
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
            // The shared daemon is neither enrolled nor a seed, so its own
            // children are never reached either.
            for row in rows
                .iter()
                .filter(|r| r.active() && !shared_daemon.contains(&r.pid))
            {
                if self.known.iter().any(|(old, _)| old.pid == row.pid) {
                    continue;
                }
                let Some(expected_parent) = seeds.get(&row.ppid).copied() else {
                    continue;
                };
                let parent_before = kernel(row.ppid).map_err(MemoryError::Validation)?;
                if !parent_before.is_some_and(|p| p.pid == row.ppid && p.birth == expected_parent) {
                    return Err(MemoryError::Validation(
                        "invocation ancestry seed changed before descendant authentication".into(),
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
                        "invocation descendant ancestry changed during authentication".into(),
                    ));
                }
                let parent_after = kernel(row.ppid).map_err(MemoryError::Validation)?;
                if !parent_after.is_some_and(|p| p.pid == row.ppid && p.birth == expected_parent) {
                    return Err(MemoryError::Validation(
                        "invocation ancestry seed changed during descendant authentication".into(),
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
                        "invocation descendant kernel identity changed during authentication"
                            .into(),
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
        self.live = seeds
            .into_keys()
            .filter(|pid| *pid != self.root_pid)
            .collect();
        Ok(rows)
    }

    /// Descendants newest first, then the root, each with its birth.
    fn kill_order(&self) -> impl Iterator<Item = &(ProcessRow, Birth)> + '_ {
        let root = self.root_pid;
        self.known
            .iter()
            .rev()
            .filter(move |(r, _)| r.pid != root)
            .chain(self.known.iter().filter(move |(r, _)| r.pid == root))
    }

    fn signal(&self, pid: u32, signal: i32) -> Result<(), MemoryError> {
        let Some((_, expected)) = self.known.iter().find(|(r, _)| r.pid == pid) else {
            return Err(MemoryError::Validation(
                "cannot signal unauthenticated invocation process".into(),
            ));
        };
        self.signal_known(pid, *expected, signal)
    }

    /// Signal a known process, by the birth it was enrolled with.
    fn signal_known(&self, pid: u32, expected: Birth, signal: i32) -> Result<(), MemoryError> {
        match birth(pid).map_err(MemoryError::Validation)? {
            None => Ok(()),
            Some(actual) if actual == expected => {
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
                        "invocation process signaling unsupported".into(),
                    ))
                }
            }
            Some(_) => Err(MemoryError::Validation(
                "invocation PID identity changed; refusing signal".into(),
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
                // A shared daemon a sample caught between its fork and exec,
                // still wearing its spawner's argv, is stopped and killed
                // here like any tool. That is harmless: it never bound the
                // socket or served anyone, and the next proxy spawns another.
                for (row, birth) in &self.known {
                    self.signal_known(row.pid, *birth, STOP_SIGNAL)?;
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
                    "invocation process tree did not quiesce".into(),
                ));
            }
            Ok(())
        })();
        // Continue best-effort authenticated cleanup after any uncertainty so
        // SIGSTOP cannot strand a known process. Never signal a stale group.
        let mut signal_error = None;
        let temp = std::env::temp_dir();
        for (row, birth) in self.kill_order() {
            let pid = row.pid;
            if let Some(socket) = foreign_daemon_socket(row, self.shared_daemon.as_ref(), &temp) {
                tracing::warn!(
                    pid,
                    socket = %socket,
                    configured = ?self.shared_daemon.as_ref().map(|d| d.socket.display().to_string()),
                    "Autopilot cleanup is killing an ironmem daemon on a socket other than the \
                     configured shared one; if your MCP servers use that socket, their \
                     registration and Autopilot disagree about the daemon socket"
                );
            }
            if let Err(e) = self.signal_known(pid, *birth, KILL_SIGNAL) {
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
                                "invocation tracked PID changed during cleanup".into(),
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
            "invocation descendants remained alive after cleanup".into(),
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
            "untracked process {} remained in invocation process group {} after cleanup; \
             completion is unproven",
            straggler.unwrap_or_default(),
            self.root_pid
        )))
    }
    /// The last completion check. A harness tool that leads its own session
    /// (every Muse shell tool; Codex's are in the root's group) and exits
    /// between two samples leaves a background child reparented to init.
    /// Neither PPID enrollment nor [`Self::verify_group_drained`] reaches it,
    /// but it still runs in the worktree. So completion waits while any
    /// process qualifies ([`QuietCheck`]), rescanning every [`QUIET_POLL`]
    /// for up to `grace`, and then reports what is still there.
    ///
    /// Limit: a child that `chdir`s out of the worktree before the root exits
    /// still escapes; a deliberate escape is outside this guarantee. A
    /// platform with no working-directory source, or a scan that keeps
    /// failing, is an error.
    pub(crate) fn verify_worktree_quiet(
        &self,
        worktree: &Path,
        grace: std::time::Duration,
        pids: ScanPids<'_>,
    ) -> Result<Quiet, MemoryError> {
        self.verify_worktree_quiet_with(
            worktree,
            |spawned| scan_with(pids, spawned),
            Self::rows,
            grace,
        )
    }

    fn verify_worktree_quiet_with(
        &self,
        worktree: &Path,
        mut cwd_source: impl FnMut(Birth) -> Result<Vec<CwdProcess>, ScanError>,
        mut rows: impl FnMut() -> Result<Vec<ProcessRow>, MemoryError>,
        grace: std::time::Duration,
    ) -> Result<Quiet, MemoryError> {
        let unproven = |reason: String| {
            MemoryError::Validation(format!("{reason}; invocation completion is unproven"))
        };
        let spawned = self
            .known
            .iter()
            .find(|(r, _)| r.pid == self.root_pid)
            .map(|(_, birth)| *birth)
            .ok_or_else(|| {
                unproven("the invocation root's kernel birth was never observed".into())
            })?;
        let check = QuietCheck::new(
            worktree,
            spawned,
            self.shared_daemon.as_ref(),
            self.kernel_exe,
        )
        .map_err(unproven)?;
        let deadline = std::time::Instant::now() + grace;
        loop {
            // A failed scan is retried until the grace runs out.
            let pass = cwd_source(spawned).and_then(|scanned| {
                check
                    .stragglers(scanned, || rows().map_err(|e| e.to_string()))
                    .map_err(ScanError::from)
            });
            match pass {
                Ok(found) if found.is_empty() => return Ok(Quiet::Yes),
                Ok(found) if std::time::Instant::now() >= deadline => {
                    return Ok(check.verdict(found))
                }
                // Still a hold recovery can retry later, not an error.
                Err(e) if std::time::Instant::now() >= deadline => return Ok(check.scan_failed(e)),
                _ => std::thread::sleep(QUIET_POLL.min(grace)),
            }
        }
    }

    /// Read the root's kernel record straight after spawn, before any
    /// process-table sample: our unreaped `Child` reserves its PID, so the
    /// record is the root's. That gives completion the root's birth even
    /// when the root exits before the first sample; [`Self::observed_root`]
    /// stays false until a sample binds its process-table row. A read that
    /// fails leaves the birth unknown.
    pub(crate) fn adopt_root(&mut self) {
        if self.known.iter().any(|(r, _)| r.pid == self.root_pid) {
            return;
        }
        match kernel_observation(self.root_pid) {
            Ok(Some(root)) if root.pid == self.root_pid => {
                let row = ProcessRow {
                    pid: root.pid,
                    ppid: root.ppid,
                    pgid: root.pgid,
                    stat: String::new(),
                    args: String::new(),
                };
                self.known.push((row, root.birth));
            }
            // Not fatal: the first sample may still bind the root; without
            // either, completion holds as unproven.
            outcome => tracing::debug!(
                pid = self.root_pid,
                ?outcome,
                "could not read the invocation root's kernel record at spawn"
            ),
        }
    }

    pub(crate) fn observed_root(&self) -> bool {
        self.observed_root
    }

    /// The root's kernel birth, once read ([`Self::adopt_root`] or a sample).
    pub(crate) fn root_birth(&self) -> Option<Birth> {
        self.recorded().0
    }
    /// What the identity file records: the root's kernel birth, once
    /// observed, and the descendants alive at the last sample. Exited ones
    /// are left out, so the file stays bounded over a long dispatch.
    pub(crate) fn recorded(&self) -> (Option<Birth>, Vec<RecordedProcess>) {
        let root_birth = self
            .known
            .iter()
            .find(|(r, _)| r.pid == self.root_pid)
            .map(|(_, birth)| *birth);
        let descendants = self
            .known
            .iter()
            .filter(|(r, _)| r.pid != self.root_pid && self.live.contains(&r.pid))
            .map(|(r, birth)| RecordedProcess::new(r, *birth))
            .collect();
        (root_birth, descendants)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::autopilot::implementer::Implementer;

    /// [`invocation_for`] a Codex invocation.
    pub(crate) fn invocation(dir: &Path) -> (InvocationArtifacts, ProcessRow) {
        invocation_for(dir, Implementer::Codex)
    }

    /// Artifacts and a persisted identity for one `implementer` invocation in
    /// `dir`, with the root row that harness's own command shows in `ps`.
    pub(crate) fn invocation_for(
        dir: &Path,
        implementer: Implementer,
    ) -> (InvocationArtifacts, ProcessRow) {
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
                root_birth: None,
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
            args: match implementer {
                Implementer::Codex => {
                    format!("codex exec --output-schema {} prompt", a.schema_path)
                }
                Implementer::Muse => format!(
                    "/u/.local/bin/muse-bin-1.4.13-R6996.1 exec --json --output-schema {} \
                     --prompt-file {}",
                    a.schema_path,
                    path("prompt.md")
                ),
                Implementer::Claude => panic!("Claude is not a supervised invocation"),
            },
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
    use crate::autopilot::implementer::Implementer;

    /// The fixture's root row is the named harness's own command, carrying
    /// the invocation's marker, so a Muse test never leans on a Codex row.
    #[test]
    fn the_fixture_root_row_is_the_named_harnesss_command() {
        for (implementer, exe) in [
            (Implementer::Codex, "codex"),
            (Implementer::Muse, "muse-bin-"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (a, row) = testing::invocation_for(dir.path(), implementer);
            let first = row.args.split_whitespace().next().unwrap();
            assert!(
                Path::new(first)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(exe),
                "{implementer:?}: {}",
                row.args
            );
            assert_eq!(row.marker(), Ok(Some(a.schema_path.as_str())));
            assert_eq!(
                ProcessSnapshot::Available(vec![row]).liveness(Some(&a)),
                Liveness::Alive
            );
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            testing::invocation(dir.path()).1.args,
            testing::invocation_for(dir.path(), Implementer::Codex)
                .1
                .args
        );
    }

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
            .contains("invalid invocation process identity"));
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
        // Anything naming an Autopilot-shaped marker could be hiding ours, so it still holds.
        assert!(marker(
            "codex exec --profile work --output-schema /r/.git/ironmem/autopilot/o-r-1/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json p"
        )
        .is_err());
        assert!(marker(
            "codex exec --output-schema=/r/.git/ironmem/autopilot/o-r-1/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json --profile w p"
        )
        .is_err());
        // A schema.json outside the Autopilot layout is an operator's own run.
        assert_eq!(
            marker("codex exec --brand-new-flag --output-schema /tmp/schema.json hi"),
            Ok(None)
        );
        // A relative path can never be an Autopilot marker.
        assert_eq!(
            marker("codex exec --output-schema relative/schema.json p"),
            Ok(None)
        );
    }
    fn row(args: &str) -> ProcessRow {
        ProcessRow {
            pid: 10,
            ppid: 1,
            pgid: 10,
            stat: "S".into(),
            args: args.into(),
        }
    }
    /// Measured on Muse Code 1.4.13-R6996.1, scratch paths shortened.
    const MUSE_ROOT: &str = "/Users/o/.local/bin/muse-bin-1.4.13-R6996.1 exec --json \
        --session-id 934331e6-2c8a-4e0f-bcd6-0b6300000000 \
        --output-schema /r/.git/ironmem/autopilot/o-r-7/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json \
        --prompt-file /r/.git/ironmem/autopilot/o-r-7/0f0e0d0c-0b0a-4908-8706-050403020100/prompt.md \
        --model muse-wonder-1.0 --max-model-steps 300 --workspace /w/o-r-7 \
        --approval-mode never --disable-sandbox --trust-workspace \
        --no-foreign-personal-context --disable-request-user-input";
    #[test]
    fn a_muse_root_yields_its_exact_schema_marker() {
        assert_eq!(
            row(MUSE_ROOT).marker().unwrap(),
            Some(
                "/r/.git/ironmem/autopilot/o-r-7/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json"
            )
        );
        // A bare `muse` executable is accepted too, for forward compatibility.
        let launcher = MUSE_ROOT.replacen(
            "/Users/o/.local/bin/muse-bin-1.4.13-R6996.1",
            "/Users/o/.local/bin/muse",
            1,
        );
        assert!(row(&launcher).marker().unwrap().is_some());
        let equals = MUSE_ROOT.replacen("--output-schema /", "--output-schema=/", 1);
        assert!(row(&equals).marker().unwrap().is_some());
        // A valued option in its `=` form consumes no following word.
        let valued = MUSE_ROOT.replacen("--model muse-wonder-1.0", "--model=muse-wonder-1.0", 1);
        assert!(row(&valued).marker().unwrap().is_some());
    }
    /// Measured: `~/.local/bin/muse` is a bash script that `exec`s
    /// `muse-bin-<version>` under the same PID, so until that exec ps shows
    /// the root as `bash …/muse exec …`. That row is the invocation's root
    /// and carries its exact marker; reading it as unrelated would hide a
    /// live invocation from every liveness and same-issue check.
    #[test]
    fn the_muse_launcher_phase_yields_the_roots_exact_marker() {
        let schema =
            "/r/.git/ironmem/autopilot/o-r-7/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json";
        let launched = MUSE_ROOT.replacen("muse-bin-1.4.13-R6996.1", "muse", 1);
        for shell in ["bash", "/bin/bash", "sh", "/bin/sh"] {
            let r = row(&format!("{shell} {launched}"));
            assert_eq!(r.marker(), Ok(Some(schema)), "{shell}");
        }
        // Its own ambiguity is still ambiguity, as for the binary.
        assert!(row(&format!("bash {launched} fix the bug"))
            .marker()
            .is_err());
        // A shell running anything else is not a Muse row.
        for args in [
            format!("bash {}", launched.replacen("/muse ", "/not-muse ", 1)),
            format!("bash -c {launched}"),
            format!("zsh {launched}"),
            format!("bash {}", launched.replacen(" exec ", " resume ", 1)),
        ] {
            assert_eq!(row(&args).marker(), Ok(None), "{args}");
        }
    }
    #[test]
    fn non_root_muse_rows_are_not_markers() {
        // The Muse reviewer passes no --output-schema.
        assert_eq!(
            row("/u/muse-bin-1.4.13-R6996.1 exec --json --approval-mode never --disable-write --sandbox-network restricted --no-session-log --workspace /r --prompt-file /tmp/p.md")
                .marker()
                .unwrap(),
            None
        );
        // A tool shell that merely mentions the path.
        assert_eq!(
            row("/bin/sh -c cat /r/.git/ironmem/autopilot/o-r-7/x/schema.json")
                .marker()
                .unwrap(),
            None
        );
        // Not `exec`.
        assert_eq!(
            row("/u/muse-bin-1.4.13-R6996.1 resume --last")
                .marker()
                .unwrap(),
            None
        );
    }
    #[test]
    fn an_ambiguous_muse_root_with_a_schema_word_is_an_error() {
        // A positional prompt: later words could be options or prompt text.
        assert!(row(&format!("{MUSE_ROOT} fix the bug")).marker().is_err());
        // An option this grammar does not know.
        assert!(
            row(&MUSE_ROOT.replacen("--disable-sandbox", "--brand-new-flag", 1))
                .marker()
                .is_err()
        );
        // `-w` takes an optional value, so the next word's role is unknowable.
        assert!(row(&MUSE_ROOT.replacen("--disable-sandbox", "-w", 1))
            .marker()
            .is_err());
        assert!(
            row(&MUSE_ROOT.replacen("--disable-sandbox", "--worktree=create", 1))
                .marker()
                .is_err()
        );
        // A trailing valued option with no value.
        assert!(row(&format!("{MUSE_ROOT} --model")).marker().is_err());
        // Two markers.
        assert!(row(&format!("{MUSE_ROOT} --output-schema /x/schema.json"))
            .marker()
            .is_err());
        // The same ambiguity without an Autopilot-shaped marker is someone
        // else's run, even when it names a schema.json of its own.
        assert_eq!(
            row("/u/muse-bin-1.4.13-R6996.1 exec --output-schema /tmp/other.json hello")
                .marker()
                .unwrap(),
            None
        );
        assert_eq!(
            row("/u/muse-bin-1.4.13-R6996.1 exec --output-schema /home/me/schema.json summarize")
                .marker()
                .unwrap(),
            None
        );
    }
    #[test]
    fn unobserved_history_and_live_descendants_cannot_be_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut child) = testing::invocation(dir.path());
        child.pid = 2345;
        child.pgid = 2345;
        child.args = "python gate".into();
        let mut identity = read_identity(&a).unwrap();
        identity
            .descendants
            .push(RecordedProcess::new(&child, Birth(7, 0)));
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
            shared_daemon: None,
            kernel_exe: kernel_executable,
            live: std::collections::HashSet::new(),
        };
        let order = tracker.kill_order().map(|(r, _)| r.pid).collect::<Vec<_>>();
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
                root_birth: None,
                schema_path: a.schema_path.clone(),
                invocation_id: a.invocation_id.clone(),
                descendants: vec![RecordedProcess::new(&child_row, Birth(7, 0))],
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

    /// The operator's configured shared-daemon socket in these fixtures.
    const SOCKET: &str = "/Users/o/.ironmem/daemon.sock";
    const DAEMON: &str = "/opt/homebrew/bin/ironmem serve --listen /Users/o/.ironmem/daemon.sock";
    const DAEMON_DB: &str =
        "/opt/homebrew/bin/ironmem serve --listen /Users/o/.ironmem/daemon.sock \
        --db /Users/o/.ironmem/memory.db";
    /// The same daemon argv, but on a test suite's temp socket.
    const OTHER_DAEMON: &str = "/opt/homebrew/bin/ironmem serve --listen /tmp/t1/daemon.sock \
        --db /tmp/t1/memory.db";

    /// The executable the controller runs in these fixtures.
    const DAEMON_EXE: &str = "/opt/homebrew/bin/ironmem";

    fn daemon() -> SharedDaemon {
        SharedDaemon {
            socket: SOCKET.into(),
            exe: DAEMON_EXE.into(),
        }
    }

    /// Every process runs the installed ironmem: only argv decides.
    fn installed_exe(_pid: u32) -> Option<PathBuf> {
        Some(DAEMON_EXE.into())
    }

    fn daemon_tracker(kernel_exe: fn(u32) -> Option<PathBuf>) -> ProcessTracker {
        let mut tracker = ProcessTracker::exempting(10, &daemon());
        tracker.kernel_exe = kernel_exe;
        tracker
    }

    fn args_row(pid: u32, ppid: u32, pgid: u32, args: &str) -> ProcessRow {
        ProcessRow {
            args: args.into(),
            ..process_row(pid, ppid, pgid)
        }
    }

    #[test]
    fn only_the_exact_daemon_argv_on_the_configured_socket_is_the_shared_daemon() {
        let socket = &daemon();
        for args in [
            DAEMON,
            DAEMON_DB,
            "ironmem serve --listen /Users/o/.ironmem/daemon.sock",
        ] {
            assert!(
                args_row(40, 30, 40, args).is_shared_ironmem_daemon(socket),
                "{args}"
            );
        }
        // Not its own group leader.
        assert!(!args_row(40, 30, 10, DAEMON_DB).is_shared_ironmem_daemon(socket));
        for args in [
            // Another socket: some other daemon, such as a test suite's.
            OTHER_DAEMON,
            "/opt/homebrew/bin/ironmem serve --listen /Users/o/.ironmem/daemon.sock.other",
            // Extra or incomplete arguments.
            &format!("{DAEMON_DB} --verbose") as &str,
            &format!("{DAEMON} extra"),
            "/opt/homebrew/bin/ironmem serve --listen /Users/o/.ironmem/daemon.sock --db",
            "/opt/homebrew/bin/ironmem serve --listen",
            "/opt/homebrew/bin/ironmem serve --listen /Users/o/.ironmem/daemon.sock --other /x",
            // The proxy, a plain server, other subcommands.
            "/opt/homebrew/bin/ironmem serve --connect /Users/o/.ironmem/daemon.sock",
            "/opt/homebrew/bin/ironmem serve",
            "/opt/homebrew/bin/ironmem serve --db /x --listen /Users/o/.ironmem/daemon.sock",
            "/opt/homebrew/bin/ironmem autopilot lead owner/repo",
            // A different executable.
            "/opt/homebrew/bin/ironmem-dev serve --listen /Users/o/.ironmem/daemon.sock",
            "/usr/bin/python3 serve --listen /Users/o/.ironmem/daemon.sock",
        ] {
            assert!(
                !args_row(40, 30, 40, args).is_shared_ironmem_daemon(socket),
                "{args}"
            );
        }
        // ps loses argv boundaries, so a socket path with whitespace is never
        // recognised: that daemon is tracked and cleaned up, failing safe.
        let spaced = &SharedDaemon {
            socket: "/Users/o/My Files/daemon.sock".into(),
            exe: DAEMON_EXE.into(),
        };
        assert!(!args_row(
            40,
            30,
            40,
            "/b/ironmem serve --listen /Users/o/My Files/daemon.sock"
        )
        .is_shared_ironmem_daemon(spaced));
    }

    /// A versioned install (`ironmem-0.9`, or a Linux `current_exe` that
    /// resolves a symlink to one) spawns its daemon under that name. The
    /// controller's own executable name counts as well as `ironmem`; any
    /// other name still does not, and the kernel's path still decides.
    #[test]
    fn the_controllers_own_executable_name_is_a_daemon_name() {
        let versioned = SharedDaemon {
            socket: SOCKET.into(),
            exe: "/opt/ironmem/bin/ironmem-0.9".into(),
        };
        let row = args_row(
            40,
            30,
            40,
            "/opt/ironmem/bin/ironmem-0.9 serve --listen /Users/o/.ironmem/daemon.sock",
        );
        assert!(row.is_shared_ironmem_daemon(&versioned));
        assert!(!row.is_shared_ironmem_daemon(&daemon()));
        assert!(args_row(40, 30, 40, DAEMON).is_shared_ironmem_daemon(&versioned));
        assert!(!args_row(
            40,
            30,
            40,
            "/opt/ironmem/bin/ironmem-1.0 serve --listen /Users/o/.ironmem/daemon.sock"
        )
        .is_shared_ironmem_daemon(&versioned));
    }

    /// A socket the process table cannot show unambiguously is named, so
    /// the operator learns why the daemon a harness autospawns there is
    /// cleaned up with each invocation.
    #[test]
    fn a_socket_the_process_table_cannot_show_is_reported() {
        assert_eq!(SharedDaemon::unrecognisable(Path::new(SOCKET)), None);
        let spaced =
            SharedDaemon::unrecognisable(Path::new("/Users/Jane Doe/.ironmem/d.sock")).unwrap();
        assert!(spaced.contains("whitespace"), "{spaced}");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bytes = std::ffi::OsStr::from_bytes(b"/tmp/\xff/daemon.sock");
            let raw = SharedDaemon::unrecognisable(Path::new(bytes)).unwrap();
            assert!(raw.contains("UTF-8"), "{raw}");
        }
    }

    /// Cleanup kills a daemon on any socket but the configured one. Off the
    /// system temp directory (where an ironmem test suite puts its own),
    /// that means the operator's MCP registration and Autopilot disagree
    /// about the socket, which silently reintroduces the daemon kill, so it
    /// is named.
    #[test]
    fn a_killed_daemon_on_another_non_temp_socket_is_named() {
        let temp = Path::new("/var/folders/x/T");
        let foreign = |args: &str, daemon: Option<&SharedDaemon>| {
            foreign_daemon_socket(&args_row(40, 30, 40, args), daemon, temp)
        };
        let elsewhere = "/opt/homebrew/bin/ironmem serve --listen /Users/o/other/daemon.sock";
        assert_eq!(
            foreign(elsewhere, Some(&daemon())).as_deref(),
            Some("/Users/o/other/daemon.sock")
        );
        // With no exemption at all, every daemon killed is named.
        assert_eq!(
            foreign(DAEMON, None).as_deref(),
            Some("/Users/o/.ironmem/daemon.sock")
        );
        for quiet in [
            // The configured one is never killed, so never named.
            DAEMON_DB,
            // A test suite's temp socket.
            "/opt/homebrew/bin/ironmem serve --listen /var/folders/x/T/.tmpA/daemon.sock",
            // Not a daemon at all.
            PROXY,
            "/usr/bin/python3 serve --listen /Users/o/other/daemon.sock",
        ] {
            assert_eq!(foreign(quiet, Some(&daemon())), None, "{quiet}");
        }
        // Not leading its own group: not the daemon's shape.
        assert_eq!(
            foreign_daemon_socket(&args_row(40, 30, 30, elsewhere), Some(&daemon()), temp),
            None
        );
    }

    /// Owning the operator's daemon is never the silent default: a tracker
    /// says which it does.
    #[test]
    fn a_tracker_names_whether_it_exempts_the_shared_daemon() {
        let mut exempting = ProcessTracker::exempting(10, &daemon());
        exempting.kernel_exe = installed_exe;
        exempting
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert_eq!(owned(&exempting), vec![10, 20]);
        let mut owning = ProcessTracker::owning_every_descendant(10);
        owning.kernel_exe = installed_exe;
        owning
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert_eq!(owned(&owning), vec![10, 20, 30, 31]);
    }

    fn daemon_kernel(pid: u32) -> Result<Option<KernelProcess>, String> {
        Ok(match pid {
            10 => Some(kernel_process(10, 1, 10, 1)),
            20 => Some(kernel_process(20, 10, 10, 2)),
            30 => Some(kernel_process(30, 20, 30, 3)),
            31 => Some(kernel_process(31, 30, 30, 4)),
            _ => None,
        })
    }

    const PROXY: &str = "/opt/homebrew/bin/ironmem serve --connect /Users/o/.ironmem/daemon.sock";

    /// Root 10 → proxy 20 → daemon 30 (own group) → its helper 31.
    fn daemon_rows(daemon_args: &str) -> Vec<ProcessRow> {
        vec![
            process_row(10, 1, 10),
            args_row(20, 10, 10, PROXY),
            args_row(30, 20, 30, daemon_args),
            args_row(31, 30, 30, "/opt/homebrew/bin/node mcp-helper"),
        ]
    }

    fn owned(tracker: &ProcessTracker) -> Vec<u32> {
        let mut owned: Vec<_> = tracker.known.iter().map(|(r, _)| r.pid).collect();
        owned.sort_unstable();
        owned
    }

    /// The proxy a harness's MCP config starts autospawns the shared daemon
    /// as its own child. The tracker owns the proxy, never the daemon or the
    /// daemon's own children, even when it first saw the daemon before its
    /// `exec` (still wearing the proxy's argv).
    #[test]
    fn the_shared_daemon_and_its_children_are_never_enrolled_or_signalled() {
        let assert_owns_only_root_and_proxy = |tracker: &ProcessTracker| {
            assert_eq!(owned(tracker), vec![10, 20]);
            assert_eq!(
                tracker.kill_order().map(|(r, _)| r.pid).collect::<Vec<_>>(),
                vec![20, 10]
            );
            for pid in [30, 31] {
                assert!(tracker
                    .signal(pid, STOP_SIGNAL)
                    .unwrap_err()
                    .to_string()
                    .contains("unauthenticated"));
            }
        };

        let mut tracker = daemon_tracker(installed_exe);
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert_owns_only_root_and_proxy(&tracker);

        // Sampled between fork and exec, the daemon wore the proxy's argv (and
        // had no children yet) and was enrolled; once the same process, by
        // kernel birth, shows the daemon argv it is released before anything
        // is enrolled under it.
        let mut tracker = daemon_tracker(installed_exe);
        let mut before_exec = daemon_rows(PROXY);
        before_exec.pop();
        tracker.observe_rows(before_exec, daemon_kernel).unwrap();
        assert!(tracker.known.iter().any(|(r, _)| r.pid == 30));
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert_owns_only_root_and_proxy(&tracker);

        // The proxy exited after ps sampled the daemon as its child: that
        // stale row is not a descendant that escaped authentication.
        let mut tracker = daemon_tracker(installed_exe);
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), |pid| {
                if pid == 20 {
                    Ok(None)
                } else {
                    daemon_kernel(pid)
                }
            })
            .unwrap();
    }

    /// A daemon on any socket but the configured one, such as one an
    /// ironmem test suite autospawns on a temp socket, is an ordinary tool:
    /// it and its children are owned and cleaned up.
    #[test]
    fn a_daemon_on_another_socket_is_enrolled_like_any_tool() {
        for daemon in [Some(daemon()), None] {
            let mut tracker = match &daemon {
                Some(daemon) => ProcessTracker::exempting(10, daemon),
                None => ProcessTracker::owning_every_descendant(10),
            };
            tracker.kernel_exe = installed_exe;
            tracker
                .observe_rows(daemon_rows(OTHER_DAEMON), daemon_kernel)
                .unwrap();
            assert_eq!(owned(&tracker), vec![10, 20, 30, 31], "{daemon:?}");
        }
        // Without a configured socket nothing is exempt.
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert_eq!(owned(&tracker), vec![10, 20, 30, 31]);
    }

    /// The daemon argv alone is not enough: an agent-built
    /// `target/debug/ironmem serve --listen <configured socket>` is owned
    /// and cleaned up like any tool. Only the controller's own executable,
    /// read from the kernel, is exempt, and an unreadable path is not.
    #[test]
    fn the_daemon_exemption_requires_the_controllers_own_executable() {
        let mut tracker = daemon_tracker(installed_exe);
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert_eq!(owned(&tracker), vec![10, 20]);
        let worktree_build: fn(u32) -> Option<PathBuf> =
            |_| Some("/w/owner-repo-7/target/debug/ironmem".into());
        let unreadable: fn(u32) -> Option<PathBuf> = |_| None;
        for kernel_exe in [worktree_build, unreadable] {
            let mut tracker = daemon_tracker(kernel_exe);
            tracker
                .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
                .unwrap();
            assert_eq!(owned(&tracker), vec![10, 20, 30, 31]);
        }
    }

    /// `known` keeps every exited descendant for the whole dispatch, and a
    /// build enrols hundreds. The daemon test (argv, then a kernel path
    /// read) runs once per row per observe, never once per dead descendant.
    #[test]
    fn the_daemon_test_does_not_scale_with_exited_descendants() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LOOKUPS: AtomicUsize = AtomicUsize::new(0);
        let mut tracker = daemon_tracker(|pid| {
            LOOKUPS.fetch_add(1, Ordering::SeqCst);
            installed_exe(pid)
        });
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        // Fifty descendants that have since exited.
        for pid in 1000..1050 {
            tracker
                .known
                .push((process_row(pid, 20, 10), Birth(u64::from(pid), 0)));
        }
        LOOKUPS.store(0, Ordering::SeqCst);
        tracker
            .observe_rows(daemon_rows(DAEMON_DB), daemon_kernel)
            .unwrap();
        assert!(LOOKUPS.load(Ordering::SeqCst) <= 2, "{LOOKUPS:?}");
    }

    /// A tool shell that exits between samples leaves its background child
    /// reparented to init, outside every tracked ancestry and the root's
    /// group. Completion is refused while any process the current user
    /// started after the invocation's root still has its working directory
    /// in the worktree. The operator's older shell there, other users'
    /// processes, the supervisor itself and the shared daemon (with its
    /// children) are not the invocation's.
    #[test]
    fn only_a_new_same_user_process_in_the_worktree_withholds_completion() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().canonicalize().unwrap();
        let sibling = PathBuf::from(format!("{}-other", worktree.display()));
        let mut tracker = daemon_tracker(installed_exe);
        tracker
            .observe_rows(vec![process_row(10, 1, 10)], |pid| {
                Ok((pid == 10).then(|| kernel_process(10, 1, 10, 100)))
            })
            .unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let me = std::process::id();
        let rows = vec![
            args_row(40, 1, 40, "sleep 30"),
            args_row(30, 1, 30, DAEMON_DB),
            args_row(31, 30, 30, "/opt/homebrew/bin/node mcp-helper"),
        ];
        let process = |pid, uid, start, cwd: &Path| CwdProcess {
            pid,
            uid,
            birth: Birth(start, 0),
            cwd: cwd.to_path_buf(),
        };
        let check = |tracker: &ProcessTracker, found: CwdProcess| -> Result<(), String> {
            match tracker.verify_worktree_quiet_with(
                &worktree,
                |_| Ok(vec![found.clone()]),
                || Ok(rows.clone()),
                Duration::ZERO,
            ) {
                Ok(Quiet::Yes) => Ok(()),
                Ok(Quiet::Held(hold)) => {
                    // What was found is recorded by PID and birth too.
                    assert_eq!(hold.stragglers.len(), 1, "{hold:?}");
                    Err(hold.reason)
                }
                Err(e) => Err(e.to_string()),
            }
        };
        for started in [100, 150] {
            let err = check(&tracker, process(40, uid, started, &worktree.join("src")))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("process 40") && err.contains("sleep 30"),
                "{err}"
            );
        }
        for ignored in [
            process(40, uid, 99, &worktree),
            process(40, uid + 1, 150, &worktree),
            process(me, uid, 150, &worktree),
            process(30, uid, 150, &worktree),
            process(31, uid, 150, &worktree),
            process(40, uid, 150, Path::new("/elsewhere")),
            process(40, uid, 150, &sibling),
        ] {
            assert!(check(&tracker, ignored.clone()).is_ok(), "{ignored:?}");
        }
        // The same daemon argv on another executable is no exemption.
        tracker.kernel_exe = |_| Some("/w/target/debug/ironmem".into());
        assert!(check(&tracker, process(30, uid, 150, &worktree)).is_err());
        // An enumeration that fails proves nothing.
        // An enumeration that keeps failing proves nothing either, but it
        // is a hold recovery can retry, recorded as such, not an error.
        let Quiet::Held(hold) = tracker
            .verify_worktree_quiet_with(
                &worktree,
                |_| Err("lookup refused".into()),
                || Ok(rows.clone()),
                Duration::ZERO,
            )
            .unwrap()
        else {
            panic!("a failing scan must hold");
        };
        assert!(hold.stragglers.is_empty());
        assert!(
            hold.scan_failed
                .as_deref()
                .is_some_and(|e| e.contains("lookup refused")),
            "{hold:?}"
        );
        assert_eq!(
            (hold.worktree.as_path(), hold.uid),
            (worktree.as_path(), uid)
        );
    }

    /// Only a process the invocation could have started can fail the scan.
    /// Its owner and birth are read first: an older process (the operator's
    /// protected app, say) is skipped before its working directory is
    /// touched, so its being unreadable is irrelevant. An in-window one whose
    /// directory cannot be read holds. One that exits mid-scan is gone.
    #[test]
    fn only_an_unreadable_process_started_in_the_window_fails_the_scan() {
        let spawned = Birth(100, 0);
        let uid = 501;
        let owner_birth = |pid: u32| -> Result<Option<(u32, Birth)>, String> {
            Ok(match pid {
                1 => Some((uid, Birth(50, 0))),
                3 => None,
                6 => Some((uid + 1, Birth(150, 0))),
                _ => Some((uid, Birth(150, 0))),
            })
        };
        let cwd_reads = std::cell::RefCell::new(Vec::new());
        let mut cwd_of = |pid: u32| -> Result<Option<PathBuf>, String> {
            cwd_reads.borrow_mut().push(pid);
            match pid {
                1 | 2 => Err("Operation not permitted".into()),
                4 => Ok(None),
                _ => Ok(Some(PathBuf::from(format!("/w/{pid}")))),
            }
        };
        let found = scan_cwds([1, 3, 4, 6, 7], uid, spawned, owner_birth, &mut cwd_of).unwrap();
        assert_eq!(found.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![7]);
        assert_eq!(
            cwd_reads.borrow().clone(),
            vec![4, 7],
            "old, gone and foreign processes are never read"
        );
        let err = scan_cwds([1, 2, 7], uid, spawned, owner_birth, &mut cwd_of).unwrap_err();
        assert!(
            err.reason.contains("process 2") && err.reason.contains("not permitted"),
            "{err}"
        );
        // The process that failed it is named by PID and birth, so a hold
        // can wait for that very process to go.
        assert_eq!(err.unreadable, Some((2, Birth(150, 0))), "{err}");
        // Exited between the directory read and the birth re-check: gone.
        let mut reads = 0;
        let vanishing = |_pid: u32| -> Result<Option<(u32, Birth)>, String> {
            reads += 1;
            Ok((reads == 1).then_some((uid, Birth(150, 0))))
        };
        assert!(scan_cwds([8], uid, spawned, vanishing, &mut cwd_of)
            .unwrap()
            .is_empty());
        // Replaced by a new process under the same PID: the directory may
        // be the newcomer's, so the scan cannot vouch for it.
        let mut reads = 0;
        let replaced = |_pid: u32| -> Result<Option<(u32, Birth)>, String> {
            reads += 1;
            Ok(Some((uid, Birth(150 + reads, 0))))
        };
        let err = scan_cwds([9], uid, spawned, replaced, &mut cwd_of).unwrap_err();
        assert_eq!(
            err.unreadable, None,
            "a replaced PID names no process: {err}"
        );
    }

    /// A scan that keeps failing on one in-window process's unreadable
    /// directory records that process as a straggler, so recovery waits for
    /// it by birth even if it later becomes readable after leaving the
    /// worktree. The reason says so in plain words.
    #[test]
    fn a_scan_failed_by_one_process_holds_for_that_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker
            .observe_rows(vec![process_row(10, 1, 10)], |pid| {
                Ok((pid == 10).then(|| kernel_process(10, 1, 10, 100)))
            })
            .unwrap();
        let hold = |error: ScanError| {
            let Quiet::Held(hold) = tracker
                .verify_worktree_quiet_with(
                    dir.path(),
                    |_| Err(error.clone()),
                    || Ok(Vec::new()),
                    std::time::Duration::ZERO,
                )
                .unwrap()
            else {
                panic!("a failing scan must hold");
            };
            hold
        };
        let unreadable = hold(ScanError {
            reason: "process 40, started during the invocation, has an unreadable working \
                     directory: Operation not permitted"
                .into(),
            unreadable: Some((40, Birth(150, 0))),
        });
        assert_eq!(unreadable.stragglers, vec![(40, Birth(150, 0))]);
        assert!(unreadable.scan_failed.is_some(), "{unreadable:?}");
        assert!(
            !unreadable.reason.contains("scan_failed"),
            "{}",
            unreadable.reason
        );
        assert!(
            unreadable.reason.contains("could not all be checked")
                && unreadable.reason.contains("process 40"),
            "{}",
            unreadable.reason
        );
        // A failure that names no process (the listing itself) records none.
        let listing = hold(ScanError::from("cannot list processes: busy"));
        assert!(listing.stragglers.is_empty(), "{listing:?}");
        assert!(
            !listing.reason.contains("scan_failed"),
            "{}",
            listing.reason
        );
    }

    /// A failed scan is retried within the grace before completion is refused.
    #[test]
    fn a_failed_scan_is_retried_within_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker
            .observe_rows(vec![process_row(10, 1, 10)], |pid| {
                Ok((pid == 10).then(|| kernel_process(10, 1, 10, 100)))
            })
            .unwrap();
        let mut scans = 0;
        let quiet = tracker
            .verify_worktree_quiet_with(
                dir.path(),
                |_| {
                    scans += 1;
                    if scans == 1 {
                        Err("transient".into())
                    } else {
                        Ok(Vec::new())
                    }
                },
                || Ok(Vec::new()),
                std::time::Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!((quiet, scans), (Quiet::Yes, 2));
    }

    /// The real enumeration lists this test process, and the real kernel
    /// reads give its working directory, cheaply enough to run at every
    /// completion. Only this process is read: an unrelated one caught
    /// mid-exec of a setuid binary (another test's `ps`) refuses the read,
    /// which would fail a whole-machine scan, as it must.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_kernel_lists_this_process_with_its_working_directory() {
        let started = std::time::Instant::now();
        let pids = user_pids().unwrap();
        assert!(pids.contains(&std::process::id()), "{} pids", pids.len());
        let processes = processes_with_cwd(vec![std::process::id()], Birth(0, 0)).unwrap();
        let elapsed = started.elapsed();
        let me = processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("this process is listed");
        assert_eq!(
            me.cwd,
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert_eq!(Some(me.birth), birth(me.pid).unwrap());
        eprintln!(
            "{} pids listed and this one read in {elapsed:?}",
            pids.len()
        );
    }

    /// The writer enforces the reader's bound. Past it, the commands (kept
    /// only for an operator) are dropped and every PID, group, parent and
    /// birth is still written; an identity too large even then is refused
    /// rather than written unreadable.
    #[test]
    fn an_identity_past_the_read_bound_drops_commands_or_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let mut identity = read_identity(&a).unwrap();
        let long = "x".repeat(RECORDED_ARGS_LIMIT);
        identity.descendants = (2..6000)
            .map(|pid| RecordedProcess::new(&args_row(pid, 1234, 1234, &long), Birth(7, 0)))
            .collect();
        assert!(serde_json::to_vec(&identity).unwrap().len() > IDENTITY_LIMIT);
        persist_identity(&a, &identity).unwrap();
        let read = read_identity(&a).unwrap();
        assert_eq!(read.descendants.len(), identity.descendants.len());
        assert!(read.descendants.iter().all(|d| d.args.is_empty()));
        assert_eq!(read.descendants[0].birth, Some(Birth(7, 0)));
        identity.descendants = (2..40_000)
            .map(|pid| RecordedProcess::new(&args_row(pid, 1234, 1234, ""), Birth(7, 0)))
            .collect();
        let err = persist_identity(&a, &identity).unwrap_err().to_string();
        assert!(err.contains("identity"), "{err}");
        // The earlier, readable identity stands.
        assert_eq!(read_identity(&a).unwrap().descendants.len(), 5998);
    }

    /// The identity is rewritten as descendants enrol, and `read_identity`
    /// refuses anything over 1 MiB. Only descendants alive at the last sample
    /// are recorded, each with its kernel birth and a bounded command, so a
    /// build that enrols hundreds of long-argv tools stays well under it.
    #[test]
    fn the_recorded_identity_holds_only_live_descendants_and_stays_bounded() {
        let long = format!("/usr/bin/rustc {}", "--cfg feature=x ".repeat(1000));
        let mut rows = vec![process_row(10, 1, 10)];
        rows.extend((100..600).map(|pid| args_row(pid, 10, 10, &long)));
        let alive = |pid: u32| {
            Ok(Some(kernel_process(
                pid,
                if pid == 10 { 1 } else { 10 },
                10,
                u64::from(pid),
            )))
        };
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker.observe_rows(rows.clone(), alive).unwrap();
        let (root_birth, descendants) = tracker.recorded();
        assert_eq!(root_birth, Some(Birth(10, 0)));
        assert_eq!(descendants.len(), 500);
        assert!(descendants
            .iter()
            .all(|d| d.args.len() <= 256 && d.birth.is_some()));
        let identity = ProcessIdentity {
            version: 1,
            root_pid: 10,
            root_pgid: 10,
            root_birth,
            schema_path: "/s".into(),
            invocation_id: "i".into(),
            descendants,
        };
        let bytes = serde_json::to_vec(&identity).unwrap();
        assert!(bytes.len() < 512 * 1024, "{} bytes", bytes.len());
        // Three hundred of them exit: only the live ones stay recorded.
        tracker
            .observe_rows(rows, |pid| {
                if (100..400).contains(&pid) {
                    Ok(None)
                } else {
                    alive(pid)
                }
            })
            .unwrap();
        let (_, descendants) = tracker.recorded();
        assert_eq!(descendants.len(), 200);
        assert!(descendants.iter().all(|d| d.pid >= 400));
    }

    /// `clear-dispatch` asks `live_evidence` whether anything could still be
    /// the issue's work. An identity it cannot read (corrupt, or over the
    /// size cap) hides the recorded tree, so that is evidence, not "nothing".
    #[test]
    fn an_unreadable_identity_is_live_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let empty = ProcessSnapshot::Available(Vec::new());
        assert_eq!(empty.live_evidence("owner-repo-42", Some(&a)), None);
        for bad in [b"not json".to_vec(), vec![b' '; 1024 * 1024 + 1]] {
            std::fs::write(identity_path(&a), bad).unwrap();
            let evidence = empty.live_evidence("owner-repo-42", Some(&a));
            assert!(
                evidence.as_deref().is_some_and(|e| e.contains("identity")),
                "{evidence:?}"
            );
        }
        // No identity at all: nothing was ever spawned to hide.
        std::fs::remove_file(identity_path(&a)).unwrap();
        assert_eq!(empty.live_evidence("owner-repo-42", Some(&a)), None);
    }

    /// Release is for the enrolled process itself after its exec. A tracked
    /// tool that exited and whose PID now belongs to a daemon-shaped process
    /// is a reuse, and must hold rather than silently drop the tracked tool.
    #[test]
    fn a_reused_pid_showing_the_daemon_argv_is_a_reuse_not_a_release() {
        let mut tracker = daemon_tracker(installed_exe);
        let mut before = daemon_rows(PROXY);
        before.pop();
        tracker.observe_rows(before, daemon_kernel).unwrap();
        assert!(tracker.known.iter().any(|(r, _)| r.pid == 30));
        let err = tracker
            .observe_rows(daemon_rows(DAEMON_DB), |pid| {
                if pid == 30 {
                    Ok(Some(kernel_process(30, 1, 30, 99)))
                } else {
                    daemon_kernel(pid)
                }
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("tracked PID was reused"), "{err}");
    }

    /// `live_evidence` (what `clear-dispatch` refuses on) holds an issue for
    /// any active row naming one of that issue's Autopilot markers, whatever
    /// runs it: a launcher shape the grammar does not parse (`bash -e`,
    /// `zsh`) still carries the marker. Another issue's marker, or a path
    /// not shaped like one, is not this issue's.
    #[test]
    fn any_row_naming_this_issues_marker_is_live_evidence() {
        let mine =
            "/r/.git/ironmem/autopilot/o-r-7/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json";
        let theirs = mine.replace("o-r-7", "o-r-8");
        let evidence = |args: &str| {
            ProcessSnapshot::Available(vec![args_row(40, 1, 40, args)]).live_evidence("o-r-7", None)
        };
        for args in [
            format!("bash -e /u/.local/bin/muse exec --json --output-schema {mine}"),
            format!("zsh /u/.local/bin/muse exec --output-schema={mine}"),
            format!("/usr/bin/env node /x/codex exec --output-schema {mine} prompt"),
        ] {
            let e = evidence(&args).unwrap_or_else(|| panic!("no evidence for {args}"));
            assert!(e.contains("process 40") && e.contains("o-r-7"), "{e}");
        }
        for args in [
            format!("zsh /u/.local/bin/muse exec --output-schema {theirs}"),
            "zsh /u/.local/bin/muse exec --output-schema /tmp/o-r-7/schema.json".into(),
        ] {
            assert_eq!(evidence(&args), None, "{args}");
        }
        // Liveness is unchanged by it: only this issue is held, and only
        // where clearing is decided.
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        testing::complete(&a);
        let unrelated = ProcessSnapshot::Available(vec![args_row(
            40,
            1,
            40,
            &format!("zsh /u/muse exec --output-schema {mine}"),
        )]);
        assert_eq!(unrelated.liveness(Some(&a)), Liveness::NotListed);
    }

    /// If cleanup's own sample failed before its release pass, the identity
    /// still names a shared daemon an earlier sample enrolled before its
    /// exec. That PID, now the operator's daemon by argv and executable, is
    /// no evidence of this invocation; any other recorded PID still is.
    #[test]
    fn a_recorded_pid_now_running_the_shared_daemon_is_not_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let mut identity = read_identity(&a).unwrap();
        identity.descendants = vec![
            RecordedProcess::new(&process_row(30, 1234, 30), Birth(3, 0)),
            RecordedProcess::new(&process_row(31, 1234, 31), Birth(4, 0)),
        ];
        let is_daemon = |r: &ProcessRow| is_shared_daemon(Some(&daemon()), installed_exe, r);
        let daemon_only = vec![args_row(30, 1, 30, DAEMON_DB)];
        assert_eq!(
            recorded_tree_evidence(&daemon_only, &identity, false, &is_daemon),
            None
        );
        // Without the exemption it is a recorded PID like any other.
        assert!(recorded_tree_evidence(&daemon_only, &identity, false, &|_| false).is_some());
        let tool = vec![args_row(30, 1, 30, DAEMON_DB), process_row(31, 1, 31)];
        assert!(recorded_tree_evidence(&tool, &identity, false, &is_daemon)
            .is_some_and(|e| e.contains("process 31")));
        // The root's own PID is never exempt.
        let mut root = identity.clone();
        root.root_pid = 30;
        root.root_pgid = 30;
        assert!(recorded_tree_evidence(&daemon_only, &root, false, &is_daemon).is_some());
    }

    /// Restart recovery never mistakes the daemon for invocation evidence:
    /// it is never recorded as a descendant, carries no marker, and leads a
    /// group of its own.
    #[test]
    fn a_daemon_spawned_under_a_finished_invocation_is_not_evidence_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        testing::complete(&a);
        let daemon = args_row(4321, 1234, 4321, DAEMON_DB);
        assert!(daemon.is_shared_ironmem_daemon(&self::daemon()));
        let snapshot = ProcessSnapshot::Available(vec![daemon]);
        assert_eq!(snapshot.liveness(Some(&a)), Liveness::NotListed);
        assert_eq!(snapshot.live_evidence("owner-repo-42", Some(&a)), None);
        assert!(snapshot.orphan_markers().is_empty());
    }

    #[test]
    fn stale_descendant_row_cannot_enroll_a_reused_unrelated_pid() {
        let root = kernel_process(10, 1, 10, 1);
        for replacement in [Some(kernel_process(20, 999, 999, 2)), None] {
            let mut tracker = ProcessTracker::owning_every_descendant(10);
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
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
            let mut tracker = ProcessTracker::owning_every_descendant(10);
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
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
