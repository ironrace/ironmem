//! Read-only status of one repo's unattended-route prerequisites (#420).
//!
//! `ironmem autopilot status <owner/repo>` answers two questions the
//! `/evaluate-issue` router asks before it recommends GOAL or AUTOPILOT: does
//! the repo have an approved gate config, and does it have the labels
//! Autopilot needs. Before this module the router answered the first by
//! reading Autopilot's drawer directly, which tied prose instructions to a
//! storage layout.
//!
//! # Read-only by how the database is opened
//!
//! The gate config is read by [`read_gate_config_status`] over a connection
//! opened `SQLITE_OPEN_READ_ONLY`, which is what refuses a write. The handle
//! it reads through, [`ReadOnlyDb`], offers only read methods, but it also
//! lends its raw connection, so the open flag is the guarantee and the type
//! is not. The labels are read with one `gh label list`. Nothing here writes
//! the database file or creates a drawer or a label, though a read-only
//! connection to a WAL-mode database may create the `-wal`/`-shm` side files.
//!
//! The label half is read-only because this module only ever builds a
//! `label list` argv, not because a type forbids more.
//!
//! # What the types hold
//!
//! - [`RepoName`] is the only repo argument the reads and the report take,
//!   and only [`RepoName::parse`] builds one, so no unchecked string reaches
//!   the database lookup or a `gh` argv.
//! - [`GateConfigRead`] is built only by [`read_gate_config_status`] and
//!   carries the repo asked about, the database that answered and what it
//!   held. A [`RepoStatus`] is built only from one, by
//!   [`GateConfigRead::with_labels_read_in`], which reads the labels for that
//!   same repo, so a report cannot name a database or a repo other than the
//!   ones that were read, or carry another repo's labels.
//! - [`GateConfigStatus`] is built only from a stored [`GateConfig`], and a
//!   [`LabelSet`] only from a label list that was read in full.
//!
//! # A failed read is never a missing label
//!
//! [`LabelStatus`] is an enum, not a struct of optional fields: when `gh` is
//! missing or could not be started, exited non-zero, printed output that is
//! not the expected JSON, returned a label row with no name, returned a list
//! long enough to have been cut off, or was never run because `--path` is not
//! a directory it can run in, the status is [`LabelStatus::Failed`] and
//! carries no label answers at all. Its JSON has no `agent` or `risk` key, so
//! a consumer must check `read` first; a strict lookup finds nothing, though a
//! loose one (a missing key read as false) can still be fooled, which is why
//! `read` exists.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

use crate::db::read_only::ReadOnlyDb;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::gate_config::{
    get_gate_config_read_only, stored_gate_configs_read_only, BrokenGateConfig, GateConfig,
    GateConfigState, StoredGateConfig, StoredGateConfigsError,
};
use super::gh::{repo_labels, GhCli, GhRunner};
use super::labels::AgentLabel;
use super::{repo_slug, validate_repo};

/// A repo name that passed the shape check: `owner/name` or `host/owner/name`,
/// each part non-empty, made only of ASCII letters, digits, `.`, `_` and `-`,
/// not starting with `-` and not made only of `.`, and with a name that does
/// not end in `.git`. The check does not know every name `gh` refuses.
///
/// Every character it lets through is one a POSIX shell reads literally, so
/// the report's hints print a name without quoting it.
///
/// Measured against `gh` 2.96.0 for `gh label list --repo`: a different
/// letter case and a `github.com/` prefix both resolve to the same repository
/// (which is why [`read_gate_config_status`] refuses them as near-misses of a
/// stored name); a name without a host is read against `GH_HOST` when that is
/// set; a `.git` suffix and `host:port` do not resolve; and a dash-led value
/// is read as a repo name, not as a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoName {
    /// The name exactly as it was given.
    raw: String,
    /// Boxed so that a [`StatusError`] holding a name beside a path and a
    /// list stays small enough to return by value.
    identity: Box<RepoIdentity>,
}

impl RepoName {
    /// Check `given`'s shape. The only way to build a [`RepoName`].
    pub fn parse(given: &str) -> Result<Self, StatusError> {
        validate_repo(given).map_err(StatusError::Other)?;
        let not_a_repo_name = || StatusError::NotARepoName {
            given: given.to_string(),
        };
        let parts: Vec<&str> = given.split('/').collect();
        // A part made only of dots (`.`, `..`) is a directory reference:
        // GitHub reserves those names, and `gh` resolves no repo for one.
        let part_ok = |part: &&str| {
            !part.is_empty()
                && !part.starts_with('-')
                && !part.chars().all(|c| c == '.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        if !parts.iter().all(part_ok) {
            return Err(not_a_repo_name());
        }
        let (host, owner, name) = match parts.as_slice() {
            [owner, name] => (None, owner, name),
            [host, owner, name] => (Some(host), owner, name),
            _ => return Err(not_a_repo_name()),
        };
        let identity = RepoIdentity {
            host: host.map(|host| host.to_ascii_lowercase()),
            owner: owner.to_ascii_lowercase(),
            name: name.to_ascii_lowercase(),
        };
        if identity.name.ends_with(".git") {
            return Err(StatusError::GitSuffix {
                given: given.to_string(),
            });
        }
        Ok(Self {
            raw: given.to_string(),
            identity: Box::new(identity),
        })
    }

    /// The name exactly as it was given.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether a stored row that does not parse could be this repo's. The
    /// row is judged by the `repo` its content still holds, as a row that
    /// parses is. When it holds none that names an owner, only the slug in
    /// its storage key is left; a slug cannot be turned back into a name, so
    /// that comparison also matches some names of other repos, which is the
    /// safe direction: the read fails instead of reporting "not onboarded".
    fn may_be_stored_as(&self, row: &BrokenGateConfig) -> bool {
        match row.repo.as_deref().and_then(RepoIdentity::of_stored) {
            Some(identity) => self.identity.may_be(&identity),
            None => slugs_may_match(&repo_slug(&self.raw), row.slug()),
        }
    }
}

/// Whether two storage-key slugs could belong to one repository: the same
/// slug in any letter case, or one that is the other with a host in front
/// (`host-owner-name` ends in `-owner-name`).
fn slugs_may_match(one: &str, other: &str) -> bool {
    let (one, other) = (one.to_ascii_lowercase(), other.to_ascii_lowercase());
    one == other || one.ends_with(&format!("-{other}")) || other.ends_with(&format!("-{one}"))
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Serialized as the bare name, a JSON string.
impl Serialize for RepoName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

/// The parts of a repo name that decide which repository it is, ASCII
/// lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoIdentity {
    /// `None` for an `owner/name`.
    host: Option<String>,
    owner: String,
    name: String,
}

impl RepoIdentity {
    /// The identity of a stored repo name. Stored names are not shape-checked,
    /// so this reads leniently: the last three `/`-separated segments, and
    /// `None` for a string with no `/`.
    fn of_stored(repo: &str) -> Option<Self> {
        let mut segments = repo.rsplit('/');
        let name = segments.next()?.to_ascii_lowercase();
        let owner = segments.next()?.to_ascii_lowercase();
        let host = segments.next().map(str::to_ascii_lowercase);
        Some(Self { host, owner, name })
    }

    /// Whether two names could name the same repository: the same owner and
    /// name, and not two different hosts. A name without a host matches any
    /// host, because `gh` reads it against its default host, which is not
    /// known here.
    fn may_be(&self, other: &Self) -> bool {
        let hosts_may_match = match (&self.host, &other.host) {
            (Some(host), Some(other_host)) => host == other_host,
            _ => true,
        };
        self.owner == other.owner && self.name == other.name && hosts_may_match
    }
}

/// Why `ironmem autopilot status` could not report. Every variant is exit 1
/// with nothing on stdout.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    /// The string is not shaped like a repo name.
    #[error(
        "'{given}' is not a repo name — expected `owner/name` (or `host/owner/name`), each part \
         made of letters, digits, `.`, `_` and `-`, not starting with `-` and not made only of `.`"
    )]
    NotARepoName { given: String },

    /// The string is shaped like a repo name but ends in `.git`.
    #[error("'{given}' ends in `.git`, which `gh` does not resolve — drop the `.git` suffix")]
    GitSuffix { given: String },

    /// The database path could not be made absolute, so the report could not
    /// say which file answered.
    #[error("could not make the database path '{}' absolute: {source}", .path.display())]
    DbPathNotAbsolute {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The absolute database path is not UTF-8, so neither the JSON report
    /// nor a `--db` hint could name the file exactly.
    #[error(
        "the database path '{}' is not valid UTF-8, so the report could not name it exactly — \
         use a database whose full path is UTF-8",
        .path.display()
    )]
    DbPathNotUtf8 { path: PathBuf },

    /// No file is at the database path.
    #[error(
        "db not found at {} — `status` never creates one; pass an existing database with \
         `--db <path>` or set `IRONMEM_DB_PATH`",
        .path.display()
    )]
    DatabaseNotFound { path: PathBuf },

    /// The database path could not be examined, so whether a file is there
    /// is not known.
    #[error("could not reach the database at {}: {source}", .path.display())]
    DatabaseUnreachable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The database file is there, and SQLite could not open it.
    #[error(
        "could not open the database at {}: the file exists but could not be opened — check its \
         permissions and that it is a file",
        .path.display()
    )]
    CannotOpen { path: PathBuf },

    /// SQLite failed while the database was being opened or read.
    #[error(
        "could not read the database at {}: {source}. `status` opens it read-only and never \
         writes it.{}",
        .path.display(),
        .hint.map_or("", DbHint::sentence)
    )]
    Database {
        path: PathBuf,
        hint: Option<DbHint>,
        #[source]
        source: rusqlite::Error,
    },

    /// No config is stored under the name asked about, but one is stored
    /// under a spelling `gh` can read as the same repository.
    #[error("{}", stored_under_another_name(.asked, .stored, *.host_ambiguous))]
    StoredUnderAnotherName {
        asked: RepoName,
        /// Every stored name that could be the repo asked about.
        stored: Vec<String>,
        /// True when a stored name and the asked one differ in whether they
        /// name a host, the one case that may be two repositories.
        host_ambiguous: bool,
    },

    /// The config stored under the asked name's storage key is another
    /// repo's.
    #[error(
        "the gate config stored under this key belongs to '{stored}', not '{asked}' — two repo \
         names that reduce to the same storage key share one config, so this status cannot be \
         reported"
    )]
    KeyCollision { asked: RepoName, stored: String },

    /// The config stored under the asked name does not parse.
    #[error(
        "the gate config stored for '{repo}' in {} does not parse: {source}. To replace it, {}",
        .db.display(),
        replace_commands(.repo, .db)
    )]
    StoredConfigDoesNotParse {
        repo: RepoName,
        /// The absolute path of the database that holds the config.
        db: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// No config is stored under the name asked about, and a stored config
    /// that does not parse could be that repo's under another spelling.
    #[error("{}", broken_under_another_name(.asked, .db, .rows))]
    BrokenUnderAnotherName {
        asked: RepoName,
        /// The absolute path of the database that holds the rows.
        db: PathBuf,
        /// Every stored row that does not parse and could be the repo asked
        /// about.
        rows: Vec<BrokenGateConfig>,
    },

    /// No config is stored under the name asked about, and too many are
    /// stored to compare each with it, so one under another spelling cannot
    /// be ruled out. Listing more is #426.
    #[error(
        "'{asked}' has no gate config under that exact name, and {} holds {limit} or more gate \
         configs, more than `status` compares, so it cannot rule out one stored under another \
         spelling of this name. If the repo is onboarded, ask with the exact name it was \
         onboarded under: an exact name is read without comparing the others",
        .db.display()
    )]
    TooManyStoredConfigs {
        asked: RepoName,
        /// The absolute path of the database that holds the configs.
        db: PathBuf,
        limit: usize,
    },

    /// Any other failure, reported as the error it already is: a string that
    /// fails the crate-wide repo check, or a read that failed for a reason
    /// that is not SQLite's.
    #[error(transparent)]
    Other(MemoryError),
}

