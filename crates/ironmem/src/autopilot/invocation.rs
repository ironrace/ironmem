//! Durable supervised CLI dispatch (Codex, Muse). Artifacts live in the
//! common Git directory, so a replacement controller can inspect them after
//! the checkout disappears. Each harness contributes only a [`SupervisedCli`].
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::autopilot::dispatch::{DispatchOutcome, DispatchSpec, SessionRefusal, Verdict};
use crate::autopilot::implementer::Implementer;
use crate::autopilot::process_tree::{Birth, QuietHold, SharedDaemon};
use crate::autopilot::scrub::scrub_and_bound;
use crate::autopilot::IssueRef;
use crate::error::MemoryError;

pub const VERDICT_JSON_SCHEMA: &str = super::dispatch::IC_VERDICT_JSON_SCHEMA;
pub const SUPERVISED_DISPATCH_TIMEOUT_SECS: u64 = 1200;
const JOURNAL_LIMIT: u64 = 16 * 1024 * 1024;
const STDERR_LIMIT: u64 = 1024 * 1024;
const FINAL_LIMIT: u64 = 64 * 1024;
const EXIT_LIMIT: u64 = 4096;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How often supervision rescans the process table. A `ps -axww` scan costs
/// tens of milliseconds of CPU on a busy machine, so scanning on every exit
/// poll kept a core busy for a whole dispatch. Exit and artifact limits stay
/// on [`POLL_INTERVAL`]; descendants enrol at this pace, and cleanup rescans
/// the frozen tree until it is stable. A process that lives less than one
/// interval is never enrolled, and anything it leaves running is reparented
/// to init, out of every tracked ancestry. Completion therefore also
/// requires the root's process group to drain (Codex's tools stay in it) and
/// the worktree to hold no process started during the invocation (Muse's
/// tools each lead their own session, so the group check never sees them);
/// see `ProcessTracker::verify_worktree_quiet` for what that cannot catch.
const OBSERVE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationArtifacts {
    pub invocation_id: String,
    pub schema_path: String,
    pub journal_path: String,
    pub final_path: String,
    pub stderr_path: String,
    pub exit_path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InvocationRecovery {
    NoPriorInvocation,
    Alive,
    Unknown { reason: String },
    Finished(DispatchOutcome),
}

/// What one harness contributes to a supervised invocation. Everything else —
/// artifacts, exit/completion records, the process tracker, limits — is shared.
pub trait SupervisedCli: Sync {
    fn implementer(&self) -> Implementer;

    /// "Codex" / "Muse", for operator-facing messages.
    fn label(&self) -> &'static str {
        self.implementer().label()
    }

    /// The exact argv after the binary. Must fail on any invalid spec before
    /// state is written or spend is reserved.
    fn argv(
        &self,
        repo: &Path,
        spec: &DispatchSpec,
        a: &InvocationArtifacts,
    ) -> Result<Vec<String>, MemoryError>;

    /// Files [`SupervisedCli::prepare_spawn`] creates. Any of them already
    /// present proves a prior spawn was attempted into this invocation, so
    /// the core refuses before writing. The list must name exactly the files
    /// `prepare_spawn` creates, no more and no fewer. Default: none.
    fn spawn_files(&self, _a: &InvocationArtifacts) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Files the harness reads at spawn (Muse's prompt file). Default: none.
    fn prepare_spawn(
        &self,
        _spec: &DispatchSpec,
        _a: &InvocationArtifacts,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    /// Extra child environment. Default: none.
    fn env(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// The native session ID the run must report, when the controller chose it.
    fn expected_native_id(&self, spec: &DispatchSpec) -> Option<String>;

    /// Whether [`SupervisedCli::parse`] itself reports a journal that never
    /// names `expected_native_id`. The core then adds no generic mismatch of
    /// its own when the parse found no native ID and already reported why.
    /// Default: no.
    fn parse_reports_missing_session(&self) -> bool {
        false
    }

    /// Whether the harness refused to run on `expected_native_id` at all, from
    /// its bounded raw stderr. Asked only of a run with no valid verdict that
    /// launched, exited non-zero and was not timed out. Default: never.
    fn session_refusal(
        &self,
        _stderr: &str,
        _expected_native_id: Option<&str>,
    ) -> Option<SessionRefusal> {
        None
    }

    /// Parse the bounded journal and final-file text into evidence.
    /// `expected_native_id` is the exit record's `expected_native_id`.
    fn parse(
        &self,
        journal: &str,
        final_text: Result<String, MemoryError>,
        expected_native_id: Option<&str>,
    ) -> ParsedRun;
}

/// Harness-parsed evidence; [`read_artifacts`] combines it with the exit
/// record. It has exactly two shapes, one per constructor: a verdict with its
/// native session ID and no errors, or a failure with no verdict. A verdict
/// alongside an error, or without a native ID, cannot be built.
#[derive(Debug)]
pub struct ParsedRun {
    native_id: Option<String>,
    verdict: Option<FinalVerdict>,
    turns: u32,
    errors: Vec<String>,
    observed_output: bool,
}

impl ParsedRun {
    /// A run that produced its schema-shaped verdict on `native_id` and
    /// nothing wrong. `observed_output`: the journal or final file held
    /// any bytes at all.
    pub fn with_verdict(
        native_id: String,
        verdict: FinalVerdict,
        turns: u32,
        observed_output: bool,
    ) -> Self {
        Self {
            native_id: Some(native_id),
            verdict: Some(verdict),
            turns,
            errors: Vec::new(),
            observed_output,
        }
    }

    /// A run with no trusted verdict. `errors` says why; when it is empty
    /// the core reports the missing verdict (and native ID) itself.
    /// `native_id` is kept so a replacement controller can resume it.
    pub fn failed(
        errors: Vec<String>,
        native_id: Option<String>,
        turns: u32,
        observed_output: bool,
    ) -> Self {
        Self {
            native_id,
            verdict: None,
            turns,
            errors,
            observed_output,
        }
    }

    /// [`Self::with_verdict`] when `errors` is empty and both the verdict and the
    /// native ID are present, otherwise [`Self::failed`], dropping the
    /// verdict. For a parser that gathers evidence as it goes. When it
    /// reported nothing wrong, the failure names each missing piece under
    /// `label` ("Codex", "Muse").
    pub fn from_evidence(
        label: &str,
        mut errors: Vec<String>,
        native_id: Option<String>,
        verdict: Option<FinalVerdict>,
        turns: u32,
        observed_output: bool,
    ) -> Self {
        match (verdict, native_id) {
            (Some(verdict), Some(native_id)) if errors.is_empty() => {
                Self::with_verdict(native_id, verdict, turns, observed_output)
            }
            (verdict, native_id) => {
                if errors.is_empty() {
                    if verdict.is_none() {
                        errors.push(format!("{label} reported no final verdict"));
                    }
                    if native_id.is_none() {
                        errors.push(format!("{label} reported no native session ID"));
                    }
                }
                Self::failed(errors, native_id, turns, observed_output)
            }
        }
    }

    pub fn native_id(&self) -> Option<&str> {
        self.native_id.as_deref()
    }
    pub fn verdict(&self) -> Option<&FinalVerdict> {
        self.verdict.as_ref()
    }
    pub fn turns(&self) -> u32 {
        self.turns
    }
    pub fn errors(&self) -> &[String] {
        &self.errors
    }
    pub fn observed_output(&self) -> bool {
        self.observed_output
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalVerdict {
    pub verdict: Verdict,
    pub reason: String,
}

/// The CLI for a recovering harness; Claude is not supervised here.
pub fn cli_for(implementer: Implementer) -> Result<&'static dyn SupervisedCli, MemoryError> {
    match implementer {
        Implementer::Codex => Ok(&super::codex_dispatch::CodexCli),
        Implementer::Muse => Ok(&super::muse_dispatch::MuseCli),
        Implementer::Claude => Err(validation(
            "the Claude implementer is not a supervised CLI invocation",
        )),
    }
}

pub(crate) fn validation(message: impl Into<String>) -> MemoryError {
    MemoryError::Validation(message.into())
}

fn supported_path(path: &Path) -> Result<&str, MemoryError> {
    let text = path.to_str().ok_or_else(|| {
        validation("unsupported invocation artifact path: use a UTF-8 path without whitespace")
    })?;
    if !path.is_absolute() || text.chars().any(char::is_whitespace) {
        return Err(validation("unsupported invocation artifact path: move the repository to an absolute path without whitespace; process identity recovery cannot parse whitespace"));
    }
    // The process table is decoded lossily, so U+FFFD in a marker could match
    // another process's undecodable argv bytes.
    if text.contains(char::REPLACEMENT_CHARACTER) {
        return Err(validation("unsupported invocation artifact path: move the repository to a path without U+FFFD; process identity recovery decodes undecodable argv bytes to it"));
    }
    Ok(text)
}

pub(crate) fn create_private(path: &Path) -> Result<File, MemoryError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

/// Remove an invocation directory that [`prepare_artifacts`] made but nothing
/// launched from, and that no stored state names. Best effort: a directory
/// left behind holds no process and is never read again.
pub(crate) fn discard_unlaunched(a: &InvocationArtifacts) {
    if let Some(dir) = Path::new(&a.schema_path).parent() {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "autopilot could not remove an unlaunched invocation directory"
            );
        }
    }
}

/// Allocate once, then persist these exact paths before spawning the dispatch.
/// Reject unsupported paths before creating any artifact directories or files.
pub fn prepare_artifacts(
    worktree: &Path,
    issue: &IssueRef,
) -> Result<InvocationArtifacts, MemoryError> {
    let mut git = Command::new("git");
    git.args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(worktree);
    crate::mcp::tools::scrub_git_environment(&mut git);
    let output = git.output()?;
    if !output.status.success() {
        return Err(validation(format!(
            "cannot resolve common Git directory for invocation artifacts: {}",
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
    let a = InvocationArtifacts {
        invocation_id,
        schema_path: supported_path(&dir.join("schema.json"))?.into(),
        journal_path: supported_path(&dir.join("journal.jsonl"))?.into(),
        final_path: supported_path(&dir.join("final.json"))?.into(),
        stderr_path: supported_path(&dir.join("stderr.log"))?.into(),
        exit_path: supported_path(&dir.join("exit.json"))?.into(),
    };
    let mut schema = create_private(Path::new(&a.schema_path))?;
    schema.write_all(VERDICT_JSON_SCHEMA.as_bytes())?;
    schema.sync_all()?;
    for path in [&a.journal_path, &a.final_path, &a.stderr_path, &a.exit_path] {
        create_private(Path::new(path))?;
    }
    Ok(a)
}

pub(crate) fn validate_spec<'a>(
    cli: &dyn SupervisedCli,
    spec: &'a DispatchSpec,
) -> Result<&'a InvocationArtifacts, MemoryError> {
    if spec.implementer != cli.implementer() {
        return Err(validation(format!(
            "{} dispatch requires implementer {} and persisted invocation artifacts",
            cli.label(),
            cli.implementer().cli_name()
        )));
    }
    let a = spec.invocation_artifacts.as_ref().ok_or_else(|| {
        validation(format!(
            "{} dispatch requires persisted invocation artifacts; prepare and save them before spawning",
            cli.label()
        ))
    })?;
    for path in [
        &a.schema_path,
        &a.journal_path,
        &a.final_path,
        &a.stderr_path,
        &a.exit_path,
    ] {
        supported_path(Path::new(path))?;
    }
    Ok(a)
}

/// The exact backend argv for `spec`, refusing any invalid input before a
/// caller publishes an invocation or reserves its spend.
pub fn validate_argv(repo: &Path, spec: &DispatchSpec) -> Result<Vec<String>, MemoryError> {
    let cli = cli_for(spec.implementer)?;
    let a = validate_spec(cli, spec)?;
    cli.argv(repo, spec, a)
}

/// [`validate_argv`] before any artifact exists, so a definite input error
/// (model, workspace, session, step cap) leaves no invocation directory
/// behind. Placeholder artifacts stand in for [`prepare_artifacts`]'s; the
/// caller still validates the exact argv once the real ones exist.
pub(crate) fn validate_argv_before_artifacts(
    repo: &Path,
    spec: &DispatchSpec,
) -> Result<(), MemoryError> {
    let dir = "/ironmem-argv-check";
    let mut probe = spec.clone();
    probe.invocation_artifacts = Some(InvocationArtifacts {
        invocation_id: "00000000-0000-4000-8000-000000000000".into(),
        schema_path: format!("{dir}/schema.json"),
        journal_path: format!("{dir}/journal.jsonl"),
        final_path: format!("{dir}/final.json"),
        stderr_path: format!("{dir}/stderr.log"),
        exit_path: format!("{dir}/exit.json"),
    });
    validate_argv(repo, &probe).map(|_| ())
}

pub(crate) fn dispatch_timeout(spec: &DispatchSpec) -> Duration {
    spec.wall_clock_timeout
        .unwrap_or(Duration::from_secs(SUPERVISED_DISPATCH_TIMEOUT_SECS))
}

/// `shared_daemon` is the operator's shared ironmem daemon: its configured
/// socket (`Config::daemon_socket_path`) and executable. A daemon a
/// harness's MCP proxy autospawns there is never treated as part of the
/// invocation. `None` owns every descendant.
pub fn run_supervised(
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
    shared_daemon: Option<&SharedDaemon>,
) -> Result<DispatchOutcome, MemoryError> {
    run_supervised_bounded_with(
        cli_for(spec.implementer)?,
        bin,
        repo,
        spec,
        dispatch_timeout(spec),
        shared_daemon,
    )
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
    /// Set instead of `completion` when cleanup and the group drain
    /// succeeded but processes started during the invocation still ran in
    /// the worktree. Recovery moves it to `completion` once the recorded tree
    /// is gone and the worktree is quiet ([`settle_awaiting_quiet`]).
    /// Omitted otherwise, keeping every other record byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    awaiting_quiet: Option<AwaitingQuiet>,
}

/// The completion an awaiting-quiet record holds back, with what the
/// supervisor found, so recovery decides on that evidence rather than on
/// whatever a later scan by whoever asks happens to see.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AwaitingQuiet {
    completion: CompletionRecord,
    /// The effective user ID the supervisor scanned as.
    uid: u32,
    /// The canonical worktree it scanned.
    worktree: PathBuf,
    /// What it found there, by PID and kernel birth.
    stragglers: Vec<Straggler>,
    /// Set when the scan itself kept failing. `stragglers` then holds only
    /// the in-window process whose unreadable directory failed it, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan_failed: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Straggler {
    pid: u32,
    birth: Birth,
}

/// Hold `record`'s completion back until recovery sees `hold` resolved.
fn await_quiet(record: &mut ExitRecord, hold: &QuietHold) {
    record.awaiting_quiet = record.completion.take().map(|completion| AwaitingQuiet {
        completion,
        uid: hold.uid,
        worktree: hold.worktree.clone(),
        stragglers: hold
            .stragglers
            .iter()
            .map(|&(pid, birth)| Straggler { pid, birth })
            .collect(),
        scan_failed: hold.scan_failed.clone(),
    });
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionRecord {
    version: u32,
    invocation_id: String,
    schema_path: String,
}
impl CompletionRecord {
    fn names(&self, a: &InvocationArtifacts) -> bool {
        self.version == 1
            && self.invocation_id == a.invocation_id
            && self.schema_path == a.schema_path
    }
}
impl ExitRecord {
    fn proves_completion(&self, a: &InvocationArtifacts) -> bool {
        self.completion.as_ref().is_some_and(|c| c.names(a))
    }
    fn awaits_quiet(&self, a: &InvocationArtifacts) -> bool {
        self.completion.is_none()
            && self
                .awaiting_quiet
                .as_ref()
                .is_some_and(|w| w.completion.names(a))
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
        return Err(validation("invocation artifacts must be regular files"));
    }
    Ok(file)
}

fn limits(a: &InvocationArtifacts) -> Option<String> {
    [
        (&a.journal_path, JOURNAL_LIMIT, "journal"),
        (&a.stderr_path, STDERR_LIMIT, "stderr"),
        (&a.final_path, FINAL_LIMIT, "final"),
    ]
    .into_iter()
    .find_map(|(path, limit, name)| match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= limit => None,
        _ => Some(format!(
            "invocation {name} artifact exceeded its {limit}-byte limit or is unavailable"
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
                .ok_or_else(|| validation("invocation artifact requires a parent directory"))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

fn persist_exit(a: &InvocationArtifacts, record: &ExitRecord) -> Result<(), MemoryError> {
    persist_private_atomic(
        Path::new(&a.exit_path),
        "exit",
        &serde_json::to_vec(record)?,
    )
}

/// The supervisor's terminal record for one invocation. `expected_native_id`
/// is the harness's [`SupervisedCli::expected_native_id`] for the spec.
fn exit_record(
    a: &InvocationArtifacts,
    expected_native_id: Option<String>,
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
        expected_native_id,
        launch_failure,
        awaiting_quiet: None,
    }
}

/// Tests' entry point: no shared-daemon socket, so every descendant is owned.
#[cfg(test)]
pub(crate) fn run_supervised_bounded(
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
    timeout: Duration,
) -> Result<DispatchOutcome, MemoryError> {
    run_supervised_bounded_with(cli_for(spec.implementer)?, bin, repo, spec, timeout, None)
}

/// Every invocation has a wall-clock ceiling. stdout/stderr are inherited
/// regular file handles, so no pipe can wedge the child or die with its parent.
fn run_supervised_bounded_with(
    cli: &dyn SupervisedCli,
    bin: &Path,
    repo: &Path,
    spec: &DispatchSpec,
    timeout: Duration,
    shared_daemon: Option<&SharedDaemon>,
) -> Result<DispatchOutcome, MemoryError> {
    let a = validate_spec(cli, spec)?;
    let args = cli.argv(repo, spec, a)?;
    // Never overwrite evidence from a prior invocation. A retry must allocate
    // a new invocation directory, even when it resumes the same native thread.
    for path in [&a.journal_path, &a.stderr_path, &a.final_path, &a.exit_path] {
        if open_existing(path, false)?.metadata()?.len() != 0 {
            return Err(validation(format!(
                "{} artifacts already contain invocation data; prepare a new invocation before retrying",
                cli.label()
            )));
        }
    }
    // A killed child can leave every output empty. Its identity file still
    // proves a child ran, and the harness's spawn files prove a spawn was at
    // least attempted. Either way a never-launched record could mask a live
    // invocation, so refuse with an error instead of writing one.
    for path in std::iter::once(super::process_tree::identity_path(a)).chain(cli.spawn_files(a)) {
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(validation(format!(
                    "cannot stat {} to rule out a prior {} spawn: {e}",
                    path.display(),
                    cli.label()
                )))
            }
            Ok(_) => {
                return Err(validation(format!(
                    "{} invocation already holds {} from a prior spawn; prepare a new invocation before retrying",
                    cli.label(),
                    path.display()
                )))
            }
        }
    }
    let schema = read_capped(&a.schema_path, FINAL_LIMIT)?;
    if schema != VERDICT_JSON_SCHEMA {
        return Err(validation(format!(
            "{} artifacts must contain the strict verdict schema",
            cli.label()
        )));
    }
    // Nothing has spawned yet and the prior-evidence check passed, so a
    // failed pre-spawn write is recorded as proof that no child existed.
    if let Err(e) = cli.prepare_spawn(spec, a) {
        return never_launched(
            cli,
            a,
            spec,
            &format!("failed to prepare {} dispatch: {e}", cli.label()),
        );
    }
    // Still before any spawn and after the same check, so a failed open is
    // recorded the same way rather than held as ambiguous.
    let outputs = open_existing(&a.journal_path, true)
        .and_then(|journal| Ok((journal, open_existing(&a.stderr_path, true)?)));
    let (journal, stderr) = match outputs {
        Ok(outputs) => outputs,
        Err(e) => {
            return never_launched(
                cli,
                a,
                spec,
                &format!("failed to open {} output artifacts: {e}", cli.label()),
            )
        }
    };
    let mut command = Command::new(bin);
    command
        .args(args)
        .envs(cli.env().iter().copied())
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::from(journal))
        .stderr(Stdio::from(stderr));
    // Git subprocesses inside the harness must resolve this invocation's
    // checkout, rather than a controller's inherited repository-selection
    // overrides.
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
        Err(e) => {
            return never_launched(
                cli,
                a,
                spec,
                &format!("failed to launch {} dispatch: {e}", cli.label()),
            )
        }
    };
    let started = Instant::now();
    let mut identity = super::process_tree::ProcessIdentity {
        version: 1,
        root_pid: child.id(),
        root_pgid: child.id(),
        root_birth: None,
        schema_path: a.schema_path.clone(),
        invocation_id: a.invocation_id.clone(),
        descendants: Vec::new(),
    };
    let mut tracker = super::process_tree::ProcessTracker::new(child.id(), shared_daemon);
    if let Err(e) = super::process_tree::persist_identity(a, &identity) {
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
            // Record the root's birth and the live descendant set as soon as
            // either changes: a successor of a crashed controller, and
            // `clear-dispatch`, see only what the identity file holds, and
            // Muse's tools all leave the root's group. A failed write is
            // handled like the post-spawn one.
            let observed = tracker
                .observe()
                .and_then(|_| record_identity(a, &mut identity, &tracker));
            if let Err(e) = observed {
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
    if let Err(e) = record_identity(a, &mut identity, &tracker) {
        let cleanup = tracker.cleanup(true);
        return Err(with_cleanup(e, cleanup));
    }
    finish_supervised(
        cli,
        &mut tracker,
        repo,
        a,
        spec,
        status,
        started,
        timed_out,
        limit_exceeded,
        super::process_tree::QUIET_GRACE,
    )
}

/// Persist `identity` with the tracker's current root birth and live
/// descendants, only when they changed.
fn record_identity(
    a: &InvocationArtifacts,
    identity: &mut super::process_tree::ProcessIdentity,
    tracker: &super::process_tree::ProcessTracker,
) -> Result<(), MemoryError> {
    let (root_birth, descendants) = tracker.recorded();
    if root_birth == identity.root_birth && descendants == identity.descendants {
        return Ok(());
    }
    identity.root_birth = root_birth;
    identity.descendants = descendants;
    super::process_tree::persist_identity(a, identity)
}

fn never_launched(
    cli: &dyn SupervisedCli,
    a: &InvocationArtifacts,
    spec: &DispatchSpec,
    reason: &str,
) -> Result<DispatchOutcome, MemoryError> {
    let record = exit_record(
        a,
        cli.expected_native_id(spec),
        None,
        false,
        None,
        0,
        Some(reason.to_owned()),
    );
    persist_exit(a, &record).map_err(|e| {
        MemoryError::NotFound(format!(
            "{reason}; recording that it never launched also failed: {e}"
        ))
    })?;
    read_artifacts_with(cli, a)
}

/// A supervisor record proving `spawn` failed, so no process ever ran.
pub fn has_launch_failure_record(artifacts: &InvocationArtifacts) -> bool {
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
#[allow(clippy::too_many_arguments)]
fn finish_supervised(
    cli: &dyn SupervisedCli,
    tracker: &mut super::process_tree::ProcessTracker,
    repo: &Path,
    a: &InvocationArtifacts,
    spec: &DispatchSpec,
    status: std::process::ExitStatus,
    started: Instant,
    timed_out: bool,
    limit_exceeded: Option<String>,
    quiet_grace: Duration,
) -> Result<DispatchOutcome, MemoryError> {
    // Catch an artifact that crossed the limit just before process exit.
    let limit_exceeded = limit_exceeded.or_else(|| limits(a));
    // Success included: a root gone before the first sample could have
    // started a tool that left its group, and nothing tracked it.
    if !tracker.observed_root() {
        return Err(validation(format!(
            "{} root exited before process-tree observation; descendant completion is unproven",
            cli.label()
        )));
    }
    // Even a successful root can leave a gate in a separate process group.
    // Unknown cleanup returns Err so run.rs retains the invocation and slot.
    tracker.cleanup(true)?;
    tracker.verify_group_drained()?;
    let quiet = tracker.verify_worktree_quiet(repo, quiet_grace)?;
    let mut record = exit_record(
        a,
        cli.expected_native_id(spec),
        status.code(),
        timed_out,
        limit_exceeded,
        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        None,
    );
    if let super::process_tree::Quiet::Held(hold) = quiet {
        // Everything else is proven, so record the outcome without trusting
        // it, with what held it. The slot stays held; recovery finishes it
        // once that is resolved.
        await_quiet(&mut record, &hold);
        persist_exit(a, &record)?;
        return Err(validation(format!(
            "{} after cleanup; {} completion waits until the worktree is quiet",
            hold.reason,
            cli.label()
        )));
    }
    persist_exit(a, &record)?;
    let mut outcome = read_artifacts_with(cli, a)?;
    outcome.invocation_started = true;
    Ok(outcome)
}

fn read_capped_bytes(path: &str, cap: u64) -> Result<Vec<u8>, MemoryError> {
    let file = open_existing(path, false)?;
    let mut data = Vec::new();
    file.take(cap + 1).read_to_end(&mut data)?;
    if data.len() as u64 > cap {
        return Err(validation(format!(
            "invocation artifact exceeds {cap}-byte limit"
        )));
    }
    Ok(data)
}

fn read_capped(path: &str, cap: u64) -> Result<String, MemoryError> {
    String::from_utf8(read_capped_bytes(path, cap)?)
        .map_err(|_| validation("invocation artifact is not UTF-8"))
}

/// Parse bounded durable evidence. A final file alone never proves success:
/// only the supervisor's valid exit-zero record can authorize a met verdict.
/// A truncated journal invalidates the verdict but retains an earlier unique
/// native session ID so a replacement controller can resume it.
pub(crate) fn read_artifacts(
    implementer: Implementer,
    a: &InvocationArtifacts,
) -> Result<DispatchOutcome, MemoryError> {
    read_artifacts_with(cli_for(implementer)?, a)
}

fn read_artifacts_with(
    cli: &dyn SupervisedCli,
    a: &InvocationArtifacts,
) -> Result<DispatchOutcome, MemoryError> {
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
    let mut never_started = false;
    let exit = read_capped(&a.exit_path, EXIT_LIMIT).and_then(|text| {
        serde_json::from_str::<ExitRecord>(&text).map_err(|e| {
            validation(format!(
                "missing or invalid {} supervisor exit record: {e}",
                cli.label()
            ))
        })
    });
    let journal = match read_capped(&a.journal_path, JOURNAL_LIMIT) {
        Ok(journal) => journal,
        Err(e) => {
            errors.push(e.to_string());
            String::new()
        }
    };
    let expected_native_id = exit
        .as_ref()
        .ok()
        .and_then(|record| record.expected_native_id.as_deref());
    let parsed = cli.parse(
        &journal,
        read_capped(&a.final_path, FINAL_LIMIT),
        expected_native_id,
    );
    // Only a `ParsedRun::failed` with no errors reaches these: say what is
    // missing rather than fail with no reason. Codex and Muse always give one.
    if parsed.errors.is_empty() {
        if parsed.verdict.is_none() {
            errors.push(format!("{} reported no final verdict", cli.label()));
        }
        if parsed.native_id.is_none() {
            errors.push(format!("{} reported no native session ID", cli.label()));
        }
    }
    let missing_session_reported = cli.parse_reports_missing_session()
        && parsed.native_id.is_none()
        && !parsed.errors.is_empty();
    errors.extend(parsed.errors);
    let mut native_id = parsed.native_id;
    let mut observed_output = parsed.observed_output;
    let (process_success, timed_out, duration_ms) = match &exit {
        Ok(record) => {
            if !record.proves_completion(a) {
                errors.push(format!(
                    "untrusted {} supervisor completion evidence",
                    cli.label()
                ));
            } else if let Some(reason) = &record.launch_failure {
                never_started = true;
                errors.push(format!("{} never launched: {reason}", cli.label()));
            }
            if record
                .expected_native_id
                .as_ref()
                .is_some_and(|expected| native_id.as_ref() != Some(expected))
            {
                if !missing_session_reported {
                    errors.push(format!(
                        "{} native session ID did not match the requested session",
                        cli.label()
                    ));
                }
                native_id = None;
            }
            if let Some(limit) = &record.limit_exceeded {
                errors.push(limit.clone());
            }
            if record.timed_out {
                errors.push(format!(
                    "{} dispatch exceeded wall-clock timeout and was killed",
                    cli.label()
                ));
            }
            if record.exit_code != Some(0) {
                errors.push(format!(
                    "{} process exited with {:?}",
                    cli.label(),
                    record.exit_code
                ));
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
    // No errors implies a verdict (checked above), so this only ever drops a
    // verdict that the errors already invalidate.
    let final_verdict = parsed.verdict.filter(|_| errors.is_empty());
    let valid = final_verdict.is_some();
    // From the raw stderr file, not the bounded reason built below: a long
    // preamble must not push the harness's own refusal out of view.
    let session_refusal = (!valid && !process_success && !timed_out && !never_started)
        .then(|| cli.session_refusal(&stderr, expected_native_id))
        .flatten();
    let (verdict, reason) = match final_verdict {
        Some(final_verdict) => (Some(final_verdict.verdict), Some(final_verdict.reason)),
        None => {
            if !stderr.trim().is_empty() {
                errors.push(format!("stderr: {}", stderr.trim()));
            }
            if let Some(e) = stderr_error {
                errors.push(format!("stderr unreadable: {e}"));
            }
            (None, Some(scrub_and_bound(&errors.join("; "), 500).text))
        }
    };
    Ok(DispatchOutcome {
        cost_usd: None,
        num_turns: parsed.turns,
        duration_ms,
        is_error: !valid,
        native_session_id: native_id,
        verdict,
        reason,
        process_success,
        timed_out,
        invocation_started: (exit.is_ok() || observed_output) && !never_started,
        session_refusal,
    })
}

/// A typed supervisor record proves that wait observed process completion.
/// A missing, truncated or malformed record does not establish liveness.
pub fn has_valid_exit_record(artifacts: &InvocationArtifacts) -> bool {
    read_capped(&artifacts.exit_path, EXIT_LIMIT)
        .ok()
        .and_then(|text| serde_json::from_str::<ExitRecord>(&text).ok())
        .is_some_and(|record| record.proves_completion(artifacts))
}

/// A supervisor record that proved everything but a quiet worktree.
pub fn is_awaiting_quiet(artifacts: &InvocationArtifacts) -> bool {
    read_capped(&artifacts.exit_path, EXIT_LIMIT)
        .ok()
        .and_then(|text| serde_json::from_str::<ExitRecord>(&text).ok())
        .is_some_and(|record| record.awaits_quiet(artifacts))
}

/// Finish an invocation whose supervisor refused completion only because its
/// worktree was not quiet, from what that supervisor recorded. All must hold:
///
/// - this check runs as the user the supervisor scanned as (another user
///   cannot see that user's processes);
/// - each recorded straggler is gone by kernel birth, even one that has since
///   left the worktree;
/// - the root and every recorded descendant are gone by kernel birth;
/// - nothing remains in the root's group, and no process carries this
///   invocation's marker (or an ambiguous one);
/// - a fresh scan of the recorded worktree is quiet.
///
/// A kernel read that fails holds; only "no such process" or a different
/// birth under the PID is gone. `kernel` reads a birth (`kernel_birth`).
/// `Ok(true)`: upgraded to a trusted completion. `Ok(false)`: not such an
/// invocation. `Err`: still held, with what holds it.
pub(crate) fn settle_awaiting_quiet(
    a: &InvocationArtifacts,
    snapshot: &super::process_tree::ProcessSnapshot,
    daemon: Option<&SharedDaemon>,
    mut kernel: impl FnMut(u32) -> Result<Option<Birth>, String>,
) -> Result<bool, String> {
    use super::process_tree::{current_uid, read_identity, ProcessSnapshot, Quiet, QuietCheck};
    let Some(mut record) = read_capped(&a.exit_path, EXIT_LIMIT)
        .ok()
        .and_then(|text| serde_json::from_str::<ExitRecord>(&text).ok())
        .filter(|record| record.awaits_quiet(a))
    else {
        return Ok(false);
    };
    let Some(awaiting) = record.awaiting_quiet.as_ref() else {
        return Ok(false);
    };
    let uid = current_uid().ok_or("process ownership is unreadable on this platform")?;
    if uid != awaiting.uid {
        return Err(format!(
            "this check runs as uid {uid}, but the invocation's worktree was scanned as uid \
             {}; only that user can see its processes",
            awaiting.uid
        ));
    }
    let mut still_running = |pid: u32, birth: Birth, what: &str| match kernel(pid) {
        Ok(Some(actual)) if actual == birth => Err(format!("{what} {pid} is still running")),
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "cannot tell whether {what} {pid} is still running: {e}"
        )),
    };
    for straggler in &awaiting.stragglers {
        still_running(straggler.pid, straggler.birth, "process")?;
    }
    let ProcessSnapshot::Available(rows) = snapshot else {
        return Err(snapshot.reason());
    };
    let identity = read_identity(a)?;
    let root_birth = identity
        .root_birth
        .ok_or("the invocation root's kernel birth was never recorded")?;
    still_running(identity.root_pid, root_birth, "the invocation root")?;
    let active = || rows.iter().filter(|r| r.active());
    for d in &identity.descendants {
        match d.birth {
            Some(birth) => still_running(d.pid, birth, "recorded process")?,
            None if active().any(|r| r.pid == d.pid) => {
                return Err(format!("recorded process {} is still running", d.pid))
            }
            None => {}
        }
    }
    if let Some(r) = active().find(|r| r.pgid == identity.root_pgid) {
        return Err(format!(
            "process {} is in the invocation's process group {}",
            r.pid, r.pgid
        ));
    }
    for r in active() {
        match r.marker() {
            Err(reason) => {
                return Err(format!(
                    "process {} is an ambiguous Codex or Muse command: {reason}",
                    r.pid
                ))
            }
            Ok(Some(marker)) if marker == a.schema_path => {
                return Err(format!(
                    "process {} carries this invocation's marker",
                    r.pid
                ))
            }
            _ => {}
        }
    }
    if let Quiet::Held(hold) = QuietCheck::once(&awaiting.worktree, root_birth, daemon, rows)? {
        return Err(hold.reason);
    }
    record.completion = record.awaiting_quiet.take().map(|w| w.completion);
    persist_exit(a, &record).map_err(|e| e.to_string())?;
    Ok(true)
}

/// The caller MUST first confirm the invocation is dead. This file-only
/// primitive cannot establish liveness; registry callers supply Alive/Unknown.
/// Prepared, empty artifacts carry invocation_started=false and no native ID.
pub fn recover_artifacts(
    implementer: Implementer,
    artifacts: &InvocationArtifacts,
) -> Result<InvocationRecovery, MemoryError> {
    Ok(InvocationRecovery::Finished(read_artifacts(
        implementer,
        artifacts,
    )?))
}

#[cfg(test)]
mod tests {
    use super::contract_tests::THREAD;
    use super::*;
    use crate::autopilot::dispatch::{SessionMode, Verdict};

    #[test]
    fn claude_has_no_supervised_cli() {
        let err = cli_for(Implementer::Claude).err().unwrap().to_string();
        assert!(err.contains("Claude"), "got: {err}");
        assert_eq!(
            cli_for(Implementer::Codex).unwrap().implementer(),
            Implementer::Codex
        );
    }

    #[test]
    fn muse_has_a_supervised_cli() {
        let muse = cli_for(Implementer::Muse).unwrap();
        assert_eq!(muse.implementer(), Implementer::Muse);
        assert_eq!(muse.label(), "Muse");
    }

    #[test]
    fn codex_expected_native_id_is_set_only_on_resume() {
        let codex = cli_for(Implementer::Codex).unwrap();
        let mut spec = crate::autopilot::codex_dispatch::tests::spec(SessionMode::New {
            session_uuid: String::new(),
        });
        assert_eq!(codex.expected_native_id(&spec), None);
        spec.session = SessionMode::Resume {
            session_uuid: "11111111-2222-3333-4444-555555555555".into(),
        };
        assert_eq!(
            codex.expected_native_id(&spec).as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }

    /// Parsed evidence has two shapes: a verdict with its native session ID
    /// and nothing wrong, or a failure that carries no verdict. No other
    /// combination can be built.
    #[test]
    fn parsed_evidence_is_a_verdict_or_a_failure_never_both() {
        let ok = ParsedRun::with_verdict(
            THREAD.into(),
            FinalVerdict {
                verdict: Verdict::Met,
                reason: "gate".into(),
            },
            3,
            true,
        );
        assert!(ok.errors().is_empty());
        assert_eq!(ok.native_id(), Some(THREAD));
        assert_eq!(ok.verdict().map(|v| v.verdict), Some(Verdict::Met));
        assert_eq!((ok.turns(), ok.observed_output()), (3, true));
        let failed = ParsedRun::failed(vec!["broken".into()], Some(THREAD.into()), 2, false);
        assert!(failed.verdict().is_none());
        assert_eq!(failed.errors(), ["broken"]);
        assert_eq!(failed.native_id(), Some(THREAD));
        assert_eq!((failed.turns(), failed.observed_output()), (2, false));
    }

    /// Evidence gathered with nothing wrong reported but a piece missing
    /// says which piece, not a generic "no final verdict".
    #[test]
    fn evidence_missing_a_piece_names_that_piece() {
        let met = || FinalVerdict {
            verdict: Verdict::Met,
            reason: "gate".into(),
        };
        let no_id = ParsedRun::from_evidence("Fake", Vec::new(), None, Some(met()), 1, true);
        assert!(no_id.verdict().is_none());
        assert_eq!(no_id.errors(), ["Fake reported no native session ID"]);
        let no_verdict =
            ParsedRun::from_evidence("Fake", Vec::new(), Some(THREAD.into()), None, 1, true);
        assert_eq!(no_verdict.errors(), ["Fake reported no final verdict"]);
        assert_eq!(no_verdict.native_id(), Some(THREAD));
        let neither = ParsedRun::from_evidence("Fake", Vec::new(), None, None, 1, true);
        assert_eq!(
            neither.errors(),
            [
                "Fake reported no final verdict",
                "Fake reported no native session ID"
            ]
        );
        // Errors the parser already gave are kept as they are.
        let explained =
            ParsedRun::from_evidence("Fake", vec!["broken".into()], None, Some(met()), 1, true);
        assert_eq!(explained.errors(), ["broken"]);
    }

    #[test]
    fn read_artifacts_routes_parsing_by_implementer() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = crate::autopilot::process_tree::testing::invocation(dir.path());
        // A Codex journal read as Claude is refused rather than parsed.
        assert!(read_artifacts(Implementer::Claude, &a).is_err());
        assert!(read_artifacts(Implementer::Codex, &a).is_ok());
    }
}

#[cfg(test)]
pub(crate) mod contract_tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::autopilot::codex_dispatch::CodexCli;
    use crate::autopilot::dispatch::{DispatchSpec, SessionMode, Verdict};
    use crate::autopilot::implementer::Implementer;
    use crate::autopilot::IssueRef;

    pub(crate) const THREAD: &str = "11111111-2222-3333-4444-555555555555";
    fn git(repo: &Path) -> std::process::Command {
        let mut command = std::process::Command::new("git");
        command.current_dir(repo);
        crate::mcp::tools::scrub_git_environment(&mut command);
        command
    }
    pub(crate) fn setup() -> (tempfile::TempDir, DispatchSpec) {
        let dir = tempfile::tempdir().unwrap();
        let status = git(dir.path()).args(["init", "-q"]).status().unwrap();
        assert!(status.success());
        let artifacts = prepare_artifacts(dir.path(), &IssueRef::new("owner/repo", 42)).unwrap();
        let spec = DispatchSpec {
            implementer: Implementer::Codex,
            invocation_artifacts: Some(artifacts),
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
    pub(crate) fn completed(
        spec: &DispatchSpec,
        final_json: &str,
        journal: &str,
        exit: Option<i32>,
    ) -> DispatchOutcome {
        let a = spec.invocation_artifacts.as_ref().unwrap();
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
        read_artifacts(spec.implementer, a).unwrap()
    }
    pub(crate) fn journal() -> String {
        format!("{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD}\"}}\n{{\"type\":\"turn.completed\",\"usage\":{{}}}}\n")
    }

    #[test]
    fn non_utf8_stderr_still_explains_a_failure_and_never_decides_validity() {
        let (_dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        std::fs::write(&a.stderr_path, b"model \xff rejected\n").unwrap();
        let met = completed(
            &s,
            r#"{"verdict":"met","reason":"gate"}"#,
            &journal(),
            Some(0),
        );
        assert!(met.is_met(), "stderr is diagnostic only");
        let (_dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
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
        let a = s.invocation_artifacts.unwrap();
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
        // The producer layout and the liveness helper must not drift apart.
        assert!(crate::autopilot::process_tree::is_autopilot_marker_path(
            &a.schema_path
        ));
        assert_eq!(
            std::fs::read_to_string(&a.schema_path).unwrap(),
            VERDICT_JSON_SCHEMA
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
        s.invocation_artifacts.as_mut().unwrap().final_path = "/tmp/has space/final".into();
        assert!(run_supervised_bounded(
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
        use crate::autopilot::process_tree::{ProcessRow, ProcessSnapshot};
        use crate::autopilot::registry::Liveness;
        let (dir, s) = setup();
        let a = s.invocation_artifacts.clone().unwrap();
        let o = run_supervised_bounded(
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
        let recovered = read_artifacts(Implementer::Codex, &a).unwrap();
        assert!(!recovered.invocation_started && !recovered.is_met());
        let empty = ProcessSnapshot::Available(Vec::new());
        assert_eq!(empty.liveness(Some(&a)), Liveness::NotListed);
        // A live process carrying the marker contradicts the record: hold.
        let live = ProcessSnapshot::Available(vec![ProcessRow {
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
        assert!(run_supervised_bounded(
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
        s.invocation_artifacts = None;
        assert!(validate_argv(dir.path(), &s).is_err());
        let (_, mut s) = setup();
        s.implementer = Implementer::Claude;
        assert!(run_supervised_bounded(
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
    pub(crate) fn binary(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-codex");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(unix)]
    /// A stub that outlives the supervisor's first process sample, as any
    /// real Codex does; one that exits first is held as unobserved.
    pub(crate) fn observable(extra: &str) -> String {
        format!("sleep 0.5\n{extra}")
    }
    pub(crate) fn script(extra: &str, code: i32) -> String {
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
        let a = spec.invocation_artifacts.as_ref().unwrap();
        let mut command = Command::new(bin);
        command
            .args(validate_argv(repo, spec).unwrap())
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::from(open_existing(&a.journal_path, true).unwrap()))
            .stderr(Stdio::from(open_existing(&a.stderr_path, true).unwrap()))
            .process_group(0);
        crate::mcp::tools::scrub_git_environment(&mut command);
        let child = super::super::dispatch::retry_on_etxtbsy(|| command.spawn()).unwrap();
        super::super::process_tree::persist_identity(
            a,
            &super::super::process_tree::ProcessIdentity {
                version: 1,
                root_pid: child.id(),
                root_pgid: child.id(),
                root_birth: None,
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
        let mut tracker = super::super::process_tree::ProcessTracker::new(child.id(), None);
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
            finish_supervised(
                &CodexCli,
                &mut tracker,
                repo,
                spec.invocation_artifacts.as_ref().unwrap(),
                spec,
                status,
                started,
                false,
                None,
                super::super::process_tree::QUIET_GRACE,
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
                run_supervised_bounded(&bin, dir.path(), &s, Duration::from_secs(3)).unwrap()
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
            let recovered =
                read_artifacts(Implementer::Codex, s.invocation_artifacts.as_ref().unwrap())
                    .unwrap();
            assert_eq!(recovered.is_met(), code == 0);
        }
    }
    #[cfg(unix)]
    #[test]
    fn abnormal_exit_before_any_process_observation_holds_even_after_met_output() {
        let (dir, spec) = setup();
        let a = spec.invocation_artifacts.as_ref().unwrap();
        let bin = binary(dir.path(), &script("", 7));
        let started = Instant::now();
        let mut child = spawn_fixture_invocation(&bin, dir.path(), &spec);
        let mut tracker = super::super::process_tree::ProcessTracker::new(child.id(), None);
        // Reap first, deliberately making useful root observation impossible.
        // This is the scheduler ordering the old combined fixture assumed away.
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(!tracker.observed_root());
        let final_verdict: FinalVerdict =
            serde_json::from_str(&std::fs::read_to_string(&a.final_path).unwrap()).unwrap();
        assert_eq!(final_verdict.verdict, Verdict::Met);
        let error = finish_supervised(
            &CodexCli,
            &mut tracker,
            dir.path(),
            a,
            &spec,
            status,
            started,
            false,
            None,
            super::super::process_tree::QUIET_GRACE,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("before process-tree observation"));
        assert!(!has_valid_exit_record(a));
        let process =
            super::super::process_tree::snapshot(&mut super::super::process_tree::UnixProcesses);
        assert!(process.is_available());
        assert_eq!(
            process.liveness(Some(a)),
            super::super::registry::Liveness::Unknown
        );
        let outcome = read_artifacts(Implementer::Codex, a).unwrap();
        assert_eq!(outcome.native_session_id.as_deref(), Some(THREAD));
        assert!(!outcome.is_met());
        assert!(!outcome.process_success);
    }

    #[cfg(unix)]
    #[test]
    fn large_journal_does_not_deadlock_and_limits_kill_with_scrubbed_bounded_reason() {
        let (dir, s) = setup();
        let extra = "i=0; while [ \"$i\" -lt 3000 ]; do printf '%s\\n' '{\"type\":\"item.completed\",\"item\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}'; i=$((i+1)); done";
        let o = run_supervised_bounded(
            &binary(dir.path(), &script(&observable(extra), 0)),
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
            let a = s.invocation_artifacts.as_ref().unwrap();
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
            let o = run_supervised_bounded(
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
    #[cfg(unix)]
    #[test]
    fn resume_never_switches_to_an_unrequested_native_thread() {
        let (dir, mut s) = setup();
        s.session = SessionMode::Resume {
            session_uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        };
        let o = run_supervised_bounded(
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
        assert!(
            !read_artifacts(Implementer::Codex, s.invocation_artifacts.as_ref().unwrap())
                .unwrap()
                .is_met()
        );
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
    fn supervised_git_commands_are_isolated_from_inherited_git_overrides() {
        const CHILD_MARKER: &str = "IRONMEM_SUPERVISED_GIT_ISOLATION_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "autopilot::invocation::contract_tests::supervised_git_commands_are_isolated_from_inherited_git_overrides", "--nocapture"])
                .env(CHILD_MARKER, "1")
                .env("GIT_DIR", "/nonexistent/hostile-git-dir")
                .env("GIT_WORK_TREE", "/nonexistent/hostile-worktree")
                .output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "child test failed: {} {}",
                stdout,
                String::from_utf8_lossy(&output.stderr)
            );
            // A stale test path filters to zero tests and still exits 0.
            assert!(
                stdout.contains("1 passed"),
                "child run did not execute this test: {stdout}"
            );
            return;
        }
        let (dir, s) = setup();
        let extra = observable("git rev-parse --show-toplevel > git-cwd || exit 8");
        let o = run_supervised_bounded(
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
        let a = s.invocation_artifacts.as_ref().unwrap();
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
        let mut tracker = super::super::process_tree::ProcessTracker::new(child.id(), None);
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
        let result = finish_supervised(
            &CodexCli,
            &mut tracker,
            dir.path(),
            a,
            &s,
            status,
            started,
            false,
            None,
            super::super::process_tree::QUIET_GRACE,
        );
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
    /// Muse runs each tool as its own session leader. One that backgrounds a
    /// child and exits between two samples leaves that child reparented to
    /// init, in a group that is not the root's, so neither ancestry nor the
    /// group drain reaches it. It still runs in the worktree, so completion
    /// is refused and the slot held, but the supervisor records that only
    /// the worktree stood in the way. Once that process is gone, recovery
    /// finishes the invocation with its verdict, with no `clear-dispatch`.
    /// The only scan happens before the root starts the tool, which makes
    /// the escape deterministic.
    #[cfg(unix)]
    #[test]
    fn a_background_child_of_an_exited_session_leader_tool_withholds_completion_until_gone() {
        use crate::autopilot::registry::{
            invocation_state_liveness_with, invocation_state_reason_with, Liveness,
        };
        struct KillOnDrop(Option<i32>);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    // SAFETY: the fixture's own escaped sleep.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        let (dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        let bin = binary(
            dir.path(),
            &script(
                r#"while [ ! -f root-release ]; do sleep 0.01; done
/usr/bin/python3 -c 'import os; os.setsid(); os.execv("/bin/sh", ["sh", "-c", "(sleep 30 & echo $! > escaped.pid) ; exit 0"])'
while [ ! -s escaped.pid ]; do sleep 0.01; done"#,
                0,
            ),
        );
        let started = Instant::now();
        let mut child = spawn_fixture_invocation(&bin, dir.path(), &s);
        let mut tracker = super::super::process_tree::ProcessTracker::new(child.id(), None);
        tracker.observe().unwrap();
        // As the supervision loop does after each sample.
        let mut identity = super::super::process_tree::read_identity(a).unwrap();
        record_identity(a, &mut identity, &tracker).unwrap();
        std::fs::write(dir.path().join("root-release"), "release").unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "root hung");
            std::thread::sleep(Duration::from_millis(5));
        };
        let result = finish_supervised(
            &CodexCli,
            &mut tracker,
            dir.path(),
            a,
            &s,
            status,
            started,
            false,
            None,
            Duration::from_millis(500),
        );
        let escaped = std::fs::read_to_string(dir.path().join("escaped.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        let mut guard = KillOnDrop(escaped);
        assert!(status.success());
        let err = result.unwrap_err().to_string();
        let pid = escaped.expect("the tool recorded its background child");
        assert!(
            err.contains(&format!("process {pid}")) && err.contains("worktree"),
            "{err}"
        );
        assert!(!has_valid_exit_record(a));
        assert!(is_awaiting_quiet(a));
        // The state names some other directory: recovery scans the
        // worktree the supervisor recorded, never this one.
        let elsewhere = tempfile::tempdir().unwrap();
        let state = held_state(a, elsewhere.path());
        let snapshot =
            || super::super::process_tree::snapshot(&mut super::super::process_tree::UnixProcesses);
        // Still there: held, naming it.
        assert_eq!(
            invocation_state_liveness_with(&snapshot(), &state, no_daemon),
            Liveness::Unknown
        );
        let reason = invocation_state_reason_with(&snapshot(), &state, no_daemon);
        assert!(reason.contains(&format!("process {pid}")), "{reason}");
        assert!(!has_valid_exit_record(a));
        // Gone: recovery upgrades the record and finishes with the verdict.
        if let Some(pid) = guard.0.take() {
            // SAFETY: the fixture's own escaped sleep.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut liveness = Liveness::Unknown;
        while liveness != Liveness::NotListed && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            liveness = invocation_state_liveness_with(&snapshot(), &state, no_daemon);
        }
        assert_eq!(liveness, Liveness::NotListed);
        assert!(has_valid_exit_record(a) && !is_awaiting_quiet(a));
        let outcome = read_artifacts(Implementer::Codex, a).unwrap();
        assert!(outcome.is_met() && outcome.process_success, "{outcome:?}");
    }

    /// The shared-daemon exemption, injected so recovery tests never read
    /// the operator's real config.
    fn no_daemon() -> Option<SharedDaemon> {
        None
    }

    pub(crate) fn held_state(
        a: &InvocationArtifacts,
        worktree: &Path,
    ) -> crate::autopilot::dispatch_state::DispatchState {
        crate::autopilot::dispatch_state::DispatchState {
            implementer: Implementer::Codex,
            native_session_id: None,
            invocation_artifacts: Some(a.clone()),
            issue: IssueRef::new("owner/repo", 42),
            worktree_path: worktree.to_str().unwrap().into(),
            ic_session_name: "ic-owner-repo-42".into(),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "dispatching".into(),
            started_at: "2026-10-27T00:00:00Z".into(),
            session_uuid: "controller-only".into(),
            turn_n: 1,
            session_claimed: true,
            session_model: None,
            resume_refusals: 0,
        }
    }

    /// An exited process's PID and the birth it had: its PID now shows no
    /// process with that birth.
    #[cfg(unix)]
    fn exited_process() -> (u32, super::super::process_tree::Birth) {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        let birth = super::super::process_tree::kernel_birth(child.id())
            .unwrap()
            .unwrap();
        child.wait().unwrap();
        (child.id(), birth)
    }

    /// What the supervisor writes when only the worktree stood in the way.
    pub(crate) fn write_awaiting(
        a: &InvocationArtifacts,
        uid: u32,
        worktree: &Path,
        stragglers: Vec<(u32, super::super::process_tree::Birth)>,
    ) {
        let mut record = exit_record(a, None, Some(0), false, None, 1, None);
        let hold = super::super::process_tree::QuietHold {
            worktree: worktree.canonicalize().unwrap(),
            uid,
            stragglers,
            scan_failed: None,
            reason: "fixture".into(),
        };
        await_quiet(&mut record, &hold);
        persist_exit(a, &record).unwrap();
    }

    /// Recovery upgrades an awaiting-quiet record from what the supervisor
    /// recorded, not from whoever asks: each recorded straggler must be gone
    /// by kernel birth (even one that left the worktree), the checker must
    /// be the same user, a kernel read error holds, and the recorded
    /// worktree is the one scanned.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_awaiting_quiet_invocation_upgrades_only_on_the_recorded_evidence() {
        use crate::autopilot::process_tree::{kernel_birth, ProcessIdentity, ProcessSnapshot};
        let (_dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let me = std::process::id();
        let my_birth = kernel_birth(me).unwrap().unwrap();
        let (root, root_birth) = exited_process();
        super::super::process_tree::persist_identity(
            a,
            &ProcessIdentity {
                version: 1,
                root_pid: root,
                root_pgid: root,
                root_birth: Some(root_birth),
                schema_path: a.schema_path.clone(),
                invocation_id: a.invocation_id.clone(),
                descendants: Vec::new(),
            },
        )
        .unwrap();
        let rows = ProcessSnapshot::Available(Vec::new());
        let settle = |kernel: &dyn Fn(u32) -> Result<Option<_>, String>| {
            settle_awaiting_quiet(a, &rows, None, kernel)
        };
        let real = |pid| kernel_birth(pid);
        // A recorded straggler still alive, though no longer in the worktree.
        write_awaiting(a, uid, worktree.path(), vec![(me, my_birth)]);
        let held = settle(&real).unwrap_err();
        assert!(held.contains(&format!("process {me}")), "{held}");
        // Another user's check cannot see the invocation's processes.
        write_awaiting(a, uid + 1, worktree.path(), Vec::new());
        let held = settle(&real).unwrap_err();
        assert!(
            held.contains(&uid.to_string()) && held.contains(&(uid + 1).to_string()),
            "{held}"
        );
        // A kernel read error is no proof that the root is gone.
        write_awaiting(a, uid, worktree.path(), Vec::new());
        let held = settle(&|pid| {
            if pid == root {
                Err("lookup refused".into())
            } else {
                kernel_birth(pid)
            }
        })
        .unwrap_err();
        assert!(held.contains("lookup refused"), "{held}");
        assert!(is_awaiting_quiet(a) && !has_valid_exit_record(a));
        // Gone by birth (its PID shows a different process) and a quiet
        // recorded worktree: the record becomes a trusted completion.
        let (other, other_birth) = exited_process();
        let _ = other;
        write_awaiting(a, uid, worktree.path(), vec![(me, other_birth)]);
        assert_eq!(settle(&real), Ok(true));
        assert!(has_valid_exit_record(a) && !is_awaiting_quiet(a));
        // A scan that failed on one process's unreadable directory recorded
        // it: it holds by birth, though a fresh scan of the worktree (here,
        // empty) would now read it as quiet.
        let mut record = exit_record(a, None, Some(0), false, None, 1, None);
        let hold = super::super::process_tree::QuietHold {
            worktree: worktree.path().canonicalize().unwrap(),
            uid,
            stragglers: vec![(me, my_birth)],
            scan_failed: Some(format!(
                "process {me}, started during the invocation, has an unreadable working \
                 directory: Operation not permitted"
            )),
            reason: "fixture".into(),
        };
        await_quiet(&mut record, &hold);
        persist_exit(a, &record).unwrap();
        let held = settle(&real).unwrap_err();
        assert!(
            held.contains(&format!("process {me} is still running")),
            "{held}"
        );
        assert!(is_awaiting_quiet(a) && !has_valid_exit_record(a));
    }

    /// The worktree the supervisor recorded is the one scanned. A new
    /// process there holds the invocation even when the dispatch state
    /// names a quiet directory.
    #[cfg(unix)]
    #[test]
    fn recovery_scans_the_recorded_worktree_not_the_states() {
        use crate::autopilot::process_tree::ProcessIdentity;
        use crate::autopilot::registry::{invocation_state_liveness_with, Liveness};
        let (_dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        let recorded = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let (root, root_birth) = exited_process();
        super::super::process_tree::persist_identity(
            a,
            &ProcessIdentity {
                version: 1,
                root_pid: root,
                root_pgid: root,
                root_birth: Some(root_birth),
                schema_path: a.schema_path.clone(),
                invocation_id: a.invocation_id.clone(),
                descendants: Vec::new(),
            },
        )
        .unwrap();
        write_awaiting(a, uid, recorded.path(), Vec::new());
        let mut newcomer = std::process::Command::new("/bin/sleep")
            .arg("30")
            .current_dir(recorded.path())
            .spawn()
            .unwrap();
        let snapshot =
            super::super::process_tree::snapshot(&mut super::super::process_tree::UnixProcesses);
        let liveness =
            invocation_state_liveness_with(&snapshot, &held_state(a, elsewhere.path()), no_daemon);
        let _ = newcomer.kill();
        let _ = newcomer.wait();
        assert_eq!(liveness, Liveness::Unknown);
        assert!(is_awaiting_quiet(a));
    }

    #[cfg(unix)]
    #[test]
    fn post_exit_artifact_overflow_kills_the_surviving_gate_process() {
        use std::os::unix::process::CommandExt;

        let (dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
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
        let mut tracker = super::super::process_tree::ProcessTracker::new(child.id(), None);
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
        let outcome = finish_supervised(
            &CodexCli,
            &mut tracker,
            dir.path(),
            a,
            &s,
            status,
            started,
            false,
            previous_limit,
            super::super::process_tree::QUIET_GRACE,
        )
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
        let o = run_supervised_bounded(&bin, dir.path(), &s, Duration::from_millis(100)).unwrap();
        assert!(o.timed_out);
        assert!(!o.is_met());
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!dir.path().join("descendant-survived").exists());
        assert!(
            read_artifacts(Implementer::Codex, s.invocation_artifacts.as_ref().unwrap())
                .unwrap()
                .timed_out
        );
    }
    #[cfg(unix)]
    #[test]
    fn timeout_kills_tool_in_a_separate_process_group() {
        let (dir, s) = setup();
        let python = "/usr/bin/python3";
        // The tool waits for a release the test gives only after the bound
        // fired, so it can mark survival only if the kill missed it. The
        // bound is generous because a cold python start on a CI runner can
        // outlast a short one, leaving nothing to kill; the loop ends on its
        // own if a red run leaves it behind.
        let extra = format!("{python} -c 'import os,time; os.setsid(); open(\"detached-ready\",\"w\").close()\nfor _ in range(3000):\n  time.sleep(0.01)\n  if os.path.exists(\"release\"): open(\"detached-survived\",\"w\").close(); break' &\nwhile [ ! -f detached-ready ]; do sleep 0.01; done\nsleep 30");
        let bin = binary(dir.path(), &script(&extra, 0));
        let o = run_supervised_bounded(&bin, dir.path(), &s, Duration::from_secs(5)).unwrap();
        assert!(o.timed_out);
        assert!(dir.path().join("detached-ready").exists());
        std::fs::write(dir.path().join("release"), "").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !dir.path().join("detached-survived").exists(),
            "escaped tool survived timeout"
        );
    }

    /// A harness whose evidence is scripted, so these tests pin what the
    /// shared core does with whatever a harness reports.
    struct FakeCli {
        verdict: bool,
        native_id: bool,
        prepare_fails: bool,
    }

    const FAKE_ENV: &[(&str, &str)] = &[("IRONMEM_FAKE_HARNESS_ENV", "from-env")];

    fn fake_prompt(a: &InvocationArtifacts) -> std::path::PathBuf {
        Path::new(&a.schema_path).with_file_name("prompt.txt")
    }

    impl SupervisedCli for FakeCli {
        fn implementer(&self) -> Implementer {
            Implementer::Codex
        }
        fn label(&self) -> &'static str {
            "Fake"
        }
        fn argv(
            &self,
            _repo: &Path,
            _spec: &DispatchSpec,
            a: &InvocationArtifacts,
        ) -> Result<Vec<String>, MemoryError> {
            Ok(vec![fake_prompt(a).to_str().unwrap().to_owned()])
        }
        fn prepare_spawn(
            &self,
            _spec: &DispatchSpec,
            a: &InvocationArtifacts,
        ) -> Result<(), MemoryError> {
            if self.prepare_fails {
                return Err(validation("prompt file could not be written"));
            }
            let mut file = create_private(&fake_prompt(a))?;
            file.write_all(b"the prompt")?;
            Ok(())
        }
        fn spawn_files(&self, a: &InvocationArtifacts) -> Vec<std::path::PathBuf> {
            vec![fake_prompt(a)]
        }
        fn env(&self) -> &'static [(&'static str, &'static str)] {
            FAKE_ENV
        }
        fn expected_native_id(&self, _spec: &DispatchSpec) -> Option<String> {
            None
        }
        fn parse(
            &self,
            journal: &str,
            _final_text: Result<String, MemoryError>,
            _expected_native_id: Option<&str>,
        ) -> ParsedRun {
            let native_id = self.native_id.then(|| THREAD.to_owned());
            match (self.verdict, native_id) {
                (true, Some(native_id)) => ParsedRun::with_verdict(
                    native_id,
                    FinalVerdict {
                        verdict: Verdict::Met,
                        reason: "gate".into(),
                    },
                    1,
                    !journal.is_empty(),
                ),
                // Reported with no reason, so the core names what is missing.
                (_, native_id) => ParsedRun::failed(Vec::new(), native_id, 1, !journal.is_empty()),
            }
        }
    }

    #[test]
    fn a_harness_reporting_no_verdict_or_native_id_fails_closed_rather_than_erroring() {
        let complete = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: false,
        };
        let (_dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        crate::autopilot::process_tree::testing::complete(a);
        assert!(read_artifacts_with(&complete, a).unwrap().is_met());
        for (cli, missing) in [
            (
                FakeCli {
                    verdict: false,
                    ..complete
                },
                "Fake reported no final verdict",
            ),
            (
                FakeCli {
                    native_id: false,
                    ..complete
                },
                "Fake reported no native session ID",
            ),
        ] {
            let (_dir, s) = setup();
            let a = s.invocation_artifacts.as_ref().unwrap();
            crate::autopilot::process_tree::testing::complete(a);
            let o = read_artifacts_with(&cli, a).unwrap();
            assert!(o.is_error && !o.is_met(), "{o:?}");
            assert_eq!(o.verdict, None);
            let reason = o.reason.unwrap();
            assert!(reason.contains(missing), "{reason}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_supervised_run_gives_the_child_its_harness_env_and_pre_spawn_files() {
        let (dir, s) = setup();
        let cli = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: false,
        };
        let bin = binary(
            dir.path(),
            &observable(
                "[ \"$IRONMEM_FAKE_HARNESS_ENV\" = from-env ] || exit 3\n\
                 cat \"$1\" > prompt-seen || exit 4\n\
                 echo journal",
            ),
        );
        let o =
            run_supervised_bounded_with(&cli, &bin, dir.path(), &s, Duration::from_secs(5), None)
                .unwrap();
        assert!(o.process_success && o.is_met(), "{o:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompt-seen")).unwrap(),
            "the prompt"
        );
    }

    /// The core adds nothing to the agent's environment that changes how
    /// ironmem itself behaves. An `IRONMEM_NO_DAEMON` there would reach the
    /// agent's own shell, and an Autopilot run on an ironmem issue would then
    /// fail the daemon autospawn tests it is asked to keep green. Keeping
    /// the shared daemon alive is the tracker's job instead (see
    /// `cleanup_leaves_a_shared_daemon_the_harness_autospawned_running`).
    #[cfg(unix)]
    #[test]
    fn a_supervised_child_gets_no_daemon_autospawn_override() {
        if std::env::var_os("IRONMEM_NO_DAEMON").is_some() {
            eprintln!("IRONMEM_NO_DAEMON is exported to the test run; skipping");
            return;
        }
        let (dir, s) = setup();
        let cli = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: false,
        };
        let bin = binary(
            dir.path(),
            &observable(
                "[ -z \"${IRONMEM_NO_DAEMON+set}\" ] || exit 3\n\
                 echo journal",
            ),
        );
        let o =
            run_supervised_bounded_with(&cli, &bin, dir.path(), &s, Duration::from_secs(5), None)
                .unwrap();
        assert!(o.process_success && o.is_met(), "{o:?}");
    }

    /// An `ironmem serve --connect` MCP proxy the harness starts autospawns
    /// the shared daemon under the harness when none is running. Cleanup
    /// must leave a daemon on the operator's configured socket running, and
    /// still clean up one on any other socket (an ironmem test suite's own
    /// temp-socket daemon, say). The fake daemon is `/bin/sh` behind a
    /// symlink named `ironmem` (a copied system binary is killed on macOS),
    /// running a script named `serve` from its own directory, so ps shows
    /// the daemon's exact argv in a session of its own, with a `sleep` child.
    #[cfg(unix)]
    #[test]
    fn cleanup_leaves_a_shared_daemon_the_harness_autospawned_running() {
        struct KillGroup(i32);
        impl Drop for KillGroup {
            fn drop(&mut self) {
                // SAFETY: a negative PID signals the fake daemon's own group.
                unsafe {
                    libc::kill(-self.0, libc::SIGKILL);
                }
            }
        }
        // SAFETY: signal 0 only probes that the process exists.
        let exists = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        for on_configured_socket in [true, false] {
            let (dir, s) = setup();
            let daemon_dir = dir.path().join("daemon");
            std::fs::create_dir(&daemon_dir).unwrap();
            std::os::unix::fs::symlink("/bin/sh", daemon_dir.join("ironmem")).unwrap();
            std::fs::write(daemon_dir.join("serve"), "sleep 30\n:\n").unwrap();
            let daemon = daemon_dir.join("ironmem");
            let configured = daemon_dir.join("daemon.sock");
            let socket = if on_configured_socket {
                configured.clone()
            } else {
                daemon_dir.join("temp-test.sock")
            };
            let cli = FakeCli {
                verdict: true,
                native_id: true,
                prepare_fails: false,
            };
            // The child that spawned it outlives a few process samples, so
            // the tracker sees the daemon while its ancestry still leads to
            // the root.
            let bin = binary(
                dir.path(),
                &observable(&format!(
                    "/usr/bin/python3 -c \"import subprocess,time; \
                     p=subprocess.Popen(['{}','serve','--listen','{}'],start_new_session=True,cwd='{}'); \
                     open('daemon.pid','w').write(str(p.pid)); time.sleep(1)\" || exit 5\n\
                     echo journal",
                    daemon.display(),
                    socket.display(),
                    daemon_dir.display()
                )),
            );
            let result = run_supervised_bounded_with(
                &cli,
                &bin,
                dir.path(),
                &s,
                Duration::from_secs(10),
                Some(&SharedDaemon {
                    socket: configured.clone(),
                    // The fake daemon's real executable, as the kernel
                    // reports it. macOS `/bin/sh` re-execs the shell that
                    // `/var/select/sh` selects (bash by default) under the
                    // same argv (measured); elsewhere `/bin/sh` resolves
                    // to the shell itself.
                    exe: Path::new(if cfg!(target_os = "macos") {
                        "/var/select/sh"
                    } else {
                        "/bin/sh"
                    })
                    .canonicalize()
                    .unwrap(),
                }),
            );
            let pid: i32 = std::fs::read_to_string(dir.path().join("daemon.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let _cleanup = KillGroup(pid);
            let o = result.unwrap();
            assert!(o.process_success && o.is_met(), "{o:?}");
            if on_configured_socket {
                assert!(exists(pid), "supervision cleanup killed the shared daemon");
            } else {
                // Killed by cleanup; give its new parent a moment to reap it.
                let deadline = Instant::now() + Duration::from_secs(2);
                while exists(pid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(!exists(pid), "a daemon on another socket escaped cleanup");
            }
        }
    }

    /// A crashed controller's successor and `clear-dispatch` see only what
    /// the identity file records. A tool the tracker enrols is recorded
    /// there while the root still runs, not only once it exits.
    #[cfg(unix)]
    #[test]
    fn an_enrolled_descendant_is_recorded_before_the_root_exits() {
        let (dir, s) = setup();
        let a = s.invocation_artifacts.clone().unwrap();
        let cli = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: false,
        };
        let bin = binary(
            dir.path(),
            &observable(
                "sleep 30 &\n\
                 echo $! > tool.pid\n\
                 while [ ! -f root-release ]; do sleep 0.01; done\n\
                 kill \"$(cat tool.pid)\"\n\
                 echo journal",
            ),
        );
        std::thread::scope(|scope| {
            let run = scope.spawn(|| {
                run_supervised_bounded_with(
                    &cli,
                    &bin,
                    dir.path(),
                    &s,
                    Duration::from_secs(10),
                    None,
                )
            });
            let started = Instant::now();
            let mut recorded = false;
            while !recorded && !run.is_finished() && started.elapsed() < Duration::from_secs(5) {
                let tool = std::fs::read_to_string(dir.path().join("tool.pid"))
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok());
                recorded = tool.is_some_and(|tool| {
                    crate::autopilot::process_tree::read_identity(&a)
                        .is_ok_and(|identity| identity.descendants.iter().any(|d| d.pid == tool))
                });
                std::thread::sleep(Duration::from_millis(10));
            }
            // Released either way, so a red run never leaves the root waiting.
            std::fs::write(dir.path().join("root-release"), "release").unwrap();
            let outcome = run.join().unwrap().unwrap();
            assert!(
                recorded,
                "the enrolled tool was not recorded while the root ran"
            );
            assert!(outcome.is_met(), "{outcome:?}");
        });
    }

    #[test]
    fn a_failed_pre_spawn_write_is_recorded_as_never_launched() {
        use crate::autopilot::process_tree::ProcessSnapshot;
        use crate::autopilot::registry::Liveness;
        let (dir, s) = setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        let cli = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: true,
        };
        let o = run_supervised_bounded_with(
            &cli,
            Path::new("/missing-binary"),
            dir.path(),
            &s,
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert!(!o.invocation_started && !o.is_met(), "{o:?}");
        let reason = o.reason.unwrap();
        assert!(
            reason.contains("prompt file could not be written"),
            "{reason}"
        );
        assert!(has_launch_failure_record(a));
        assert_eq!(
            ProcessSnapshot::Available(Vec::new()).liveness(Some(a)),
            Liveness::NotListed
        );
    }

    /// Opening the journal or stderr for the child happens after
    /// `prepare_spawn` and before any spawn, so a failure there is also
    /// proof that nothing ran, not an ambiguous hold that only
    /// `clear-dispatch` releases.
    #[cfg(unix)]
    #[test]
    fn a_failed_artifact_open_before_spawn_is_recorded_as_never_launched() {
        use std::os::unix::fs::PermissionsExt;

        use crate::autopilot::process_tree::ProcessSnapshot;
        use crate::autopilot::registry::Liveness;
        let cli = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: false,
        };
        let artifacts: [fn(&InvocationArtifacts) -> &str; 2] =
            [|a| &a.journal_path, |a| &a.stderr_path];
        for artifact in artifacts {
            let (dir, s) = setup();
            let a = s.invocation_artifacts.as_ref().unwrap();
            // Readable, so the prior-evidence check passes; not writable.
            std::fs::set_permissions(artifact(a), std::fs::Permissions::from_mode(0o400)).unwrap();
            let bin = binary(dir.path(), "touch spawned");
            let o = run_supervised_bounded_with(
                &cli,
                &bin,
                dir.path(),
                &s,
                Duration::from_secs(5),
                None,
            )
            .unwrap();
            assert!(!o.invocation_started && !o.is_met(), "{o:?}");
            let reason = o.reason.unwrap();
            assert!(reason.contains("failed to open"), "{reason}");
            assert!(has_launch_failure_record(a));
            assert!(!dir.path().join("spawned").exists());
            assert_eq!(
                ProcessSnapshot::Available(Vec::new()).liveness(Some(a)),
                Liveness::NotListed
            );
        }
    }

    /// A prior spawn into this directory leaves its identity file or the
    /// harness's spawn files behind even when every output artifact is empty
    /// (the child was killed before writing). Writing a never-launched
    /// record there could mask a live invocation, so the core refuses with
    /// an error and leaves the directory untouched.
    #[test]
    fn a_prior_identity_or_spawn_file_refuses_the_spawn_without_a_record() {
        let cli = FakeCli {
            verdict: true,
            native_id: true,
            prepare_fails: false,
        };
        let leftovers: [fn(&InvocationArtifacts) -> std::path::PathBuf; 2] = [
            |a| Path::new(&a.schema_path).with_file_name("process.json"),
            fake_prompt,
        ];
        for leftover in leftovers {
            let (dir, s) = setup();
            let a = s.invocation_artifacts.as_ref().unwrap();
            std::fs::write(leftover(a), b"prior").unwrap();
            let err = run_supervised_bounded_with(
                &cli,
                Path::new("/missing-binary"),
                dir.path(),
                &s,
                Duration::from_secs(1),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("prior spawn"), "{err}");
            assert_eq!(std::fs::metadata(&a.exit_path).unwrap().len(), 0);
            assert!(!has_launch_failure_record(a));
            assert_eq!(std::fs::read(leftover(a)).unwrap(), b"prior");
        }
    }
}
