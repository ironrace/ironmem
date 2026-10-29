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
    /// Whether the row is a running process rather than a zombie. Linux's
    /// `ps` shows `Zl` for a process whose main thread called `pthread_exit`
    /// while another thread runs (measured): it still runs and still takes
    /// SIGKILL, so it is active. macOS's `ps` has no `l` flag.
    pub fn active(&self) -> bool {
        !self.stat.starts_with('Z') || self.stat.contains('l')
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
            // `-`: the prompt is read from stdin, and is Autopilot's last
            // word; anything after it is a shape Autopilot never writes.
            "-" => {
                if words.next().is_some() {
                    return Err("Codex argv continues after its stdin prompt `-`".into());
                }
                break;
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

/// Rows of `ps -o pid=,ppid=,pgid=,stat=,args=` output, one per non-blank
/// line. Only splits the columns: the first four are whitespace-separated and
/// the rest of the line is the command. Reading a supervised harness's
/// invocation marker out of that command (Codex or Muse alike) is
/// [`ProcessRow::marker`]'s job, not this one's.
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
    #[non_exhaustive]
    Available(Vec<ProcessRow>),
    Unavailable {
        reason: String,
    },
}

/// Where process-table rows come from: the real `ps` ([`UnixProcesses`]) or
/// a test's fixture. Every supervised harness (Codex, Muse) is judged from
/// the same rows.
pub trait ProcessSource {
    /// The whole process table, or why it could not be read.
    fn list(&mut self) -> Result<Vec<ProcessRow>, String>;
}

/// The machine's process table from a bounded `ps -axww` run: two seconds
/// and 16 MiB at most, so a hung or flooding `ps` cannot stall supervision.
/// Unsupported off Unix.
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
    // Through the cached `find_launcher` resolution, like every ps spawn.
    let mut command = Command::new(crate::launcher::cached_launcher("ps"));
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

/// One reading of `source`: the rows, or why they are unavailable. Liveness,
/// recovery and clearing decisions for every supervised harness take this,
/// and an unavailable table decides nothing.
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
/// more, and [`persist_identity`] never writes more. An exit record, which
/// lists the processes still in the worktree, shares it
/// (`invocation::EXIT_LIMIT`).
pub(crate) const IDENTITY_LIMIT: usize = 1024 * 1024;

/// Write `identity` within [`IDENTITY_LIMIT`]. Past it, each descendant's
/// command (kept only for an operator) is dropped, and every PID, group,
/// parent and birth recovery reads is still written; an identity too large
/// even then is refused, leaving the previous file in place, rather than
/// written unreadable.
pub(crate) fn persist_identity(
    a: &InvocationArtifacts,
    identity: &ProcessIdentity,
) -> Result<(), MemoryError> {
    persist_identity_synced(a, identity, true)
}

/// [`persist_identity`], durable across a power loss only when `full`
/// (see [`super::invocation::persist_private_atomic_synced`]).
pub(crate) fn persist_identity_synced(
    a: &InvocationArtifacts,
    identity: &ProcessIdentity,
    full: bool,
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
    super::invocation::persist_private_atomic_synced(&identity_path(a), "process", &bytes, full)
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
    let descendants: std::collections::HashSet<u32> =
        identity.descendants.iter().map(|d| d.pid).collect();
    if !completed {
        if let Some(r) = active()
            .find(|r| r.pid == identity.root_pid || (descendants.contains(&r.pid) && !is_daemon(r)))
        {
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
        if let Some(pending) = super::invocation::launch_pending(a) {
            return pending;
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
            if let Some(unreadable) = super::invocation::exit_record_unreadable(a) {
                return format!(
                    "{unreadable}; historical descendants cannot be ruled out until it is \
                     repaired or the invocation is cleared"
                );
            }
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
        // No identity (a launch in progress, `invocation::launch_pending`,
        // or one that never spawned) or an unreadable one: never idle.
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
                Ok(Some(marker))
                    if is_autopilot_marker_path(marker)
                        && marker_issue_slug(marker) == Some(issue_slug) =>
                {
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
        // invocation, unless its launch may still be in progress (the
        // identity follows the spawn). One that exists but cannot be read
        // hides the recorded tree, so it is evidence, never "nothing
        // running".
        if let Some(pending) = super::invocation::launch_pending(a) {
            return Some(pending);
        }
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
                .filter_map(|r| match r.marker() {
                    Ok(marker) => marker,
                    Err(reason) => {
                        tracing::warn!(pid = r.pid, reason = %super::scrub::scrub_and_bound(&reason, 500).text,
                            "autopilot orphan scan found an ambiguous invocation marker; inspect this process");
                        None
                    }
                })
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Birth(u64, u64);

/// The kernel birth a process started at wall-clock `at` would carry, for
/// comparing a recorded start time against process births. macOS births
/// are wall-clock; Linux births are clock ticks since boot, so its boot time
/// (`btime` in `/proc/stat`) is subtracted ([`birth_since_boot`]), and a
/// start before boot counts from boot. `None` where that cannot be read.
pub(crate) fn birth_at(at: std::time::SystemTime) -> Option<Birth> {
    let since_epoch = at.duration_since(std::time::UNIX_EPOCH).ok()?;
    #[cfg(target_os = "macos")]
    {
        Some(Birth(
            since_epoch.as_secs(),
            u64::from(since_epoch.subsec_micros()),
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let boot: u64 = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: sysconf has no preconditions.
        let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
        Some(birth_since_boot(since_epoch, boot, ticks))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = since_epoch;
        None
    }
}
/// The Linux birth (clock ticks since boot, at `ticks` a second) for a time
/// `since_epoch` after the epoch, with the machine booted `boot_secs` after
/// it. A time before boot is clamped to boot: a recorded start from before a
/// reboot (or skewed) then checks every process born since boot, which is
/// every process running, instead of reading as unknown. Linux only, but
/// tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn birth_since_boot(since_epoch: std::time::Duration, boot_secs: u64, ticks: u64) -> Birth {
    let after_boot = since_epoch.saturating_sub(std::time::Duration::from_secs(boot_secs));
    Birth(after_boot.as_secs() * ticks, 0)
}

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

// `kinfo_proc` is read at its LP64 offsets below; a 32-bit Darwin build
// would misread every field, so it does not build.
#[cfg(all(target_os = "macos", not(target_pointer_width = "64")))]
compile_error!("autopilot reads `kinfo_proc` at its LP64 offsets; 32-bit macOS is not supported");

/// One process's `kinfo_proc`, as `sysctl(KERN_PROC_PID)` reports it.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
struct KinfoProc {
    process: KernelProcess,
    ruid: u32,
    euid: u32,
}

/// The kernel's record for `pid` from `sysctl(CTL_KERN, KERN_PROC,
/// KERN_PROC_PID)`, or `None` once it has exited (a zombie included).
///
/// Unlike `proc_pidinfo(PROC_PIDTBSDINFO)`, which refuses a process running
/// a setuid binary (an agent's `ps`, `top` or `sudo`) with EPERM, this
/// reads any process without privilege (measured on macOS 27). `libc` has
/// no `kinfo_proc`, so its fields are read at their offsets in the 648-byte
/// LP64 layout of `<sys/sysctl.h>` (`the_sysctl_record_agrees_with_the_bsd_
/// record` checks them against `PROC_PIDTBSDINFO` for this process).
#[cfg(target_os = "macos")]
fn kinfo_proc(pid: u32) -> Result<Option<KinfoProc>, String> {
    Ok(kinfo_proc_record(pid)?.and_then(|(record, zombie)| (!zombie).then_some(record)))
}

/// [`kinfo_proc`], keeping a zombie's record (`SZOMB`), marked `true`: its
/// start time, parent and group are still its own (macOS `ps` lists a
/// zombie with its start time), which [`root_observation`] needs for an
/// invocation root that has exited but is not yet reaped.
#[cfg(target_os = "macos")]
fn kinfo_proc_record(pid: u32) -> Result<Option<(KinfoProc, bool)>, String> {
    const SIZE: usize = 648;
    const P_STARTTIME_SEC: usize = 0;
    const P_STARTTIME_USEC: usize = 8;
    const P_STAT: usize = 36;
    const P_PID: usize = 40;
    const P_RUID: usize = 392;
    const CR_UID: usize = 420;
    const E_PPID: usize = 560;
    const E_PGID: usize = 564;
    /// `SZOMB` from `<sys/proc.h>`.
    const SZOMB: u8 = 5;
    let pid_arg = i32::try_from(pid).map_err(|_| format!("process {pid} is out of range"))?;
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid_arg,
    ];
    let mut buffer = [0u8; SIZE];
    let mut length = SIZE;
    // SAFETY: the kernel writes at most `length` bytes into `buffer`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
            return Ok(None);
        }
        return Err(format!("cannot authenticate process {pid}: {error}"));
    }
    if length == 0 {
        return Ok(None);
    }
    if length != SIZE {
        return Err("incomplete process birth identity".into());
    }
    let bytes4 = |at: usize| -> [u8; 4] { buffer[at..at + 4].try_into().expect("4 bytes") };
    let seconds = i64::from_ne_bytes(
        buffer[P_STARTTIME_SEC..P_STARTTIME_SEC + 8]
            .try_into()
            .expect("8 bytes"),
    );
    let micros = i32::from_ne_bytes(bytes4(P_STARTTIME_USEC));
    let record_pid = i32::from_ne_bytes(bytes4(P_PID));
    if record_pid != pid_arg {
        return Err("kernel process identity mismatch".into());
    }
    let zombie = buffer[P_STAT] == SZOMB;
    let (Ok(seconds), Ok(micros)) = (u64::try_from(seconds), u64::try_from(micros)) else {
        return Err("invalid process birth identity".into());
    };
    let ppid = i32::from_ne_bytes(bytes4(E_PPID));
    let pgid = i32::from_ne_bytes(bytes4(E_PGID));
    let record = KinfoProc {
        process: KernelProcess {
            pid,
            ppid: u32::try_from(ppid).map_err(|_| "invalid process parent")?,
            pgid: u32::try_from(pgid).map_err(|_| "invalid process group")?,
            birth: Birth(seconds, micros),
        },
        ruid: u32::from_ne_bytes(bytes4(P_RUID)),
        euid: u32::from_ne_bytes(bytes4(CR_UID)),
    };
    Ok(Some((record, zombie)))
}

/// `pid`'s real and effective user IDs, or `None` when they cannot be read.
pub(crate) fn real_and_effective_uid(pid: u32) -> Option<(u32, u32)> {
    #[cfg(target_os = "macos")]
    {
        kinfo_proc(pid).ok().flatten().map(|k| (k.ruid, k.euid))
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        uids_from_status(&status)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// Ancestry and birth come from the same kernel record, never a stale ps row.
fn kernel_observation(pid: u32) -> Result<Option<KernelProcess>, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(kinfo_proc(pid)?.map(|record| record.process))
    }
    #[cfg(target_os = "linux")]
    {
        match proc_stat_text(pid)? {
            Some(text) => kernel_process_from_stat(pid, &text),
            None => Ok(None),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        Err("kernel process identity is unsupported on this platform".into())
    }
}

/// Whether cleanup's SIGSTOP shows on `row`: `T` in `ps`. A `Z`-led row
/// that is still active (`Zl`, see [`ProcessRow::active`]) keeps its dead
/// leader's `Z` in `ps` after the stop reaches its live threads (measured),
/// so its tasks are read instead (`task_states`, each task's state letter):
/// every one that has not exited (`Z`, `X`) must read `T` or `t`. A task
/// list that cannot be read is not stopped, so quiesce keeps sampling and
/// then fails closed ("did not quiesce") rather than call it stable while a
/// thread may still fork.
fn row_stopped(
    row: &ProcessRow,
    task_states: impl FnOnce(u32) -> Result<Vec<char>, String>,
) -> bool {
    if row.stat.starts_with('T') {
        return true;
    }
    if !row.stat.starts_with('Z') {
        return false;
    }
    task_states(row.pid).is_ok_and(|states| {
        states
            .into_iter()
            .filter(|state| !matches!(state, 'Z' | 'X'))
            .all(|state| matches!(state, 'T' | 't'))
    })
}

/// Each task's state letter under Linux's `/proc/<pid>/task`
/// ([`states_of_tasks`]); an error wherever one cannot be read. No other
/// platform has per-task states here.
fn task_states(pid: u32) -> Result<Vec<char>, String> {
    #[cfg(target_os = "linux")]
    {
        states_of_tasks(
            pid,
            |pid| {
                std::fs::read_dir(format!("/proc/{pid}/task"))?
                    .map(|task| Ok(task?.path()))
                    .collect()
            },
            |path: &Path| std::fs::read_to_string(path),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(format!(
            "process {pid}'s task states are unreadable on this platform"
        ))
    }
}

/// The state letter of each task `list_tasks` gives for `pid`, from its
/// `stat` through `read`. A listed task whose `stat` then reads as gone
/// (ENOENT, ESRCH) has just exited and is skipped; any other failure is an
/// error. Linux only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn states_of_tasks(
    pid: u32,
    list_tasks: impl FnOnce(u32) -> std::io::Result<Vec<PathBuf>>,
    read: impl Fn(&Path) -> std::io::Result<String>,
) -> Result<Vec<char>, String> {
    let tasks = list_tasks(pid).map_err(|e| format!("cannot list process {pid}'s tasks: {e}"))?;
    let mut states = Vec::new();
    for task in tasks {
        match read(&task.join("stat")) {
            Ok(stat) => states.push(task_state(&stat).ok_or("malformed task stat")?),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(libc::ESRCH) => {}
            Err(e) => return Err(format!("cannot read a task of process {pid}: {e}")),
        }
    }
    Ok(states)
}

/// The state letter of a Linux task's `stat` text: the first field after
/// the LAST `)` of comm. Linux only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn task_state(stat: &str) -> Option<char> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

/// [`linux_cwd`] on this machine's `/proc`.
#[cfg(target_os = "linux")]
fn proc_cwd(pid: u32) -> Result<Option<PathBuf>, String> {
    linux_cwd(
        pid,
        |path: &Path| std::fs::read_link(path),
        |pid| {
            std::fs::read_dir(format!("/proc/{pid}/task"))?
                .map(|task| {
                    Ok(task?
                        .file_name()
                        .to_string_lossy()
                        .parse::<u32>()
                        .unwrap_or(0))
                })
                .collect()
        },
        |pid| kernel_observation(pid).map(|process| process.is_some()),
    )
}

/// The working directory of `pid` for the worktree scan on Linux, from
/// `/proc/<pid>/cwd` through `read_link`. `None` once it is gone.
///
/// A process whose leader thread exited while another thread runs (`ps`
/// `Zl`) has no `/proc/<pid>/cwd` (ENOENT), yet still runs, and its live
/// thread's `/proc/<pid>/task/<tid>/cwd` reads where it is (measured). So
/// when its own link is gone but the kernel (`running`) still has it
/// running, each other task's link (from `list_tasks`) is read; if none
/// reads, its directory is unreadable, which holds, never skipped. Linux
/// only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn linux_cwd(
    pid: u32,
    read_link: impl Fn(&Path) -> std::io::Result<PathBuf>,
    list_tasks: impl FnOnce(u32) -> std::io::Result<Vec<u32>>,
    running: impl FnOnce(u32) -> Result<bool, String>,
) -> Result<Option<PathBuf>, String> {
    let gone = |e: &std::io::Error| {
        e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH)
    };
    match read_link(Path::new(&format!("/proc/{pid}/cwd"))) {
        Ok(cwd) => return Ok(Some(cwd)),
        Err(e) if !gone(&e) => return Err(e.to_string()),
        Err(_) => {}
    }
    if !running(pid)? {
        return Ok(None);
    }
    let tasks = match list_tasks(pid) {
        Ok(tasks) => tasks,
        Err(e) if gone(&e) => return Ok(None),
        Err(e) => return Err(format!("its tasks cannot be listed: {e}")),
    };
    // The first refusal that does not mean "that thread is gone" says why
    // the hold cannot be lifted (EACCES, say).
    let mut refusal = None;
    for tid in tasks.into_iter().filter(|tid| *tid != pid) {
        match read_link(Path::new(&format!("/proc/{pid}/task/{tid}/cwd"))) {
            Ok(cwd) => return Ok(Some(cwd)),
            Err(e) if !gone(&e) => {
                refusal.get_or_insert(e);
            }
            Err(_) => {}
        }
    }
    let reason = "its main thread has exited and no live thread's working directory can be read";
    Err(match refusal {
        Some(e) => format!("{reason}: {e}"),
        None => reason.into(),
    })
}

/// One process's kernel record from the text of its Linux
/// `/proc/<pid>/stat`, or `None` once it has exited, a zombie or a dead
/// process (`X`) included, as macOS's `kinfo_proc` reads `SZOMB`. A zombie
/// nobody reaps (a container whose PID 1 never waits) would otherwise hold
/// everything waiting on its birth for ever. Its PID stays reserved, so
/// nothing reads a reuse under it meanwhile, and it runs nothing, so
/// skipping its signal changes nothing.
///
/// `Z` alone does not make a zombie: a process whose main thread called
/// `pthread_exit` while another thread runs reads `Z` too, with
/// `num_threads` (field 20) of 2 or more (measured). It still runs and
/// still takes SIGKILL, so it is read as running; only a `Z` with at most
/// one thread is a zombie, and a `Z` whose thread count cannot be read is an
/// error, never "exited". The comm, in parentheses, is whatever the process
/// named itself, spaces and `)` included, so every field after it is read
/// after the LAST `)`. Linux only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn kernel_process_from_stat(pid: u32, text: &str) -> Result<Option<KernelProcess>, String> {
    Ok(proc_stat_record(pid, text)?
        .filter(|record| !record.exited)
        .map(|record| record.process))
}

/// [`kernel_process_from_stat`] for the invocation root straight after
/// spawn ([`ProcessTracker::adopt_root`]): a zombie's record is returned,
/// marked exited, rather than counted gone ([`adoptable`] decides). A dead
/// task (`X`) is still gone. Linux only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn root_process_from_stat(pid: u32, text: &str) -> Result<Option<RootRecord>, String> {
    proc_stat_record(pid, text)
}

/// The parse behind [`kernel_process_from_stat`]: the record, marked
/// exited for a zombie (`Z` with at most one thread), or `None` for a dead
/// task (`X`).
#[cfg(any(target_os = "linux", test))]
fn proc_stat_record(pid: u32, text: &str) -> Result<Option<RootRecord>, String> {
    let fields = text
        .rsplit_once(')')
        .ok_or("malformed proc stat")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let record_pid = text
        .split_once(' ')
        .ok_or("missing proc pid")?
        .0
        .parse::<u32>()
        .map_err(|_| "invalid proc pid")?;
    if record_pid != pid {
        return Err("kernel process identity mismatch".into());
    }
    let exited = match fields.first() {
        Some(&"X") => return Ok(None),
        Some(&"Z") => {
            let threads: u64 = fields
                .get(17)
                .ok_or("missing proc thread count")?
                .parse()
                .map_err(|_| "invalid proc thread count")?;
            threads <= 1
        }
        _ => false,
    };
    let start = fields
        .get(19)
        .ok_or("missing proc birth identity")?
        .parse()
        .map_err(|_| "invalid proc birth identity")?;
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
    Ok(Some(RootRecord {
        process: KernelProcess {
            pid,
            ppid,
            pgid,
            birth: Birth(start, 0),
        },
        exited,
    }))
}

/// The text of Linux's `/proc/<pid>/stat`, or `None` once it is gone.
#[cfg(target_os = "linux")]
fn proc_stat_text(pid: u32) -> Result<Option<String>, String> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => Ok(Some(text)),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(libc::ESRCH) =>
        {
            Ok(None)
        }
        Err(e) => Err(format!("cannot authenticate process {pid}: {e}")),
    }
}