impl StatusError {
    /// Classify a failure of the open or of a read: a SQLite error names
    /// `db_path` and carries the hint that is true of it, anything else
    /// passes through.
    fn from_read(db_path: &Path, err: MemoryError) -> Self {
        match err {
            MemoryError::Db(source) => Self::Database {
                path: db_path.to_path_buf(),
                hint: DbHint::for_error(&source),
                source,
            },
            other => Self::Other(other),
        }
    }
}

/// The message of [`StatusError::StoredUnderAnotherName`].
fn stored_under_another_name(asked: &RepoName, stored: &[String], host_ambiguous: bool) -> String {
    let names = stored
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let other_host = if host_ambiguous {
        format!(" (if '{asked}' is a different repository on another host, it is not onboarded)")
    } else {
        String::new()
    };
    format!(
        "'{asked}' has no gate config under that exact name, but one is stored for {names}, \
         which differs only in letter case or host prefix — ask with the stored name{other_host}"
    )
}

/// The commands that replace `repo`'s stored gate config in the database at
/// `db`, as a clause: `onboard` writes a new proposal over a row that does
/// not parse, and `approve` approves it.
fn replace_commands(repo: &RepoName, db: &Path) -> String {
    let db = shell_quote(&db.display().to_string());
    format!(
        "run `ironmem autopilot onboard {repo} --path . --db {db}` from the repo's checkout, \
         then `ironmem autopilot approve {repo} --db {db}`"
    )
}

/// The message of [`StatusError::BrokenUnderAnotherName`].
fn broken_under_another_name(asked: &RepoName, db: &Path, rows: &[BrokenGateConfig]) -> String {
    let listed = rows
        .iter()
        .map(|row| {
            let named = match &row.repo {
                Some(repo) => format!(", whose `repo` is {repo:?},"),
                None => String::new(),
            };
            format!(
                "the config stored under the key '{}'{named} does not parse: {}",
                row.storage_key, row.error
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "'{asked}' has no gate config under that exact name, but {} holds one that cannot be \
         read and could be this repo's under another spelling — {listed}. Whether '{asked}' is \
         onboarded cannot be reported until that config is replaced: run `ironmem autopilot \
         onboard` with the name it was onboarded under and `--db {}`, then ask again",
        db.display(),
        shell_quote(&db.display().to_string())
    )
}

/// The extra sentence a [`StatusError::Database`] carries, for the SQLite
/// errors it is true of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbHint {
    /// SQLite refused the read as read-only.
    WalSideFiles,
    /// The file is not a database, or is one without ironmem's tables.
    NotAnIronmemDatabase,
}

impl DbHint {
    /// The hint that is true of `e`. A busy or locked database gets none.
    fn for_error(e: &rusqlite::Error) -> Option<Self> {
        let rusqlite::Error::SqliteFailure(failure, detail) = e else {
            return None;
        };
        // A foreign or empty SQLite file arrives as a generic `SQLITE_ERROR`
        // (code `Unknown`) whose message is "no such table: drawers".
        let no_such_table = detail
            .as_deref()
            .is_some_and(|text| text.starts_with("no such table"));
        match failure.code {
            rusqlite::ErrorCode::ReadOnly => Some(Self::WalSideFiles),
            code if code == rusqlite::ErrorCode::NotADatabase || no_such_table => {
                Some(Self::NotAnIronmemDatabase)
            }
            _ => None,
        }
    }

    fn sentence(self) -> &'static str {
        match self {
            Self::WalSideFiles => {
                " A WAL-mode database whose -wal/-shm side files are missing can only be read \
                 from a directory SQLite can create them in."
            }
            Self::NotAnIronmemDatabase => {
                " If this is not an ironmem database, pass the right one with --db."
            }
        }
    }
}

/// How a [`StatusError`] reaches the command line, where `main` prints
/// `Error: {MemoryError}` and every `MemoryError` variant adds its own prefix.
/// The variant is chosen for the prefix that is true of the failure; the
/// typed data stays on the [`StatusError`].
///
/// A SQLite failure becomes [`MemoryError::Db`], whose prefix is
/// "Database error:". That variant holds a `rusqlite::Error`, and the one
/// `rusqlite::Error` that displays a message of the caller's choosing is
/// `SqliteFailure` with a detail string, so the explained message travels
/// there, beside the failure's own result code. A `rusqlite` error that has
/// no result code is given SQLite's generic one, and a file that could not be
/// opened gets `SQLITE_CANTOPEN`, the only code `open_read_only` reports as
/// "not found".
///
/// A stored config that does not parse becomes [`MemoryError::Json`], whose
/// prefix is "JSON error:", holding the whole explained message.
impl From<StatusError> for MemoryError {
    fn from(err: StatusError) -> Self {
        let message = err.to_string();
        let database = |failure: rusqlite::ffi::Error, message: String| {
            MemoryError::Db(rusqlite::Error::SqliteFailure(failure, Some(message)))
        };
        match err {
            StatusError::Other(inner) => inner,
            StatusError::NotARepoName { .. }
            | StatusError::GitSuffix { .. }
            | StatusError::DbPathNotUtf8 { .. }
            | StatusError::StoredUnderAnotherName { .. }
            | StatusError::KeyCollision { .. }
            | StatusError::TooManyStoredConfigs { .. } => MemoryError::Validation(message),
            StatusError::DatabaseNotFound { .. } => MemoryError::NotFound(message),
            StatusError::DbPathNotAbsolute { source, .. }
            | StatusError::DatabaseUnreachable { source, .. } => {
                MemoryError::Io(std::io::Error::new(source.kind(), message))
            }
            StatusError::StoredConfigDoesNotParse { .. }
            | StatusError::BrokenUnderAnotherName { .. } => {
                MemoryError::Json(serde::de::Error::custom(message))
            }
            StatusError::CannotOpen { .. } => database(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                message,
            ),
            StatusError::Database {
                source: rusqlite::Error::SqliteFailure(failure, _),
                ..
            } => database(failure, message),
            StatusError::Database { .. } => database(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                message,
            ),
        }
    }
}

/// Everything `ironmem autopilot status` reports for one repo. Its
/// serialized form is the command's `--json` output, a contract the
/// `/evaluate-issue` router reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    repo: RepoName,
    /// The absolute path of the database file the gate config was read from.
    db: String,
    /// Whether a gate config exists at all, in any state. Always equal to
    /// `gate_config.is_some()`; stated so a reader need not infer it.
    onboarded: bool,
    /// `None` (JSON `null`) for a repo that was never onboarded.
    gate_config: Option<GateConfigStatus>,
    labels: LabelStatus,
}

impl RepoStatus {
    /// The report for one gate-config read and `labels`. The repo, the
    /// database and the gate config all come from `read`, so they describe
    /// one read, and `onboarded` is derived here, so it cannot disagree with
    /// `gate_config`. Private: [`GateConfigRead::with_labels_read_in`] is the
    /// way to a report, and it reads the labels for `read`'s own repo.
    fn new(read: GateConfigRead, labels: LabelStatus) -> Self {
        let GateConfigRead { repo, db, config } = read;
        Self {
            repo,
            db,
            onboarded: config.is_some(),
            gate_config: config,
            labels,
        }
    }
}

/// One read of a repo's gate config: the repo asked about, the database that
/// answered and what it held. Built only by [`read_gate_config_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateConfigRead {
    repo: RepoName,
    /// The absolute path of the database file that was read.
    db: String,
    /// `None` for a repo that was never onboarded.
    config: Option<GateConfigStatus>,
}

impl GateConfigRead {
    /// The report for this read: its gate config, and the labels of the repo
    /// it was made for, read with the `gh` on `PATH` run in `path`. The only
    /// way to a [`RepoStatus`], so a report's labels are always the labels
    /// of the repo its gate config was read for.
    ///
    /// Never an error: a label read that fails is part of the report
    /// ([`LabelStatus::Failed`]), and the gate config is still reported.
    pub fn with_labels_read_in(self, path: &Path) -> RepoStatus {
        let labels = LabelStatus::read_in(path, &self.repo);
        RepoStatus::new(self, labels)
    }
}

/// The reported part of a stored gate config. Built only from a
/// [`GateConfig`], which cannot exist without passing its own guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateConfigStatus {
    state: GateConfigState,
    /// Never empty and never holding a blank entry: a stored config that
    /// breaks that rule fails to read instead of being reported.
    gate_commands: Vec<String>,
    /// Problems a human must read before approving; empty when there are none.
    manifest_warnings: Vec<String>,
    proposed_at: String,
    /// As stored. The writers set it exactly when they approve a config, but
    /// nothing checks a stored row for that, so it can disagree with `state`.
    /// The Lead dispatches on `state` alone, and so does the text report.
    approved_at: Option<String>,
}

impl From<GateConfig> for GateConfigStatus {
    fn from(config: GateConfig) -> Self {
        Self {
            state: config.state,
            gate_commands: config.gate_commands().to_vec(),
            manifest_warnings: config.manifest_warnings,
            proposed_at: config.proposed_at,
            approved_at: config.approved_at,
        }
    }
}

/// What the repo's labels are, or that they could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "read")]
pub enum LabelStatus {
    /// The label list was read in full.
    #[serde(rename = "ok")]
    Ok(LabelSet),
    /// The label list could not be read. Says nothing about any label.
    #[serde(rename = "failed")]
    Failed { error: String },
}

