//! Durable Codex exec dispatch. Artifacts live in the common Git directory,
//! so a replacement controller can inspect them after the checkout disappears.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::autopilot::dispatch::{DispatchOutcome, DispatchSpec, SessionMode, Verdict};
use crate::autopilot::implementer::Implementer;
use crate::autopilot::scrub::scrub_and_bound;
use crate::autopilot::IssueRef;
use crate::error::MemoryError;

pub const CODEX_VERDICT_JSON_SCHEMA: &str = super::dispatch::IC_VERDICT_JSON_SCHEMA;
pub const CODEX_DISPATCH_TIMEOUT_SECS: u64 = 1200;
const JOURNAL_LIMIT: u64 = 16 * 1024 * 1024;
const STDERR_LIMIT: u64 = 1024 * 1024;
const FINAL_LIMIT: u64 = 64 * 1024;
const EXIT_LIMIT: u64 = 4096;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How often supervision rescans the process table. A `ps -axww` scan costs
/// tens of milliseconds of CPU on a busy machine, so scanning on every exit
/// poll kept a core busy for a whole dispatch. Exit and artifact limits stay
/// on [`POLL_INTERVAL`]; descendants enrol at this pace, and cleanup rescans
/// the frozen tree until it is stable. A tool the root starts after the last
/// scan and leaves running at exit is reparented before any scan can enrol
/// it, so completion also requires the root's process group to drain.
const OBSERVE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexArtifacts {
    pub invocation_id: String,
    pub schema_path: String,
    pub journal_path: String,
    pub final_path: String,
    pub stderr_path: String,
    pub exit_path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexRecovery {
    NoPriorInvocation,
    Alive,
    Unknown { reason: String },
    Finished(DispatchOutcome),
}

fn validation(message: impl Into<String>) -> MemoryError {
    MemoryError::Validation(message.into())
}

fn supported_path(path: &Path) -> Result<&str, MemoryError> {
    let text = path.to_str().ok_or_else(|| {
        validation("unsupported Codex artifact path: use a UTF-8 path without whitespace")
    })?;
    if !path.is_absolute() || text.chars().any(char::is_whitespace) {
        return Err(validation("unsupported Codex artifact path: move the repository to an absolute path without whitespace; process identity recovery cannot parse whitespace"));
    }
    // The process table is decoded lossily, so U+FFFD in a marker could match
    // another process's undecodable argv bytes.
    if text.contains(char::REPLACEMENT_CHARACTER) {
        return Err(validation("unsupported Codex artifact path: move the repository to a path without U+FFFD; process identity recovery decodes undecodable argv bytes to it"));
    }
    Ok(text)
}

fn create_private(path: &Path) -> Result<File, MemoryError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

/// Allocate once, then persist these exact paths before spawning the dispatch.
/// Reject unsupported paths before creating any artifact directories or files.
pub fn prepare_artifacts(worktree: &Path, issue: &IssueRef) -> Result<CodexArtifacts, MemoryError> {
    let mut git = Command::new("git");
    git.args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(worktree);
    crate::mcp::tools::scrub_git_environment(&mut git);
    let output = git.output()?;
    if !output.status.success() {
        return Err(validation(format!(
            "cannot resolve common Git directory for Codex artifacts: {}",
            scrub_and_bound(&String::from_utf8_lossy(&output.stderr), 500).text
        )));
    }
    let common_dir = std::str::from_utf8(&output.stdout)
        .map_err(|_| validation("common Git directory is not UTF-8"))?
        .trim_end_matches(['\n', '\r']);
    let invocation_id = uuid::Uuid::new_v4().to_string();
    let dir = Path::new(common_dir)
        .join("ironmem/autopilot")
        .join(issue.slug())
        .join(&invocation_id);
    supported_path(&dir)?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    let a = CodexArtifacts {
        invocation_id,
        schema_path: supported_path(&dir.join("schema.json"))?.into(),
        journal_path: supported_path(&dir.join("journal.jsonl"))?.into(),
        final_path: supported_path(&dir.join("final.json"))?.into(),
        stderr_path: supported_path(&dir.join("stderr.log"))?.into(),
        exit_path: supported_path(&dir.join("exit.json"))?.into(),
    };
    let mut schema = create_private(Path::new(&a.schema_path))?;
    schema.write_all(CODEX_VERDICT_JSON_SCHEMA.as_bytes())?;
    schema.sync_all()?;
    for path in [&a.journal_path, &a.final_path, &a.stderr_path, &a.exit_path] {
        create_private(Path::new(path))?;
    }
    Ok(a)
}

fn validate_spec(spec: &DispatchSpec) -> Result<&CodexArtifacts, MemoryError> {
    if spec.implementer != Implementer::Codex {
        return Err(validation(
            "Codex dispatch requires implementer codex and persisted Codex artifacts",
        ));
    }
    let a = spec.codex_artifacts.as_ref().ok_or_else(|| validation("Codex dispatch requires persisted Codex artifacts; prepare and save them before spawning"))?;
    for path in [
        &a.schema_path,
        &a.journal_path,
        &a.final_path,
        &a.stderr_path,
        &a.exit_path,
    ] {
        supported_path(Path::new(path))?;
    }
    if let SessionMode::Resume { session_uuid } = &spec.session {
        uuid::Uuid::parse_str(session_uuid).map_err(|_| {
            validation("Codex resume requires the native UUID from thread.started.thread_id")
        })?;
    }
    Ok(a)
}

/// argv verified against codex-cli 0.163.0. Resume has no `-C`; cwd is set
/// by the supervisor. Controller UUIDs and Claude-specific limits stay out.
pub fn build_argv(repo: &Path, spec: &DispatchSpec) -> Result<Vec<String>, MemoryError> {
    let a = validate_spec(spec)?;
    let mut args = vec!["exec".into()];
    if matches!(spec.session, SessionMode::Resume { .. }) {
        args.push("resume".into());
    }
    args.extend([
        "--json".into(),
        "--output-schema".into(),
        a.schema_path.clone(),
        "-o".into(),
        a.final_path.clone(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--dangerously-bypass-hook-trust".into(),
    ]);
    if matches!(spec.session, SessionMode::New { .. }) {
        args.extend([
            "-C".into(),
            repo.to_str()
                .ok_or_else(|| validation("Codex repository path must be UTF-8"))?
                .into(),
        ]);
    }
    args.extend(["-m".into(), spec.model.clone()]);
    if let SessionMode::Resume { session_uuid } = &spec.session {
        args.push(session_uuid.clone());
    }
    args.push(spec.condition.clone());
    Ok(args)
}

pub fn dispatch_timeout(spec: &DispatchSpec) -> Duration {
    spec.wall_clock_timeout
        .unwrap_or(Duration::from_secs(CODEX_DISPATCH_TIMEOUT_SECS))
}

pub fn run_codex(
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
) -> Result<DispatchOutcome, MemoryError> {
    run_codex_bounded(bin, repo, spec, dispatch_timeout(spec))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitRecord {
    #[serde(default)]
    completion: Option<CompletionRecord>,
    exit_code: Option<i32>,
    timed_out: bool,
    limit_exceeded: Option<String>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    expected_native_id: Option<String>,
    /// Set only when `spawn` itself failed, so no child ever existed. Omitted
    /// otherwise, keeping every launched record byte-identical to before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_failure: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionRecord {
    version: u32,
    invocation_id: String,
    schema_path: String,
}
impl ExitRecord {
    fn proves_completion(&self, a: &CodexArtifacts) -> bool {
        self.completion.as_ref().is_some_and(|c| {
            c.version == 1 && c.invocation_id == a.invocation_id && c.schema_path == a.schema_path
        })
    }
}

fn open_existing(path: &str, write: bool) -> Result<File, MemoryError> {
    let mut options = OpenOptions::new();
    options.read(!write).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(validation("Codex artifacts must be regular files"));
    }
    Ok(file)
}

fn limits(a: &CodexArtifacts) -> Option<String> {
    [
        (&a.journal_path, JOURNAL_LIMIT, "journal"),
        (&a.stderr_path, STDERR_LIMIT, "stderr"),
        (&a.final_path, FINAL_LIMIT, "final"),
    ]
    .into_iter()
    .find_map(|(path, limit, name)| match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= limit => None,
        _ => Some(format!(
            "Codex {name} artifact exceeded its {limit}-byte limit or is unavailable"
        )),
    })
}

/// Durably replace `path` with `bytes`: a private temp file named
/// `<tmp_prefix>-<uuid>.tmp`, synced, renamed over it, and the directory
/// synced, so a reader sees the old record or the new one and never a torn one.
pub(crate) fn persist_private_atomic(
    path: &Path,
    tmp_prefix: &str,
    bytes: &[u8],
) -> Result<(), MemoryError> {
    let tmp = path.with_file_name(format!("{tmp_prefix}-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = create_private(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        File::open(
            path.parent()
                .ok_or_else(|| validation("Codex artifact requires a parent directory"))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

fn persist_exit(a: &CodexArtifacts, record: &ExitRecord) -> Result<(), MemoryError> {
    persist_private_atomic(
        Path::new(&a.exit_path),
        "exit",
        &serde_json::to_vec(record)?,
    )
}

/// The supervisor's terminal record for one invocation.
fn exit_record(
    a: &CodexArtifacts,
    spec: &DispatchSpec,
    exit_code: Option<i32>,
    timed_out: bool,
    limit_exceeded: Option<String>,
    duration_ms: u64,
    launch_failure: Option<String>,
) -> ExitRecord {
    ExitRecord {
        completion: Some(CompletionRecord {
            version: 1,
            invocation_id: a.invocation_id.clone(),
            schema_path: a.schema_path.clone(),
        }),
        exit_code,
        timed_out,
        limit_exceeded,
        duration_ms,
        expected_native_id: match &spec.session {
            SessionMode::Resume { session_uuid } => Some(session_uuid.clone()),
            SessionMode::New { .. } => None,
        },
        launch_failure,
    }
}

/// Every invocation has a wall-clock ceiling. stdout/stderr are inherited
/// regular file handles, so no pipe can wedge the child or die with its parent.
pub fn run_codex_bounded(
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
    timeout: Duration,
) -> Result<DispatchOutcome, MemoryError> {
    let args = build_argv(repo, spec)?;
    let a = validate_spec(spec)?;
    // Never overwrite evidence from a prior invocation. A retry must allocate
    // a new invocation directory, even when it resumes the same native thread.
    for path in [&a.journal_path, &a.stderr_path, &a.final_path, &a.exit_path] {
        if open_existing(path, false)?.metadata()?.len() != 0 {
            return Err(validation("Codex artifacts already contain invocation data; prepare a new invocation before retrying"));
        }
    }
    let schema = read_capped(&a.schema_path, FINAL_LIMIT)?;
    if schema != CODEX_VERDICT_JSON_SCHEMA {
        return Err(validation(
            "Codex artifacts must contain the strict verdict schema",
        ));
    }
    let mut command = Command::new(bin);
    command
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::from(open_existing(&a.journal_path, true)?))
        .stderr(Stdio::from(open_existing(&a.stderr_path, true)?));
    // Git subprocesses inside Codex must resolve this invocation's checkout,
    // rather than a controller's inherited repository-selection overrides.
    crate::mcp::tools::scrub_git_environment(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match super::dispatch::retry_on_etxtbsy(|| command.spawn()) {
        Ok(child) => child,
        // A failed spawn proves no child exists. Record that as supervisor
        // evidence so neither this run nor a replacement controller holds an
        // invocation that never ran.
        Err(e) => return never_launched(a, spec, &format!("failed to launch Codex dispatch: {e}")),
    };
    let started = Instant::now();
    let mut identity = super::codex_process::ProcessIdentity {
        version: 1,
        root_pid: child.id(),
        root_pgid: child.id(),
        schema_path: a.schema_path.clone(),
        invocation_id: a.invocation_id.clone(),
        descendants: Vec::new(),
    };
    let mut tracker = super::codex_process::ProcessTracker::new(child.id());
    if let Err(e) = super::codex_process::persist_identity(a, &identity) {
        // The root is still our unreaped Child; cleanup is authenticated by
        // kernel birth identity, and cannot authorize a retry on uncertainty.
        let cleanup = tracker.cleanup(false);
        let _ = child.kill();
        let _ = child.wait();
        return Err(with_cleanup(e, cleanup));
    }
    let mut timed_out = false;
    let mut limit_exceeded;
    let mut last_observed: Option<Instant> = None;
    let status = loop {
        // The first pass scans immediately after spawn.
        if last_observed.is_none_or(|at| at.elapsed() >= OBSERVE_INTERVAL) {
            if let Err(e) = tracker.observe() {
                let cleanup = tracker.cleanup(false);
                let _ = child.kill();
                let _ = child.wait();
                return Err(with_cleanup(e, cleanup));
            }
            last_observed = Some(Instant::now());
        }
        limit_exceeded = limits(a);
        if limit_exceeded.is_some() || started.elapsed() >= timeout {
            timed_out = limit_exceeded.is_none();
            let cleanup = tracker.cleanup(false);
            if cleanup.is_err() {
                let _ = child.kill();
            }
            let waited = child.wait();
            cleanup?;
            break waited?;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => {
                let cleanup = tracker.cleanup(false);
                let _ = child.kill();
                let _ = child.wait();
                return Err(with_cleanup(e.into(), cleanup));
            }
        }
    };
    identity.descendants = tracker.descendants();
    if let Err(e) = super::codex_process::persist_identity(a, &identity) {
        let cleanup = tracker.cleanup(true);
        return Err(with_cleanup(e, cleanup));
    }
    finish_codex_dispatch(
        &mut tracker,
        a,
        spec,
        status,
        started,
        timed_out,
        limit_exceeded,
    )
}

fn never_launched(
    a: &CodexArtifacts,
    spec: &DispatchSpec,
    reason: &str,
) -> Result<DispatchOutcome, MemoryError> {
    let record = exit_record(a, spec, None, false, None, 0, Some(reason.to_owned()));
    persist_exit(a, &record).map_err(|e| {
        MemoryError::NotFound(format!(
            "{reason}; recording that it never launched also failed: {e}"
        ))
    })?;
    read_artifacts(a)
}

/// A supervisor record proving `spawn` failed, so no process ever ran.
pub fn has_launch_failure_record(artifacts: &CodexArtifacts) -> bool {
    read_capped(&artifacts.exit_path, EXIT_LIMIT)
        .ok()
        .and_then(|text| serde_json::from_str::<ExitRecord>(&text).ok())
        .is_some_and(|record| {
            record.proves_completion(artifacts) && record.launch_failure.is_some()
        })
}

/// A dispatch error must not hide a failed cleanup: descendants that may
/// still be running in the worktree are the operator's next problem.
fn with_cleanup(err: MemoryError, cleanup: Result<(), MemoryError>) -> MemoryError {
    match cleanup {
        Ok(()) => err,
        Err(c) => validation(format!(
            "{err}; process-tree cleanup also failed, so descendants may still be running: {c}"
        )),
    }
}

/// Complete supervision after the root process has been observed and reaped.
/// Descendants may still be running and writing the invocation's artifacts.
fn finish_codex_dispatch(
    tracker: &mut super::codex_process::ProcessTracker,
    a: &CodexArtifacts,
    spec: &DispatchSpec,
    status: std::process::ExitStatus,
    started: Instant,
    timed_out: bool,
    limit_exceeded: Option<String>,
) -> Result<DispatchOutcome, MemoryError> {
    // Catch an artifact that crossed the limit just before process exit.
    let limit_exceeded = limit_exceeded.or_else(|| limits(a));
    // Success included: a root gone before the first sample could have
    // started a tool that left its group, and nothing tracked it.
    if !tracker.observed_root() {
        return Err(validation(
            "Codex root exited before process-tree observation; descendant completion is unproven",
        ));
    }
    // Even a successful root can leave a gate in a separate process group.
    // Unknown cleanup returns Err so run.rs retains the invocation and slot.
    tracker.cleanup(true)?;
    tracker.verify_group_drained()?;
    let record = exit_record(
        a,
        spec,
        status.code(),
        timed_out,
        limit_exceeded,
        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        None,
    );
    persist_exit(a, &record)?;
    let mut outcome = read_artifacts(a)?;
    outcome.invocation_started = true;
    Ok(outcome)
}

fn read_capped_bytes(path: &str, cap: u64) -> Result<Vec<u8>, MemoryError> {
    let file = open_existing(path, false)?;
    let mut data = Vec::new();
    file.take(cap + 1).read_to_end(&mut data)?;
    if data.len() as u64 > cap {
        return Err(validation(format!(
            "Codex artifact exceeds {cap}-byte limit"
        )));
    }
    Ok(data)
}

fn read_capped(path: &str, cap: u64) -> Result<String, MemoryError> {
    String::from_utf8(read_capped_bytes(path, cap)?)
        .map_err(|_| validation("Codex artifact is not UTF-8"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalVerdict {
    verdict: Verdict,
    reason: String,
}

/// Parse bounded durable evidence. A final file alone never proves success:
/// only the supervisor's valid exit-zero record can authorize a met verdict.
/// A truncated journal invalidates the verdict but retains an earlier unique
/// native thread ID so a replacement controller can resume it.
pub fn read_artifacts(a: &CodexArtifacts) -> Result<DispatchOutcome, MemoryError> {
    for path in [
        &a.schema_path,
        &a.journal_path,
        &a.final_path,
        &a.stderr_path,
        &a.exit_path,
    ] {
        supported_path(Path::new(path))?;
    }
    let mut errors = Vec::new();
    let mut native_id = None;
    let mut conflict = false;
    let mut turns = 0u32;
    let mut observed_output = false;
    let mut never_started = false;
    match read_capped(&a.journal_path, JOURNAL_LIMIT) {
        Ok(journal) => {
            observed_output |= !journal.is_empty();
            for line in journal.lines().filter(|line| !line.trim().is_empty()) {
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(value) if value.is_object() => {
                        match value.get("type").and_then(|v| v.as_str()) {
                            Some("thread.started") => {
                                if let Some(id) = value
                                    .get("thread_id")
                                    .and_then(|v| v.as_str())
                                    .filter(|s| uuid::Uuid::parse_str(s).is_ok())
                                {
                                    if native_id.as_deref().is_some_and(|prior| prior != id) {
                                        conflict = true;
                                        errors.push("conflicting native Codex thread IDs".into());
                                    } else {
                                        native_id = Some(id.to_owned());
                                    }
                                } else {
                                    errors.push("invalid native Codex thread ID".into());
                                }
                            }
                            Some("turn.completed") => turns = turns.saturating_add(1),
                            _ => {}
                        }
                    }
                    _ => errors.push("malformed Codex JSONL journal".into()),
                }
            }
        }
        Err(e) => errors.push(e.to_string()),
    }
    if conflict {
        native_id = None;
    }
    if native_id.is_none() {
        errors.push("missing unique native Codex thread.started ID".into());
    }
    let final_result = read_capped(&a.final_path, FINAL_LIMIT).and_then(|text| {
        observed_output |= !text.is_empty();
        serde_json::from_str::<FinalVerdict>(&text)
            .map_err(|e| validation(format!("invalid Codex final verdict: {e}")))
    });
    if let Err(e) = &final_result {
        errors.push(e.to_string());
    }
    let exit = read_capped(&a.exit_path, EXIT_LIMIT).and_then(|text| {
        serde_json::from_str::<ExitRecord>(&text).map_err(|e| {
            validation(format!(
                "missing or invalid Codex supervisor exit record: {e}"
            ))
        })
    });
    let (process_success, timed_out, duration_ms) = match &exit {
        Ok(record) => {
            if !record.proves_completion(a) {
                errors.push("untrusted Codex supervisor completion evidence".into());
            } else if let Some(reason) = &record.launch_failure {
                never_started = true;
                errors.push(format!("Codex never launched: {reason}"));
            }
            if record
                .expected_native_id
                .as_ref()
                .is_some_and(|expected| native_id.as_ref() != Some(expected))
            {
                errors.push("Codex thread.started ID did not match requested resume thread".into());
                native_id = None;
            }
            if let Some(limit) = &record.limit_exceeded {
                errors.push(limit.clone());
            }
            if record.timed_out {
                errors.push("Codex dispatch exceeded wall-clock timeout and was killed".into());
            }
            if record.exit_code != Some(0) {
                errors.push(format!("Codex process exited with {:?}", record.exit_code));
            }
            (
                record.proves_completion(a)
                    && record.exit_code == Some(0)
                    && !record.timed_out
                    && record.limit_exceeded.is_none(),
                record.timed_out,
                record.duration_ms,
            )
        }
        Err(e) => {
            errors.push(e.to_string());
            (false, false, 0)
        }
    };
    // Also enforce caps at recovery, even if an exit record says zero.
    if let Some(limit) = limits(a) {
        errors.push(limit);
    }
    // Stderr is diagnostic only: it never decides validity, but a failed read
    // is reported with the other errors rather than silently dropped.
    let (stderr, stderr_error) = match read_capped_bytes(&a.stderr_path, STDERR_LIMIT) {
        Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), None),
        Err(e) => (String::new(), Some(e)),
    };
    observed_output |= !stderr.is_empty();
    let valid = errors.is_empty();
    let (verdict, reason) = if valid {
        let final_result = final_result.map_err(|e| validation(e.to_string()))?;
        (Some(final_result.verdict), Some(final_result.reason))
    } else {
        if !stderr.trim().is_empty() {
            errors.push(format!("stderr: {}", stderr.trim()));
        }
        if let Some(e) = stderr_error {
            errors.push(format!("stderr unreadable: {e}"));
        }
        (None, Some(scrub_and_bound(&errors.join("; "), 500).text))
    };
    Ok(DispatchOutcome {
        cost_usd: None,
        num_turns: turns,
        duration_ms,
        is_error: !valid,
        native_session_id: native_id,
        verdict,
        reason,
        process_success,
        timed_out,
        invocation_started: (exit.is_ok() || observed_output) && !never_started,
    })
}

/// A typed supervisor record proves that wait observed process completion.
/// A missing, truncated or malformed record does not establish liveness.
pub fn has_valid_exit_record(artifacts: &CodexArtifacts) -> bool {
    read_capped(&artifacts.exit_path, EXIT_LIMIT)
        .ok()
        .and_then(|text| serde_json::from_str::<ExitRecord>(&text).ok())
        .is_some_and(|record| record.proves_completion(artifacts))
}

/// The caller MUST first confirm the invocation is dead. This file-only
/// primitive cannot establish liveness; registry callers supply Alive/Unknown.
/// Prepared, empty artifacts carry invocation_started=false and no native ID.
pub fn recover_artifacts(artifacts: Option<&CodexArtifacts>) -> Result<CodexRecovery, MemoryError> {
    match artifacts {
        None => Ok(CodexRecovery::NoPriorInvocation),
        Some(a) => Ok(CodexRecovery::Finished(read_artifacts(a)?)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{build_argv, CODEX_VERDICT_JSON_SCHEMA};
    use crate::autopilot::dispatch::{CodexArtifacts, DispatchSpec, SessionMode};
    use crate::autopilot::implementer::Implementer;

    fn artifacts() -> CodexArtifacts {
        CodexArtifacts {
            invocation_id: "invocation".into(),
            schema_path: "/tmp/schema.json".into(),
            journal_path: "/tmp/journal.jsonl".into(),
            final_path: "/tmp/final.json".into(),
            stderr_path: "/tmp/stderr.log".into(),
            exit_path: "/tmp/exit.json".into(),
        }
    }

    fn spec(session: SessionMode) -> DispatchSpec {
        DispatchSpec {
            implementer: Implementer::Codex,
            codex_artifacts: Some(artifacts()),
            session,
            name: "unused-by-codex".into(),
            model: "gpt-test".into(),
            max_budget_usd: 1.0,
            max_turns: 2,
            condition: "make the gate pass".into(),
            wall_clock_timeout: None,
        }
    }

    #[test]
    fn codex_schema_is_the_exact_strict_verdict_contract() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(CODEX_VERDICT_JSON_SCHEMA).unwrap(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "verdict": {"type": "string", "enum": ["met", "impossible", "not_met"]},
                    "reason": {"type": "string"}
                },
                "required": ["verdict", "reason"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn new_invocation_uses_verified_codex_exec_arguments() {
        let args = build_argv(
            Path::new("/repo"),
            &spec(SessionMode::New {
                session_uuid: "ignored-controller-id".into(),
            }),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "exec",
                "--json",
                "--output-schema",
                "/tmp/schema.json",
                "-o",
                "/tmp/final.json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust",
                "-C",
                "/repo",
                "-m",
                "gpt-test",
                "make the gate pass"
            ]
        );
    }

    #[test]
    fn resume_uses_native_thread_id_and_omits_c_and_goal_prefix() {
        let args = build_argv(
            Path::new("/repo"),
            &spec(SessionMode::Resume {
                session_uuid: "11111111-2222-3333-4444-555555555555".into(),
            }),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "exec",
                "resume",
                "--json",
                "--output-schema",
                "/tmp/schema.json",
                "-o",
                "/tmp/final.json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust",
                "-m",
                "gpt-test",
                "11111111-2222-3333-4444-555555555555",
                "make the gate pass"
            ]
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::autopilot::dispatch::{DispatchSpec, SessionMode, Verdict};
    use crate::autopilot::implementer::Implementer;
    use crate::autopilot::IssueRef;

    const THREAD: &str = "11111111-2222-3333-4444-555555555555";
    fn git(repo: &Path) -> std::process::Command {
        let mut command = std::process::Command::new("git");
        command.current_dir(repo);
        crate::mcp::tools::scrub_git_environment(&mut command);
        command
    }
    fn setup() -> (tempfile::TempDir, DispatchSpec) {
        let dir = tempfile::tempdir().unwrap();
        let status = git(dir.path()).args(["init", "-q"]).status().unwrap();
        assert!(status.success());
        let artifacts = prepare_artifacts(dir.path(), &IssueRef::new("owner/repo", 42)).unwrap();
        let spec = DispatchSpec {
            implementer: Implementer::Codex,
            codex_artifacts: Some(artifacts),
            session: SessionMode::New {
                session_uuid: "controller-only".into(),
            },
            name: "unused".into(),
            model: "gpt-test".into(),
            max_budget_usd: 1.0,
            max_turns: 2,
            condition: "gate".into(),
            wall_clock_timeout: None,
        };
        (dir, spec)
    }
    fn completed(
        spec: &DispatchSpec,
        final_json: &str,
        journal: &str,
        exit: Option<i32>,
    ) -> DispatchOutcome {
        let a = spec.codex_artifacts.as_ref().unwrap();
        std::fs::write(&a.final_path, final_json).unwrap();
        std::fs::write(&a.journal_path, journal).unwrap();
        if let Some(code) = exit {
            std::fs::write(
                &a.exit_path,
                serde_json::to_vec(
                    &serde_json::json!({"exit_code":code,"timed_out":false,"limit_exceeded":null,"completion":{"version":1,"invocation_id":a.invocation_id,"schema_path":a.schema_path}}),
                )
                .unwrap(),
            )
            .unwrap();
        }
        read_artifacts(a).unwrap()
    }
    fn journal() -> String {
        format!("{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD}\"}}\n{{\"type\":\"turn.completed\",\"usage\":{{}}}}\n")
    }

    #[test]
    fn strict_final_and_native_identity_and_supervisor_exit_are_all_required() {
        for verdict in ["met", "impossible", "not_met"] {
            let (_dir, s) = setup();
            let o = completed(
                &s,
                &format!("{{\"verdict\":\"{verdict}\",\"reason\":\"gate\"}}"),
                &journal(),
                Some(0),
            );
            assert_eq!(o.is_met(), verdict == "met");
            assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
            assert_eq!(o.num_turns, 1);
            assert_eq!(o.cost_usd, None);
            assert_eq!(
                o.verdict,
                Some(match verdict {
                    "met" => Verdict::Met,
                    "impossible" => Verdict::Impossible,
                    _ => Verdict::NotMet,
                })
            );
        }
        for final_json in [
            "",
            "{",
            "{}",
            r#"{"verdict":"met"}"#,
            r#"{"verdict":"unknown","reason":"x"}"#,
            r#"{"verdict":"met","reason":"x","extra":1}"#,
        ] {
            let (_dir, s) = setup();
            assert!(
                !completed(&s, final_json, &journal(), Some(0)).is_met(),
                "{final_json}"
            );
        }
        for log in ["".into(), "{bad\n".into(), "{\"type\":\"thread.started\",\"thread_id\":\"\"}\n".into(), "{\"type\":\"thread.started\",\"thread_id\":\"not-uuid\"}\n".into(), format!("{}{{\"type\":\"thread.started\",\"thread_id\":\"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\"}}\n", journal())] {
            let (_dir, s) = setup();
            assert!(!completed(&s, r#"{"verdict":"met","reason":"gate"}"#, &log, Some(0)).is_met(), "{log}");
        }
        for code in [None, Some(1)] {
            let (_dir, s) = setup();
            let o = completed(&s, r#"{"verdict":"met","reason":"gate"}"#, &journal(), code);
            assert!(!o.is_met());
            assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
        }
    }
    #[test]
    fn non_utf8_stderr_still_explains_a_failure_and_never_decides_validity() {
        let (_dir, s) = setup();
        let a = s.codex_artifacts.as_ref().unwrap();
        std::fs::write(&a.stderr_path, b"model \xff rejected\n").unwrap();
        let met = completed(
            &s,
            r#"{"verdict":"met","reason":"gate"}"#,
            &journal(),
            Some(0),
        );
        assert!(met.is_met(), "stderr is diagnostic only");
        let (_dir, s) = setup();
        let a = s.codex_artifacts.as_ref().unwrap();
        std::fs::write(&a.stderr_path, b"model \xff rejected\n").unwrap();
        let failed = completed(
            &s,
            r#"{"verdict":"met","reason":"gate"}"#,
            &journal(),
            Some(1),
        );
        let reason = failed.reason.unwrap();
        assert!(reason.contains("model \u{FFFD} rejected"), "{reason}");
    }
    #[test]
    fn artifacts_are_private_absolute_and_in_common_git_directory() {
        let (dir, s) = setup();
        let a = s.codex_artifacts.unwrap();
        for p in [
            &a.schema_path,
            &a.journal_path,
            &a.final_path,
            &a.stderr_path,
            &a.exit_path,
        ] {
            assert!(Path::new(p).is_absolute());
            assert!(Path::new(p).starts_with(
                dir.path()
                    .canonicalize()
                    .unwrap()
                    .join(".git/ironmem/autopilot/owner-repo-42")
            ));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        assert_eq!(
            std::fs::read_to_string(&a.schema_path).unwrap(),
            CODEX_VERDICT_JSON_SCHEMA
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(Path::new(&a.schema_path).parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }
    #[test]
    fn whitespace_is_rejected_before_artifact_creation_and_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("has space");
        std::fs::create_dir(&repo).unwrap();
        assert!(git(&repo).args(["init", "-q"]).status().unwrap().success());
        let err = prepare_artifacts(&repo, &IssueRef::new("o/r", 1))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unsupported") && err.contains("whitespace"),
            "{err}"
        );
        assert!(!repo.join(".git/ironmem").exists());
        let (_d, mut s) = setup();
        s.codex_artifacts.as_mut().unwrap().final_path = "/tmp/has space/final".into();
        assert!(run_codex_bounded(
            Path::new("/missing-binary"),
            dir.path(),
            &s,
            Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("whitespace"));
        // Lossy ps decoding turns undecodable argv bytes into U+FFFD, so a
        // marker containing it could match a different process's bytes.
        let repo = dir.path().join("rep\u{FFFD}o");
        std::fs::create_dir(&repo).unwrap();
        assert!(git(&repo).args(["init", "-q"]).status().unwrap().success());
        let err = prepare_artifacts(&repo, &IssueRef::new("o/r", 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("U+FFFD"), "{err}");
        assert!(!repo.join(".git/ironmem").exists());
    }
    #[test]
    fn a_spawn_that_never_launched_is_a_durable_infrastructure_failure_not_a_hold() {
        use crate::autopilot::codex_process::{CodexProcessSnapshot, ProcessRow};
        use crate::autopilot::registry::Liveness;
        let (dir, s) = setup();
        let a = s.codex_artifacts.clone().unwrap();
        let o = run_codex_bounded(
            Path::new("/missing-binary"),
            dir.path(),
            &s,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!o.invocation_started && !o.process_success && !o.is_met());
        let reason = o.reason.unwrap();
        assert!(reason.contains("never launched"), "{reason}");
        // A replacement controller reads the same supervisor evidence: idle,
        // and recovered as the same unstarted failure rather than held.
        assert!(has_valid_exit_record(&a));
        let recovered = read_artifacts(&a).unwrap();
        assert!(!recovered.invocation_started && !recovered.is_met());
        let empty = CodexProcessSnapshot::Available(Vec::new());
        assert_eq!(empty.liveness(Some(&a)), Liveness::NotListed);
        // A live process carrying the marker contradicts the record: hold.
        let live = CodexProcessSnapshot::Available(vec![ProcessRow {
            pid: 70,
            ppid: 1,
            pgid: 70,
            stat: "S".into(),
            args: format!("codex exec --output-schema {} prompt", a.schema_path),
        }]);
        assert_eq!(live.liveness(Some(&a)), Liveness::Unknown);
        // So does an identity file: only a spawned child writes one.
        std::fs::write(
            Path::new(&a.schema_path).with_file_name("process.json"),
            b"{}",
        )
        .unwrap();
        assert_eq!(empty.liveness(Some(&a)), Liveness::Unknown);
        // The record never authorizes reuse of the invocation's artifacts.
        assert!(run_codex_bounded(
            Path::new("/missing-binary"),
            dir.path(),
            &s,
            Duration::from_secs(1)
        )
        .is_err());
    }
    #[test]
    fn timeout_default_override_and_backend_artifact_pairing() {
        let (dir, mut s) = setup();
        assert_eq!(dispatch_timeout(&s), Duration::from_secs(1200));
        s.wall_clock_timeout = Some(Duration::from_millis(15));
        assert_eq!(dispatch_timeout(&s), Duration::from_millis(15));
        s.codex_artifacts = None;
        assert!(build_argv(dir.path(), &s).is_err());
        let (_, mut s) = setup();
        s.implementer = Implementer::Claude;
        assert!(run_codex_bounded(
            Path::new("/missing-binary"),
            dir.path(),
            &s,
            Duration::from_secs(1)
        )
        .is_err());
        assert!(crate::autopilot::dispatch::run_dispatch(
            Path::new("/missing-binary"),
            dir.path(),
            &s
        )
        .unwrap_err()
        .to_string()
        .contains("artifacts"));
    }
    #[cfg(unix)]
    fn binary(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-codex");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(unix)]
    /// A stub that outlives the supervisor's first process sample, as any
    /// real Codex does; one that exits first is held as unobserved.
    fn observable(extra: &str) -> String {
        format!("sleep 0.5\n{extra}")
    }
    fn script(extra: &str, code: i32) -> String {
        format!(
            r#"while [ "$#" -gt 0 ]; do
if [ "$1" = '-o' ]; then shift; final="$1"; fi
shift
done
pwd > actual-cwd
printf '%s\n' '{{"type":"thread.started","thread_id":"{THREAD}"}}'
printf '%s\n' '{{"type":"turn.completed","usage":{{}}}}'
printf '%s\n' '{{"verdict":"met","reason":"gate"}}' > "$final"
{extra}
exit {code}"#
        )
    }
    #[cfg(unix)]
    fn spawn_fixture_invocation(
        bin: &Path,
        repo: &Path,
        spec: &DispatchSpec,
    ) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let a = spec.codex_artifacts.as_ref().unwrap();
        let mut command = Command::new(bin);
        command
            .args(build_argv(repo, spec).unwrap())
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::from(open_existing(&a.journal_path, true).unwrap()))
            .stderr(Stdio::from(open_existing(&a.stderr_path, true).unwrap()))
            .process_group(0);
        crate::mcp::tools::scrub_git_environment(&mut command);
        let child = super::super::dispatch::retry_on_etxtbsy(|| command.spawn()).unwrap();
        super::super::codex_process::persist_identity(
            a,
            &super::super::codex_process::ProcessIdentity {
                version: 1,
                root_pid: child.id(),
                root_pgid: child.id(),
                schema_path: a.schema_path.clone(),
                invocation_id: a.invocation_id.clone(),
                descendants: Vec::new(),
            },
        )
        .unwrap();
        child
    }

    #[cfg(unix)]
    fn run_observed_fixture(bin: &Path, repo: &Path, spec: &DispatchSpec) -> DispatchOutcome {
        let started = Instant::now();
        let mut child = spawn_fixture_invocation(bin, repo, spec);
        let mut tracker = super::super::codex_process::ProcessTracker::new(child.id());
        let result = (|| {
            while !repo.join("root-ready").exists() {
                if child.try_wait()?.is_some() || started.elapsed() >= Duration::from_secs(3) {
                    return Err(validation("fixture did not wait for process observation"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // The release follows this actual ps/kernel observation, rather
            // than an arbitrary delay intended to make observation likely.
            tracker.observe()?;
            if !tracker.observed_root() {
                return Err(validation("fixture root was not observed"));
            }
            std::fs::write(repo.join("root-release"), "release")?;
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if started.elapsed() >= Duration::from_secs(3) {
                    return Err(validation("fixture did not exit after release"));
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            finish_codex_dispatch(
                &mut tracker,
                spec.codex_artifacts.as_ref().unwrap(),
                spec,
                status,
                started,
                false,
                None,
            )
        })();
        if result.is_err() {
            let _ = tracker.cleanup(false);
            let _ = child.kill();
            let _ = child.wait();
        }
        result.unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_uses_cwd_durable_files_and_refuses_nonzero_after_met() {
        for code in [0, 7] {
            let (dir, s) = setup();
            let extra = if code == 0 {
                observable("")
            } else {
                "touch root-ready; while [ ! -f root-release ]; do sleep 0.01; done".into()
            };
            let bin = binary(dir.path(), &script(&extra, code));
            let o = if code == 0 {
                run_codex_bounded(&bin, dir.path(), &s, Duration::from_secs(3)).unwrap()
            } else {
                run_observed_fixture(&bin, dir.path(), &s)
            };
            assert!(
                !o.timed_out,
                "the nonzero fixture must exit normally after observation"
            );
            assert_eq!(o.process_success, code == 0);
            assert_eq!(o.is_error, code != 0);
            assert_eq!(o.is_met(), code == 0);
            assert!(o.invocation_started);
            assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
            assert_eq!(
                std::fs::read_to_string(dir.path().join("actual-cwd"))
                    .unwrap()
                    .trim(),
                dir.path().canonicalize().unwrap().to_str().unwrap()
            );
            let recovered = read_artifacts(s.codex_artifacts.as_ref().unwrap()).unwrap();
            assert_eq!(recovered.is_met(), code == 0);
        }
    }
    #[cfg(unix)]
    #[test]
    fn abnormal_exit_before_any_process_observation_holds_even_after_met_output() {
        let (dir, spec) = setup();
        let a = spec.codex_artifacts.as_ref().unwrap();
        let bin = binary(dir.path(), &script("", 7));
        let started = Instant::now();
        let mut child = spawn_fixture_invocation(&bin, dir.path(), &spec);
        let mut tracker = super::super::codex_process::ProcessTracker::new(child.id());
        // Reap first, deliberately making useful root observation impossible.
        // This is the scheduler ordering the old combined fixture assumed away.
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(!tracker.observed_root());
        let final_verdict: FinalVerdict =
            serde_json::from_str(&std::fs::read_to_string(&a.final_path).unwrap()).unwrap();
        assert_eq!(final_verdict.verdict, Verdict::Met);
        let error = finish_codex_dispatch(&mut tracker, a, &spec, status, started, false, None)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("before process-tree observation"));
        assert!(!has_valid_exit_record(a));
        let process = super::super::codex_process::snapshot(
            &mut super::super::codex_process::UnixCodexProcesses,
        );
        assert!(process.is_available());
        assert_eq!(
            process.liveness(Some(a)),
            super::super::registry::Liveness::Unknown
        );
        let outcome = read_artifacts(a).unwrap();
        assert_eq!(outcome.native_session_id.as_deref(), Some(THREAD));
        assert!(!outcome.is_met());
        assert!(!outcome.process_success);
    }

    #[cfg(unix)]
    #[test]
    fn large_journal_does_not_deadlock_and_limits_kill_with_scrubbed_bounded_reason() {
        let (dir, s) = setup();
        let extra = "i=0; while [ \"$i\" -lt 3000 ]; do printf '%s\\n' '{\"type\":\"item.completed\",\"item\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}'; i=$((i+1)); done";
        let o = run_codex_bounded(
            &binary(dir.path(), &script(extra, 0)),
            dir.path(),
            &s,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(o.is_met(), "{o:?}");
        for (field, bytes) in [
            ("journal", 17 * 1024 * 1024),
            ("stderr", 2 * 1024 * 1024),
            ("final", 65 * 1024),
        ] {
            let (dir, s) = setup();
            let a = s.codex_artifacts.as_ref().unwrap();
            let target = match field {
                "journal" => a.journal_path.as_str(),
                "stderr" => a.stderr_path.as_str(),
                _ => a.final_path.as_str(),
            };
            // Build a synthetic credential at runtime so secret scanners do
            // not mistake the redaction fixture for a checked-in credential.
            let fixture_token = format!("sk-{}", "abcdefghijklmnopqrstuvwxyz1234567890");
            let extra=format!("printf '%s' 'OPENAI_API_KEY={fixture_token}' >&2\nhead -c {bytes} /dev/zero > '{target}'\nsleep 10");
            let start = std::time::Instant::now();
            let o = run_codex_bounded(
                &binary(dir.path(), &script(&extra, 0)),
                dir.path(),
                &s,
                Duration::from_secs(5),
            )
            .unwrap();
            assert!(start.elapsed() < Duration::from_secs(4), "{field}");
            assert!(!o.is_met());
            let reason = o.reason.unwrap();
            assert!(reason.contains("limit"), "{reason}");
            assert!(reason.chars().count() <= 500);
            assert!(!reason.contains(&fixture_token));
        }
    }
    #[test]
    fn truncated_journal_retains_unique_native_id_without_trusting_verdict() {
        let (_dir, s) = setup();
        let o = completed(
            &s,
            r#"{"verdict":"met","reason":"gate"}"#,
            &format!("{}{{broken", journal()),
            Some(0),
        );
        assert_eq!(o.native_session_id.as_deref(), Some(THREAD));
        assert!(!o.is_met());
        assert_eq!(o.verdict, None);
    }
    #[cfg(unix)]
    #[test]
    fn resume_never_switches_to_an_unrequested_native_thread() {
        let (dir, mut s) = setup();
        s.session = SessionMode::Resume {
            session_uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        };
        let o = run_codex_bounded(
            &binary(dir.path(), &script(&observable(""), 0)),
            dir.path(),
            &s,
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(!o.is_met());
        assert_eq!(
            o.native_session_id, None,
            "an unrequested thread must never replace the caller's native session"
        );
        assert!(o.reason.unwrap().contains("requested"));
        assert!(!read_artifacts(s.codex_artifacts.as_ref().unwrap())
            .unwrap()
            .is_met());
    }
    #[test]
    fn linked_worktree_artifacts_live_outside_the_checkout() {
        let (dir, _) = setup();
        let worktree = dir.path().join("linked");
        let mut git = git(dir.path());
        let status = git
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(self::git(dir.path())
            .args(["worktree", "add", "--detach", "-q"])
            .arg(&worktree)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let a = prepare_artifacts(&worktree, &IssueRef::new("o/r", 3)).unwrap();
        assert!(
            Path::new(&a.journal_path).starts_with(dir.path().canonicalize().unwrap().join(".git"))
        );
        assert!(!Path::new(&a.journal_path).starts_with(&worktree));
        std::fs::remove_dir_all(worktree).unwrap();
        assert!(Path::new(&a.journal_path).exists());
    }
    #[cfg(unix)]
    #[test]
    fn codex_process_git_commands_are_isolated_from_inherited_git_overrides() {
        const CHILD_MARKER: &str = "IRONMEM_CODEX_GIT_ISOLATION_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "autopilot::codex_dispatch::contract_tests::codex_process_git_commands_are_isolated_from_inherited_git_overrides", "--nocapture"])
                .env(CHILD_MARKER, "1")
                .env("GIT_DIR", "/nonexistent/hostile-git-dir")
                .env("GIT_WORK_TREE", "/nonexistent/hostile-worktree")
                .output().unwrap();
            assert!(
                output.status.success(),
                "child test failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (dir, s) = setup();
        let extra = observable("git rev-parse --show-toplevel > git-cwd || exit 8");
        let o = run_codex_bounded(
            &binary(dir.path(), &script(&extra, 0)),
            dir.path(),
            &s,
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(o.is_met(), "{o:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("git-cwd"))
                .unwrap()
                .trim(),
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
    }
    #[cfg(unix)]
    #[test]
    fn a_tool_started_after_the_last_scan_and_left_running_withholds_completion() {
        let (dir, s) = setup();
        let a = s.codex_artifacts.as_ref().unwrap();
        let bin = binary(
            dir.path(),
            &script(
                "while [ ! -f root-release ]; do sleep 0.01; done\n\
                 sh -c 'echo $$ > straggler.pid; exec sleep 5' &\n\
                 while [ ! -f straggler.pid ]; do sleep 0.01; done",
                0,
            ),
        );
        let started = Instant::now();
        let mut child = spawn_fixture_invocation(&bin, dir.path(), &s);
        let mut tracker = super::super::codex_process::ProcessTracker::new(child.id());
        // The supervisor's last scan sees only the root...
        tracker.observe().unwrap();
        std::fs::write(dir.path().join("root-release"), "release").unwrap();
        // ...which then starts a tool and exits before the next one.
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(started.elapsed() < Duration::from_secs(3), "root hung");
            std::thread::sleep(Duration::from_millis(5));
        };
        let result = finish_codex_dispatch(&mut tracker, a, &s, status, started, false, None);
        if let Some(pid) = std::fs::read_to_string(dir.path().join("straggler.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
        {
            // SAFETY: the fixture's own sleep, still alive and in its group.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let err = result.unwrap_err().to_string();
        assert!(err.contains("process group"), "{err}");
        assert!(!has_valid_exit_record(a));
    }
    #[cfg(unix)]
    #[test]
    fn post_exit_artifact_overflow_kills_the_surviving_gate_process() {
        use std::os::unix::process::CommandExt;

        let (dir, s) = setup();
        let a = s.codex_artifacts.as_ref().unwrap();
        let script = format!(
            "(touch descendant-ready; while [ ! -f gate-release ]; do sleep 0.02; done; touch descendant-survived) &\n\
             while [ ! -f root-release ]; do sleep 0.02; done\n\
             head -c {} /dev/zero > '{}'\nexit 0",
            FINAL_LIMIT + 1,
            a.final_path
        );
        let mut command = std::process::Command::new(binary(dir.path(), &script));
        command
            .current_dir(dir.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut child = crate::autopilot::dispatch::retry_on_etxtbsy(|| command.spawn()).unwrap();
        let started = Instant::now();
        let mut tracker = super::super::codex_process::ProcessTracker::new(child.id());
        let ready = dir.path().join("descendant-ready");
        while !ready.exists() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !ready.exists() {
            crate::autopilot::dispatch::kill_process_group(&mut child);
            let _ = child.wait();
            panic!("descendant did not reach its gate");
        }
        tracker.observe().unwrap();
        // This is the supervisor's pre-try_wait snapshot: still under cap.
        let previous_limit = limits(a);
        assert_eq!(previous_limit, None);
        std::fs::write(dir.path().join("root-release"), "release").unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() >= Duration::from_secs(3) {
                crate::autopilot::dispatch::kill_process_group(&mut child);
                let _ = child.wait();
                panic!("root did not finish writing the oversized artifact");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(status.success());
        assert!(limits(a).is_some());
        let outcome =
            finish_codex_dispatch(&mut tracker, a, &s, status, started, false, previous_limit)
                .unwrap();
        assert!(!outcome.is_met());
        assert!(outcome.reason.unwrap().contains("limit"));
        std::fs::write(dir.path().join("gate-release"), "release").unwrap();
        let survived = dir.path().join("descendant-survived");
        let released = Instant::now();
        while !survived.exists() && released.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Also clean up on red, before asserting, to avoid leaking fixtures.
        crate::autopilot::dispatch::kill_process_group(&mut child);
        assert!(
            !survived.exists(),
            "post-exit overflow left the gate process alive"
        );
    }
    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendant_gate_process() {
        let (dir, s) = setup();
        let bin = binary(
            dir.path(),
            &script("(sleep 1; touch descendant-survived) &\nsleep 10", 0),
        );
        let o = run_codex_bounded(&bin, dir.path(), &s, Duration::from_millis(100)).unwrap();
        assert!(o.timed_out);
        assert!(!o.is_met());
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!dir.path().join("descendant-survived").exists());
        assert!(
            read_artifacts(s.codex_artifacts.as_ref().unwrap())
                .unwrap()
                .timed_out
        );
    }
    #[cfg(unix)]
    #[test]
    fn timeout_kills_tool_in_a_separate_process_group() {
        let (dir, s) = setup();
        let python = "/usr/bin/python3";
        let extra = format!("{python} -c 'import os,time; os.setsid(); open(\"detached-ready\",\"w\").close(); time.sleep(1); open(\"detached-survived\",\"w\").close()' &\nwhile [ ! -f detached-ready ]; do sleep 0.01; done\nsleep 10");
        let bin = binary(dir.path(), &script(&extra, 0));
        let o = run_codex_bounded(&bin, dir.path(), &s, Duration::from_millis(400)).unwrap();
        assert!(o.timed_out);
        assert!(dir.path().join("detached-ready").exists());
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !dir.path().join("detached-survived").exists(),
            "escaped tool survived timeout"
        );
    }
}