/// A kernel record read for the invocation root, which may already have
/// exited ([`root_observation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootRecord {
    process: KernelProcess,
    /// Its zombie: it has exited but is not yet reaped.
    exited: bool,
}

/// [`kernel_observation`] for the invocation root straight after spawn,
/// keeping a zombie's record, marked exited, rather than counting it gone,
/// on macOS ([`kinfo_proc_record`]) and Linux ([`root_process_from_stat`]).
fn root_observation(pid: u32) -> Result<Option<RootRecord>, String> {
    #[cfg(target_os = "linux")]
    {
        match proc_stat_text(pid)? {
            Some(text) => root_process_from_stat(pid, &text),
            None => Ok(None),
        }
    }
    #[cfg(target_os = "macos")]
    {
        Ok(kinfo_proc_record(pid)?.map(|(record, zombie)| RootRecord {
            process: record.process,
            exited: zombie,
        }))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(kernel_observation(pid)?.map(|process| RootRecord {
            process,
            exited: false,
        }))
    }
}

/// The root's record to adopt from what [`root_observation`] read, if any.
/// A root, running or exited, is adopted only while it is our own child (its
/// parent is `me`, this process): the unreaped child, or its zombie, then
/// holds the PID, so the record is the root's. A caller that had already
/// reaped it could otherwise adopt a reused PID's record, a stranger that
/// may well be running. A debugger attached to the root at spawn (macOS
/// reports the tracer as `e_ppid`) fails the check too; that only takes the
/// existing non-fatal path, where the first sample may still bind the root.
fn adoptable(root_pid: u32, me: u32, record: Option<RootRecord>) -> Option<KernelProcess> {
    let record = record?;
    (record.process.pid == root_pid && record.process.ppid == me).then_some(record.process)
}

/// The operator's shared ironmem daemon, which supervision never owns (see
/// [`ProcessRow::is_shared_ironmem_daemon`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDaemon {
    /// The configured shared-daemon socket (`Config::daemon_socket_path`).
    pub(crate) socket: PathBuf,
    /// The canonical ironmem executable the daemon must be running.
    pub(crate) exe: PathBuf,
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
pub(crate) const QUIET_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// A process the current user owns, with its working directory, as the
/// kernel reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CwdProcess {
    pid: u32,
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
fn scan_cwds<O: Into<ProcOwner>>(
    pids: impl IntoIterator<Item = u32>,
    uid: u32,
    spawned: Birth,
    known: &Known,
    mut owner_birth: impl FnMut(u32) -> Result<Option<(O, Birth)>, String>,
    mut cwd_of: impl FnMut(u32) -> Result<Option<PathBuf>, String>,
) -> Result<Vec<CwdProcess>, ScanError> {
    let mut processes = Vec::new();
    for pid in pids {
        let Some((owner, birth)) = owner_birth(pid)? else {
            continue;
        };
        if !owner.into().is_users(uid, known, spawned) || birth < spawned {
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
        processes.push(CwdProcess { pid, birth, cwd });
    }
    Ok(processes)
}

/// Who a process belongs to, as a worktree scan judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcOwner {
    euid: u32,
    ruid: u32,
    ppid: u32,
    pgid: u32,
    /// The parent's kernel record, read only for a process that is the
    /// user's by real uid alone.
    parent: Parent,
}

/// What a scan learned of a process's parent ([`ProcOwner::parent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    /// Not read: the process is the user's by effective uid, which decides.
    NotRead,
    /// Read, with this kernel birth.
    Born(Birth),
    /// Gone when read: the process is about to be reparented.
    Gone,
    /// The read failed, so nothing rules out its being the invocation's.
    Unreadable,
}

impl Parent {
    /// The parent as a kernel read of it answered.
    fn read<E>(read: Result<Option<Birth>, E>) -> Self {
        match read {
            Ok(Some(birth)) => Self::Born(birth),
            Ok(None) => Self::Gone,
            Err(_) => Self::Unreadable,
        }
    }
}

/// An owner known only by its effective uid (a test seam's shorthand).
impl From<u32> for ProcOwner {
    fn from(uid: u32) -> Self {
        Self {
            euid: uid,
            ruid: uid,
            ppid: 0,
            pgid: 0,
            parent: Parent::NotRead,
        }
    }
}

impl ProcOwner {
    /// Whether the scan counts the process as `uid`'s. Its effective uid
    /// decides, as before. One that is `uid`'s only by real uid is a setuid
    /// image (`login` behind every Terminal tab, `sudo`, `top`, `/bin/ps`)
    /// whose working directory is unreadable, so it counts only when tied
    /// to the invocation (its own birth, checked by the caller, is in the
    /// window):
    ///
    /// - its parent is a process the tracker knows (`known`);
    /// - its process group is led by one (the root's group, say), which an
    ///   orphan keeps when Linux hands it to a `systemd --user` subreaper;
    /// - it was reparented to init (`launchd` on macOS);
    /// - its parent was itself born during the invocation (`spawned` or
    ///   later): a process that appeared during it, between two samples;
    /// - its parent cannot be authenticated: gone when read (it is being
    ///   reparented) or a read that failed. Either way nothing rules the
    ///   process out, so the scan fails closed on it.
    ///
    /// One whose parent and group predate the invocation (the operator's
    /// shells and terminals: a Terminal tab's `login`, a `sudo` or `top` run
    /// by hand) is skipped and never fails the scan.
    fn is_users(self, uid: u32, known: &Known, spawned: Birth) -> bool {
        self.euid == uid
            || (self.ruid == uid
                && (self.ppid == 1
                    || known.contains(&self.ppid)
                    || known.contains(&self.pgid)
                    || match self.parent {
                        Parent::Born(born) => born >= spawned,
                        Parent::Gone | Parent::Unreadable => true,
                        Parent::NotRead => false,
                    }))
    }
}

/// Real and effective uid from a Linux `/proc/<pid>/status` text: the first
/// two fields of its `Uid:` line. Linux only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn uids_from_status(status: &str) -> Option<(u32, u32)> {
    let mut ids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .map(|id| id.parse::<u32>().ok());
    Some((ids.next()??, ids.next()??))
}