/// The labels of a repo whose label list was read in full. Built only by
/// matching that list's names, so `agent` always has every
/// [`AgentLabel::ALL`] entry and `risk` only `risk:<class>` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LabelSet {
    /// One entry per [`AgentLabel::ALL`] label, keyed by its canonical
    /// lowercase GitHub name, `true` when the repo has it.
    agent: BTreeMap<&'static str, bool>,
    /// The repo's `risk:<class>` labels, sorted case-insensitively. Each is
    /// spelled as it is on GitHub, not lowercased: a repo's `Risk:Logic` is
    /// listed as `Risk:Logic`. GitHub label names are case-insensitive and
    /// the Lead reads them that way, so compare an entry case-insensitively.
    risk: Vec<String>,
}

impl LabelSet {
    /// Match the repo's label names by name — trimmed and case-insensitive — never by search.
    fn from_names(names: &[String]) -> Self {
        // Driven by `AgentLabel::ALL`, so a new variant cannot be left out
        // of the report.
        let agent = AgentLabel::ALL
            .into_iter()
            .map(|label| {
                let present = names
                    .iter()
                    .any(|name| AgentLabel::from_label_str(name) == Some(label));
                (label.as_str(), present)
            })
            .collect();
        let mut risk: Vec<String> = names
            .iter()
            .filter(|name| super::queue::risk_class_of(name).is_some())
            .cloned()
            .collect();
        risk.sort_by_cached_key(|name| (name.to_ascii_lowercase(), name.clone()));
        Self { agent, risk }
    }
}

impl LabelStatus {
    /// Read `repo`'s labels with the `gh` on `PATH`, run in `path`. Never an
    /// `Err`: a `path` that is not a directory or cannot be examined, a `gh`
    /// that cannot be found and every failure of the read itself are
    /// [`LabelStatus::Failed`], so one unreadable half of the report does not
    /// hide the other.
    fn read_in(path: &Path, repo: &RepoName) -> Self {
        match std::fs::metadata(path) {
            Ok(found) if found.is_dir() => {}
            Ok(_) => {
                return Self::failed(format!(
                    "--path '{}' is not a directory, so gh was not run",
                    path.display()
                ));
            }
            // Missing, or not readable by this user: the reason is the
            // filesystem's, and "not a directory" would be a guess.
            Err(reason) => {
                return Self::failed(format!(
                    "--path '{}' cannot be used as the directory gh runs in ({reason}), so gh \
                     was not run",
                    path.display()
                ));
            }
        }
        match GhCli::resolve(path) {
            Ok(mut gh) => Self::read(&mut gh, repo),
            Err(err) => Self::failed(bare_message(err)),
        }
    }

    /// Read `repo`'s labels through `gh`. Never an `Err`: every failure is
    /// [`LabelStatus::Failed`], worded without a [`MemoryError`] prefix.
    fn read(gh: &mut dyn GhRunner, repo: &RepoName) -> Self {
        match repo_labels(gh, repo.as_str()) {
            Ok(names) => Self::Ok(LabelSet::from_names(&names)),
            Err(err) => Self::failed(bare_message(err)),
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
        }
    }
}

/// A label-read failure's message without the prefix its [`MemoryError`]
/// variant adds. The report already says the labels could not be read, and
/// the prefix would misname the cause: a `gh` that exited non-zero is carried
/// as `NotFound`, and "Not found:" in front of a bad token says the repo is
/// missing.
fn bare_message(err: MemoryError) -> String {
    match err {
        MemoryError::NotFound(message) | MemoryError::Validation(message) => message,
        other => other.to_string(),
    }
}

/// Read `repo`'s gate config from the database at `db_path`, opening it
/// read-only. A SQLite failure is explained rather than passed through bare,
/// and a `db_path` that cannot be made absolute or is not UTF-8 is an error,
/// because the report could not name the file that answered.
///
/// A read whose config is `None` means no gate config is stored under this
/// name or under another spelling of it. When one is stored under a spelling
/// `gh` can read as the same repository (a different letter case, or a host
/// on one side only), this is an error naming the stored name instead, so a
/// repo onboarded under another spelling is not reported as "not onboarded".
/// A stored config that does not parse and could be this repo's is an error
/// too, whether it is stored under this name or another spelling. Two names
/// with different hosts are different repositories and never match.
///
/// The exact name is read first and needs no listing, so it is still
/// answered when more configs are stored than can be compared.
pub fn read_gate_config_status(
    db_path: &Path,
    repo: &RepoName,
) -> Result<GateConfigRead, StatusError> {
    // Before the open, so a path the report could not name is refused
    // whether or not a database is there.
    let reported_db = absolute_db_path(db_path)?;
    // Every error from here on names the absolute path, as the report does.
    let db_path = Path::new(&reported_db);
    let db = match Database::open_read_only(db_path) {
        // `open_read_only` calls every `CannotOpen` "not found".
        Err(MemoryError::NotFound(_)) => return Err(why_not_opened(db_path)),
        other => other.map_err(|err| StatusError::from_read(db_path, err))?,
    };
    let config = gate_config_status(&db, db_path, repo)?;
    if config.is_none() {
        refuse_another_spelling(&db, db_path, repo)?;
    }
    Ok(GateConfigRead {
        repo: repo.clone(),
        db: reported_db,
        config,
    })
}

/// Why SQLite could not open `db_path`, from what the filesystem says about
/// it: only a path with nothing at it is "not found". A file that is there
/// is a different problem, and so is a path that cannot be examined at all
/// (a parent directory this user may not search, say), where whether a file
/// is there is not known.
fn why_not_opened(db_path: &Path) -> StatusError {
    let path = db_path.to_path_buf();
    match std::fs::metadata(db_path) {
        Ok(_) => StatusError::CannotOpen { path },
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            StatusError::DatabaseNotFound { path }
        }
        Err(source) => StatusError::DatabaseUnreachable { path, source },
    }
}

/// `db` as the report names it: absolute, so the report names one file
/// whatever the cwd, and UTF-8, so the JSON string and a `--db` hint name
/// that file exactly. A path that cannot be both is an error: a relative or
/// lossily converted path would name a file that may not exist.
fn absolute_db_path(db: &Path) -> Result<String, StatusError> {
    let absolute = std::path::absolute(db).map_err(|source| StatusError::DbPathNotAbsolute {
        path: db.to_path_buf(),
        source,
    })?;
    match absolute.to_str() {
        Some(utf8) => Ok(utf8.to_string()),
        None => Err(StatusError::DbPathNotUtf8 { path: absolute }),
    }
}

/// An error when a gate config is, or may be, stored under another spelling
/// of `repo`. Called when none is stored under the exact name: `gh` can read
/// a different letter case, or a name with a host and one without, as the
/// same repository, so such a config means the repo may already be onboarded
/// under another name.
///
/// A stored row that does not parse is not passed over: one that could be
/// `repo`'s ([`RepoName::may_be_stored_as`]) is an error too, since nothing
/// else on this read would report it. A config that parses is named first,
/// because asking with its name is then a read that succeeds.
///
/// `db_path` is the absolute path the errors name.
fn refuse_another_spelling(
    db: &ReadOnlyDb,
    db_path: &Path,
    repo: &RepoName,
) -> Result<(), StatusError> {
    let stored = stored_gate_configs_read_only(db).map_err(|err| match err {
        StoredGateConfigsError::Read(err) => StatusError::from_read(db_path, err),
        StoredGateConfigsError::LimitReached { limit } => StatusError::TooManyStoredConfigs {
            asked: repo.clone(),
            db: db_path.to_path_buf(),
            limit,
        },
    })?;
    let mut near: Vec<(String, RepoIdentity)> = Vec::new();
    let mut broken: Vec<BrokenGateConfig> = Vec::new();
    for row in stored {
        match row {
            StoredGateConfig::Parsed(name) => {
                let identity = RepoIdentity::of_stored(&name)
                    .filter(|identity| name != repo.as_str() && repo.identity.may_be(identity));
                if let Some(identity) = identity {
                    near.push((name, identity));
                }
            }
            StoredGateConfig::Broken(row) => {
                if repo.may_be_stored_as(&row) {
                    broken.push(row);
                }
            }
        }
    }
    if !near.is_empty() {
        // Only one side naming a host is the one case that may be two
        // repositories, so the error says what is true if it is.
        let host_ambiguous = near
            .iter()
            .any(|(_, identity)| identity.host.is_some() != repo.identity.host.is_some());
        return Err(StatusError::StoredUnderAnotherName {
            asked: repo.clone(),
            stored: near.into_iter().map(|(stored, _)| stored).collect(),
            host_ambiguous,
        });
    }
    if !broken.is_empty() {
        return Err(StatusError::BrokenUnderAnotherName {
            asked: repo.clone(),
            db: db_path.to_path_buf(),
            rows: broken,
        });
    }
    Ok(())
}

/// Read `repo`'s gate config, whatever state it is in. `Ok(None)` is no
/// config stored under this exact name; an `Err` is a read that failed, a
/// stored config that does not parse, or one that belongs to another repo
/// name. `db_path` is the absolute path the errors name.
fn gate_config_status(
    db: &ReadOnlyDb,
    db_path: &Path,
    repo: &RepoName,
) -> Result<Option<GateConfigStatus>, StatusError> {
    let config = match get_gate_config_read_only(db, repo.as_str()) {
        Ok(Some(config)) => config,
        Ok(None) => return Ok(None),
        // The read parses nothing but the stored config, so a JSON error is
        // that config's.
        Err(MemoryError::Json(source)) => {
            return Err(StatusError::StoredConfigDoesNotParse {
                repo: repo.clone(),
                db: db_path.to_path_buf(),
                source,
            });
        }
        Err(err) => return Err(StatusError::from_read(db_path, err)),
    };
    if config.repo != repo.as_str() {
        return Err(StatusError::KeyCollision {
            asked: repo.clone(),
            stored: config.repo,
        });
    }
    Ok(Some(GateConfigStatus::from(config)))
}

/// `arg` quoted for a POSIX shell: unchanged when it is non-empty and made only
/// of characters a shell never treats specially, otherwise in single quotes
/// with each `'` written as `'\''`.
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '@' | '-'));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The human-readable report. When `hint_db` is true (the command was run
/// with `--db`), the `onboard` and `approve` hints end with `--db` and the
/// report's absolute database path, so they act on the database that was
/// read wherever they are run from. The `labels` hint takes no `--db`.
pub fn render_text(status: &RepoStatus, hint_db: bool) -> String {
    // Printed bare in the hinted commands: a `RepoName` holds no character
    // a shell treats specially.
    let repo = &status.repo;
    let db_flag = if hint_db {
        format!(" --db {}", shell_quote(&status.db))
    } else {
        String::new()
    };
    let mut out = String::new();
    match &status.gate_config {
        None => out.push_str(&format!(
            "{repo}: not onboarded — no gate config. Run `ironmem autopilot onboard \
             {repo} --path .{db_flag}` from the repo's checkout first.\n"
        )),
        Some(config) => {
            // Everything that depends on whether the config is approved is
            // decided by this one match on `state`, as the Lead decides it,
            // so a row whose `approved_at` disagrees with its `state` cannot
            // print a headline and an approval line that contradict.
            let (warnings_heading, approval) = match config.state {
                GateConfigState::Pending => {
                    let mut lines = format!(
                        "  approved: not yet — run `ironmem autopilot approve \
                         {repo}{db_flag}`\n"
                    );
                    if let Some(at) = &config.approved_at {
                        lines.push_str(&format!(
                            "  note: the stored config is pending but carries an approval time \
                             ({at}), which a pending config should not have\n"
                        ));
                    }
                    ("  warnings (read these before approving):\n", lines)
                }
                GateConfigState::Approved => (
                    "  warnings recorded on the proposal:\n",
                    match &config.approved_at {
                        Some(at) => format!("  approved: {at}\n"),
                        None => "  approved: time not recorded\n".to_string(),
                    },
                ),
            };
            let state = config.state.as_str();
            out.push_str(&format!("{repo}: gate config {state}\n"));
            for command in &config.gate_commands {
                out.push_str(&format!("  - {command}\n"));
            }
            if !config.manifest_warnings.is_empty() {
                out.push_str(warnings_heading);
                for warning in &config.manifest_warnings {
                    out.push_str(&format!("    - {warning}\n"));
                }
            }
            out.push_str(&format!("  proposed: {}\n", config.proposed_at));
            out.push_str(&approval);
        }
    }
    match &status.labels {
        LabelStatus::Failed { error } => {
            out.push_str(&format!("Labels: could not be read — {error}\n"));
        }
        LabelStatus::Ok(LabelSet { agent, risk }) => {
            out.push_str("Labels:\n");
            for (label, present) in agent {
                let word = if *present { "present" } else { "MISSING" };
                out.push_str(&format!("  {label} — {word}\n"));
            }
            if risk.is_empty() {
                out.push_str("  risk:* — none\n");
            } else {
                out.push_str(&format!("  risk:* — {}\n", risk.join(", ")));
            }
            if agent.values().any(|present| !present) {
                out.push_str(&format!(
                    "  create the missing agent:* labels with `ironmem autopilot labels \
                     {repo}`\n"
                ));
            }
        }
    }
    out.push_str(&format!("Database: {}\n", status.db));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gate_config::testing::{store_gate_configs, store_raw_gate_config};
    use crate::autopilot::gate_config::{approve_gate_config, propose_gate_config};
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::{label_list_argv, GhOutput};
    use crate::autopilot::testing::on_disk_db;

    fn repo(name: &str) -> RepoName {
        RepoName::parse(name).unwrap()
    }

    /// The report for `owner/repo` read from `db`, without a database.
    fn status_of(db: &str, config: Option<GateConfigStatus>, labels: LabelStatus) -> RepoStatus {
        RepoStatus::new(
            GateConfigRead {
                repo: repo("owner/repo"),
                db: db.to_string(),
                config,
            },
            labels,
        )
    }

    fn label_json(names: &[&str]) -> String {
        let rows: Vec<serde_json::Value> = names
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect();
        serde_json::to_string(&rows).unwrap()
    }

    fn read_labels(names: &[&str]) -> LabelStatus {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok(&label_json(names)))]);
        let status = LabelStatus::read(&mut gh, &repo("owner/repo"));
        assert_eq!(gh.seen, vec![label_list_argv("owner/repo")]);
        status
    }

    fn agent_map(ready: bool, blocked: bool, exhausted: bool) -> BTreeMap<&'static str, bool> {
        BTreeMap::from([
            ("agent:ready", ready),
            ("agent:blocked", blocked),
            ("agent:exhausted", exhausted),
        ])
    }

    // ── labels ──────────────────────────────────────────────────────────

    #[test]
    fn a_repo_with_every_label_reports_each_present_and_its_risk_labels_sorted() {
        let status = read_labels(&[
            "bug",
            "agent:exhausted",
            "risk:logic",
            "agent:ready",
            "risk:documentation",
            "agent:blocked",
        ]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(true, true, true),
                risk: vec!["risk:documentation".into(), "risk:logic".into()],
            })
        );
    }

    #[test]
    fn a_missing_agent_label_is_reported_false_not_left_out() {
        let status = read_labels(&["agent:ready", "agent:blocked"]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(true, true, false),
                risk: vec![],
            })
        );
    }

    #[test]
    fn labels_match_by_exact_name_not_by_resemblance() {
        // Everything a fuzzy `--search "agent:"` or `--search "risk:"` could
        // return without being the label asked about.
        let status = read_labels(&[
            "agent:ready-ish",
            "not-agent:ready",
            "agent:something-else",
            "sev:critical",
            "xrisk:logic",
            "risk:",
            "risky",
        ]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(false, false, false),
                risk: vec![],
            })
        );
    }

    #[test]
    fn a_differently_cased_label_is_the_same_label() {
        // GitHub label names are case-insensitive for uniqueness, and
        // `AgentLabel::from_label_str` already reads them that way.
        let status = read_labels(&["Agent:Ready", "Risk:Logic"]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(true, false, false),
                risk: vec!["Risk:Logic".into()],
            })
        );
    }

    #[test]
    fn a_refused_gh_is_a_failed_read_that_carries_the_error() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed(
            "",
            "HTTP 401: Bad credentials (https://api.github.com/graphql)",
        ))]);

        let status = LabelStatus::read(&mut gh, &repo("owner/repo"));

        // The error as `gh.rs` words it, with no "Not found:" in front: a
        // refused `gh` has not said the repo is missing.
        assert_eq!(
            status,
            LabelStatus::failed(
                "gh label list on owner/repo failed (exit 1): HTTP 401: Bad credentials \
                 (https://api.github.com/graphql) — if the cause is authentication, check `gh \
                 auth status`"
            )
        );
    }

    #[test]
    fn a_label_list_that_may_be_cut_off_is_a_failed_read_with_no_variant_prefix() {
        let names: Vec<String> = (0..1000).map(|n| format!("label-{n}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok(&label_json(&names)))]);

        let status = LabelStatus::read(&mut gh, &repo("owner/repo"));

        match status {
            LabelStatus::Failed { error } => {
                assert!(
                    error.starts_with("gh label list on owner/repo returned 1000 labels"),
                    "{error}"
                );
            }
            other => panic!("a list at the limit must be a failed read, got {other:?}"),
        }
    }

    #[test]
    fn a_gh_that_cannot_start_is_a_failed_read() {
        let mut gh = ScriptedGh::new(vec![Err(MemoryError::NotFound(
            "failed to start gh label list".into(),
        ))]);
        let status = LabelStatus::read(&mut gh, &repo("owner/repo"));
        assert_eq!(status, LabelStatus::failed("failed to start gh label list"));
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_a_failed_read_without_asking_gh() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        let file = dir.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            LabelStatus::read_in(&file, &repo("owner/repo")),
            LabelStatus::failed(format!(
                "--path '{}' is not a directory, so gh was not run",
                file.display()
            ))
        );
        // A path that cannot be examined is reported with the reason, not
        // called "not a directory".
        let reason = std::fs::metadata(&missing).unwrap_err();
        assert_eq!(
            LabelStatus::read_in(&missing, &repo("owner/repo")),
            LabelStatus::failed(format!(
                "--path '{}' cannot be used as the directory gh runs in ({reason}), so gh was \
                 not run",
                missing.display()
            ))
        );
    }

    #[test]
    fn a_missing_gh_is_a_failed_read_with_no_variant_prefix() {
        let found = MemoryError::NotFound("could not find `gh` on PATH".into());
        assert_eq!(bare_message(found), "could not find `gh` on PATH");
        let invalid = MemoryError::Validation("could not parse the list".into());
        assert_eq!(bare_message(invalid), "could not parse the list");
        // Any other variant keeps its prefix: it is not a label-read wording.
        let config = MemoryError::Config("bad setting".into());
        assert_eq!(bare_message(config), "Config error: bad setting");
    }

    #[test]
    fn a_failed_read_serializes_with_no_label_answers_at_all() {
        // No `agent` and no `risk` key: "could not read" must not be
        // readable as "missing".
        assert_eq!(
            serde_json::to_value(LabelStatus::failed("boom")).unwrap(),
            serde_json::json!({ "read": "failed", "error": "boom" })
        );
    }

    // ── gate config ─────────────────────────────────────────────────────

    #[test]
    fn an_approved_config_reports_its_state_commands_and_both_timestamps() {
        let (_dir, path) = on_disk_db();
        let stored = {
            let db = Database::open(&path).unwrap();
            propose_gate_config(
                &db,
                "owner/repo",
                vec!["cargo fmt --all -- --check".into(), "cargo test".into()],
                vec![],
            )
            .unwrap();
            approve_gate_config(&db, "owner/repo").unwrap()
        };

        let ro = Database::open_read_only(&path).unwrap();
        let status = gate_config_status(&ro, &path, &repo("owner/repo"))
            .unwrap()
            .unwrap();

        assert_eq!(status.state, GateConfigState::Approved);
        assert_eq!(
            status.gate_commands,
            vec![
                "cargo fmt --all -- --check".to_string(),
                "cargo test".to_string()
            ]
        );
        assert_eq!(status.manifest_warnings, Vec::<String>::new());
        assert_eq!(status.proposed_at, stored.proposed_at);
        assert_eq!(status.approved_at, stored.approved_at);
        assert!(status.approved_at.is_some());
    }

    #[test]
    fn a_pending_config_reports_pending_with_no_approval_time() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "owner/repo", vec!["cargo test".into()], vec![]).unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let status = gate_config_status(&ro, &path, &repo("owner/repo"))
            .unwrap()
            .unwrap();

        assert_eq!(status.state, GateConfigState::Pending);
        assert_eq!(status.approved_at, None);
    }

    #[test]
    fn a_repo_never_onboarded_has_no_gate_config_status() {
        let (_dir, path) = on_disk_db();
        let ro = Database::open_read_only(&path).unwrap();
        assert_eq!(
            gate_config_status(&ro, &path, &repo("owner/repo")).unwrap(),
            None
        );
    }

    #[test]
    fn a_parsed_repo_name_keeps_the_spelling_it_was_given() {
        let name = repo("GitHub.com/Owner/My.Repo_2");
        assert_eq!(name.as_str(), "GitHub.com/Owner/My.Repo_2");
        assert_eq!(name.to_string(), "GitHub.com/Owner/My.Repo_2");
        assert_eq!(
            serde_json::to_value(&name).unwrap(),
            serde_json::json!("GitHub.com/Owner/My.Repo_2")
        );
    }

    #[test]
    fn an_empty_repo_string_is_refused() {
        let err = RepoName::parse("  ").unwrap_err();
        assert!(
            matches!(err, StatusError::Other(MemoryError::Validation(_))),
            "{err:?}"
        );
    }

    #[test]
    fn a_malformed_repo_name_is_refused_not_reported_as_not_onboarded() {
        let (_dir, path) = on_disk_db();
        for bad in [
            "not a repo",
            "ironmem",
            "owner/",
            "/name",
            "a/b/c/d",
            "owner /name",
            "-R/x",
            "owner/-x",
            "owner/re$po;x",
            "owner/it's",
            "owner/repo\\n",
            // A part made only of dots is a directory reference, never a
            // GitHub owner or repo name.
            "../repo",
            "owner/..",
            "./.",
            "owner/.",
            "owner/...",
            "../owner/repo",
            "github.com/../repo",
        ] {
            let err = RepoName::parse(bad).unwrap_err();
            assert!(
                matches!(&err, StatusError::NotARepoName { given } if given == bad),
                "{bad}: {err:?}"
            );
            let message = err.to_string();
            assert!(message.contains("is not a repo name"), "{bad}: {message}");
            assert!(
                message.contains("not starting with `-`"),
                "{bad}: {message}"
            );
            assert!(message.contains("not made only of `.`"), "{bad}: {message}");
            assert!(
                matches!(MemoryError::from(err), MemoryError::Validation(_)),
                "{bad}"
            );
        }
        for good in [
            "owner/repo",
            "my-org/my.repo_2",
            "ghe.example.com/owner/repo",
            // Dots beside other characters are ordinary names.
            "owner/.github",
            "owner/a..b",
            "owner/repo.",
        ] {
            assert_eq!(
                read_gate_config_status(&path, &repo(good)).unwrap().config,
                None,
                "{good}"
            );
        }
    }

    #[test]
    fn reading_by_path_explains_a_file_that_is_not_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        std::fs::write(
            &path,
            "this is not sqlite, it is a text file of some length\n".repeat(50),
        )
        .unwrap();
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(
                &err,
                StatusError::Database {
                    path: named,
                    hint: Some(DbHint::NotAnIronmemDatabase),
                    ..
                } if named == &path
            ),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("could not read the database at"),
            "{message}"
        );
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("never writes it"), "{message}");
        assert!(
            message.contains("pass the right one with --db"),
            "{message}"
        );
        assert!(!message.contains("-wal/-shm"), "{message}");
    }

    #[test]
    fn an_empty_sqlite_file_is_explained_as_not_an_ironmem_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.sqlite3");
        rusqlite::Connection::open(&path).unwrap();
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(
                &err,
                StatusError::Database {
                    hint: Some(DbHint::NotAnIronmemDatabase),
                    source: rusqlite::Error::SqliteFailure(_, Some(detail)),
                    ..
                } if detail.starts_with("no such table")
            ),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("no such table"), "{message}");
        assert!(
            message.contains("pass the right one with --db"),
            "{message}"
        );
        assert!(!message.contains("-wal/-shm"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_database_file_that_cannot_be_opened_is_not_reported_as_missing() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, path) = on_disk_db();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root, the file opens anyway and the case cannot arise.
        if std::fs::File::open(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            eprintln!("skipped: running as root, a file mode does not stop the open");
            return;
        }
        let result = read_gate_config_status(&path, &repo("owner/repo"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = result.unwrap_err();
        assert!(
            matches!(&err, StatusError::CannotOpen { path: named } if named == &path),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("could not open the database at"),
            "{message}"
        );
        assert!(
            message.contains("the file exists but could not be opened"),
            "{message}"
        );
        assert!(!message.contains("db not found"), "{message}");
    }

    #[test]
    fn a_read_only_failure_gets_the_wal_sentence_and_a_locked_one_gets_neither() {
        let readonly = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
            Some("attempt to write a readonly database".into()),
        );
        let err = StatusError::from_read(Path::new("/x/m.db"), MemoryError::Db(readonly));
        assert!(
            matches!(
                err,
                StatusError::Database {
                    hint: Some(DbHint::WalSideFiles),
                    ..
                }
            ),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("-wal/-shm"), "{message}");
        assert!(!message.contains("pass the right one"), "{message}");

        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let err = StatusError::from_read(Path::new("/x/m.db"), MemoryError::Db(busy));
        assert!(
            matches!(err, StatusError::Database { hint: None, .. }),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("database is locked"), "{message}");
        assert!(!message.contains("-wal/-shm"), "{message}");
        assert!(!message.contains("pass the right one"), "{message}");
    }

    #[test]
    fn a_relative_database_path_is_reported_as_an_absolute_one() {
        let db = absolute_db_path(Path::new("m.db")).unwrap();
        assert!(Path::new(&db).is_absolute(), "{db}");
        assert!(db.ends_with("m.db"), "{db}");
    }

    #[test]
    fn a_repo_padded_with_whitespace_is_refused() {
        for bad in [" owner/repo", "owner/repo "] {
            assert!(
                matches!(RepoName::parse(bad), Err(StatusError::NotARepoName { .. })),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_missing_database_is_not_found_and_the_error_says_how_to_name_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sqlite3");
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(&err, StatusError::DatabaseNotFound { path: named } if named == &path),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.starts_with(&format!("db not found at {}", path.display())),
            "{message}"
        );
        assert!(message.contains("`--db <path>`"), "{message}");
        assert!(message.contains("`IRONMEM_DB_PATH`"), "{message}");
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::NotFound(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("Not found: {message}"));
        assert!(!path.exists());
    }

    #[test]
    fn an_error_names_a_relative_database_path_by_its_absolute_path() {
        let relative = Path::new("no-such-dir-420/absent.sqlite3");
        let err = read_gate_config_status(relative, &repo("owner/repo")).unwrap_err();
        match err {
            StatusError::DatabaseNotFound { path } => {
                assert!(path.is_absolute(), "{}", path.display());
                assert!(path.ends_with(relative), "{}", path.display());
            }
            other => panic!("expected a missing database, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_database_whose_directory_cannot_be_searched_is_not_reported_as_missing() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, path) = on_disk_db();
        let locked = std::fs::Permissions::from_mode(0o000);
        let open = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir.path(), locked).unwrap();
        if std::fs::metadata(&path).is_ok() {
            std::fs::set_permissions(dir.path(), open).unwrap();
            eprintln!("skipped: running as root, a directory mode does not stop the read");
            return;
        }
        let result = read_gate_config_status(&path, &repo("owner/repo"));
        std::fs::set_permissions(dir.path(), open).unwrap();
        let err = result.unwrap_err();
        assert!(
            matches!(
                &err,
                StatusError::DatabaseUnreachable { path: named, source }
                    if named == &path && source.kind() == std::io::ErrorKind::PermissionDenied
            ),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "could not reach the database at {}: ",
                path.display()
            )),
            "{message}"
        );
        assert!(!message.contains("not found"), "{message}");
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Io(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("IO error: {message}"));
    }

    #[test]
    fn reading_by_path_reports_a_stored_config() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "owner/repo", vec!["cargo test".into()], vec![]).unwrap();
        }
        let read = read_gate_config_status(&path, &repo("owner/repo")).unwrap();
        assert_eq!(read.repo.as_str(), "owner/repo");
        assert_eq!(read.db, path.display().to_string());
        assert_eq!(
            read.config.unwrap().gate_commands,
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn the_text_report_ends_with_the_database_that_answered() {
        let status = status_of("/tmp/mem.db", None, read_labels(&[]));
        let text = render_text(&status, false);
        assert!(text.ends_with("Database: /tmp/mem.db\n"), "{text}");
    }

    // ── a stored config that does not parse ─────────────────────────────

    /// Row content that is JSON and names `repo`, and fails `GateConfig`'s
    /// guard: it has no gate commands.
    fn no_gate(repo: &str) -> String {
        serde_json::json!({
            "repo": repo,
            "state": "approved",
            "gate_commands": [],
            "proposed_at": "2026-01-01T00:00:00+00:00",
            "approved_at": null,
        })
        .to_string()
    }

    /// A database whose only gate config row is `content`, stored under the
    /// key of `stored`.
    fn db_with_raw_config(stored: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let (dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            store_raw_gate_config(&db, stored, content);
        }
        (dir, path)
    }

    #[test]
    fn a_stored_config_that_does_not_parse_names_the_repo_the_database_and_the_remedy() {
        let (_dir, path) = db_with_raw_config("owner/repo", &no_gate("owner/repo"));

        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();

        match &err {
            StatusError::StoredConfigDoesNotParse {
                repo: named,
                db,
                source,
            } => {
                assert_eq!(named.as_str(), "owner/repo");
                assert_eq!(db, &path);
                assert!(
                    source
                        .to_string()
                        .contains(crate::autopilot::EMPTY_GATE_COMMANDS_MSG),
                    "{source}"
                );
            }
            other => panic!("expected the parse failure, got {other:?}"),
        }
        let message = err.to_string();
        let shown_db = path.display().to_string();
        assert!(
            message.starts_with(&format!(
                "the gate config stored for 'owner/repo' in {shown_db} does not parse: "
            )),
            "{message}"
        );
        assert!(
            message.contains(crate::autopilot::EMPTY_GATE_COMMANDS_MSG),
            "{message}"
        );
        let quoted = shell_quote(&shown_db);
        assert!(
            message.ends_with(&format!(
                "To replace it, run `ironmem autopilot onboard owner/repo --path . --db \
                 {quoted}` from the repo's checkout, then `ironmem autopilot approve owner/repo \
                 --db {quoted}`"
            )),
            "{message}"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Json(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("JSON error: {message}"));
    }

    #[test]
    fn a_broken_config_under_another_spelling_is_an_error_not_a_repo_never_onboarded() {
        // (the name the row was written for, its content, the name asked
        // about, the row's storage key, the `repo` the content still holds)
        let cases = [
            (
                "IronRace/ironmem",
                no_gate("IronRace/ironmem"),
                "ironrace/ironmem",
                "gate-config:IronRace-ironmem",
                Some("IronRace/ironmem"),
            ),
            (
                "github.com/owner/repo",
                no_gate("github.com/owner/repo"),
                "Owner/Repo",
                "gate-config:github.com-owner-repo",
                Some("github.com/owner/repo"),
            ),
            (
                "Owner/Repo",
                "not json".to_string(),
                "owner/repo",
                "gate-config:Owner-Repo",
                None,
            ),
            (
                "github.com/owner/repo",
                "not json".to_string(),
                "owner/repo",
                "gate-config:github.com-owner-repo",
                None,
            ),
            (
                "owner/repo",
                "not json".to_string(),
                "GitHub.com/Owner/Repo",
                "gate-config:owner-repo",
                None,
            ),
        ];
        for (stored, content, asked, key, held) in cases {
            let (_dir, path) = db_with_raw_config(stored, &content);

            let err = match read_gate_config_status(&path, &repo(asked)) {
                Err(err) => err,
                Ok(read) => panic!("{stored} / {asked}: expected an error, got {read:?}"),
            };

            let parse_error = match &err {
                StatusError::BrokenUnderAnotherName {
                    asked: named,
                    db,
                    rows,
                } => {
                    assert_eq!(named.as_str(), asked);
                    assert_eq!(db, &path);
                    assert_eq!(rows.len(), 1, "{rows:?}");
                    assert_eq!(rows[0].storage_key, key);
                    assert_eq!(rows[0].repo.as_deref(), held);
                    rows[0].error.to_string()
                }
                other => panic!("{stored} / {asked}: expected the broken row, got {other:?}"),
            };
            let message = err.to_string();
            assert!(message.contains(&format!("'{asked}'")), "{message}");
            assert!(
                message.contains(&format!("stored under the key '{key}'")),
                "{message}"
            );
            assert!(message.contains(&parse_error), "{message}");
            assert!(message.contains(&path.display().to_string()), "{message}");
            if let Some(held) = held {
                assert!(message.contains(&format!("{held:?}")), "{message}");
            }
            let shown = MemoryError::from(err);
            assert!(matches!(shown, MemoryError::Json(_)), "{shown:?}");
            assert_eq!(shown.to_string(), format!("JSON error: {message}"));
        }
    }

    #[test]
    fn a_broken_config_of_another_repo_does_not_hide_this_repos_answer() {
        for (stored, content, asked) in [
            ("other/thing", "not json".to_string(), "owner/repo"),
            ("other/thing", no_gate("other/thing"), "owner/repo"),
            ("owner/repo2", "not json".to_string(), "owner/repo"),
            // Two different hosts are two repositories.
            (
                "ghe.example.com/owner/repo",
                "not json".to_string(),
                "github.com/owner/repo",
            ),
            (
                "ghe.example.com/owner/repo",
                no_gate("ghe.example.com/owner/repo"),
                "github.com/Owner/Repo",
            ),
        ] {
            let (_dir, path) = db_with_raw_config(stored, &content);
            assert_eq!(
                read_gate_config_status(&path, &repo(asked)).unwrap().config,
                None,
                "{stored} / {asked}"
            );
        }
    }

    #[test]
    fn a_broken_row_is_judged_by_the_name_it_holds_and_by_its_key_only_without_one() {
        // `foo-bar/baz` is stored under the slug `foo-bar-baz`, which is also
        // what `bar/baz` on a host named `foo` would be stored under.
        let (_dir, path) = db_with_raw_config("foo-bar/baz", &no_gate("foo-bar/baz"));
        assert_eq!(
            read_gate_config_status(&path, &repo("bar/baz"))
                .unwrap()
                .config,
            None,
            "the row names another repo"
        );

        // With no name to read, the slug is all there is, and the read fails
        // rather than report a repo that may be onboarded as not onboarded.
        let (_dir, path) = db_with_raw_config("foo-bar/baz", "not json");
        let err = read_gate_config_status(&path, &repo("bar/baz")).unwrap_err();
        assert!(
            matches!(&err, StatusError::BrokenUnderAnotherName { rows, .. }
                if rows.len() == 1 && rows[0].storage_key == "gate-config:foo-bar-baz"),
            "{err:?}"
        );

        // A `repo` that names no owner cannot be compared as a name either.
        let (_dir, path) = db_with_raw_config("Owner/Repo", &no_gate("repo"));
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(&err, StatusError::BrokenUnderAnotherName { rows, .. }
                if rows.len() == 1 && rows[0].repo.as_deref() == Some("repo")),
            "{err:?}"
        );
    }

    #[test]
    fn a_config_that_parses_under_another_spelling_is_named_before_a_broken_one() {
        let (_dir, path) = db_with_configs(&["owner/repo"]);
        {
            let db = Database::open(&path).unwrap();
            store_raw_gate_config(&db, "github.com/owner/repo", "not json");
        }
        let near = near_miss_at(&path, "Owner/Repo");
        assert_eq!(near.stored, vec!["owner/repo".to_string()]);
    }

    #[test]
    fn slugs_match_in_any_letter_case_and_with_a_host_in_front_of_either() {
        assert!(slugs_may_match("owner-repo", "Owner-Repo"));
        assert!(slugs_may_match("github.com-owner-repo", "Owner-Repo"));
        assert!(slugs_may_match("owner-repo", "GHE.example.com-owner-repo"));
        assert!(!slugs_may_match("owner-repo", "owner-repo2"));
        assert!(!slugs_may_match("owner-repo", "xowner-repo"));
        assert!(!slugs_may_match(
            "github.com-owner-repo",
            "ghe.example.com-owner-repo"
        ));
    }

    // ── more stored configs than can be compared ────────────────────────

    #[test]
    fn too_many_stored_configs_is_an_error_that_says_what_to_do() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            store_gate_configs(&db, 1000);
        }

        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();

        match &err {
            StatusError::TooManyStoredConfigs { asked, db, limit } => {
                assert_eq!(asked.as_str(), "owner/repo");
                assert_eq!(db, &path);
                assert_eq!(*limit, 1000);
            }
            other => panic!("expected the limit, got {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("'owner/repo'"), "{message}");
        assert!(
            message.contains(&format!(
                "{} holds 1000 or more gate configs",
                path.display()
            )),
            "{message}"
        );
        assert!(
            message.contains("ask with the exact name it was onboarded under"),
            "{message}"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Validation(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("Validation error: {message}"));

        // What the message tells the reader to do works: an exact name is
        // read without the listing.
        let read = read_gate_config_status(&path, &repo("many/repo-7")).unwrap();
        assert!(read.config.is_some());
    }

    /// A database holding a gate config for each of `stored`.
    fn db_with_configs(stored: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            for repo in stored {
                propose_gate_config(&db, repo, vec!["cargo test".into()], vec![]).unwrap();
            }
        }
        (dir, path)
    }

    /// What a near-miss error carries, and what it says.
    struct NearMiss {
        stored: Vec<String>,
        host_ambiguous: bool,
        message: String,
    }

    /// The near-miss a read of `asked` gets from the database at `path`.
    fn near_miss_at(path: &Path, asked: &str) -> NearMiss {
        let err = match read_gate_config_status(path, &repo(asked)) {
            Err(err) => err,
            Ok(read) => panic!("{asked}: expected the near-miss error, got {read:?}"),
        };
        let message = err.to_string();
        match err {
            StatusError::StoredUnderAnotherName {
                asked: named,
                stored,
                host_ambiguous,
            } => {
                assert_eq!(named.as_str(), asked);
                NearMiss {
                    stored,
                    host_ambiguous,
                    message,
                }
            }
            other => panic!("{asked}: expected the near-miss error, got {other:?}"),
        }
    }

    /// The near-miss a read of `asked` gets against a database holding `stored`.
    fn near_miss(stored: &[&str], asked: &str) -> NearMiss {
        let (_dir, path) = db_with_configs(stored);
        near_miss_at(&path, asked)
    }

    #[test]
    fn a_differently_cased_or_host_prefixed_name_is_refused_with_the_stored_one() {
        for (stored, asked) in [
            ("ironrace/ironmem", "IronRace/IronMem"),
            ("ironrace/ironmem", "github.com/ironrace/ironmem"),
            ("ghe.example.com/ironrace/ironmem", "ironrace/ironmem"),
        ] {
            let NearMiss {
                stored: named,
                message,
                ..
            } = near_miss(&[stored], asked);
            assert_eq!(named, vec![stored.to_string()]);
            assert!(message.contains(&format!("'{asked}'")), "{message}");
            assert!(message.contains(&format!("'{stored}'")), "{message}");
            assert!(message.contains("ask with the stored name"), "{message}");
        }
    }

    #[test]
    fn every_matching_stored_name_is_listed() {
        let NearMiss {
            mut stored,
            message,
            ..
        } = near_miss(&["owner/repo", "github.com/owner/repo"], "OWNER/REPO");
        stored.sort();
        assert_eq!(
            stored,
            vec![
                "github.com/owner/repo".to_string(),
                "owner/repo".to_string()
            ]
        );
        assert!(message.contains("'owner/repo'"), "{message}");
        assert!(message.contains("'github.com/owner/repo'"), "{message}");
    }

    #[test]
    fn the_exact_name_wins_over_a_differently_cased_one() {
        let (_dir, path) = db_with_configs(&["owner/repo", "Owner/Repo"]);
        let read = read_gate_config_status(&path, &repo("owner/repo")).unwrap();
        assert!(read.config.is_some());
    }

    #[test]
    fn an_unrelated_or_longer_named_repo_is_not_a_near_miss() {
        let (_dir, path) = db_with_configs(&["other/thing", "owner/repo"]);
        assert_eq!(
            read_gate_config_status(&path, &repo("owner/repo2"))
                .unwrap()
                .config,
            None
        );
        assert_eq!(
            read_gate_config_status(&path, &repo("xowner/repo"))
                .unwrap()
                .config,
            None
        );
        let (_dir, path) = db_with_configs(&["other/thing"]);
        assert_eq!(
            read_gate_config_status(&path, &repo("owner/repo"))
                .unwrap()
                .config,
            None
        );
    }

    #[test]
    fn two_names_with_different_hosts_are_different_repositories() {
        let (_dir, path) = db_with_configs(&["ghe.example.com/owner/repo"]);
        for asked in ["github.com/owner/repo", "github.com/Owner/Repo"] {
            assert_eq!(
                read_gate_config_status(&path, &repo(asked)).unwrap().config,
                None,
                "{asked}"
            );
        }
    }

    #[test]
    fn the_same_host_in_another_letter_case_is_a_near_miss() {
        let near = near_miss(&["github.com/owner/repo"], "GitHub.com/Owner/repo");
        assert_eq!(near.stored, vec!["github.com/owner/repo".to_string()]);
        assert!(!near.host_ambiguous);
        assert!(
            near.message.contains("'github.com/owner/repo'"),
            "{}",
            near.message
        );
    }

    #[test]
    fn a_near_miss_that_could_be_another_host_says_so() {
        // `gh` reads a name without a host against its default host, which
        // `GH_HOST` changes, so only one side naming a host is ambiguous.
        for (stored, asked) in [
            ("ghe.example.com/owner/repo", "owner/repo"),
            ("owner/repo", "ghe.example.com/owner/repo"),
        ] {
            let NearMiss {
                host_ambiguous,
                message,
                ..
            } = near_miss(&[stored], asked);
            assert!(host_ambiguous, "{stored} / {asked}");
            assert!(
                message.contains(&format!(
                    "if '{asked}' is a different repository on another host, it is not onboarded"
                )),
                "{message}"
            );
        }
        // Letter case alone is never another host.
        for (stored, asked) in [
            ("owner/repo", "Owner/Repo"),
            ("github.com/owner/repo", "github.com/Owner/Repo"),
        ] {
            let NearMiss {
                host_ambiguous,
                message,
                ..
            } = near_miss(&[stored], asked);
            assert!(!host_ambiguous, "{stored} / {asked}");
            assert!(!message.contains("another host"), "{message}");
        }
    }

    #[test]
    fn a_superseded_config_row_reads_the_same_by_name_and_as_a_near_miss() {
        // Autopilot never supersedes a gate config, but `add_drawer`'s
        // `supersedes` can mark any row. The read by name is a lookup by id
        // that ignores the mark, so the listing must ignore it too, or the two
        // would disagree about one row.
        let (_dir, path) = db_with_configs(&["owner/repo"]);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let changed = conn
                .execute(
                    "UPDATE drawers SET superseded_by = 'some-successor' \
                     WHERE wing = 'autopilot' AND room = 'backlog-lineage' \
                     AND source_file LIKE 'logical:gate-config:%'",
                    [],
                )
                .unwrap();
            assert_eq!(changed, 1, "the UPDATE must hit the gate config row");
        }

        assert!(read_gate_config_status(&path, &repo("owner/repo"))
            .unwrap()
            .config
            .is_some());
        let near = near_miss_at(&path, "Owner/Repo");
        assert_eq!(near.stored, vec!["owner/repo".to_string()]);
        assert!(near.message.contains("'owner/repo'"), "{}", near.message);
    }

    #[test]
    fn a_git_suffix_is_refused_with_a_message_to_drop_it() {
        for bad in [
            "owner/repo.git",
            "owner/repo.GIT",
            "github.com/owner/repo.git",
        ] {
            let err = RepoName::parse(bad).unwrap_err();
            assert!(
                matches!(&err, StatusError::GitSuffix { given } if given == bad),
                "{bad}: {err:?}"
            );
            let message = err.to_string();
            assert!(
                message.contains("drop the `.git` suffix"),
                "{bad}: {message}"
            );
            assert!(
                matches!(MemoryError::from(err), MemoryError::Validation(_)),
                "{bad}"
            );
        }
        // A name that merely contains `.git` is fine.
        let (_dir, path) = on_disk_db();
        assert_eq!(
            read_gate_config_status(&path, &repo("owner/my.github"))
                .unwrap()
                .config,
            None
        );
    }

    #[test]
    fn a_config_stored_under_a_colliding_key_is_refused() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(&db, "foo-bar/baz", vec!["cargo test".into()], vec![]).unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let err = gate_config_status(&ro, &path, &repo("foo/bar-baz")).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("'foo-bar/baz'"), "{message}");
        assert!(message.contains("'foo/bar-baz'"), "{message}");
        match err {
            StatusError::KeyCollision { asked, stored } => {
                assert_eq!(asked.as_str(), "foo/bar-baz");
                assert_eq!(stored, "foo-bar/baz");
            }
            other => panic!("expected the key collision, got {other:?}"),
        }
    }

    #[test]
    fn risk_labels_sort_case_insensitively_where_a_byte_sort_would_differ() {
        // A byte sort would put the capital `Z` first.
        let status = read_labels(&["Risk:Zeta", "risk:alpha"]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(false, false, false),
                risk: vec!["risk:alpha".into(), "Risk:Zeta".into()],
            })
        );
    }

    // ── the report ──────────────────────────────────────────────────────

    #[test]
    fn a_repo_that_is_not_onboarded_serializes_a_null_gate_config() {
        let status = status_of("/tmp/mem.db", None, LabelStatus::failed("boom"));
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "repo": "owner/repo",
                "db": "/tmp/mem.db",
                "onboarded": false,
                "gate_config": null,
                "labels": { "read": "failed", "error": "boom" },
            })
        );
    }

    #[test]
    fn the_text_report_says_not_onboarded_and_that_labels_were_not_read() {
        let text = render_text(
            &status_of(
                "/tmp/mem.db",
                None,
                LabelStatus::failed("gh label list on owner/repo failed (exit Some(1)): HTTP 401"),
            ),
            false,
        );
        assert!(text.contains("owner/repo: not onboarded"), "{text}");
        assert!(
            text.contains(
                "Run `ironmem autopilot onboard owner/repo --path .` from the repo's checkout first."
            ),
            "{text}"
        );
        assert!(
            text.contains("Labels: could not be read — gh label list"),
            "{text}"
        );
        assert!(
            !text.contains("MISSING"),
            "a failed read names no label: {text}"
        );
    }

    #[test]
    fn the_text_report_lists_the_gate_and_marks_a_missing_label() {
        let text = render_text(
            &status_of(
                "/tmp/mem.db",
                Some(config_status(GateConfigState::Pending, vec![], None)),
                read_labels(&["agent:ready", "agent:blocked", "risk:logic"]),
            ),
            false,
        );
        assert!(text.contains("owner/repo: gate config pending"), "{text}");
        assert!(text.contains("  - cargo test"), "{text}");
        assert!(
            text.contains("ironmem autopilot approve owner/repo"),
            "{text}"
        );
        assert!(text.contains("  agent:ready — present"), "{text}");
        assert!(text.contains("  agent:exhausted — MISSING"), "{text}");
        assert!(text.contains("  risk:* — risk:logic"), "{text}");
    }

    /// The status of a stored row with these fields, read through
    /// `GateConfig`'s own guard as a stored row is.
    fn config_status(
        state: GateConfigState,
        warnings: Vec<String>,
        approved_at: Option<&str>,
    ) -> GateConfigStatus {
        let stored: GateConfig = serde_json::from_value(serde_json::json!({
            "repo": "owner/repo",
            "state": state,
            "gate_commands": ["cargo test"],
            "manifest_warnings": warnings,
            "proposed_at": "2026-01-01T00:00:00+00:00",
            "approved_at": approved_at,
        }))
        .unwrap();
        GateConfigStatus::from(stored)
    }

    #[test]
    fn stored_manifest_warnings_are_reported() {
        let (_dir, path) = on_disk_db();
        {
            let db = Database::open(&path).unwrap();
            propose_gate_config(
                &db,
                "owner/repo",
                vec!["cargo test".into()],
                vec!["manifest X could not be read".into()],
            )
            .unwrap();
        }

        let ro = Database::open_read_only(&path).unwrap();
        let status = gate_config_status(&ro, &path, &repo("owner/repo"))
            .unwrap()
            .unwrap();

        assert_eq!(
            status.manifest_warnings,
            vec!["manifest X could not be read".to_string()]
        );
    }

    #[test]
    fn the_text_report_prints_manifest_warnings_after_the_commands() {
        let text = render_text(
            &status_of(
                "/tmp/mem.db",
                Some(config_status(
                    GateConfigState::Pending,
                    vec!["manifest X could not be read".into()],
                    None,
                )),
                read_labels(&[]),
            ),
            false,
        );
        assert!(
            text.contains(
                "  - cargo test\n  warnings (read these before approving):\n    - manifest X could not be read\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_approved_config_labels_its_warnings_as_recorded_not_as_pre_approval() {
        let text = render_text(
            &status_of(
                "/tmp/mem.db",
                Some(config_status(
                    GateConfigState::Approved,
                    vec!["manifest X could not be read".into()],
                    Some("2026-02-02T00:00:00+00:00"),
                )),
                read_labels(&[]),
            ),
            false,
        );
        assert!(
            text.contains(
                "  warnings recorded on the proposal:\n    - manifest X could not be read\n"
            ),
            "{text}"
        );
        assert!(!text.contains("before approving"), "{text}");
    }

    #[test]
    fn the_text_report_has_no_warnings_block_when_there_are_none() {
        let text = render_text(
            &status_of(
                "/tmp/mem.db",
                Some(config_status(GateConfigState::Pending, vec![], None)),
                read_labels(&[]),
            ),
            false,
        );
        assert!(!text.contains("warnings"), "{text}");
    }

    #[test]
    fn a_gate_config_status_serializes_every_key() {
        assert_eq!(
            serde_json::to_value(config_status(
                GateConfigState::Pending,
                vec!["w".into()],
                None
            ))
            .unwrap(),
            serde_json::json!({
                "state": "pending",
                "gate_commands": ["cargo test"],
                "manifest_warnings": ["w"],
                "proposed_at": "2026-01-01T00:00:00+00:00",
                "approved_at": null,
            })
        );
    }

    #[test]
    fn a_full_repo_status_serializes_the_pinned_shape() {
        let status = status_of(
            "/tmp/mem.db",
            Some(config_status(GateConfigState::Pending, vec![], None)),
            read_labels(&["agent:ready", "risk:logic"]),
        );
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "repo": "owner/repo",
                "db": "/tmp/mem.db",
                "onboarded": true,
                "gate_config": {
                    "state": "pending",
                    "gate_commands": ["cargo test"],
                    "manifest_warnings": [],
                    "proposed_at": "2026-01-01T00:00:00+00:00",
                    "approved_at": null,
                },
                "labels": {
                    "read": "ok",
                    "agent": {
                        "agent:ready": true,
                        "agent:blocked": false,
                        "agent:exhausted": false,
                    },
                    "risk": ["risk:logic"],
                },
            })
        );
    }

    #[test]
    fn the_json_report_is_pinned_byte_for_byte() {
        // `serde_json::Value` equality ignores key order, and the report is
        // read as printed text, so the bytes are pinned too.
        let reported = status_of(
            "/tmp/mem.db",
            Some(config_status(
                GateConfigState::Approved,
                vec!["w".into()],
                Some("2026-02-02T00:00:00+00:00"),
            )),
            read_labels(&["agent:ready", "Risk:Logic", "risk:documentation"]),
        );
        assert_eq!(
            serde_json::to_string(&reported).unwrap(),
            concat!(
                r#"{"repo":"owner/repo","db":"/tmp/mem.db","onboarded":true,"#,
                r#""gate_config":{"state":"approved","gate_commands":["cargo test"],"#,
                r#""manifest_warnings":["w"],"proposed_at":"2026-01-01T00:00:00+00:00","#,
                r#""approved_at":"2026-02-02T00:00:00+00:00"},"#,
                r#""labels":{"read":"ok","agent":{"agent:blocked":false,"#,
                r#""agent:exhausted":false,"agent:ready":true},"#,
                r#""risk":["risk:documentation","Risk:Logic"]}}"#,
            )
        );
        let unread = status_of("/tmp/mem.db", None, LabelStatus::failed("boom"));
        assert_eq!(
            serde_json::to_string(&unread).unwrap(),
            concat!(
                r#"{"repo":"owner/repo","db":"/tmp/mem.db","onboarded":false,"#,
                r#""gate_config":null,"labels":{"read":"failed","error":"boom"}}"#,
            )
        );
    }

    #[test]
    fn the_text_report_for_an_approved_config_shows_the_approval_time() {
        let text = render_text(
            &status_of(
                "/tmp/mem.db",
                Some(config_status(
                    GateConfigState::Approved,
                    vec![],
                    Some("2026-02-02T00:00:00+00:00"),
                )),
                read_labels(&[]),
            ),
            false,
        );
        assert!(text.contains("owner/repo: gate config approved"), "{text}");
        assert!(
            text.contains("  approved: 2026-02-02T00:00:00+00:00"),
            "{text}"
        );
        assert!(!text.contains("not yet"), "{text}");
    }

    #[test]
    fn an_empty_label_list_is_a_read_with_every_label_missing() {
        let status = read_labels(&[]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(false, false, false),
                risk: vec![],
            })
        );
        let text = render_text(&status_of("/tmp/mem.db", None, status), false);
        assert!(text.contains("  risk:* — none"), "{text}");
        assert!(text.contains("  agent:ready — MISSING"), "{text}");
    }

    #[test]
    fn gh_output_that_is_not_json_is_a_failed_read() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok("this is not json"))]);
        let status = LabelStatus::read(&mut gh, &repo("owner/repo"));
        assert!(matches!(status, LabelStatus::Failed { .. }), "{status:?}");
    }

    #[test]
    fn an_unrecognized_risk_class_is_still_listed() {
        let status = read_labels(&["risk:banana"]);
        assert_eq!(
            status,
            LabelStatus::Ok(LabelSet {
                agent: agent_map(false, false, false),
                risk: vec!["risk:banana".into()],
            })
        );
    }

    #[test]
    fn hints_carry_the_db_the_command_was_run_with_quoted() {
        let status = status_of(
            "/tmp/x y.sqlite3",
            Some(config_status(GateConfigState::Pending, vec![], None)),
            read_labels(&[]),
        );
        let text = render_text(&status, true);
        assert!(
            text.contains("ironmem autopilot approve owner/repo --db '/tmp/x y.sqlite3'"),
            "{text}"
        );

        let not_onboarded = status_of("/tmp/x y.sqlite3", None, read_labels(&[]));
        let text = render_text(&not_onboarded, true);
        assert!(
            text.contains(
                "ironmem autopilot onboard owner/repo --path . --db '/tmp/x y.sqlite3'` from the repo's checkout first."
            ),
            "{text}"
        );
        assert!(!text.contains('<') && !text.contains('>'), "{text}");
    }

    #[test]
    fn a_relative_database_is_hinted_by_its_absolute_path() {
        let status = status_of(
            &absolute_db_path(Path::new("rel.sqlite3")).unwrap(),
            None,
            read_labels(&[]),
        );
        let text = render_text(&status, true);
        let absolute = std::path::absolute("rel.sqlite3").unwrap();
        assert!(
            text.contains(&format!(
                "--db {}`",
                shell_quote(&absolute.display().to_string())
            )),
            "{text}"
        );
        assert!(!text.contains("--db rel.sqlite3"), "{text}");
    }

    #[test]
    fn no_db_is_hinted_when_none_was_given() {
        let status = status_of("/tmp/mem.db", None, read_labels(&[]));
        let text = render_text(&status, false);
        assert!(!text.contains("--db"), "{text}");
        assert!(
            text.contains(
                "Run `ironmem autopilot onboard owner/repo --path .` from the repo's checkout first."
            ),
            "{text}"
        );
    }

    #[test]
    fn a_missing_agent_label_gets_a_create_hint_without_a_db() {
        let status = status_of("/tmp/mem.db", None, read_labels(&["agent:ready"]));
        let text = render_text(&status, true);
        assert!(
            text.contains(
                "  create the missing agent:* labels with `ironmem autopilot labels owner/repo`\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn no_create_hint_when_no_label_is_missing_or_the_read_failed() {
        let all = status_of(
            "/tmp/mem.db",
            None,
            read_labels(&["agent:ready", "agent:blocked", "agent:exhausted"]),
        );
        assert!(!render_text(&all, false).contains("create the missing"));
        let failed = status_of("/tmp/mem.db", None, LabelStatus::failed("boom"));
        assert!(!render_text(&failed, false).contains("create the missing"));
    }

    #[test]
    fn the_label_lines_follow_the_map_and_never_invent_a_missing_entry() {
        // A one-entry map is one no read can produce, built here by literal
        // to show the renderer prints the map and adds nothing to it.
        let status = status_of(
            "/tmp/mem.db",
            None,
            LabelStatus::Ok(LabelSet {
                agent: BTreeMap::from([("agent:ready", true)]),
                risk: vec![],
            }),
        );
        let text = render_text(&status, false);
        assert!(text.contains("  agent:ready — present"), "{text}");
        assert!(!text.contains("agent:blocked"), "{text}");
    }

    // ── a row whose state and approval time disagree ────────────────────

    #[test]
    fn an_approved_config_with_no_approval_time_says_the_time_is_not_recorded() {
        let config = config_status(GateConfigState::Approved, vec![], None);
        // The JSON reports the row as stored; the Lead dispatches on `state`.
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["state"], serde_json::json!("approved"));
        assert_eq!(json["approved_at"], serde_json::Value::Null);

        let text = render_text(
            &status_of("/tmp/mem.db", Some(config), read_labels(&[])),
            true,
        );

        assert!(
            text.contains("owner/repo: gate config approved\n"),
            "{text}"
        );
        assert!(text.contains("  approved: time not recorded\n"), "{text}");
        assert!(!text.contains("not yet"), "{text}");
        assert!(!text.contains("autopilot approve"), "{text}");
    }

    #[test]
    fn a_pending_config_that_carries_an_approval_time_is_reported_pending_with_a_note() {
        let config = config_status(
            GateConfigState::Pending,
            vec![],
            Some("2026-02-02T00:00:00+00:00"),
        );
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["state"], serde_json::json!("pending"));
        assert_eq!(
            json["approved_at"],
            serde_json::json!("2026-02-02T00:00:00+00:00")
        );

        let text = render_text(
            &status_of("/tmp/mem.db", Some(config), read_labels(&[])),
            false,
        );

        assert!(text.contains("owner/repo: gate config pending\n"), "{text}");
        assert!(
            text.contains(
                "  approved: not yet — run `ironmem autopilot approve owner/repo`\n  note: the \
                 stored config is pending but carries an approval time \
                 (2026-02-02T00:00:00+00:00), which a pending config should not have\n"
            ),
            "{text}"
        );
        assert!(!text.contains("  approved: 2026-02-02"), "{text}");
    }

    // ── a database path the report could not name ───────────────────────

    #[test]
    fn a_database_path_that_cannot_be_made_absolute_is_an_error_not_a_relative_path() {
        // An empty path is the one input `std::path::absolute` refuses
        // whatever the working directory is.
        let err = read_gate_config_status(Path::new(""), &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(&err, StatusError::DbPathNotAbsolute { path, .. } if path == Path::new("")),
            "{err:?}"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Io(_)), "{shown:?}");
        assert!(
            shown
                .to_string()
                .contains("could not make the database path '' absolute"),
            "{shown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_database_path_that_is_not_utf8_is_refused_before_it_is_opened() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(std::ffi::OsStr::from_bytes(
            b"/no-such-dir-\xff/memory.sqlite3",
        ));
        let err = read_gate_config_status(path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(&err, StatusError::DbPathNotUtf8 { path: named } if named == path),
            "{err:?}"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Validation(_)), "{shown:?}");
        assert!(shown.to_string().contains("is not valid UTF-8"), "{shown}");
    }

    // ── how a SQLite failure reaches the command line ───────────────────

    #[test]
    fn a_sqlite_failure_is_shown_as_a_database_error_and_keeps_its_result_code() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let err = StatusError::from_read(Path::new("/x/m.db"), MemoryError::Db(busy));
        let explained = err.to_string();

        let shown = MemoryError::from(err);

        assert_eq!(shown.to_string(), format!("Database error: {explained}"));
        assert!(explained.starts_with("could not read the database at /x/m.db: "));
        match shown {
            MemoryError::Db(rusqlite::Error::SqliteFailure(failure, _)) => {
                assert_eq!(failure.code, rusqlite::ErrorCode::DatabaseBusy)
            }
            other => panic!("expected a SQLite failure, got {other:?}"),
        }
    }

    #[test]
    fn a_database_file_that_cannot_be_opened_is_shown_as_a_database_error() {
        let err = StatusError::CannotOpen {
            path: PathBuf::from("/x/m.db"),
        };
        let explained = err.to_string();

        let shown = MemoryError::from(err);

        assert_eq!(shown.to_string(), format!("Database error: {explained}"));
        match shown {
            MemoryError::Db(rusqlite::Error::SqliteFailure(failure, _)) => {
                assert_eq!(failure.code, rusqlite::ErrorCode::CannotOpen)
            }
            other => panic!("expected a SQLite failure, got {other:?}"),
        }
    }

    #[test]
    fn a_rusqlite_failure_with_no_result_code_is_still_shown_as_a_database_error() {
        let no_code =
            rusqlite::Error::InvalidColumnType(2, "content".into(), rusqlite::types::Type::Null);
        let err = StatusError::from_read(Path::new("/x/m.db"), MemoryError::Db(no_code));
        assert!(
            matches!(
                &err,
                StatusError::Database {
                    hint: None,
                    source: rusqlite::Error::InvalidColumnType(..),
                    ..
                }
            ),
            "{err:?}"
        );
        let explained = err.to_string();

        let shown = MemoryError::from(err);

        assert_eq!(shown.to_string(), format!("Database error: {explained}"));
    }

    #[test]
    fn a_name_refused_for_what_is_stored_is_shown_as_a_validation_error() {
        for err in [
            StatusError::StoredUnderAnotherName {
                asked: repo("Owner/Repo"),
                stored: vec!["owner/repo".into()],
                host_ambiguous: false,
            },
            StatusError::KeyCollision {
                asked: repo("foo/bar-baz"),
                stored: "foo-bar/baz".into(),
            },
        ] {
            let explained = err.to_string();

            let shown = MemoryError::from(err);

            assert!(matches!(shown, MemoryError::Validation(_)), "{shown:?}");
            assert_eq!(shown.to_string(), format!("Validation error: {explained}"));
        }
    }

    #[test]
    fn a_repo_name_needs_no_quoting_in_a_hinted_command() {
        // The hints print a repo name bare. Every character the shape check
        // lets into a name must therefore be one a shell reads literally.
        for c in (0u8..128).map(char::from) {
            let name = format!("owner/re{c}po");
            if let Ok(parsed) = RepoName::parse(&name) {
                assert_eq!(shell_quote(parsed.as_str()), name, "{c:?}");
            }
        }
        for name in ["GitHub.com/my-org/My.Repo_2", "owner/.github"] {
            assert_eq!(shell_quote(repo(name).as_str()), name);
        }
    }

    #[test]
    fn shell_quote_quotes_only_what_needs_it() {
        assert_eq!(shell_quote("owner/repo"), "owner/repo");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        // zsh expands a leading `=word` to a command path.
        assert_eq!(shell_quote("=x"), "'=x'");
        assert_eq!(shell_quote("a=b"), "'a=b'");
    }
}