/// The invocation's PIDs a worktree scan ties processes to
/// ([`ProcOwner::is_users`]).
type Known = std::collections::HashSet<u32>;

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
        /// `PROC_UID_ONLY` and `PROC_RUID_ONLY` from `<sys/proc_info.h>`,
        /// absent from libc: by effective, then by real user.
        const PROC_UID_ONLY: u32 = 4;
        const PROC_RUID_ONLY: u32 = 5;
        const MAX_PIDS: usize = 1 << 20;
        let list = |filter: u32| -> Result<Vec<i32>, String> {
            let mut pids = vec![0i32; 4096];
            loop {
                let capacity = (pids.len() * std::mem::size_of::<i32>()) as i32;
                // SAFETY: the kernel writes at most `capacity` bytes of PIDs.
                let bytes =
                    unsafe { libc::proc_listpids(filter, uid, pids.as_mut_ptr().cast(), capacity) };
                if bytes < 0 {
                    return Err(format!(
                        "cannot list processes: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let count = bytes as usize / std::mem::size_of::<i32>();
                if count < pids.len() {
                    pids.truncate(count);
                    return Ok(pids);
                }
                if pids.len() >= MAX_PIDS {
                    return Err("process list exceeded its size bound".into());
                }
                pids.resize(pids.len() * 2, 0);
            }
        };
        // A setuid tool keeps the user's real uid (cleanup can signal it)
        // but not the effective one, so it is listed only by real uid.
        let mut pids: Vec<u32> = list(PROC_UID_ONLY)?
            .into_iter()
            .chain(list(PROC_RUID_ONLY)?)
            .filter(|pid| *pid > 0)
            .map(|pid| pid as u32)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
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
/// A process is the user's when its effective *or* real uid is: an escaped
/// setuid tool keeps the real one, and cleanup can signal it, so it must
/// keep the worktree from reading quiet (its working directory may then be
/// unreadable, which holds the scan). macOS reads `sysctl(KERN_PROC_PID)`
/// and `PROC_PIDVNODEPATHINFO`: about 1.5 ms for ~600 processes, measured.
/// Linux reads `/proc`.
fn processes_with_cwd(
    pids: Vec<u32>,
    spawned: Birth,
    known: &Known,
) -> Result<Vec<CwdProcess>, ScanError> {
    let uid = current_uid().ok_or("process working directories are unreadable here")?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let owner_birth = |pid: u32| {
            Ok(kinfo_proc(pid)?.map(|record| {
                // The parent is read only for a real-uid-only process.
                let parent = if record.euid != uid && record.ruid == uid {
                    Parent::read(
                        kinfo_proc(record.process.ppid).map(|p| p.map(|p| p.process.birth)),
                    )
                } else {
                    Parent::NotRead
                };
                (
                    ProcOwner {
                        euid: record.euid,
                        ruid: record.ruid,
                        ppid: record.process.ppid,
                        pgid: record.process.pgid,
                        parent,
                    },
                    record.process.birth,
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
        scan_cwds(pids, uid, spawned, known, owner_birth, cwd_of)
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
            let ruid = real_and_effective_uid(pid).map_or(owner, |(real, _)| real);
            Ok(kernel_observation(pid)?.map(|process| {
                // The parent is read only for a real-uid-only process.
                let parent = if owner != uid && ruid == uid {
                    Parent::read(kernel_observation(process.ppid).map(|p| p.map(|p| p.birth)))
                } else {
                    Parent::NotRead
                };
                (
                    ProcOwner {
                        euid: owner,
                        ruid,
                        ppid: process.ppid,
                        pgid: process.pgid,
                        parent,
                    },
                    process.birth,
                )
            }))
        };
        let refused = |pid: u32| {
            std::fs::read_link(format!("/proc/{pid}/cwd"))
                .is_err_and(|e| matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM)))
        };
        let cwd_of = |pid: u32| {
            cwd_unless_refused_stranger(pid, known, spawned, proc_cwd, refused, kernel_observation)
        };
        scan_cwds(pids, uid, spawned, known, owner_birth, cwd_of)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pids, uid, spawned, known);
        Err("process working directories are unreadable on this platform".into())
    }
}

/// `pid`'s working directory through `read` (Linux: [`proc_cwd`]), except
/// for a same-user process the invocation cannot have started whose
/// directory is refused, which is skipped (`None`).
///
/// A non-dumpable process (`ssh-agent`, a sandboxed Chromium or Electron
/// renderer) refuses `/proc/<pid>/cwd` with EACCES even to its own user, so
/// the operator's own such process, started during the invocation anywhere
/// on the machine, would hold every scan. A failed read for which `refused`
/// (a re-read) confirms EACCES or EPERM fails the scan only for a process
/// tied to the invocation, by `kernel` records: its parent or process group
/// is one the tracker knows (`known`), or its parent was born during the
/// invocation (`spawned` or later). A kernel read that fails, or a parent
/// or process already gone, counts it as tied, so the scan still fails
/// closed. Any other failure fails it as before.
///
/// Known gap: a non-dumpable process the invocation detached with `setsid`
/// and that a subreaper born before the invocation then adopted is skipped.
/// Linux only, but tested everywhere.
#[cfg(any(target_os = "linux", test))]
fn cwd_unless_refused_stranger(
    pid: u32,
    known: &Known,
    spawned: Birth,
    read: impl FnOnce(u32) -> Result<Option<PathBuf>, String>,
    refused: impl FnOnce(u32) -> bool,
    kernel: impl Fn(u32) -> Result<Option<KernelProcess>, String>,
) -> Result<Option<PathBuf>, String> {
    let read = read(pid);
    if read.is_err() && refused(pid) {
        let tied = match kernel(pid) {
            Ok(Some(process)) => {
                known.contains(&process.ppid)
                    || known.contains(&process.pgid)
                    || !matches!(
                        kernel(process.ppid),
                        Ok(Some(parent)) if parent.birth < spawned
                    )
            }
            _ => true,
        };
        if !tied {
            return Ok(None);
        }
    }
    read
}

/// [`processes_with_cwd`] of whatever `pids` lists; `known` are the
/// invocation's processes ([`ProcOwner::is_users`]).
fn scan_with(
    pids: ScanPids<'_>,
    spawned: Birth,
    known: &Known,
) -> Result<Vec<CwdProcess>, ScanError> {
    processes_with_cwd(pids()?, spawned, known)
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

/// How many processes an operator-facing hold names before it says how many
/// more there are.
const NAMED_PROCESSES: usize = 5;

/// How many process-table parents [`QuietCheck::in_shared_daemon_tree`]
/// climbs looking for the shared daemon before it gives up.
const DAEMON_ANCESTRY_DEPTH: usize = 64;

/// How long [`ProcessTracker::verify_gone`] waits for the killed tree to
/// leave the process table, with polls backing off from 10ms to 100ms.
const VERIFY_GONE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

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
        known: &[u32],
        daemon: Option<&'a SharedDaemon>,
        rows: &[ProcessRow],
        pids: ScanPids<'_>,
    ) -> Result<Quiet, String> {
        let check = Self::new(worktree, spawned, daemon, kernel_executable)?;
        let known: Known = known.iter().copied().collect();
        let scanned = scan_with(pids, spawned, &known).map_err(|e| e.reason)?;
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
            .filter(|p| p.pid != me && p.birth >= self.spawned && p.cwd.starts_with(&self.worktree))
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let rows = rows()?;
        let by_pid: std::collections::HashMap<u32, &ProcessRow> =
            rows.iter().map(|r| (r.pid, r)).collect();
        let mut daemon = std::collections::HashMap::new();
        Ok(candidates
            .iter()
            .filter(|p| !self.in_shared_daemon_tree(p.pid, &by_pid, &mut daemon))
            .map(|p| {
                let command = by_pid
                    .get(&p.pid)
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
        let more = found.len().saturating_sub(NAMED_PROCESSES);
        let mut named = found
            .iter()
            .take(NAMED_PROCESSES)
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
    fn in_shared_daemon_tree(
        &self,
        pid: u32,
        rows: &std::collections::HashMap<u32, &ProcessRow>,
        daemon: &mut std::collections::HashMap<u32, bool>,
    ) -> bool {
        let mut current = pid;
        for _ in 0..DAEMON_ANCESTRY_DEPTH {
            let Some(row) = rows.get(&current).filter(|r| r.active()) else {
                return false;
            };
            if *daemon
                .entry(current)
                .or_insert_with(|| is_shared_daemon(self.daemon, self.kernel_exe, row))
            {
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

/// The kernel birth of `pid`, or `None` once it has exited (a zombie
/// included, on every platform). A read that fails is an error, never
/// "exited": a caller deciding that a process is gone must hold on it.
pub(crate) fn kernel_birth(pid: u32) -> Result<Option<Birth>, String> {
    birth(pid)
}

pub(crate) struct ProcessTracker {
    root_pid: u32,
    /// The root's kernel birth, once read ([`Self::adopt_root`] or a
    /// sample). Kept when the root exits, for the worktree check.
    root_birth: Option<Birth>,
    /// Why [`Self::adopt_root`] could not read the root's birth, if it
    /// could not, for the hold that its absence later causes.
    root_unread: Option<String>,
    /// Processes owned and not yet seen gone, the root included while it
    /// runs, by PID: each with its authenticated row and kernel birth.
    known: std::collections::HashMap<u32, Tracked>,
    /// PIDs once in `known` that the kernel has since shown gone. They are
    /// never read, signalled or waited for again: the PID may already be a
    /// stranger's. A scan still counts them as the invocation's
    /// ([`Self::scan_known`]), which fails closed.
    exited: std::collections::HashSet<u32>,
    /// The next enrolment's sequence number ([`Tracked::seq`]).
    next_seq: u64,
    observed_root: bool,
    root_reaped: bool,
    /// A daemon on its socket, running its executable, is never owned;
    /// `None` owns every descendant.
    shared_daemon: Option<SharedDaemon>,
    /// [`kernel_executable`]; a seam so tests need no real daemon binary.
    kernel_exe: fn(u32) -> Option<PathBuf>,
    /// [`signal_pinned`]; a seam so tests can stand in for a process the
    /// kernel will not let the operator signal.
    kill: fn(&PinnedProcess, i32) -> std::io::Result<()>,
    /// Known processes cleanup was refused permission to signal: they no
    /// longer have the operator's real uid (an agent's `sudo`). Never a
    /// failure of the run, and never ignored: completion waits until each
    /// is gone ([`Self::hold_unsignalable`]).
    refused: std::collections::HashSet<u32>,
}

/// `e`'s message, without the "Validation error" prefix a validation
/// error displays with, for joining several into one.
pub(crate) fn detail(e: MemoryError) -> String {
    match e {
        MemoryError::Validation(message) => message,
        other => other.to_string(),
    }
}

/// One process the tracker owns.
#[derive(Debug, Clone)]
struct Tracked {
    row: ProcessRow,
    birth: Birth,
    /// Enrolment order: cleanup kills the newest first.
    seq: u64,
}

/// What signalling one known process did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signalled {
    Sent,
    Gone,
    /// The kernel refused it (EPERM): the process is not the operator's to
    /// signal, by its real uid.
    Refused,
}

/// `kill(2)`, as an `io::Result`.
pub(crate) fn unix_kill(pid: u32, signal: i32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let pid = i32::try_from(pid).map_err(|_| std::io::Error::other("pid out of range"))?;
        // SAFETY: a positive PID the caller checked against a kernel birth.
        if unsafe { libc::kill(pid, signal) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(std::io::Error::other(
            "invocation process signaling unsupported",
        ))
    }
}

/// A process held for signalling. On Linux a pidfd refers to the process
/// itself, not its PID: taken before the birth check, it makes the signal
/// reach the very process that check authenticated, or nothing once it has
/// exited, never a newcomer that reused the PID in between. Elsewhere, and
/// on a Linux kernel without pidfds, only the PID is held (`fd` is `None`).
pub(crate) struct PinnedProcess {
    pub pid: u32,
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
}

impl PinnedProcess {
    /// `pid`, pinned; `None` when it has already exited.
    fn open(pid: u32) -> Result<Option<Self>, MemoryError> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let raw = i32::try_from(pid)
                .map_err(|_| MemoryError::Validation(format!("process {pid} is out of range")))?;
            // SAFETY: pidfd_open takes a PID and flags, and returns a new
            // descriptor or -1.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw, 0) };
            if fd >= 0 {
                // SAFETY: a descriptor the kernel just opened for us alone.
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
                return Ok(Some(Self { pid, fd: Some(fd) }));
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            // No pidfds here (ENOSYS before Linux 5.3, or a seccomp filter
            // that refuses them): hold the PID, as on every other platform.
            Ok(Some(Self { pid, fd: None }))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Some(Self { pid }))
        }
    }
}

/// Send `signal` to `pinned`: through its pidfd where there is one
/// (`pidfd_send_signal`), else [`unix_kill`] on its PID.
pub(crate) fn signal_pinned(pinned: &PinnedProcess, signal: i32) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(fd) = &pinned.fd {
        use std::os::fd::AsRawFd;
        // SAFETY: a pidfd we own, a signal number, no siginfo, no flags.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        return if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        };
    }
    unix_kill(pinned.pid, signal)
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
            root_birth: None,
            root_unread: None,
            known: std::collections::HashMap::new(),
            exited: std::collections::HashSet::new(),
            next_seq: 0,
            observed_root: false,
            root_reaped: false,
            shared_daemon: shared_daemon.cloned(),
            kernel_exe: kernel_executable,
            kill: signal_pinned,
            refused: std::collections::HashSet::new(),
        }
    }

    /// Own `row`'s process at `birth`, newest so far; a PID seen exit
    /// before and now authenticated again is a new process.
    /// An old refusal ([`Self::refused`]) under the same PID was another
    /// process's.
    fn enrol(&mut self, row: ProcessRow, birth: Birth) {
        self.exited.remove(&row.pid);
        self.refused.remove(&row.pid);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.known.insert(row.pid, Tracked { row, birth, seq });
    }

    /// [`Self::enrol`], for a test that needs a tracker owning `row`.
    #[cfg(test)]
    pub(crate) fn enrol_for_test(&mut self, row: ProcessRow, birth: Birth) {
        self.enrol(row, birth);
    }

    /// Signal through `kill` instead of [`signal_pinned`] (tests).
    #[cfg(test)]
    pub(crate) fn set_kill(&mut self, kill: fn(&PinnedProcess, i32) -> std::io::Result<()>) {
        self.kill = kill;
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
        let mut by_parent: std::collections::HashMap<u32, Vec<&ProcessRow>> =
            std::collections::HashMap::new();
        for row in &rows {
            by_parent.entry(row.ppid).or_default().push(row);
        }
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
        // and never per tracked descendant.
        let shared_daemon: std::collections::HashSet<u32> = rows
            .iter()
            .filter(|r| r.active() && self.is_shared_daemon(r))
            .map(|r| r.pid)
            .collect();
        let mut released = Vec::new();
        for (&pid, tracked) in &self.known {
            let shows_daemon = pid != self.root_pid && shared_daemon.contains(&pid);
            if shows_daemon
                && kernel(pid)
                    .map_err(MemoryError::Validation)?
                    .is_some_and(|actual| actual.pid == pid && actual.birth == tracked.birth)
            {
                released.push(pid);
            }
        }
        for pid in released {
            self.known.remove(&pid);
        }
        // Existing descendants remain owned only while their kernel birth
        // token matches. Keep those tokens with the ancestry seeds; a bare
        // PID from ps must never authorize enrollment of a new subtree. One
        // the kernel shows gone moves to `exited` and is never read again,
        // so a later reuse of its PID is a stranger, not an identity change.
        let mut seeds = std::collections::HashMap::new();
        let mut gone = Vec::new();
        for (&pid, tracked) in &self.known {
            let expected = &tracked.birth;
            match kernel(pid).map_err(MemoryError::Validation)? {
                Some(actual) if actual.pid == pid && actual.birth == *expected => {
                    seeds.insert(pid, *expected);
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
                    for row in by_parent
                        .get(&pid)
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|row| {
                            row.active()
                                && !shared_daemon.contains(&row.pid)
                                && !self.known.contains_key(&row.pid)
                        })
                    {
                        if kernel(row.pid).map_err(MemoryError::Validation)?.is_some() {
                            return Err(MemoryError::Validation(
                                "invocation ancestry seed disappeared before descendant authentication"
                                    .into(),
                            ));
                        }
                    }
                    gone.push(pid);
                }
            }
        }
        for pid in gone {
            self.known.remove(&pid);
            self.exited.insert(pid);
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
                        || self
                            .root_birth
                            .is_some_and(|expected| expected != actual.birth)
                    {
                        return Err(MemoryError::Validation(
                            "invocation root identity changed during authentication".into(),
                        ));
                    }
                    self.observed_root = true;
                    self.root_birth = Some(actual.birth);
                    seeds.insert(root.pid, actual.birth);
                    if !self.known.contains_key(&root.pid) {
                        self.enrol(root.clone(), actual.birth);
                    }
                }
                None => {
                    // The owned, unreaped root cannot be PID-reused. A stale
                    // active ps row may already have become an inert exit.
                    seeds.remove(&root.pid);
                }
            }
        }
        // One adjacency index and breadth-first traversal per sample. Keep
        // the before/after kernel captures: memoizing those would erase the
        // PID-reuse/parent-change checks that authorize ownership.
        let mut frontier: std::collections::VecDeque<u32> = seeds.keys().copied().collect();
        while let Some(parent) = frontier.pop_front() {
            for row in by_parent
                .get(&parent)
                .into_iter()
                .flatten()
                .copied()
                .filter(|r| r.active() && !shared_daemon.contains(&r.pid))
            {
                if self.known.contains_key(&row.pid) {
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
                self.enrol(authenticated, actual.birth);
                seeds.insert(row.pid, actual.birth);
                frontier.push_back(row.pid);
            }
        }
        Ok(rows)
    }

    /// Descendants newest first, then the root.
    fn kill_order(&self) -> Vec<&Tracked> {
        let mut order: Vec<&Tracked> = self.known.values().collect();
        // The root sorts last whenever it enrolled.
        order.sort_by_key(|t| {
            (
                std::cmp::Reverse(t.row.pid != self.root_pid),
                std::cmp::Reverse(t.seq),
            )
        });
        order
    }

    fn signal(&self, pid: u32, signal: i32) -> Result<(), MemoryError> {
        let Some(tracked) = self.known.get(&pid) else {
            return Err(MemoryError::Validation(
                "cannot signal unauthenticated invocation process".into(),
            ));
        };
        match self.signal_known(pid, tracked.birth, signal)? {
            Signalled::Refused => Err(MemoryError::Validation(format!(
                "not permitted to signal invocation process {pid}"
            ))),
            Signalled::Sent | Signalled::Gone => Ok(()),
        }
    }

    /// Signal a known process, by the birth it was enrolled with. It is
    /// pinned ([`PinnedProcess`]) before that birth is checked, so on Linux
    /// the signal reaches the process the check authenticated or nothing.
    ///
    /// Residual race elsewhere (macOS has no pidfd): between the birth
    /// check and `kill(2)`, a process that is not stopped yet (the first
    /// SIGSTOP pass, or one the kernel would not stop) can exit, be reaped
    /// by init and have its PID reused, and the signal then reaches the
    /// newcomer. That needs a PID to wrap within microseconds, so it is
    /// accepted rather than closed.
    fn signal_known(
        &self,
        pid: u32,
        expected: Birth,
        signal: i32,
    ) -> Result<Signalled, MemoryError> {
        let Some(pinned) = PinnedProcess::open(pid)? else {
            return Ok(Signalled::Gone);
        };
        match birth(pid).map_err(MemoryError::Validation)? {
            None => Ok(Signalled::Gone),
            Some(actual) if actual == expected => match (self.kill)(&pinned, signal) {
                Ok(()) => Ok(Signalled::Sent),
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(Signalled::Gone),
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => Ok(Signalled::Refused),
                Err(e) => Err(e.into()),
            },
            Some(_) => Err(MemoryError::Validation(
                "invocation PID identity changed; refusing signal".into(),
            )),
        }
    }

    /// [`Self::signal_known`], remembering a refusal ([`Self::refused`]).
    fn signal_or_keep(
        &mut self,
        pid: u32,
        expected: Birth,
        signal: i32,
    ) -> Result<(), MemoryError> {
        if self.signal_known(pid, expected, signal)? == Signalled::Refused {
            self.refused.insert(pid);
        }
        Ok(())
    }

    /// Freeze the owned tree before enumerating/killing it. Separate setsid
    /// groups remain children and are discovered through PPID, not PGID.
    /// Sampling is practical tool cleanup, not malicious fork containment.
    pub(crate) fn cleanup(&mut self, root_reaped: bool) -> Result<(), MemoryError> {
        self.root_reaped = root_reaped;
        // A reaped root's PID is free for reuse: never read or signal it
        // again. Its birth stays in `root_birth`.
        if root_reaped && self.known.remove(&self.root_pid).is_some() {
            self.exited.insert(self.root_pid);
        }
        let result = (|| {
            if !root_reaped {
                self.observe()?;
                if let Some(birth) = self.known.get(&self.root_pid).map(|t| t.birth) {
                    self.signal_or_keep(self.root_pid, birth, STOP_SIGNAL)?;
                } else if !self.exited.contains(&self.root_pid) {
                    // A root seen gone needs no stop; one never seen
                    // cannot be signalled unauthenticated.
                    self.signal(self.root_pid, STOP_SIGNAL)?;
                }
            }
            let mut stable = false;
            for _ in 0..32 {
                // Enrolments, not the count: exits now shrink `known`, so
                // one exit and one new enrolment in a pass would leave its
                // size unchanged.
                let before = self.next_seq;
                let rows = self.observe()?;
                // A shared daemon a sample caught between its fork and exec,
                // still wearing its spawner's argv, is stopped and killed
                // here like any tool. That is harmless: it never bound the
                // socket or served anyone, and the next proxy spawns another.
                let known: Vec<(u32, Birth)> =
                    self.known.iter().map(|(&pid, t)| (pid, t.birth)).collect();
                for (pid, birth) in known {
                    self.signal_or_keep(pid, birth, STOP_SIGNAL)?;
                }
                // One it may not signal never stops; it is kept as evidence.
                let stopped = rows
                    .iter()
                    .filter(|r| {
                        r.active()
                            && !self.refused.contains(&r.pid)
                            && self.known.contains_key(&r.pid)
                    })
                    .all(|r| row_stopped(r, task_states));
                if before == self.next_seq && stopped {
                    stable = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // Fail closed, by design: a refused process (one cleanup may not
            // signal) that keeps forking enrols a child on every pass, so
            // the tree never reads as stable. Cleanup then ends "did not
            // quiesce", no exit record is written, the invocation stays held,
            // and only `clear-dispatch`, once that process is gone, releases
            // it.
            if !stable {
                return Err(MemoryError::Validation(
                    "invocation process tree did not quiesce".into(),
                ));
            }
            Ok(())
        })();
        // Continue best-effort authenticated cleanup after any uncertainty so
        // SIGSTOP cannot strand a known process. Never signal a stale group.
        // Every process that could not be killed is reported, beside any
        // failure above: neither hides the other.
        let mut signal_errors = Vec::new();
        let mut refused = Vec::new();
        let temp = std::env::temp_dir();
        for tracked in self.kill_order() {
            let (pid, row, birth) = (tracked.row.pid, &tracked.row, &tracked.birth);
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
            match self.signal_known(pid, *birth, KILL_SIGNAL) {
                Ok(Signalled::Refused) => refused.push(pid),
                Ok(_) => {}
                Err(e) => signal_errors.push(format!("process {pid}: {}", detail(e))),
            }
        }
        self.refused.extend(refused);
        let killed = if signal_errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "cleanup could not kill {}",
                signal_errors.join("; ")
            ))
        };
        match (result, killed) {
            (Ok(()), Ok(())) => self.verify_gone(),
            (Ok(()), Err(killed)) => Err(MemoryError::Validation(killed)),
            (Err(e), Ok(())) => Err(e),
            (Err(e), Err(killed)) => {
                Err(MemoryError::Validation(format!("{}; {killed}", detail(e))))
            }
        }
    }
    pub(crate) fn verify_gone(&self) -> Result<(), MemoryError> {
        let deadline = std::time::Instant::now() + VERIFY_GONE_GRACE;
        let mut poll = std::time::Duration::from_millis(10);
        loop {
            let active: std::collections::HashSet<u32> = Self::rows()?
                .into_iter()
                .filter(ProcessRow::active)
                .map(|r| r.pid)
                .collect();
            let mut live = false;
            // One cleanup may not signal is evidence, not a failure: it is
            // waited for by `hold_unsignalable`, after cleanup.
            for (&pid, tracked) in self
                .known
                .iter()
                .filter(|(pid, _)| !self.refused.contains(pid))
            {
                let expected = &tracked.birth;
                if active.contains(&pid) {
                    match birth(pid).map_err(MemoryError::Validation)? {
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
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(
                poll.min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
            poll = (poll * 2).min(std::time::Duration::from_millis(100));
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
        self.verify_group_drained_with(Self::rows, std::time::Duration::from_secs(1))
    }

    /// [`Self::verify_group_drained`] over `rows`, for `grace`.
    fn verify_group_drained_with(
        &self,
        mut rows: impl FnMut() -> Result<Vec<ProcessRow>, MemoryError>,
        grace: std::time::Duration,
    ) -> Result<(), MemoryError> {
        let deadline = std::time::Instant::now() + grace;
        let mut poll = std::time::Duration::from_millis(10);
        let straggler = loop {
            let rows = rows()?;
            // Once the root is reaped, a live process holding its PID shows
            // the group ID was freed and reused: the group is someone
            // else's (as `recorded_tree_evidence` and settling read it).
            if self.root_reaped && rows.iter().any(|r| r.active() && r.pid == self.root_pid) {
                return Ok(());
            }
            let Some(straggler) = rows.into_iter().find(|r| {
                r.active() && r.pgid == self.root_pid && !self.known.contains_key(&r.pid)
            }) else {
                return Ok(());
            };
            if std::time::Instant::now() >= deadline {
                break straggler.pid;
            }
            std::thread::sleep(
                poll.min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
            poll = (poll * 2).min(std::time::Duration::from_millis(100));
        };
        Err(MemoryError::Validation(format!(
            "untracked process {straggler} remained in invocation process group {} after \
             cleanup; completion is unproven",
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
        let known = self.scan_known();
        self.verify_worktree_quiet_with(
            worktree,
            |spawned| scan_with(pids, spawned, &known),
            Self::rows,
            grace,
        )
    }

    /// Every PID the invocation ever had, exited ones included, for a scan
    /// to tie processes to ([`ProcOwner::is_users`]): a group an exited
    /// descendant led still ties its members, which fails closed.
    fn scan_known(&self) -> Known {
        std::iter::once(self.root_pid)
            .chain(self.known.keys().copied())
            .chain(self.exited.iter().copied())
            .collect()
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
        let spawned = self.root_birth.ok_or_else(|| {
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

    /// `quiet`, also held for every known process cleanup was refused
    /// permission to signal that still runs, and for every process under
    /// one in the process table: none of it could be stopped, so completion
    /// waits, recorded, until each is gone by kernel birth. Anything such a
    /// process starts after this check and moves out of its tree is beyond
    /// the guarantee, like a deliberate escape; the hold lasts at least as
    /// long as the process itself.
    pub(crate) fn hold_unsignalable(
        &self,
        worktree: &Path,
        quiet: Quiet,
    ) -> Result<Quiet, MemoryError> {
        if self.refused.is_empty() {
            return Ok(quiet);
        }
        let rows = Self::rows()?;
        let mut held: Vec<(u32, Birth)> = Vec::new();
        for (&pid, tracked) in self
            .known
            .iter()
            .filter(|(pid, _)| self.refused.contains(pid))
        {
            if birth(pid).map_err(MemoryError::Validation)? == Some(tracked.birth) {
                held.push((pid, tracked.birth));
            }
        }
        // Their descendants in the process table, by parent, to any depth.
        let mut by_parent: std::collections::HashMap<u32, Vec<&ProcessRow>> =
            std::collections::HashMap::new();
        for row in rows.iter().filter(|r| r.active()) {
            by_parent.entry(row.ppid).or_default().push(row);
        }
        let mut seen: std::collections::HashSet<u32> = held.iter().map(|(pid, _)| *pid).collect();
        let mut frontier: Vec<u32> = held.iter().map(|(pid, _)| *pid).collect();
        while let Some(parent) = frontier.pop() {
            for row in by_parent.get(&parent).into_iter().flatten().copied() {
                if !seen.insert(row.pid) {
                    continue;
                }
                if let Some(child) = kernel_observation(row.pid).map_err(MemoryError::Validation)? {
                    if child.ppid == parent {
                        held.push((row.pid, child.birth));
                        frontier.push(row.pid);
                    }
                }
            }
        }
        if held.is_empty() {
            return Ok(quiet);
        }
        let named = held
            .iter()
            .take(NAMED_PROCESSES)
            .map(|(pid, _)| {
                let ruid = real_and_effective_uid(*pid)
                    .map_or_else(|| "unknown".to_owned(), |(ruid, _)| ruid.to_string());
                format!("process {pid} (real uid {ruid})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let reason = format!(
            "cleanup was not permitted to signal {named}{}, started by the invocation; \
             completion waits until it is gone",
            if held.len() > NAMED_PROCESSES {
                " and more"
            } else {
                ""
            }
        );
        Ok(Quiet::Held(match quiet {
            Quiet::Held(mut hold) => {
                for straggler in held {
                    if !hold.stragglers.contains(&straggler) {
                        hold.stragglers.push(straggler);
                    }
                }
                hold.reason = format!("{reason}; {}", hold.reason);
                hold
            }
            Quiet::Yes => QuietHold {
                worktree: worktree.canonicalize().map_err(|e| {
                    MemoryError::Validation(format!(
                        "cannot resolve the invocation worktree {}: {e}",
                        worktree.display()
                    ))
                })?,
                uid: current_uid().ok_or_else(|| {
                    MemoryError::Validation("process ownership is unreadable here".into())
                })?,
                stragglers: held,
                scan_failed: None,
                reason,
            },
        }))
    }

    /// Read the root's kernel record straight after spawn, before any
    /// process-table sample: our unreaped `Child` reserves its PID, so the
    /// record is the root's. That gives completion the root's birth even
    /// when the root exits before the first sample; [`Self::observed_root`]
    /// stays false until a sample binds its process-table row. A read that
    /// fails leaves the birth unknown, logged and kept for the hold that
    /// follows ([`Self::root_unread`]).
    pub(crate) fn adopt_root(&mut self) {
        self.adopt_root_with(root_observation);
    }

    /// [`Self::adopt_root`] over `read`, a seam for a failed kernel read.
    pub(crate) fn adopt_root_with(
        &mut self,
        read: impl FnOnce(u32) -> Result<Option<RootRecord>, String>,
    ) {
        if self.root_birth.is_some() {
            return;
        }
        let read = read(self.root_pid);
        match read
            .as_ref()
            .ok()
            .and_then(|record| adoptable(self.root_pid, std::process::id(), *record))
        {
            Some(root) => {
                let row = ProcessRow {
                    pid: root.pid,
                    ppid: root.ppid,
                    pgid: root.pgid,
                    stat: String::new(),
                    args: String::new(),
                };
                self.root_birth = Some(root.birth);
                self.enrol(row, root.birth);
            }
            // Not fatal: the first sample may still bind the root; without
            // either, completion holds as unproven.
            None => {
                let why = match &read {
                    Err(e) => format!("its kernel record could not be read at spawn: {e}"),
                    Ok(None) => "it was already gone at spawn".to_owned(),
                    Ok(Some(_)) => {
                        "the record read at spawn was not this supervisor's child".to_owned()
                    }
                };
                tracing::warn!(
                    pid = self.root_pid,
                    "could not read the invocation root's kernel birth: {why}"
                );
                self.root_unread = Some(why);
            }
        }
    }

    /// Why the root's kernel birth was never read at spawn, if it was not.
    pub(crate) fn root_unread(&self) -> Option<&str> {
        self.root_unread.as_deref()
    }

    pub(crate) fn observed_root(&self) -> bool {
        self.observed_root
    }

    /// The root's kernel birth, once read ([`Self::adopt_root`] or a sample).
    pub(crate) fn root_birth(&self) -> Option<Birth> {
        self.root_birth
    }
    /// What the identity file records: the root's kernel birth, once
    /// observed, and the descendants not yet seen gone, oldest first.
    /// Exited ones are left out, so the file stays bounded over a long
    /// dispatch.
    pub(crate) fn recorded(&self) -> (Option<Birth>, Vec<RecordedProcess>) {
        let mut descendants: Vec<&Tracked> = self
            .known
            .values()
            .filter(|t| t.row.pid != self.root_pid)
            .collect();
        descendants.sort_by_key(|t| t.seq);
        let descendants = descendants
            .into_iter()
            .map(|t| RecordedProcess::new(&t.row, t.birth))
            .collect();
        (self.root_birth, descendants)
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
        // In the layout `prepare_artifacts` makes, for the issue
        // most callers build a state for; `invocation_in` is the general
        // form.
        invocation_in(dir, implementer, "owner-repo-42")
    }

    /// [`invocation_for`] for the issue `slug` names, in its layout
    /// under `dir` as a common directory.
    pub(crate) fn invocation_in(
        dir: &Path,
        implementer: Implementer,
        slug: &str,
    ) -> (InvocationArtifacts, ProcessRow) {
        let invocation_id = "11111111-2222-3333-4444-555555555555";
        let nested = dir
            .join("ironmem")
            .join("autopilot")
            .join(slug)
            .join(invocation_id);
        std::fs::create_dir_all(&nested).unwrap();
        let path = |name: &str| nested.join(name).to_str().unwrap().to_owned();
        let a = InvocationArtifacts {
            invocation_id: invocation_id.into(),
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
pub(crate) mod tests {
    use super::*;
    use crate::autopilot::implementer::Implementer;

    /// A scan's [`Known`] PIDs.
    fn pid_set(pids: &[u32]) -> Known {
        pids.iter().copied().collect()
    }

    #[test]
    fn ultrareview_group_drain_backs_off_process_table_spawns() {
        let tracker = ProcessTracker::owning_every_descendant(10);
        let mut scans = 0;
        let result = tracker.verify_group_drained_with(
            || {
                scans += 1;
                Ok(vec![row("tool")])
            },
            std::time::Duration::from_millis(250),
        );
        assert!(result.is_err());
        assert!(scans <= 8, "{scans} full process-table scans in 250ms");
    }

    #[test]
    fn ultrareview_real_harness_argv_is_understood_by_the_marker_parser() {
        use super::super::dispatch::SessionMode;
        use super::super::invocation::{cli_for, contract_tests::setup};
        let (_dir, mut spec) = setup();
        // ps cannot represent a whitespace path. Use a controlled marker
        // spelling here; argv itself is always built by the real harness.
        let a = spec.invocation_artifacts.as_mut().unwrap();
        let base = "/r/.git/ironmem/autopilot/owner-repo-42/11111111-2222-4333-8444-555555555555";
        a.schema_path = format!("{base}/schema.json");
        a.final_path = format!("{base}/final.json");
        for (implementer, model) in [
            (Implementer::Codex, "gpt-test"),
            (Implementer::Muse, "muse-test"),
        ] {
            spec.implementer = implementer;
            spec.model = model.into();
            spec.session = SessionMode::New {
                session_uuid: "11111111-2222-4333-8444-555555555555".into(),
            };
            let a = spec.invocation_artifacts.as_ref().unwrap();
            let args = cli_for(implementer)
                .unwrap()
                .argv(Path::new("/workspace"), &spec, a)
                .unwrap();
            let command = format!("{} {}", implementer.cli_name(), args.join(" "));
            assert_eq!(
                row(&command).marker().unwrap(),
                Some(a.schema_path.as_str()),
                "{command}"
            );
        }
    }

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
    /// An exit record that exists but cannot be read or parsed is said to
    /// be so, not reported as missing: the operator looks for a corrupt
    /// record, not an absent one. It still proves nothing.
    #[test]
    fn an_unreadable_exit_record_is_named_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        let empty = ProcessSnapshot::Available(Vec::new());
        // Not yet written (the empty file prepare_artifacts makes): missing.
        std::fs::write(&a.exit_path, "").unwrap();
        assert!(empty
            .invocation_reason(Some(&a))
            .contains("no trusted supervisor"));
        std::fs::write(&a.exit_path, "{").unwrap();
        let reason = empty.invocation_reason(Some(&a));
        assert!(
            reason.contains("the exit record cannot be read"),
            "{reason}"
        );
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        assert!(super::super::invocation::exit_record_unreadable(&a)
            .is_some_and(|e| e.contains("cannot be read")));
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
        // Autopilot's `-` (prompt on stdin) is the last word; anything after
        // it makes the row ambiguous.
        let ours =
            "/r/.git/ironmem/autopilot/o-r-1/0f0e0d0c-0b0a-4908-8706-050403020100/schema.json";
        assert_eq!(
            marker(&format!(
                "codex exec --json --output-schema {ours} -m gpt -"
            )),
            Ok(Some(ours.to_owned()))
        );
        assert!(marker(&format!("codex exec --output-schema {ours} -m gpt - extra")).is_err());
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
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker.enrol(row(20), Birth(1, 0));
        tracker.enrol(row(10), Birth(2, 0));
        tracker.enrol(row(30), Birth(3, 0));
        let order = tracker
            .kill_order()
            .into_iter()
            .map(|t| t.row.pid)
            .collect::<Vec<_>>();
        // Descendants newest first, the root last.
        assert_eq!(order, vec![30, 20, 10]);
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
        let mut owned: Vec<_> = tracker.known.keys().copied().collect();
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
                tracker
                    .kill_order()
                    .into_iter()
                    .map(|t| t.row.pid)
                    .collect::<Vec<_>>(),
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
        assert!(tracker.known.contains_key(&30));
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
            tracker.enrol(process_row(pid, 20, 10), Birth(u64::from(pid), 0));
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
        let process = |pid, start, cwd: &Path| CwdProcess {
            pid,
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
            let err = check(&tracker, process(40, started, &worktree.join("src")))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("process 40") && err.contains("sleep 30"),
                "{err}"
            );
        }
        for ignored in [
            process(40, 99, &worktree),
            process(me, 150, &worktree),
            process(30, 150, &worktree),
            process(31, 150, &worktree),
            process(40, 150, Path::new("/elsewhere")),
            process(40, 150, &sibling),
        ] {
            assert!(check(&tracker, ignored.clone()).is_ok(), "{ignored:?}");
        }
        // The same daemon argv on another executable is no exemption.
        tracker.kernel_exe = |_| Some("/w/target/debug/ironmem".into());
        assert!(check(&tracker, process(30, 150, &worktree)).is_err());
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
        let found = scan_cwds(
            [1, 3, 4, 6, 7],
            uid,
            spawned,
            &pid_set(&[]),
            owner_birth,
            &mut cwd_of,
        )
        .unwrap();
        assert_eq!(found.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![7]);
        assert_eq!(
            cwd_reads.borrow().clone(),
            vec![4, 7],
            "old, gone and foreign processes are never read"
        );
        let err = scan_cwds(
            [1, 2, 7],
            uid,
            spawned,
            &pid_set(&[]),
            owner_birth,
            &mut cwd_of,
        )
        .unwrap_err();
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
        assert!(
            scan_cwds([8], uid, spawned, &pid_set(&[]), vanishing, &mut cwd_of)
                .unwrap()
                .is_empty()
        );
        // Replaced by a new process under the same PID: the directory may
        // be the newcomer's, so the scan cannot vouch for it.
        let mut reads = 0;
        let replaced = |_pid: u32| -> Result<Option<(u32, Birth)>, String> {
            reads += 1;
            Ok(Some((uid, Birth(150 + reads, 0))))
        };
        let err = scan_cwds([9], uid, spawned, &pid_set(&[]), replaced, &mut cwd_of).unwrap_err();
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
        let processes =
            processes_with_cwd(vec![std::process::id()], Birth(0, 0), &pid_set(&[])).unwrap();
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
        // No identity at all, past the launch window: nothing was ever
        // spawned to hide.
        std::fs::remove_file(identity_path(&a)).unwrap();
        std::fs::write(&a.schema_path, "{}").unwrap();
        std::fs::File::open(&a.schema_path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        assert_eq!(empty.live_evidence("owner-repo-42", Some(&a)), None);
    }

    /// Between a run publishing an invocation's artifacts and persisting its
    /// identity just after spawn, another run reading the issue must not see
    /// it idle and spawn a second implementer: freshly published artifacts
    /// with no identity and no exit record are evidence (and Unknown). Once
    /// they are older than the launch window, nothing is spawning into
    /// them, and the existing reading stands.
    #[test]
    fn freshly_published_artifacts_with_no_identity_are_a_launch_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = testing::invocation(dir.path());
        std::fs::remove_file(identity_path(&a)).unwrap();
        std::fs::write(&a.schema_path, "{}").unwrap();
        std::fs::write(&a.exit_path, "").unwrap();
        let empty = ProcessSnapshot::Available(Vec::new());
        let evidence = empty.live_evidence("owner-repo-42", Some(&a));
        assert!(
            evidence.as_deref().is_some_and(|e| e.contains("launch")),
            "{evidence:?}"
        );
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        assert!(empty.invocation_reason(Some(&a)).contains("launch"));
        // Past the launch window: no spawn is coming, as before.
        std::fs::File::open(&a.schema_path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now()
                    - super::super::invocation::LAUNCH_WINDOW
                    - std::time::Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(empty.live_evidence("owner-repo-42", Some(&a)), None);
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
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
        assert!(tracker.known.contains_key(&30));
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

    /// `/usr/bin/top` when it is setuid (macOS), the setuid binary these
    /// tests run as a tool; `None` elsewhere, where they skip.
    #[cfg(unix)]
    pub(crate) fn setuid_top() -> Option<&'static str> {
        use std::os::unix::fs::PermissionsExt;
        let top = "/usr/bin/top";
        let setuid = std::fs::metadata(top).is_ok_and(|m| m.permissions().mode() & 0o4000 != 0);
        if !setuid {
            eprintln!("{top} is not setuid on this platform; skipping");
        }
        setuid.then_some(top)
    }

    /// The `sysctl(KERN_PROC_PID)` record, read at fixed offsets because
    /// `libc` has no `kinfo_proc`, agrees with `PROC_PIDTBSDINFO` on this
    /// process: the offsets are right.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_sysctl_record_agrees_with_the_bsd_record() {
        let me = std::process::id();
        let record = kinfo_proc(me).unwrap().unwrap();
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: the kernel writes at most the allocated size.
        let bytes = unsafe {
            libc::proc_pidinfo(
                me as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        assert_eq!(bytes as usize, std::mem::size_of::<libc::proc_bsdinfo>());
        // SAFETY: fully written above.
        let info = unsafe { info.assume_init() };
        assert_eq!(record.process.pid, info.pbi_pid);
        assert_eq!(record.process.ppid, info.pbi_ppid);
        assert_eq!(record.process.pgid, info.pbi_pgid);
        assert_eq!(
            record.process.birth,
            Birth(info.pbi_start_tvsec, info.pbi_start_tvusec)
        );
        assert_eq!((record.ruid, record.euid), (info.pbi_ruid, info.pbi_uid));
    }

    /// An agent's tool that runs a setuid binary (`ps`, `top`) still has a
    /// kernel identity the tracker can read without privilege, so its run
    /// is not torn down. Measured: `PROC_PIDTBSDINFO` refuses such a process
    /// with EPERM, `sysctl(KERN_PROC_PID)` does not.
    #[cfg(unix)]
    #[test]
    fn a_setuid_root_process_is_authenticated_without_privilege() {
        let Some(top) = setuid_top() else { return };
        let mut child = Command::new(top)
            .args(["-l", "30", "-s", "1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        // Until its exec it is still ours; wait for the setuid image.
        let (read, setuid) = loop {
            let read = kernel_observation(child.id());
            let setuid = real_and_effective_uid(child.id()).is_some_and(|(_, euid)| euid == 0);
            if setuid || started.elapsed() > std::time::Duration::from_secs(10) {
                break (read, setuid);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = child.kill();
        let _ = child.wait();
        // Otherwise only our own pre-exec image was read, proving nothing.
        assert!(setuid, "top never ran with euid 0 within 10s");
        let process = read.unwrap().expect("the live top was read as gone");
        assert_eq!(process.pid, child.id());
        assert_eq!(process.ppid, std::process::id());
        // SAFETY: getuid has no preconditions.
        assert_eq!(
            real_and_effective_uid(std::process::id()),
            Some((unsafe { libc::getuid() }, unsafe { libc::geteuid() }))
        );
    }

    /// A process that is the user's only by real uid (a setuid tool:
    /// `login` behind every Terminal tab, `sudo`, `top`) counts only when it
    /// is tied to the invocation: its parent is a process the tracker knows,
    /// or it was reparented to init. An unrelated one is skipped before its
    /// (unreadable) working directory is touched, so it never fails the scan.
    #[test]
    fn a_process_ours_only_by_real_uid_counts_only_when_tied_to_the_invocation() {
        let spawned = Birth(100, 0);
        let uid = 501;
        let root = 42;
        // Parent and group predate the invocation unless stated.
        let setuid = |ppid: u32| ProcOwner {
            euid: 0,
            ruid: uid,
            ppid,
            pgid: 300,
            parent: Parent::Born(Birth(50, 0)),
        };
        let owner_birth = |pid: u32| -> Result<Option<(ProcOwner, Birth)>, String> {
            Ok(Some(match pid {
                // A Terminal tab's `login`, born in the window.
                10 => (setuid(300), Birth(150, 0)),
                // A setuid tool the invocation's root started.
                11 => (setuid(root), Birth(150, 0)),
                // One reparented to init after its parent exited.
                12 => (setuid(1), Birth(150, 0)),
                // Same effective uid: counted whoever its parent is.
                _ => (
                    ProcOwner {
                        euid: uid,
                        ruid: uid,
                        ppid: 300,
                        pgid: 300,
                        parent: Parent::Born(Birth(50, 0)),
                    },
                    Birth(150, 0),
                ),
            }))
        };
        let reads = std::cell::RefCell::new(Vec::new());
        let cwd_of = |pid: u32| -> Result<Option<PathBuf>, String> {
            reads.borrow_mut().push(pid);
            match pid {
                10 | 11 => Err("Operation not permitted".into()),
                _ => Ok(Some(PathBuf::from(format!("/w/{pid}")))),
            }
        };
        let found = scan_cwds(
            [10, 12, 13],
            uid,
            spawned,
            &pid_set(&[root]),
            owner_birth,
            cwd_of,
        )
        .unwrap();
        assert_eq!(
            found.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![12, 13]
        );
        assert!(!reads.borrow().contains(&10), "an unrelated login was read");
        let err = scan_cwds(
            [10, 11],
            uid,
            spawned,
            &pid_set(&[root]),
            owner_birth,
            cwd_of,
        )
        .unwrap_err();
        assert_eq!(err.unreadable, Some((11, Birth(150, 0))), "{err}");
    }

    /// Linux: a same-user non-dumpable process (`ssh-agent`, a sandboxed
    /// renderer) refuses its `/proc/<pid>/cwd` with EACCES. One the
    /// invocation cannot have started is skipped, so it never holds the
    /// scan; one tied to the invocation (parent or group known, parent born
    /// in the window, or a parent or process that cannot be read) still
    /// fails it. Any other failed read fails it as before.
    #[test]
    fn a_refused_cwd_read_skips_only_a_process_not_tied_to_the_invocation() {
        let spawned = Birth(100, 0);
        let root = 42;
        // Process 7, under parent `ppid` in group `pgid`; the parent was
        // born at `parent` (`None`: gone; `Some(None)`: unreadable).
        let kernel = |ppid: u32, pgid: u32, parent: Option<Option<u64>>| {
            move |pid: u32| -> Result<Option<KernelProcess>, String> {
                match pid {
                    7 => Ok(Some(kernel_process(7, ppid, pgid, 150))),
                    _ if pid == ppid => match parent {
                        None => Ok(None),
                        Some(None) => Err("refused".into()),
                        Some(Some(start)) => Ok(Some(kernel_process(ppid, 1, ppid, start))),
                    },
                    other => panic!("unexpected kernel read of {other}"),
                }
            }
        };
        let denied = || Err::<Option<PathBuf>, _>("Permission denied (os error 13)".to_owned());
        type Kernel<'a> = &'a dyn Fn(u32) -> Result<Option<KernelProcess>, String>;
        let decide = |refused: bool, kernel: Kernel<'_>| {
            cwd_unless_refused_stranger(
                7,
                &pid_set(&[root]),
                spawned,
                |_| denied(),
                |_| refused,
                kernel,
            )
        };
        // The operator's own: an old parent, in an old group of its own.
        assert_eq!(decide(true, &kernel(300, 300, Some(Some(50)))), Ok(None));
        for tied in [
            kernel(root, 300, Some(Some(50))),
            kernel(300, root, Some(Some(50))),
            kernel(300, 300, Some(Some(100))),
            kernel(300, 300, Some(Some(150))),
            kernel(300, 300, None),
            kernel(300, 300, Some(None)),
        ] {
            assert!(decide(true, &tied).is_err());
        }
        // Its own record unreadable, or gone: nothing unties it.
        let unreadable = |_: u32| -> Result<Option<KernelProcess>, String> { Err("x".into()) };
        assert!(decide(true, &unreadable).is_err());
        assert!(decide(true, &|_: u32| Ok(None)).is_err());
        // A failure that is not a refusal fails the scan, untied or not.
        assert!(decide(false, &kernel(300, 300, Some(Some(50)))).is_err());
        // A directory that reads is the answer; nothing else is asked.
        let read = cwd_unless_refused_stranger(
            7,
            &pid_set(&[root]),
            spawned,
            |_| Ok(Some(PathBuf::from("/w"))),
            |_| panic!("re-read a readable directory"),
            |_| panic!("read the kernel for a readable directory"),
        );
        assert_eq!(read, Ok(Some(PathBuf::from("/w"))));
        // Through the scan: the stranger is skipped and the rest listed.
        let owner_birth =
            |_: u32| -> Result<Option<(u32, Birth)>, String> { Ok(Some((501, Birth(150, 0)))) };
        let found = scan_cwds(
            [7, 8],
            501,
            spawned,
            &pid_set(&[root]),
            owner_birth,
            |pid| {
                if pid == 8 {
                    return Ok(Some(PathBuf::from("/w/8")));
                }
                cwd_unless_refused_stranger(
                    pid,
                    &pid_set(&[root]),
                    spawned,
                    |_| denied(),
                    |_| true,
                    kernel(300, 300, Some(Some(50))),
                )
            },
        )
        .unwrap();
        assert_eq!(found.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![8]);
    }

    /// The same on a real Linux machine: a non-dumpable child of this test
    /// process (which predates the invocation), working outside the
    /// worktree, is skipped by the real scan; with this process as the
    /// invocation's, it fails the scan on that very process, which shows
    /// its directory really is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_non_dumpable_process_outside_the_invocation_does_not_fail_the_scan() {
        // Births are clock ticks: let one pass, so this process predates
        // the child even when the test runs alone.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: the child only makes async-signal-safe calls (chdir,
        // prctl, pause) until it is killed.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
        if child == 0 {
            // SAFETY: the forked child, as above.
            unsafe {
                libc::chdir(c"/".as_ptr());
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                loop {
                    libc::pause();
                }
            }
        }
        let child = child as u32;
        struct Reap(u32);
        impl Drop for Reap {
            fn drop(&mut self) {
                // SAFETY: our own unreaped child.
                unsafe {
                    libc::kill(self.0 as i32, libc::SIGKILL);
                    libc::waitpid(self.0 as i32, std::ptr::null_mut(), 0);
                }
            }
        }
        let _reap = Reap(child);
        let started = std::time::Instant::now();
        while !std::fs::read_link(format!("/proc/{child}/cwd"))
            .is_err_and(|e| e.raw_os_error() == Some(libc::EACCES))
        {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the child never became non-dumpable"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The invocation starts with the child (a wall-clock start reads
        // up to a second off on Linux), after this test process.
        let spawned = birth(child).unwrap().expect("the child runs");
        assert!(birth(std::process::id()).unwrap().unwrap() < spawned);
        assert_eq!(
            processes_with_cwd(vec![child], spawned, &pid_set(&[])).map(|found| found.len()),
            Ok(0)
        );
        let err =
            processes_with_cwd(vec![child], spawned, &pid_set(&[std::process::id()])).unwrap_err();
        assert_eq!(err.unreadable.map(|(pid, _)| pid), Some(child), "{err}");
    }

    /// A real-uid-only process whose parent cannot be authenticated is tied
    /// to the invocation, so the scan fails closed on it: a parent that
    /// exited before it was read (the child is not yet seen reparented) and
    /// a parent read that failed both count, as a parent born in the window
    /// does. Only a parent read and found older than the invocation unties
    /// it.
    #[test]
    fn a_real_uid_process_whose_parent_cannot_be_read_counts_as_tied() {
        let spawned = Birth(100, 0);
        let uid = 501;
        let owner = |parent: Parent| ProcOwner {
            euid: 0,
            ruid: uid,
            ppid: 5000,
            pgid: 5000,
            parent,
        };
        assert!(owner(Parent::Gone).is_users(uid, &pid_set(&[]), spawned));
        assert!(owner(Parent::Unreadable).is_users(uid, &pid_set(&[]), spawned));
        assert!(owner(Parent::Born(Birth(150, 0))).is_users(uid, &pid_set(&[]), spawned));
        assert!(!owner(Parent::Born(Birth(50, 0))).is_users(uid, &pid_set(&[]), spawned));
        assert!(!owner(Parent::NotRead).is_users(uid, &pid_set(&[]), spawned));
        // Another user's process is never the user's, whatever its parent.
        assert!(!ProcOwner {
            ruid: uid + 1,
            ..owner(Parent::Gone)
        }
        .is_users(uid, &pid_set(&[]), spawned));
    }

    /// A Linux `/proc/<pid>/stat` line: pid, comm in parentheses, then state
    /// and the rest, with starttime as field 22 (index 19 after comm).
    fn proc_stat_line(pid: u32, comm: &str, state: char) -> String {
        proc_stat_line_with_threads(pid, comm, state, "1")
    }

    /// [`proc_stat_line`] with `threads` as `num_threads` (field 20).
    fn proc_stat_line_with_threads(pid: u32, comm: &str, state: char, threads: &str) -> String {
        format!(
            "{pid} ({comm}) {state} 300 {pid} {pid} 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 {threads} 0 \
             12345 1000000 200 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0"
        )
    }

    /// A real `Zl` process on Linux: its main thread called `pthread_exit`
    /// while a worker runs. The kernel read counts it running, the scan
    /// reads its worktree through the live thread, and it reads stopped
    /// only once SIGSTOP has reached that thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_zombie_led_process_is_running_scanned_and_stopped_by_its_live_thread() {
        /// Owns the child, so it is killed and reaped however the test ends,
        /// and never signalled by PID once reaped: `Child::kill` is a no-op
        /// after `wait` has collected its status.
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let child = std::process::Command::new("/usr/bin/python3")
            .args([
                "-c",
                "import threading, time, ctypes\n\
                 def worker():\n    while True: time.sleep(0.05)\n\
                 threading.Thread(target=worker).start()\n\
                 ctypes.CDLL(None).pthread_exit(None)\n",
            ])
            .current_dir(dir.path())
            .spawn()
            .expect("python3 is needed for this test");
        let child = Reap(child);
        let pid = child.0.id();
        let leader_exited = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| task_state(&stat))
                == Some('Z')
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !leader_exited() {
            assert!(
                std::time::Instant::now() < deadline,
                "the leader never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(kernel_observation(pid).unwrap().is_some(), "it still runs");
        assert_eq!(
            proc_cwd(pid)
                .unwrap()
                .map(|cwd| cwd.canonicalize().unwrap()),
            Some(dir.path().canonicalize().unwrap()),
            "the scan reads its worktree through the live thread"
        );
        let row = ProcessRow {
            pid,
            ppid: std::process::id(),
            pgid: 0,
            stat: "Zl".into(),
            args: String::new(),
        };
        assert!(row.active());
        assert!(!row_stopped(&row, task_states), "its worker runs");
        // SAFETY: the test's own child.
        unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !row_stopped(&row, task_states) {
            assert!(
                std::time::Instant::now() < deadline,
                "the stop never showed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Killed and reaped by its guard, once.
        drop(child);
    }

    /// A root's record, running or exited, is adopted only while it is our
    /// own child (its parent is this process): a caller that had already
    /// reaped it could otherwise adopt a reused PID's record, whether that
    /// stranger still runs or has exited too.
    #[test]
    fn a_root_is_adopted_only_as_our_own_child() {
        let me = 4242;
        let record = |pid, ppid| KernelProcess {
            pid,
            ppid,
            pgid: 42,
            birth: Birth(7, 0),
        };
        let root = |process, exited| Some(RootRecord { process, exited });
        assert_eq!(
            adoptable(42, me, root(record(42, me), true)),
            Some(record(42, me))
        );
        assert_eq!(adoptable(42, me, root(record(42, 1), true)), None);
        assert_eq!(
            adoptable(42, me, root(record(42, me), false)),
            Some(record(42, me))
        );
        assert_eq!(adoptable(42, me, root(record(42, 1), false)), None);
        assert_eq!(adoptable(42, me, root(record(43, me), false)), None);
        assert_eq!(adoptable(42, me, None), None);
    }

    /// A real root that exits before adoption reads straight after spawn
    /// is still adopted, with the birth it had while running, on macOS as on
    /// Linux: its zombie is our unreaped child.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_root_that_exited_before_adoption_is_adopted_with_its_birth() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let running = kernel_birth(pid).unwrap().expect("it runs");
        drop(child.stdin.take());
        // Exited, not reaped: every ordinary read now counts it gone.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while kernel_birth(pid).unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "it never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut tracker = ProcessTracker::owning_every_descendant(pid);
        tracker.adopt_root();
        child.wait().unwrap();
        assert_eq!(tracker.root_birth(), Some(running));
    }

    /// The invocation root, read straight after spawn, is our unreaped
    /// child: if it has already exited, its zombie still holds its PID and
    /// its own record, so adoption reads its birth (as released Linux code
    /// did). Every other read counts a zombie as exited.
    #[test]
    fn an_exited_root_still_yields_its_record_for_adoption() {
        let zombie = proc_stat_line(42, "sh", 'Z');
        assert_eq!(kernel_process_from_stat(42, &zombie), Ok(None));
        let record = KernelProcess {
            pid: 42,
            ppid: 300,
            pgid: 42,
            birth: Birth(12345, 0),
        };
        assert_eq!(
            root_process_from_stat(42, &zombie),
            Ok(Some(RootRecord {
                process: record,
                exited: true
            }))
        );
        assert_eq!(
            root_process_from_stat(42, &proc_stat_line(42, "sh", 'S')),
            Ok(Some(RootRecord {
                process: record,
                exited: false
            }))
        );
        assert_eq!(
            root_process_from_stat(42, &proc_stat_line(42, "sh", 'X')),
            Ok(None)
        );
        assert!(root_process_from_stat(43, &zombie).is_err());
    }

    /// A process whose main thread called `pthread_exit` while another
    /// thread runs reads `Z` with `num_threads` 2 (measured in a Linux
    /// container): it still runs and still takes SIGKILL, so it is never
    /// read as exited. Only a `Z` with one thread or none is a zombie, and a
    /// thread count that cannot be read on a `Z` line fails closed.
    #[test]
    fn a_linux_stat_reads_a_multithreaded_zombie_leader_as_running() {
        assert_eq!(
            kernel_process_from_stat(42, &proc_stat_line_with_threads(42, "sh", 'Z', "2")),
            Ok(Some(KernelProcess {
                pid: 42,
                ppid: 300,
                pgid: 42,
                birth: Birth(12345, 0),
            }))
        );
        assert_eq!(
            kernel_process_from_stat(42, &proc_stat_line_with_threads(42, "sh", 'Z', "0")),
            Ok(None)
        );
        assert!(
            kernel_process_from_stat(42, &proc_stat_line_with_threads(42, "sh", 'Z', "many"))
                .is_err()
        );
        assert!(kernel_process_from_stat(42, "42 (sh) Z 300 42 42").is_err());
        // A dead task is gone whatever its thread count reads.
        assert_eq!(
            kernel_process_from_stat(42, &proc_stat_line_with_threads(42, "sh", 'X', "2")),
            Ok(None)
        );
    }

    /// `ps` shows that same process as `Zl` (measured, procps on Debian):
    /// a multithreaded zombie leader, still running, so still active. A
    /// plain `Z` is a zombie.
    #[test]
    fn a_ps_row_for_a_multithreaded_zombie_leader_is_active() {
        let row = |stat: &str| ProcessRow {
            pid: 42,
            ppid: 1,
            pgid: 42,
            stat: stat.into(),
            args: "[python3] <defunct>".into(),
        };
        assert!(row("Zl").active());
        assert!(row("Zl+").active());
        assert!(!row("Z").active());
        assert!(!row("Z+").active());
        assert!(!row("Zs").active());
        assert!(row("Sl").active());
    }

    /// After SIGSTOP the live thread of a `Zl` process reads `T` in its
    /// `/proc/<pid>/task/<tid>/stat`, but `ps` still shows the dead
    /// leader's `Zl` (measured). So such a row is stopped only when every
    /// live task is, and an unreadable task list is not stopped: quiesce
    /// keeps waiting, then fails closed.
    #[test]
    fn a_zombie_led_row_is_stopped_only_when_every_live_task_is() {
        let row = |stat: &str| ProcessRow {
            pid: 42,
            ppid: 1,
            pgid: 42,
            stat: stat.into(),
            args: "[python3] <defunct>".into(),
        };
        let never = |_: u32| -> Result<Vec<char>, String> { panic!("tasks read for a ps state") };
        assert!(row_stopped(&row("T"), never));
        assert!(row_stopped(&row("Tl"), never));
        assert!(!row_stopped(&row("Sl"), never));
        assert!(!row_stopped(&row("R"), never));
        let tasks = |states: &'static [char]| {
            move |pid: u32| {
                assert_eq!(pid, 42);
                Ok::<_, String>(states.to_vec())
            }
        };
        assert!(row_stopped(&row("Zl"), tasks(&['Z', 'T'])));
        assert!(row_stopped(&row("Zl"), tasks(&['Z', 't', 'T'])));
        assert!(
            !row_stopped(&row("Zl"), tasks(&['Z', 'S'])),
            "a live thread runs"
        );
        assert!(!row_stopped(&row("Zl"), tasks(&['Z', 'T', 'R'])));
        assert!(
            !row_stopped(&row("Zl"), |_| Err("unreadable".to_string())),
            "an unreadable task list is not stopped"
        );
        // Every task gone to `Z`/`X`: nothing runs.
        assert!(row_stopped(&row("Zl"), tasks(&['Z', 'X'])));
    }

    /// A task `read_dir` listed whose `stat` then reads ENOENT has just
    /// exited: it is skipped, not an error. Any other failure still is.
    #[test]
    fn a_task_that_exits_between_its_listing_and_its_read_is_skipped() {
        use std::io::{Error, ErrorKind};
        let list = |pid: u32| -> std::io::Result<Vec<PathBuf>> {
            assert_eq!(pid, 8);
            Ok(["8", "10", "11"]
                .iter()
                .map(|tid| PathBuf::from(format!("/proc/8/task/{tid}")))
                .collect())
        };
        let read = |missing: ErrorKind| {
            move |path: &Path| -> std::io::Result<String> {
                match path.to_str().unwrap() {
                    "/proc/8/task/8/stat" => Ok("8 (python3) Z 1 8 8".into()),
                    "/proc/8/task/10/stat" => Err(Error::from(missing)),
                    "/proc/8/task/11/stat" => Ok("11 (python3) T 1 8 8".into()),
                    other => panic!("unexpected read {other}"),
                }
            }
        };
        assert_eq!(
            states_of_tasks(8, list, read(ErrorKind::NotFound)),
            Ok(vec!['Z', 'T'])
        );
        assert!(states_of_tasks(8, list, read(ErrorKind::PermissionDenied)).is_err());
        assert!(states_of_tasks(
            8,
            |_| Err(Error::from(ErrorKind::NotFound)),
            read(ErrorKind::NotFound)
        )
        .is_err());
    }

    /// A Linux `/proc/<pid>/task/<tid>/stat` line's state, after the last
    /// `)` of comm.
    #[test]
    fn a_task_stat_reads_its_state_after_the_last_parenthesis() {
        assert_eq!(task_state("10 (python3) T 1 8 8 0"), Some('T'));
        assert_eq!(task_state("10 (a) Z (b) S 1 8 8 0"), Some('S'));
        assert_eq!(task_state("10 (python3"), None);
    }

    /// A process whose leader thread exited while another runs (`Zl`) has
    /// no `/proc/<pid>/cwd` (ENOENT), but its live thread's
    /// `/proc/<pid>/task/<tid>/cwd` still reads the worktree (measured). The
    /// scan reads that rather than skip it, and holds when none reads.
    #[test]
    fn a_zombie_led_process_is_scanned_by_a_live_threads_working_directory() {
        use std::io::{Error, ErrorKind};
        let enoent = || Error::from(ErrorKind::NotFound);
        let links = |own: Option<&'static str>, task10: Option<&'static str>| {
            move |path: &Path| -> std::io::Result<PathBuf> {
                let found = match path.to_str().unwrap() {
                    "/proc/8/cwd" => own,
                    "/proc/8/task/10/cwd" => task10,
                    "/proc/8/task/8/cwd" => None,
                    other => panic!("unexpected read {other}"),
                };
                found.map(PathBuf::from).ok_or_else(enoent)
            }
        };
        let tasks = |_: u32| -> std::io::Result<Vec<u32>> { Ok(vec![8, 10]) };
        let running = |_: u32| -> Result<bool, String> { Ok(true) };
        // Its own link reads: that is the answer.
        assert_eq!(
            linux_cwd(8, links(Some("/wt"), None), tasks, running),
            Ok(Some(PathBuf::from("/wt")))
        );
        // `Zl`: its own link is gone, a live thread's reads.
        assert_eq!(
            linux_cwd(8, links(None, Some("/wt")), tasks, running),
            Ok(Some(PathBuf::from("/wt")))
        );
        // Running, but no live thread's directory reads: unreadable, held.
        let err = linux_cwd(8, links(None, None), tasks, running).unwrap_err();
        assert!(err.contains("thread"), "{err}");
        // Gone by the kernel: skipped, as before.
        assert_eq!(
            linux_cwd(8, links(None, None), tasks, |_| Ok(false)),
            Ok(None)
        );
        // Its task list gone too: it exited meanwhile.
        assert_eq!(
            linux_cwd(8, links(None, None), |_| Err(enoent()), running),
            Ok(None)
        );
        // A live thread whose link is refused says why, so the hold is
        // diagnosable: the first refusal that does not mean "gone" is kept.
        let refused_task = |path: &Path| -> std::io::Result<PathBuf> {
            match path.to_str().unwrap() {
                "/proc/8/cwd" | "/proc/8/task/8/cwd" => Err(enoent()),
                "/proc/8/task/10/cwd" => Err(Error::from_raw_os_error(libc::EACCES)),
                other => panic!("unexpected read {other}"),
            }
        };
        let err = linux_cwd(8, refused_task, tasks, running).unwrap_err();
        assert!(
            err.contains("thread") && err.contains("Permission denied"),
            "{err}"
        );
        // A kernel read that fails is no proof it is gone.
        assert!(linux_cwd(8, links(None, None), tasks, |_| Err("refused".into())).is_err());
        // Any other error on its own link is unreadable, as before.
        assert!(linux_cwd(
            8,
            |_: &Path| Err(Error::from(ErrorKind::PermissionDenied)),
            tasks,
            running
        )
        .is_err());
        // In the scan, that unreadable directory fails closed on the process.
        let spawned = Birth(100, 0);
        let owner_birth =
            |_: u32| -> Result<Option<(u32, Birth)>, String> { Ok(Some((501, Birth(150, 0)))) };
        let err = scan_cwds([8], 501, spawned, &pid_set(&[]), owner_birth, |pid| {
            linux_cwd(pid, links(None, None), tasks, running)
        })
        .unwrap_err();
        assert_eq!(err.unreadable, Some((8, Birth(150, 0))), "{err}");
    }

    /// Linux reads, like macOS's `kinfo_proc`, count a zombie or a dead
    /// process as exited: an unreaped exit never holds anything for ever.
    #[test]
    fn a_linux_stat_reads_a_zombie_or_dead_process_as_exited() {
        let running = kernel_process_from_stat(42, &proc_stat_line(42, "sh", 'S')).unwrap();
        assert_eq!(
            running,
            Some(KernelProcess {
                pid: 42,
                ppid: 300,
                pgid: 42,
                birth: Birth(12345, 0),
            })
        );
        for state in ['R', 'D', 'T', 't', 'I'] {
            assert!(
                kernel_process_from_stat(42, &proc_stat_line(42, "sh", state))
                    .unwrap()
                    .is_some(),
                "state {state} is running"
            );
        }
        assert_eq!(
            kernel_process_from_stat(42, &proc_stat_line(42, "sh", 'Z')),
            Ok(None),
            "a zombie has exited"
        );
        assert_eq!(
            kernel_process_from_stat(42, &proc_stat_line(42, "sh", 'X')),
            Ok(None),
            "a dead process has exited"
        );
    }

    /// The comm is anything the process names itself, parentheses and
    /// spaces included, so the state is read after the LAST `)`.
    #[test]
    fn a_linux_stat_reads_the_state_after_the_last_parenthesis() {
        let comm = "a) Z (b c";
        let running = kernel_process_from_stat(42, &proc_stat_line(42, comm, 'S')).unwrap();
        assert_eq!(
            running.map(|p| (p.ppid, p.birth)),
            Some((300, Birth(12345, 0)))
        );
        assert_eq!(
            kernel_process_from_stat(42, &proc_stat_line(42, ") R (", 'Z')),
            Ok(None)
        );
        assert!(kernel_process_from_stat(43, &proc_stat_line(42, "sh", 'S')).is_err());
        assert!(kernel_process_from_stat(42, "42 (sh S 1 2 3").is_err());
    }

    /// A start recorded before this boot (a dispatch state from before a
    /// reboot, or clock skew) is clamped to boot: the check then covers
    /// every process born since, which is every process running, rather
    /// than reading as unknown and refusing for that reason.
    #[test]
    fn a_start_before_boot_is_counted_from_boot() {
        let at = |secs| std::time::Duration::from_secs(secs);
        assert_eq!(birth_since_boot(at(1_500), 1_000, 100), Birth(50_000, 0));
        assert_eq!(birth_since_boot(at(1_000), 1_000, 100), Birth(0, 0));
        assert_eq!(birth_since_boot(at(400), 1_000, 100), Birth(0, 0));
    }

    /// The Linux reading of a process's owners, from `/proc/<pid>/status`,
    /// feeds the same rule.
    #[test]
    fn a_linux_status_reads_real_and_effective_uid_for_the_same_rule() {
        let status = "Name:\tsudo\nPPid:\t300\nUid:\t501\t0\t0\t0\nGid:\t20\t20\t20\t20\n";
        assert_eq!(uids_from_status(status), Some((501, 0)));
        assert_eq!(uids_from_status("Name:\tx\n"), None);
        let (ruid, euid) = uids_from_status(status).unwrap();
        let spawned = Birth(100, 0);
        let root = 42;
        // `systemd --user`, a subreaper started long before the invocation.
        let subreaper = 900;
        let owner_birth = |pid: u32| -> Result<Option<(ProcOwner, Birth)>, String> {
            let (ppid, pgid, parent_birth) = match pid {
                // Under the root.
                2 => (root, root, Birth(100, 0)),
                // An orphan of the invocation, reparented to the user's
                // subreaper rather than init, still in the root's group.
                3 => (subreaper, root, Birth(10, 0)),
                // Its parent appeared during the invocation (a tool started
                // between two samples), in a group of its own.
                4 => (500, 4, Birth(120, 0)),
                // The operator's own `sudo` or `top`, under a shell that
                // predates the invocation; and a Terminal tab's `login`.
                _ => (300, pid, Birth(50, 0)),
            };
            Ok(Some((
                ProcOwner {
                    euid,
                    ruid,
                    ppid,
                    pgid,
                    parent: Parent::Born(parent_birth),
                },
                Birth(150, 0),
            )))
        };
        let reads = std::cell::RefCell::new(Vec::new());
        let cwd_of = |pid: u32| -> Result<Option<PathBuf>, String> {
            reads.borrow_mut().push(pid);
            Ok(Some(PathBuf::from(format!("/w/{pid}"))))
        };
        let found = scan_cwds(
            [1, 2, 3, 4, 5],
            501,
            spawned,
            &pid_set(&[root]),
            owner_birth,
            cwd_of,
        )
        .unwrap();
        assert_eq!(
            found.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(
            !reads.borrow().contains(&1) && !reads.borrow().contains(&5),
            "the operator's own setuid processes were read: {:?}",
            reads.borrow()
        );
    }

    /// The same with a real setuid binary: `top` started by this test
    /// process (not the invocation) is skipped; with this process as the
    /// invocation's root, it is listed or, its directory unreadable, fails
    /// the scan for that very process.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_setuid_tool_holds_the_scan_only_under_the_invocation() {
        let Some(top) = setuid_top() else { return };
        let worktree = tempfile::tempdir().unwrap();
        // The invocation starts now: this test process, top's parent,
        // predates it, as an operator's shell does.
        let spawned = birth_at(std::time::SystemTime::now()).unwrap();
        let mut child = Command::new(top)
            .args(["-l", "30", "-s", "1"])
            .current_dir(worktree.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = std::time::Instant::now();
        while !real_and_effective_uid(pid).is_some_and(|(_, euid)| euid == 0) {
            if started.elapsed() > std::time::Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("top never ran with euid 0 within 10s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let listed = user_pids();
        let unrelated = processes_with_cwd(vec![pid], spawned, &pid_set(&[]));
        let tied = processes_with_cwd(vec![pid], spawned, &pid_set(&[std::process::id()]));
        let _ = child.kill();
        let _ = child.wait();
        assert!(listed.unwrap().contains(&pid), "not listed by real uid");
        assert_eq!(unrelated.unwrap(), Vec::new());
        match tied {
            Ok(found) => assert!(found.iter().any(|p| p.pid == pid), "{found:?}"),
            Err(e) => assert_eq!(e.unreadable.map(|(p, _)| p), Some(pid), "{}", e.reason),
        }
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
            // A parsed marker naming the issue's slug outside the Autopilot
            // layout is someone else's run.
            "codex exec --output-schema /tmp/o-r-7/notes/schema.json prompt".into(),
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
            assert!(!tracker.known.contains_key(&20));
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
        tracker.enrol(process_row(20, 10, 20), parent.birth);
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
        assert!(!tracker.known.contains_key(&30));
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
        assert!(tracker
            .known
            .get(&20)
            .is_some_and(|t| t.row.pgid != tracker.root_pid && t.birth == child.birth));
    }
    #[test]
    fn vanished_known_parent_cannot_authorize_a_snapshot_child() {
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker.enrol(process_row(20, 10, 20), Birth(2, 0));
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
        assert!(!tracker.known.contains_key(&30));
        assert!(tracker
            .signal(30, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }

    /// A descendant the kernel has shown gone is never read again: its PID,
    /// reused by an unrelated process under heavy build churn, is a
    /// stranger, not a "tracked PID was reused" abort, and it is neither
    /// enrolled nor signalled.
    #[test]
    fn an_exited_descendants_reused_pid_is_a_stranger_not_an_abort() {
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker.set_kill(|p, _| panic!("signalled {}", p.pid));
        tracker
            .observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 10)],
                |pid| {
                    Ok(Some(if pid == 10 {
                        root
                    } else {
                        kernel_process(20, 10, 10, 2)
                    }))
                },
            )
            .unwrap();
        // It exits.
        tracker
            .observe_rows(vec![process_row(10, 1, 10)], |pid| {
                Ok((pid == 10).then_some(root))
            })
            .unwrap();
        // Its PID now belongs to someone else.
        let stranger = kernel_process(20, 1, 20, 9);
        let mut reads = Vec::new();
        tracker
            .observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 1, 20)],
                |pid| {
                    reads.push(pid);
                    Ok(Some(if pid == 10 { root } else { stranger }))
                },
            )
            .unwrap();
        assert!(!reads.contains(&20), "an exited PID was re-read: {reads:?}");
        assert!(tracker.recorded().1.is_empty(), "{:?}", tracker.recorded());
        assert!(tracker
            .signal(20, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }

    /// A known process is pinned before its birth is checked, and the
    /// signal goes to what was pinned. On Linux that is a pidfd: once its
    /// process is reaped it signals nothing (ESRCH), never a newcomer on the
    /// same PID.
    #[cfg(unix)]
    #[test]
    fn a_signal_reaches_only_the_process_pinned_before_its_birth_check() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let pinned = PinnedProcess::open(pid).unwrap().expect("the child runs");
        assert_eq!(pinned.pid, pid);
        // A kernel before 5.3 (ENOSYS), or a seccomp filter (EPERM), has no
        // pidfds: the fallback to kill(2) is all there is to test there.
        #[cfg(target_os = "linux")]
        if pinned.fd.is_none() {
            // SAFETY: pidfd_open with a PID and no flags; a descriptor it
            // returns is closed at once.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0) };
            let error = std::io::Error::last_os_error();
            if fd >= 0 {
                // SAFETY: the descriptor just opened.
                unsafe { libc::close(fd as i32) };
            }
            let _ = child.kill();
            let _ = child.wait();
            if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EPERM)) && fd < 0 {
                eprintln!("skipped: this kernel offers no pidfd ({error})");
                return;
            }
            panic!("no pidfd for a running child: {error}");
        }
        let born = birth(pid).unwrap().unwrap();
        let mut tracker = ProcessTracker::owning_every_descendant(pid);
        tracker.enrol(process_row(pid, std::process::id(), pid), born);
        assert_eq!(
            tracker.signal_known(pid, born, KILL_SIGNAL).unwrap(),
            Signalled::Sent
        );
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        #[cfg(target_os = "linux")]
        assert_eq!(
            signal_pinned(&pinned, 0).unwrap_err().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    /// Once the root is reaped its PID is free, so cleanup never reads or
    /// signals it again: a stranger holding it now (here, a real process
    /// enrolled under a birth it never had) is no "tracked PID was reused".
    #[cfg(unix)]
    #[test]
    fn a_reaped_roots_pid_is_never_read_again_by_cleanup() {
        let mut stranger = Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = stranger.id();
        let mut tracker = ProcessTracker::owning_every_descendant(pid);
        tracker.set_kill(|p, _| panic!("signalled {}", p.pid));
        tracker.enrol_for_test(process_row(pid, 1, pid), Birth(1, 0));
        tracker.root_birth = Some(Birth(1, 0));
        let cleaned = tracker.cleanup(true);
        let alive = stranger.try_wait().unwrap().is_none();
        let _ = stranger.kill();
        let _ = stranger.wait();
        cleaned.unwrap();
        assert!(alive);
        assert!(tracker.exited.contains(&pid) && !tracker.known.contains_key(&pid));
        assert_eq!(tracker.root_birth(), Some(Birth(1, 0)));
    }

    /// Once the root is reaped its PID, and so its group ID, can be reused.
    /// A stranger now holding the root's PID shows the group ID was freed:
    /// its members are someone else's, not the invocation's stragglers, as
    /// `recorded_tree_evidence` and settling read it. Before reaping the
    /// root's PID is ours, and an untracked member is a straggler.
    #[test]
    fn a_reused_root_group_after_reaping_is_no_straggler() {
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        let reused = vec![process_row(10, 1, 10), process_row(500, 10, 10)];
        tracker.root_reaped = true;
        tracker
            .verify_group_drained_with(|| Ok(reused.clone()), std::time::Duration::ZERO)
            .unwrap();
        // Without the root's PID in the table, the member is unaccounted for.
        let member = vec![process_row(500, 1, 10)];
        let err = tracker
            .verify_group_drained_with(|| Ok(member.clone()), std::time::Duration::ZERO)
            .unwrap_err();
        assert!(err.to_string().contains("untracked process 500"), "{err}");
        // Before reaping, the root's own row is no reuse.
        tracker.root_reaped = false;
        assert!(tracker
            .verify_group_drained_with(|| Ok(reused.clone()), std::time::Duration::ZERO)
            .is_err());
    }

    /// A PID cleanup was refused permission to signal, seen exit and then
    /// enrolled again is a new process: the old refusal is not its.
    #[test]
    fn a_reenrolled_pid_carries_no_old_refusal() {
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker
            .observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 10)],
                |pid| {
                    Ok(Some(if pid == 10 {
                        root
                    } else {
                        kernel_process(20, 10, 10, 2)
                    }))
                },
            )
            .unwrap();
        tracker.refused.insert(20);
        tracker
            .observe_rows(vec![process_row(10, 1, 10)], |pid| {
                Ok((pid == 10).then_some(root))
            })
            .unwrap();
        tracker
            .observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 10)],
                |pid| {
                    Ok(Some(if pid == 10 {
                        root
                    } else {
                        kernel_process(20, 10, 10, 5)
                    }))
                },
            )
            .unwrap();
        assert!(tracker.known.contains_key(&20));
        assert!(!tracker.refused.contains(&20));
    }

    /// A cleanup that fails to quiesce and then fails to kill reports both,
    /// and every process it could not kill, by PID: neither hides the other.
    #[cfg(unix)]
    #[test]
    fn a_failed_quiesce_and_every_failed_kill_are_all_reported() {
        let mut root = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut tracker = ProcessTracker::owning_every_descendant(root.id());
        // Stops are "sent" but never land; every kill fails.
        tracker.set_kill(|_, signal| {
            if signal == KILL_SIGNAL {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        });
        let started = std::time::Instant::now();
        while tracker.known.len() < 2 {
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            tracker.observe().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let pids: Vec<u32> = tracker.known.keys().copied().collect();
        let err = tracker.cleanup(false).unwrap_err().to_string();
        for pid in &pids {
            // SAFETY: our own child and its child, still reserved by them.
            unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
        }
        let _ = root.wait();
        assert!(err.contains("did not quiesce"), "{err}");
        for pid in pids {
            assert!(err.contains(&format!("process {pid}")), "{pid}: {err}");
        }
        assert!(err.contains(&std::io::Error::from_raw_os_error(libc::EIO).to_string()));
    }

    /// The same through cleanup, against the real process table and kernel:
    /// two processes the tracker once enrolled and saw exit, whose PIDs real
    /// processes now hold, are neither re-authenticated, stopped, killed nor
    /// waited for.
    #[cfg(unix)]
    #[test]
    fn cleanup_never_reads_or_signals_an_exited_entrys_reused_pid() {
        let spawn = || {
            Command::new("sleep")
                .arg("30")
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let (mut a, mut b) = (spawn(), spawn());
        let (pa, pb) = (a.id(), b.id());
        let me = std::process::id();
        let mut tracker = ProcessTracker::owning_every_descendant(pa);
        tracker.set_kill(|p, _| panic!("signalled {}", p.pid));
        // Enrolled under births these real processes never had…
        tracker
            .observe_rows(
                vec![process_row(pa, me, pa), process_row(pb, pa, pa)],
                |pid| {
                    Ok(Some(if pid == pa {
                        kernel_process(pa, me, pa, 1)
                    } else {
                        kernel_process(pb, pa, pa, 2)
                    }))
                },
            )
            .unwrap();
        // …then seen exit.
        tracker.observe_rows(Vec::new(), |_| Ok(None)).unwrap();
        let cleaned = tracker.cleanup(true);
        let alive = |c: &mut std::process::Child| c.try_wait().unwrap().is_none();
        let both_alive = alive(&mut a) && alive(&mut b);
        for child in [&mut a, &mut b] {
            let _ = child.kill();
            let _ = child.wait();
        }
        cleaned.unwrap();
        assert!(both_alive, "cleanup signalled a stranger");
    }

    /// Exited descendants still tie a scanned process to the invocation: a
    /// group an exited descendant led still holds its members.
    #[test]
    fn the_worktree_scan_counts_exited_descendants_as_the_invocations() {
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker
            .observe_rows(
                vec![process_row(10, 1, 10), process_row(20, 10, 20)],
                |pid| {
                    Ok(Some(if pid == 10 {
                        root
                    } else {
                        kernel_process(20, 10, 20, 2)
                    }))
                },
            )
            .unwrap();
        tracker
            .observe_rows(vec![process_row(10, 1, 10)], |pid| {
                Ok((pid == 10).then_some(root))
            })
            .unwrap();
        let known = tracker.scan_known();
        assert!(known.contains(&10) && known.contains(&20), "{known:?}");
    }

    #[test]
    fn a_tool_that_exited_with_its_vanished_parent_does_not_abort_observation() {
        // A shell ran a short-lived tool and both exited between the ps
        // sample and the kernel check: the stale child row escaped nothing.
        let root = kernel_process(10, 1, 10, 1);
        let mut tracker = ProcessTracker::owning_every_descendant(10);
        tracker.enrol(process_row(20, 10, 20), Birth(2, 0));
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
        assert!(!tracker.known.contains_key(&30));
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
        assert!(!tracker.known.contains_key(&20));
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
            assert!(!tracker.known.contains_key(&20));
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
        assert!(!tracker.known.contains_key(&20));
        assert!(tracker
            .signal(20, STOP_SIGNAL)
            .unwrap_err()
            .to_string()
            .contains("unauthenticated"));
    }
}
