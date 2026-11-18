//! Read-only status of one repo's unattended-route prerequisites (#420).
//!
//! `ironmem autopilot status <owner/repo>` reports a repo's gate config and
//! its `agent:*` / `risk:*` labels. The `/evaluate-issue` router reads the
//! exit status, `gate_config` and `db` from it before it recommends GOAL or
//! AUTOPILOT; it reads labels with its own `gh label list` calls
//! (`docs/EVALUATE_ISSUE.md`, Step 2; moving them onto this report is #428).
//! The gate config is a command's output so that the router's prose does not
//! depend on Autopilot's storage layout.
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
//! - [`DbPath`] is the database's absolute, UTF-8 path, built in one place,
//!   and it is what the report and every error that names the database hold.
//! - The parts of the report are private types. The reported gate config
//!   (`GateConfigStatus`) is built only from a stored [`GateConfig`], and the
//!   reported labels (`LabelSet`) only from a label list that was read in
//!   full.
//!
//! # A failed read is never a missing label
//!
//! The label half of the report (`LabelStatus`) is an enum, not a struct of
//! optional fields: when `gh` is missing or could not be started, exited
//! non-zero, printed output that is not the expected JSON, returned a label
//! row with no name, returned a list long enough to have been cut off, or was
//! never run because `--path` is missing, is not a directory or cannot be
//! examined, it is the `Failed` variant and carries no label answers at all.
//! Its JSON has no `agent` or `risk` key, so a consumer must check `read`
//! first; a strict lookup finds nothing, though a loose one (a missing key
//! read as false) can still be fooled, which is why `read` exists.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

use crate::db::read_only::ReadOnlyDb;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::gate_config::{
    gate_config_key, get_gate_config_read_only, stored_gate_configs_read_only, BrokenGateConfig,
    GateConfig, GateConfigState, StoredGateConfig, StoredGateConfigsError,
};
use super::gh::{repo_labels, GhCli, GhOutput, GhRunner};
use super::labels::AgentLabel;
use super::repo_slug;

/// A repo name that passed the shape check: at most 200 characters,
/// `owner/name` or `host/owner/name`, each part non-empty, made only of ASCII
/// letters, digits, `.`, `_` and `-`, not starting with `-` and not made only
/// of `.`, and with a name that does not end in `.git`. The check does not
/// know every name `gh` refuses.
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
///
/// Holds the name exactly as it was given, and nothing derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoName(String);

/// The most characters a repo name may hold: the limit the crate-wide
/// `validate_repo` puts on a name before it is stored.
const MAX_REPO_NAME_CHARS: usize = 200;

impl RepoName {
    /// Check `given`'s shape. The only way to build a [`RepoName`]. A string
    /// it refuses is [`StatusError::NameTooLong`], [`StatusError::GitSuffix`]
    /// or [`StatusError::NotARepoName`], which is also what an empty string
    /// and one holding a control character are.
    pub fn parse(given: &str) -> Result<Self, StatusError> {
        // First, so that the error for a string of any length is short.
        let len = given.chars().count();
        if len > MAX_REPO_NAME_CHARS {
            return Err(StatusError::NameTooLong { len });
        }
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
        let name = match parts.as_slice() {
            [_, name] | [_, _, name] => name,
            _ => return Err(not_a_repo_name()),
        };
        if name.to_ascii_lowercase().ends_with(".git") {
            return Err(StatusError::GitSuffix {
                given: given.to_string(),
            });
        }
        Ok(Self(given.to_string()))
    }

    /// The name exactly as it was given.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The parts that decide which repository this is. A parsed name is two
    /// or three non-empty parts, so every one of them is found.
    fn identity(&self) -> RepoIdentity {
        let mut parts = self.0.rsplit('/').map(str::to_ascii_lowercase);
        let name = parts.next().unwrap_or_default();
        let owner = parts.next().unwrap_or_default();
        RepoIdentity {
            host: parts.next(),
            owner,
            name,
        }
    }

    /// Whether another repo name can reduce to this name's storage key. A
    /// key is the name with each `/` written as `-`, so a name shares its
    /// key when the `/` could have been somewhere else: it holds a `-`, or it
    /// has a host, whose `/` a two-part name would spell `-`. A name for
    /// which no second spelling is itself a repo name (`a-/b`) is counted
    /// too; the warning this decides is the safe side.
    fn shares_its_storage_key(&self) -> bool {
        self.0.contains('-') || self.0.matches('/').count() > 1
    }

    /// Whether a stored row that does not parse could be this repo's. The
    /// row is judged by the `repo` its content still holds, as a row that
    /// parses is. When it holds none that names an owner, only the slug in
    /// its storage key is left; a slug cannot be turned back into a name, so
    /// that comparison also matches some names of other repos, which is the
    /// safe direction: the read fails instead of reporting "not onboarded".
    fn may_be_stored_as(&self, row: &BrokenGateConfig) -> bool {
        match row.repo().and_then(RepoIdentity::of_stored) {
            Some(identity) => self.identity().may_be(&identity),
            None => slugs_may_match(&repo_slug(&self.0), row.slug()),
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
        f.write_str(&self.0)
    }
}

/// Serialized as the bare name, a JSON string.
impl Serialize for RepoName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
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
    /// so this reads leniently: the last three `/`-separated segments. `None`
    /// for a string that names no owner: one with no `/`, or one whose owner
    /// or name segment is empty (`/repo`, `owner/`).
    fn of_stored(repo: &str) -> Option<Self> {
        let mut segments = repo.rsplit('/');
        let name = segments.next()?.to_ascii_lowercase();
        let owner = segments.next()?.to_ascii_lowercase();
        let host = segments.next().map(str::to_ascii_lowercase);
        (!owner.is_empty() && !name.is_empty()).then_some(Self { host, owner, name })
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

/// A database path as the report and the errors name it: absolute, so it
/// names one file whatever the working directory, and UTF-8, so a JSON string
/// and a `--db` hint spell that file exactly. One private function builds
/// it, from a path it has made absolute and checked, so a function that takes
/// a [`DbPath`] cannot be handed a relative or lossily converted path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DbPath(String);

impl DbPath {
    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path as a filesystem path.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for DbPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why `ironmem autopilot status` could not report. Every variant is exit 1
/// with nothing on stdout.
///
/// A field named `db` is the database: the `--db` path, not the `--path`
/// directory `gh` runs in.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    /// The string is not shaped like a repo name. The message shows a control
    /// character in it escaped, never raw.
    #[error(
        "'{}' is not a repo name — expected `owner/name` (or `host/owner/name`), each part \
         made of letters, digits, `.`, `_` and `-`, not starting with `-` and not made only of `.`",
        printable(.given)
    )]
    NotARepoName { given: String },

    /// The string holds more characters than a repo name may. The message
    /// gives the count and not the string.
    #[error(
        "a repo name may hold at most {} characters, and this one holds {len}",
        MAX_REPO_NAME_CHARS
    )]
    NameTooLong { len: usize },

    /// The string is shaped like a repo name but ends in `.git`.
    #[error("'{given}' ends in `.git`, which `gh` does not resolve — drop the `.git` suffix")]
    GitSuffix { given: String },

    /// The database path is empty: an empty `--db`, or an empty
    /// `IRONMEM_DB_PATH` with no `--db`.
    #[error("the database path is empty — check `--db` and `IRONMEM_DB_PATH`")]
    DbPathEmpty,

    /// A relative database path could not be made absolute, so the report
    /// could not say which file answered. `given` is the path as it was
    /// given. The working directory is what could not be read: that is the
    /// one thing making a non-empty path absolute needs.
    #[error(
        "could not make the database path '{}' absolute: {source} — a relative path is read \
         against the working directory, which could not be read; pass an absolute `--db`",
        .given.display()
    )]
    DbPathNotAbsolute {
        given: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The absolute database path is not UTF-8, so neither the JSON report
    /// nor a `--db` hint could name the file exactly.
    #[error(
        "the database path '{}' is not valid UTF-8, so the report could not name it exactly — \
         use a database whose full path is UTF-8",
        .db.display()
    )]
    DbPathNotUtf8 { db: PathBuf },

    /// No file is at the database path.
    #[error(
        "db not found at {db} — `status` never creates one; pass an existing database with \
         `--db <path>` or set `IRONMEM_DB_PATH`. If Autopilot has never been set up with this \
         database, `ironmem autopilot onboard` creates it"
    )]
    DatabaseNotFound { db: DbPath },

    /// The database path names a directory. Found before the open, because
    /// SQLite opens a directory read-only without complaint and fails on the
    /// first read with a message about the disk.
    #[error(
        "{db} is a directory, not a database file — name the database file with `--db <path>` \
         or `IRONMEM_DB_PATH`"
    )]
    DatabaseIsADirectory { db: DbPath },

    /// The database path could not be examined, so whether a file is there
    /// is not known.
    #[error("could not reach the database at {db}: {source}")]
    DatabaseUnreachable {
        db: DbPath,
        #[source]
        source: std::io::Error,
    },

    /// The database file is there, and SQLite could not open it.
    #[error(
        "could not open the database at {db}: the file exists but could not be opened — check its \
         permissions and that it is a file"
    )]
    CannotOpen { db: DbPath },

    /// SQLite failed while the database was being opened or read. The
    /// message ends with the sentence the failure's result code selects, when
    /// it selects one.
    #[error(
        "could not read the database at {db}: {source}. `status` opens it read-only and never \
         writes it.{}",
        DbHint::for_error(.source).map_or("", DbHint::sentence)
    )]
    Database {
        db: DbPath,
        #[source]
        source: rusqlite::Error,
    },

    /// No config is stored under the name asked about, but one is stored
    /// under a spelling `gh` can read as the same repository.
    #[error("{}", stored_under_another_name(.asked, .stored))]
    StoredUnderAnotherName {
        asked: RepoName,
        /// Every stored name that could be the repo asked about.
        stored: Vec<String>,
    },

    /// The config stored under the asked name's storage key is another
    /// repo's.
    #[error(
        "the gate config stored under the key '{storage_key}' in {db} belongs to '{stored}', not \
         '{asked}' — two repo names that reduce to the same storage key share one config, so \
         this status cannot be reported. Do not onboard '{asked}' into this database: that \
         would replace the config of '{stored}' (#421); use a separate database (`--db`) for \
         one of the two"
    )]
    KeyCollision {
        asked: RepoName,
        /// The `repo` the stored config holds.
        stored: String,
        /// The key both names are stored under.
        storage_key: String,
        db: DbPath,
    },

    /// The row under the asked name's storage key does not parse, and
    /// nothing in it says it is another repo's: its `repo` is the asked name,
    /// or it holds none that names an owner. Every name that reduces to the
    /// same slug shares the key, so in the second case the row may still be
    /// another repo's, and the message says so before it gives a command.
    #[error("{}", stored_config_does_not_parse(.repo, .db, .row))]
    StoredConfigDoesNotParse {
        repo: RepoName,
        db: DbPath,
        row: BrokenGateConfig,
    },

    /// The row under the asked name's storage key does not parse, and the
    /// `repo` it holds names another repo: the two names share the key. The
    /// message gives no command that onboards the asked name, because that
    /// command would overwrite the other repo's row.
    #[error("{}", broken_key_collision(.asked, .db, .row))]
    BrokenKeyCollision {
        asked: RepoName,
        db: DbPath,
        /// The row, whose `repo` names an owner and is not the asked name.
        row: BrokenGateConfig,
    },

    /// No config is stored under the name asked about, and a stored config
    /// that does not parse could be that repo's under another spelling.
    #[error("{}", broken_under_another_name(.asked, .db, .rows))]
    BrokenUnderAnotherName {
        asked: RepoName,
        db: DbPath,
        /// Every stored row that does not parse and could be the repo asked
        /// about.
        rows: Vec<BrokenGateConfig>,
    },

    /// No config is stored under the name asked about, and too many are
    /// stored to compare each with it, so one under another spelling cannot
    /// be ruled out. Listing more is #426.
    #[error(
        "'{asked}' has no gate config under that exact name, and {db} holds {limit} or more gate \
         configs, more than `status` compares, so it cannot rule out one stored under another \
         spelling of this name. If the repo is onboarded, ask with the exact name it was \
         onboarded under: an exact name is read without comparing the others"
    )]
    TooManyStoredConfigs {
        asked: RepoName,
        db: DbPath,
        limit: usize,
    },

    /// A read that failed for a reason that is not SQLite's, reported as the
    /// error it already is. As the open and the two reads are written, each
    /// of their failures is a `MemoryError::Db`, so nothing produces this
    /// variant; it keeps a read error of any other kind an error.
    #[error(transparent)]
    Other(MemoryError),
}

impl StatusError {
    /// Classify a failure of the open or of a read: a SQLite error names
    /// `db`; anything else passes through.
    fn from_read(db: &DbPath, err: MemoryError) -> Self {
        match err {
            MemoryError::Db(source) => Self::Database {
                db: db.clone(),
                source,
            },
            other => Self::Other(other),
        }
    }
}

/// `given` with each control character written as its escape (`\n`,
/// `\u{1b}`), so an error that quotes a refused string cannot move the cursor
/// or change the terminal's colours.
fn printable(given: &str) -> String {
    let mut shown = String::with_capacity(given.len());
    for c in given.chars() {
        if c.is_control() {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    shown
}

/// The message of [`StatusError::StoredUnderAnotherName`]. It ends with what
/// is true if the asked name is another repository, when a stored name and
/// the asked one differ in whether they name a host: the one case that may be
/// two repositories.
fn stored_under_another_name(asked: &RepoName, stored: &[String]) -> String {
    let names = stored
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let asked_names_a_host = asked.identity().host.is_some();
    let host_ambiguous = stored
        .iter()
        .filter_map(|name| RepoIdentity::of_stored(name))
        .any(|stored| stored.host.is_some() != asked_names_a_host);
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

/// The message of [`StatusError::StoredConfigDoesNotParse`].
fn stored_config_does_not_parse(repo: &RepoName, db: &DbPath, row: &BrokenGateConfig) -> String {
    let error = row.error();
    let replace = replace_commands(repo, db);
    if row.repo() == Some(repo.as_str()) {
        return format!(
            "the gate config stored for '{repo}' in {db} does not parse: {error}. To replace it, \
             {replace}"
        );
    }
    let key = row.storage_key();
    let remedy = if repo.shares_its_storage_key() {
        format!(
            "The config does not say which repo it was written for, and every repo name that \
             differs from '{repo}' only in `/` versus `-` is stored under this key, so it may \
             belong to another repo: check which repo was onboarded into this database before \
             replacing it (#421). To replace it as '{repo}', {replace}"
        )
    } else {
        format!("To replace it, {replace}")
    };
    format!(
        "the gate config stored under the key '{key}' in {db} does not parse: {error}. {remedy}"
    )
}

/// The message of [`StatusError::BrokenKeyCollision`].
fn broken_key_collision(asked: &RepoName, db: &DbPath, row: &BrokenGateConfig) -> String {
    let held = row.repo().unwrap_or_default();
    // Onboarding the held name replaces this row only when this is the key
    // that name is stored under; a row whose `repo` belongs under another
    // key gets no command.
    let repair = RepoName::parse(held)
        .ok()
        .filter(|held| row.is_stored_under_the_key_of(held.as_str()))
        .map(|held| {
            format!(
                " To replace the config that does not parse, {}",
                replace_commands(&held, db)
            )
        })
        .unwrap_or_default();
    format!(
        "the gate config stored under the key '{}' in {db} does not parse ({}), and its `repo` is \
         {held:?}, not '{asked}' — two repo names that reduce to the same storage key share one \
         config, so this status cannot be reported. Do not onboard '{asked}' into this database: \
         that would replace the config of {held:?} (#421); use a separate database (`--db`) for \
         one of the two.{repair}",
        row.storage_key(),
        row.error()
    )
}

/// The commands that replace `repo`'s stored gate config in the database at
/// `db`, as a clause: `onboard` writes a new proposal over whatever row is
/// under `repo`'s storage key, and `approve` approves it.
fn replace_commands(repo: &RepoName, db: &DbPath) -> String {
    let db = shell_quote(db.as_str());
    format!(
        "run `ironmem autopilot onboard {repo} --path . --db {db}` from the repo's checkout, \
         then `ironmem autopilot approve {repo} --db {db}`"
    )
}

/// The message of [`StatusError::BrokenUnderAnotherName`].
fn broken_under_another_name(asked: &RepoName, db: &DbPath, rows: &[BrokenGateConfig]) -> String {
    let listed = rows
        .iter()
        .map(|row| {
            let named = match row.repo() {
                Some(repo) => format!(", whose `repo` is {repo:?},"),
                None => String::new(),
            };
            format!(
                "the config stored under the key '{}'{named} does not parse: {}",
                row.storage_key(),
                row.error()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let quoted_db = shell_quote(db.as_str());
    let mut remedies: Vec<String> = Vec::new();
    for row in rows {
        // The command is given only for a name it would work for: one that
        // is a repo name, and whose proposal is written over this row.
        let held = row
            .repo()
            .and_then(|held| RepoName::parse(held).ok())
            .filter(|held| row.is_stored_under_the_key_of(held.as_str()));
        let remedy = match held {
            Some(held) => format!(
                "run `ironmem autopilot onboard {held} --path . --db {quoted_db}` from that \
                 repo's checkout"
            ),
            // A key cannot be turned back into a name, so the reader is told
            // how the two relate.
            None => format!(
                "run `ironmem autopilot onboard` with the name it was onboarded under and `--db \
                 {quoted_db}` (its key is `gate-config:` followed by that name with each `/` \
                 written as `-`)"
            ),
        };
        if !remedies.contains(&remedy) {
            remedies.push(remedy);
        }
    }
    let those = if rows.len() == 1 {
        "that config is"
    } else {
        "those configs are"
    };
    format!(
        "'{asked}' has no gate config under that exact name, but {db} holds one that cannot be \
         read and could be this repo's under another spelling — {listed}. Whether '{asked}' is \
         onboarded cannot be reported until {those} replaced: {}, then ask again",
        remedies.join("; ")
    )
}

/// The extra sentence a [`StatusError::Database`] ends with, chosen by the
/// SQLite failure it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbHint {
    /// SQLite refused the read as read-only. The match is on the primary
    /// result code, so every read-only failure gets the sentence, which
    /// describes the one cause this command meets: a WAL-mode database whose
    /// `-wal`/`-shm` files are missing, in a directory SQLite cannot write.
    ReadOnlyRefused,
    /// The file is not a database, or is one without ironmem's tables.
    NotAnIronmemDatabase,
    /// Another connection holds the database, or a table in it, and did not
    /// let go within the busy timeout. The failure passes by itself.
    InUse,
}

impl DbHint {
    /// The hint for `e`, chosen by its primary result code and, for a
    /// missing table, its message.
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
            rusqlite::ErrorCode::ReadOnly => Some(Self::ReadOnlyRefused),
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                Some(Self::InUse)
            }
            code if code == rusqlite::ErrorCode::NotADatabase || no_such_table => {
                Some(Self::NotAnIronmemDatabase)
            }
            _ => None,
        }
    }

    fn sentence(self) -> &'static str {
        match self {
            Self::ReadOnlyRefused => {
                " A WAL-mode database whose -wal/-shm side files are missing can only be read \
                 from a directory SQLite can create them in."
            }
            Self::NotAnIronmemDatabase => {
                " If this is not an ironmem database, pass the right one with --db."
            }
            Self::InUse => " Another process is using the database; run the command again.",
        }
    }
}

/// How a [`StatusError`] reaches the command line, where `main` prints
/// `Error: {MemoryError}` and every `MemoryError` variant adds its own prefix.
/// The variant is chosen for the prefix that is true of the failure; the
/// typed data stays on the [`StatusError`].
///
/// The converted value is for display only and must not be matched on: its
/// message is the whole explained sentence, and the typed content around
/// that message is chosen to carry it, not read from the failure. Code that
/// needs to know what failed matches the [`StatusError`] before converting.
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
/// prefix is "JSON error:", holding the whole explained message and no line
/// or column.
impl From<StatusError> for MemoryError {
    fn from(err: StatusError) -> Self {
        let message = err.to_string();
        let database = |failure: rusqlite::ffi::Error, message: String| {
            MemoryError::Db(rusqlite::Error::SqliteFailure(failure, Some(message)))
        };
        match err {
            StatusError::Other(inner) => inner,
            StatusError::NotARepoName { .. }
            | StatusError::NameTooLong { .. }
            | StatusError::GitSuffix { .. }
            | StatusError::DbPathEmpty
            | StatusError::DbPathNotUtf8 { .. }
            | StatusError::DatabaseIsADirectory { .. }
            | StatusError::StoredUnderAnotherName { .. }
            | StatusError::KeyCollision { .. }
            | StatusError::TooManyStoredConfigs { .. } => MemoryError::Validation(message),
            StatusError::DatabaseNotFound { .. } => MemoryError::NotFound(message),
            StatusError::DbPathNotAbsolute { source, .. }
            | StatusError::DatabaseUnreachable { source, .. } => {
                MemoryError::Io(std::io::Error::new(source.kind(), message))
            }
            StatusError::StoredConfigDoesNotParse { .. }
            | StatusError::BrokenKeyCollision { .. }
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
/// serialized form is the command's `--json` output. The `/evaluate-issue`
/// router depends on `gate_config` and `db`.
///
/// Built only by [`GateConfigRead::with_labels_read_in`], from one read of
/// the gate config and the labels of that read's own repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    repo: RepoName,
    /// The absolute path of the database file the gate config was read from.
    db: DbPath,
    /// Whether a gate config exists at all, in any state. Always equal to
    /// `gate_config.is_some()`; stated so a reader need not infer it.
    onboarded: bool,
    /// `None` (JSON `null`) for a repo that was never onboarded.
    gate_config: Option<GateConfigStatus>,
    labels: LabelStatus,
}

/// One read of a repo's gate config: the repo asked about, the database that
/// answered and what it held. Built only by [`read_gate_config_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateConfigRead {
    repo: RepoName,
    /// The absolute path of the database file that was read.
    db: DbPath,
    /// `None` for a repo that was never onboarded.
    config: Option<GateConfigStatus>,
}

impl GateConfigRead {
    /// The report for this read: its gate config, and the labels of the repo
    /// it was made for, read with the `gh` on `PATH` run in `path`. The only
    /// way to a [`RepoStatus`] from outside this module, so a report's labels
    /// are always the labels of the repo its gate config was read for.
    ///
    /// Never an error: a label read that fails is part of the report, and
    /// the gate config is still reported. A `path` that is not a directory
    /// or cannot be examined and a `gh` that cannot be found are failed
    /// label reads too, so an unreadable label list does not hide the gate
    /// config.
    pub fn with_labels_read_in(self, path: &Path) -> RepoStatus {
        match gh_to_run_in(path) {
            Ok(mut gh) => self.with_labels_read_through(&mut gh),
            Err(why_not) => self.reported_with(LabelStatus::failed(why_not)),
        }
    }

    /// The report for this read, with the labels `gh` lists for this read's
    /// own repo. The repo `gh` is asked about is taken from the read and is
    /// not an argument, so the two halves of a report cannot be for two
    /// repos.
    fn with_labels_read_through(self, gh: &mut dyn GhRunner) -> RepoStatus {
        let labels = LabelStatus::read(gh, &self.repo);
        self.reported_with(labels)
    }

    /// The report for this read and `labels`. The repo, the database and the
    /// gate config all come from the read, so they describe one read, and
    /// `onboarded` is derived here, so it cannot disagree with `gate_config`.
    /// `labels` must be what was learned about this read's repo: the two
    /// functions above are the callers.
    fn reported_with(self, labels: LabelStatus) -> RepoStatus {
        let Self { repo, db, config } = self;
        RepoStatus {
            repo,
            db,
            onboarded: config.is_some(),
            gate_config: config,
            labels,
        }
    }
}

/// A `gh` that is started in `path`, the directory `--path` names. When it
/// cannot be started, the error says so: a directory that is there and
/// cannot be entered fails the start, and the runner's own message names
/// only the `gh` binary and its arguments.
struct StartedIn<'a, G> {
    gh: G,
    path: &'a Path,
}

impl<G: GhRunner> GhRunner for StartedIn<'_, G> {
    fn run(&mut self, args: &[String]) -> Result<GhOutput, MemoryError> {
        // An `Err` from a runner is a `gh` that did not start; one that ran
        // and failed is an `Ok` (see [`GhRunner::run`]).
        self.gh.run(args).map_err(|err| {
            MemoryError::NotFound(format!(
                "{} — gh is started in the directory `--path` names, '{}', so check that this \
                 user can enter it",
                bare_message(err),
                self.path.display()
            ))
        })
    }
}

/// The `gh` on `PATH`, set to run in `path`, or why no `gh` can be run there:
/// `path` is not a directory or cannot be examined, or there is no `gh`.
fn gh_to_run_in(path: &Path) -> Result<StartedIn<'_, GhCli>, String> {
    match std::fs::metadata(path) {
        Ok(found) if found.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "--path '{}' is not a directory, so gh was not run",
                path.display()
            ));
        }
        // Missing, or not readable by this user: the reason is the
        // filesystem's, and "not a directory" would be a guess.
        Err(reason) => {
            return Err(format!(
                "--path '{}' cannot be used as the directory gh runs in ({reason}), so gh was \
                 not run",
                path.display()
            ));
        }
    }
    match GhCli::resolve(path) {
        Ok(gh) => Ok(StartedIn { gh, path }),
        Err(err) => Err(bare_message(err)),
    }
}

/// The reported part of a stored gate config. Built only from a
/// [`GateConfig`], which cannot exist without passing its own guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GateConfigStatus {
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
enum LabelStatus {
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
struct LabelSet {
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
/// too, whether it is under this name's storage key or under another
/// spelling. Two stored names with different hosts are different
/// repositories and never match. A config that does not parse and holds no
/// name is matched by the slug in its key instead, and that can match a name
/// on another host.
///
/// Under this name's own storage key, a config is this repo's only when the
/// `repo` it holds says so, because every name that reduces to the same slug
/// shares the key: one that names another repo is an error naming that repo,
/// whether or not it parses.
///
/// The exact name is read first and needs no listing, so it is still
/// answered when more configs are stored than can be compared.
pub fn read_gate_config_status(
    db_path: &Path,
    repo: &RepoName,
) -> Result<GateConfigRead, StatusError> {
    // Before the open, so a path the report could not name is refused
    // whether or not a database is there. Every error from here on names
    // this absolute path, as the report does.
    let db_path = absolute_db_path(db_path)?;
    // A path that cannot be examined is left to the open, whose failure
    // `why_not_opened` explains.
    if std::fs::metadata(db_path.as_path()).is_ok_and(|found| found.is_dir()) {
        return Err(StatusError::DatabaseIsADirectory { db: db_path });
    }
    let db = match Database::open_read_only(db_path.as_path()) {
        // `open_read_only` calls every `CannotOpen` "not found".
        Err(MemoryError::NotFound(_)) => return Err(why_not_opened(&db_path)),
        other => other.map_err(|err| StatusError::from_read(&db_path, err))?,
    };
    let config = gate_config_status(&db, &db_path, repo)?;
    if config.is_none() {
        refuse_another_spelling(&db, &db_path, repo)?;
    }
    Ok(GateConfigRead {
        repo: repo.clone(),
        db: db_path,
        config,
    })
}

/// Why SQLite could not open the database at `db`, from what the filesystem
/// says about it: only a path with nothing at it is "not found". A file that
/// is there is a different problem, and so is a path that cannot be examined
/// at all (a parent directory this user may not search, say), where whether a
/// file is there is not known.
fn why_not_opened(db: &DbPath) -> StatusError {
    let db = db.clone();
    match std::fs::metadata(db.as_path()) {
        Ok(_) => StatusError::CannotOpen { db },
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            StatusError::DatabaseNotFound { db }
        }
        Err(source) => StatusError::DatabaseUnreachable { db, source },
    }
}

/// `db` as the report names it: absolute, so the report names one file
/// whatever the cwd, and UTF-8, so the JSON string and a `--db` hint name
/// that file exactly. A path that cannot be both is an error: a relative or
/// lossily converted path would name a file that may not exist. The only
/// place a [`DbPath`] is built.
fn absolute_db_path(db: &Path) -> Result<DbPath, StatusError> {
    // The one path `std::path::absolute` refuses whatever the working
    // directory is, so it gets an error of its own.
    if db.as_os_str().is_empty() {
        return Err(StatusError::DbPathEmpty);
    }
    let absolute = std::path::absolute(db).map_err(|source| StatusError::DbPathNotAbsolute {
        given: db.to_path_buf(),
        source,
    })?;
    match absolute.to_str() {
        Some(utf8) => Ok(DbPath(utf8.to_string())),
        None => Err(StatusError::DbPathNotUtf8 { db: absolute }),
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
fn refuse_another_spelling(
    db: &ReadOnlyDb,
    db_path: &DbPath,
    repo: &RepoName,
) -> Result<(), StatusError> {
    let stored = stored_gate_configs_read_only(db).map_err(|err| match err {
        StoredGateConfigsError::Read(err) => StatusError::from_read(db_path, err),
        StoredGateConfigsError::LimitReached { limit } => StatusError::TooManyStoredConfigs {
            asked: repo.clone(),
            db: db_path.clone(),
            limit,
        },
    })?;
    let asked = repo.identity();
    let mut near: Vec<String> = Vec::new();
    let mut broken: Vec<BrokenGateConfig> = Vec::new();
    for row in stored {
        match row {
            StoredGateConfig::Parsed(name) => {
                let may_be_the_repo = name != repo.as_str()
                    && RepoIdentity::of_stored(&name).is_some_and(|stored| asked.may_be(&stored));
                if may_be_the_repo {
                    near.push(name);
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
        return Err(StatusError::StoredUnderAnotherName {
            asked: repo.clone(),
            stored: near,
        });
    }
    if !broken.is_empty() {
        return Err(StatusError::BrokenUnderAnotherName {
            asked: repo.clone(),
            db: db_path.clone(),
            rows: broken,
        });
    }
    Ok(())
}

/// Read `repo`'s gate config, whatever state it is in. `Ok(None)` is nothing
/// stored under this name's storage key; an `Err` is a read that failed, a
/// stored row that does not parse, or one that belongs to another repo
/// name.
///
/// The row is found by storage key, and every name that reduces to the same
/// slug shares a key, so a row is this repo's only when the `repo` it holds
/// says so. That is checked for a row that does not parse as it is for one
/// that does.
fn gate_config_status(
    db: &ReadOnlyDb,
    db_path: &DbPath,
    repo: &RepoName,
) -> Result<Option<GateConfigStatus>, StatusError> {
    let stored = get_gate_config_read_only(db, repo.as_str())
        .map_err(|err| StatusError::from_read(db_path, err))?;
    let config = match stored {
        None => return Ok(None),
        Some(Ok(config)) => config,
        Some(Err(row)) => {
            let (asked, db) = (repo.clone(), db_path.clone());
            // A `repo` that names no owner says nothing about whose row it
            // is, as in `RepoName::may_be_stored_as`.
            let names_another_repo = row.repo().is_some_and(|held| {
                held != repo.as_str() && RepoIdentity::of_stored(held).is_some()
            });
            return Err(if names_another_repo {
                StatusError::BrokenKeyCollision { asked, db, row }
            } else {
                StatusError::StoredConfigDoesNotParse {
                    repo: asked,
                    db,
                    row,
                }
            });
        }
    };
    if config.repo != repo.as_str() {
        return Err(StatusError::KeyCollision {
            asked: repo.clone(),
            stored: config.repo,
            storage_key: gate_config_key(repo.as_str()),
            db: db_path.clone(),
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
        format!(" --db {}", shell_quote(status.db.as_str()))
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
    use crate::autopilot::gh::label_list_argv;
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::testing::on_disk_db;
    use crate::autopilot::validate_repo;

    fn repo(name: &str) -> RepoName {
        RepoName::parse(name).unwrap()
    }

    /// A temporary directory and the path of `name` in it, spelled as
    /// `std::path::absolute` spells it (see `on_disk_db`).
    fn temp_path(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::path::absolute(dir.path().join(name)).unwrap();
        (dir, path)
    }

    /// `path` as a read names it, built the one way a [`DbPath`] is.
    fn db_at(path: impl AsRef<Path>) -> DbPath {
        absolute_db_path(path.as_ref()).unwrap()
    }

    /// A read of `owner/repo` from `db` that found `config`, without a
    /// database.
    fn read_of(db: &str, config: Option<GateConfigStatus>) -> GateConfigRead {
        GateConfigRead {
            repo: repo("owner/repo"),
            db: db_at(db),
            config,
        }
    }

    /// The report for `owner/repo` read from `db`, with `gh` answering the
    /// one label list it is asked for with `answer`. The labels are read for
    /// the read's own repo, as in a real report.
    fn status_from(
        db: &str,
        config: Option<GateConfigStatus>,
        answer: Result<GhOutput, MemoryError>,
    ) -> RepoStatus {
        let mut gh = ScriptedGh::new(vec![answer]);
        let status = read_of(db, config).with_labels_read_through(&mut gh);
        assert_eq!(gh.seen, vec![label_list_argv("owner/repo")]);
        status
    }

    /// [`status_from`] a `gh` that lists the labels `names`.
    fn status_of(db: &str, config: Option<GateConfigStatus>, names: &[&str]) -> RepoStatus {
        status_from(db, config, Ok(GhOutput::ok(&label_json(names))))
    }

    /// [`status_from`] a `gh` that could not be started, which is reported
    /// as `error`.
    fn status_unread(db: &str, config: Option<GateConfigStatus>, error: &str) -> RepoStatus {
        status_from(db, config, Err(MemoryError::NotFound(error.into())))
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
    fn a_gh_that_cannot_start_in_the_path_names_the_path_and_one_that_ran_is_passed_through() {
        let path = Path::new("/srv/check out");
        let ran = GhOutput::failed("", "HTTP 401");
        let mut gh = StartedIn {
            gh: ScriptedGh::new(vec![
                Ok(ran.clone()),
                Err(MemoryError::NotFound(
                    "failed to start /usr/bin/gh label list: Permission denied (os error 13)"
                        .into(),
                )),
            ]),
            path,
        };
        let argv = label_list_argv("owner/repo");

        // A `gh` that ran, whatever it exited with, is not this wrapper's
        // business.
        assert_eq!(gh.run(&argv).unwrap(), ran);
        match gh.run(&argv).unwrap_err() {
            MemoryError::NotFound(message) => assert_eq!(
                message,
                "failed to start /usr/bin/gh label list: Permission denied (os error 13) — gh \
                 is started in the directory `--path` names, '/srv/check out', so check that \
                 this user can enter it"
            ),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_a_failed_read_without_asking_gh() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        let file = dir.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            read_of("/tmp/mem.db", None)
                .with_labels_read_in(&file)
                .labels,
            LabelStatus::failed(format!(
                "--path '{}' is not a directory, so gh was not run",
                file.display()
            ))
        );
        // A path that cannot be examined is reported with the reason, not
        // called "not a directory".
        let reason = std::fs::metadata(&missing).unwrap_err();
        assert_eq!(
            read_of("/tmp/mem.db", None)
                .with_labels_read_in(&missing)
                .labels,
            LabelStatus::failed(format!(
                "--path '{}' cannot be used as the directory gh runs in ({reason}), so gh was \
                 not run",
                missing.display()
            ))
        );
    }

    #[test]
    fn bare_message_drops_the_prefix_of_the_two_variants_a_label_read_fails_with() {
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
        let status = gate_config_status(&ro, &db_at(&path), &repo("owner/repo"))
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
        let status = gate_config_status(&ro, &db_at(&path), &repo("owner/repo"))
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
            gate_config_status(&ro, &db_at(&path), &repo("owner/repo")).unwrap(),
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
    fn an_empty_or_blank_repo_string_is_refused_as_not_a_repo_name() {
        for bad in ["", "  "] {
            let err = RepoName::parse(bad).unwrap_err();
            assert!(
                matches!(&err, StatusError::NotARepoName { given } if given == bad),
                "{bad:?}: {err:?}"
            );
        }
    }

    #[test]
    fn a_repo_name_may_hold_200_characters_and_no_more() {
        let longest = format!("owner/{}", "a".repeat(194));
        assert_eq!(longest.chars().count(), 200);
        assert_eq!(repo(&longest).as_str(), longest);

        let too_long = format!("{longest}a");
        let err = RepoName::parse(&too_long).unwrap_err();
        let message = err.to_string();
        assert_eq!(
            message,
            "a repo name may hold at most 200 characters, and this one holds 201"
        );
        let shown = MemoryError::from(err);
        assert_eq!(shown.to_string(), format!("Validation error: {message}"));

        // The limit is the crate-wide one `onboard` stores names under.
        assert!(validate_repo(&longest).is_ok());
        assert!(validate_repo(&too_long).is_err());
    }

    #[test]
    fn a_control_character_is_refused_as_not_a_repo_name_and_is_not_printed_raw() {
        for (bad, shown) in [
            ("owner/re\u{7}po", "'owner/re\\u{7}po' is not a repo name"),
            ("owner/repo\n", "'owner/repo\\n' is not a repo name"),
            (
                "owner/re\u{1b}[31mpo",
                "'owner/re\\u{1b}[31mpo' is not a repo name",
            ),
        ] {
            let err = RepoName::parse(bad).unwrap_err();
            assert!(
                matches!(&err, StatusError::NotARepoName { given } if given == bad),
                "{bad:?}: {err:?}"
            );
            let message = err.to_string();
            assert!(message.starts_with(shown), "{bad:?}: {message}");
            assert!(
                !message.chars().any(char::is_control),
                "{bad:?}: {message:?}"
            );
        }
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
        let (_dir, path) = temp_path("memory.sqlite3");
        std::fs::write(
            &path,
            "this is not sqlite, it is a text file of some length\n".repeat(50),
        )
        .unwrap();
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(
                &err,
                StatusError::Database { db, source }
                    if db.as_path() == path
                        && DbHint::for_error(source) == Some(DbHint::NotAnIronmemDatabase)
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
        let (_dir, path) = temp_path("empty.sqlite3");
        rusqlite::Connection::open(&path).unwrap();
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(
                &err,
                StatusError::Database { source, .. }
                    if DbHint::for_error(source) == Some(DbHint::NotAnIronmemDatabase)
                        && matches!(
                            source,
                            rusqlite::Error::SqliteFailure(_, Some(detail))
                                if detail.starts_with("no such table")
                        )
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
            matches!(&err, StatusError::CannotOpen { db } if db.as_path() == path),
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
    fn a_read_only_failure_gets_the_wal_sentence_and_a_locked_one_is_told_to_run_again() {
        let readonly = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
            Some("attempt to write a readonly database".into()),
        );
        let err = StatusError::from_read(&db_at("/x/m.db"), MemoryError::Db(readonly));
        assert!(
            matches!(&err, StatusError::Database { source, .. }
                if DbHint::for_error(source) == Some(DbHint::ReadOnlyRefused)),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("-wal/-shm"), "{message}");
        assert!(!message.contains("pass the right one"), "{message}");

        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            Some("database table is locked".into()),
        );
        for (failure, said) in [(busy, "database is locked"), (locked, "table is locked")] {
            let err = StatusError::from_read(&db_at("/x/m.db"), MemoryError::Db(failure));
            let message = err.to_string();
            assert!(message.contains(said), "{message}");
            // The failure passes, so the reader is told to ask again.
            assert!(
                message.ends_with(
                    "never writes it. Another process is using the database; run the command \
                     again."
                ),
                "{message}"
            );
            assert!(!message.contains("-wal/-shm"), "{message}");
            assert!(!message.contains("pass the right one"), "{message}");
        }
    }

    #[test]
    fn a_relative_database_path_is_reported_as_an_absolute_one() {
        let db = absolute_db_path(Path::new("m.db")).unwrap();
        assert!(db.as_path().is_absolute(), "{db}");
        assert!(db.as_str().ends_with("m.db"), "{db}");
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
        let (_dir, path) = temp_path("absent.sqlite3");
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        assert!(
            matches!(&err, StatusError::DatabaseNotFound { db } if db.as_path() == path),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.starts_with(&format!("db not found at {}", path.display())),
            "{message}"
        );
        assert!(message.contains("`--db <path>`"), "{message}");
        assert!(message.contains("`IRONMEM_DB_PATH`"), "{message}");
        // And what makes a database, for one that was never created.
        assert!(
            message.ends_with(
                " If Autopilot has never been set up with this database, `ironmem autopilot \
                 onboard` creates it"
            ),
            "{message}"
        );
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
            StatusError::DatabaseNotFound { db } => {
                assert!(db.as_path().is_absolute(), "{db}");
                assert!(db.as_path().ends_with(relative), "{db}");
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
                StatusError::DatabaseUnreachable { db, source }
                    if db.as_path() == path
                        && source.kind() == std::io::ErrorKind::PermissionDenied
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
        let (_dir, path) = db_with_configs(&["owner/repo"]);
        let read = read_gate_config_status(&path, &repo("owner/repo")).unwrap();
        assert_eq!(read.repo.as_str(), "owner/repo");
        assert_eq!(read.db.as_str(), path.display().to_string());
        assert_eq!(
            read.config.unwrap().gate_commands,
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn the_text_report_ends_with_the_database_that_answered() {
        let status = status_of("/tmp/mem.db", None, &[]);
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
                row,
            } => {
                assert_eq!(named.as_str(), "owner/repo");
                assert_eq!(db.as_path(), path);
                assert_eq!(row.storage_key(), "gate-config:owner-repo");
                assert_eq!(row.repo(), Some("owner/repo"));
                assert!(
                    row.error()
                        .to_string()
                        .contains(crate::autopilot::EMPTY_GATE_COMMANDS_MSG),
                    "{}",
                    row.error()
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

    // `foo-bar/baz` and `foo/bar-baz` reduce to one storage key,
    // `gate-config:foo-bar-baz`, so a read by either name finds the same row.

    #[test]
    fn a_broken_row_that_names_another_repo_under_the_asked_key_is_a_collision_with_no_remedy() {
        let (_dir, path) = db_with_raw_config("foo-bar/baz", &no_gate("foo-bar/baz"));

        let err = read_gate_config_status(&path, &repo("foo/bar-baz")).unwrap_err();

        match &err {
            StatusError::BrokenKeyCollision { asked, db, row } => {
                assert_eq!(asked.as_str(), "foo/bar-baz");
                assert_eq!(db.as_path(), path);
                assert_eq!(row.storage_key(), "gate-config:foo-bar-baz");
                assert_eq!(row.repo(), Some("foo-bar/baz"));
            }
            other => panic!("expected the collision of a broken row, got {other:?}"),
        }
        let message = err.to_string();
        let shown_db = path.display().to_string();
        assert!(
            message.starts_with(&format!(
                "the gate config stored under the key 'gate-config:foo-bar-baz' in {shown_db} \
                 does not parse ("
            )),
            "{message}"
        );
        assert!(
            message.contains(crate::autopilot::EMPTY_GATE_COMMANDS_MSG),
            "{message}"
        );
        assert!(
            message.contains("its `repo` is \"foo-bar/baz\", not 'foo/bar-baz'"),
            "{message}"
        );
        // Following an `onboard foo/bar-baz` would overwrite the row of
        // `foo-bar/baz`, so the error must not print one.
        assert!(!message.contains("onboard foo/bar-baz"), "{message}");
        assert!(
            message.contains("Do not onboard 'foo/bar-baz' into this database"),
            "{message}"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Json(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("JSON error: {message}"));

        // The row is `foo-bar/baz`'s and is stored under that name's key, so
        // the command that replaces it is given for that name.
        let quoted = shell_quote(&shown_db);
        assert!(
            message.ends_with(&format!(
                " To replace the config that does not parse, run `ironmem autopilot onboard \
                 foo-bar/baz --path . --db {quoted}` from the repo's checkout, then `ironmem \
                 autopilot approve foo-bar/baz --db {quoted}`"
            )),
            "{message}"
        );

        // Asked by the name the row holds, it is that repo's own config and
        // the remedy is safe.
        let err = read_gate_config_status(&path, &repo("foo-bar/baz")).unwrap_err();
        assert!(
            matches!(&err, StatusError::StoredConfigDoesNotParse { row, .. }
                if row.repo() == Some("foo-bar/baz")),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "the gate config stored for 'foo-bar/baz' in {shown_db} does not parse: "
            )),
            "{message}"
        );
        assert!(
            message.ends_with(&format!(
                "To replace it, run `ironmem autopilot onboard foo-bar/baz --path . --db \
                 {quoted}` from the repo's checkout, then `ironmem autopilot approve foo-bar/baz \
                 --db {quoted}`"
            )),
            "{message}"
        );
    }

    #[test]
    fn a_broken_row_that_names_no_repo_under_a_key_names_can_share_warns_before_the_remedy() {
        let warning = "it may belong to another repo";
        // Not JSON, and three `repo` values that name no owner.
        for content in [
            "not json".to_string(),
            no_gate("baz"),
            no_gate("/baz"),
            no_gate("foo-bar/"),
        ] {
            let (_dir, path) = db_with_raw_config("foo-bar/baz", &content);

            let err = read_gate_config_status(&path, &repo("foo/bar-baz")).unwrap_err();

            assert!(
                matches!(&err, StatusError::StoredConfigDoesNotParse { repo, row, .. }
                    if repo.as_str() == "foo/bar-baz"
                        && row.storage_key() == "gate-config:foo-bar-baz"
                        && row.repo() != Some("foo/bar-baz")),
                "{content}: {err:?}"
            );
            let message = err.to_string();
            assert!(
                message.starts_with(&format!(
                    "the gate config stored under the key 'gate-config:foo-bar-baz' in {} does \
                     not parse: ",
                    path.display()
                )),
                "{content}: {message}"
            );
            // The row does not say whose it is, so the message must not.
            assert!(
                !message.contains("stored for 'foo/bar-baz'"),
                "{content}: {message}"
            );
            let warned = message
                .find(warning)
                .unwrap_or_else(|| panic!("{content}: no warning in: {message}"));
            let remedy = message
                .find("ironmem autopilot onboard foo/bar-baz")
                .unwrap_or_else(|| panic!("{content}: no remedy in: {message}"));
            assert!(warned < remedy, "{content}: {message}");
            let shown = MemoryError::from(err);
            assert_eq!(shown.to_string(), format!("JSON error: {message}"));
        }

        // `owner/repo` is the only repo name that reduces to its key, so a
        // row there that names no repo gets the remedy with no warning.
        let (_dir, path) = db_with_raw_config("owner/repo", "not json");
        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();
        let message = err.to_string();
        assert!(!message.contains(warning), "{message}");
        assert!(
            message.contains("To replace it, run `ironmem autopilot onboard owner/repo --path . "),
            "{message}"
        );
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
                    assert_eq!(db.as_path(), path);
                    assert_eq!(rows.len(), 1, "{rows:?}");
                    assert_eq!(rows[0].storage_key(), key);
                    assert_eq!(rows[0].repo(), held);
                    rows[0].error().to_string()
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
            // What to do about it: the command, when the row says which name
            // it was onboarded under; how to read that name off the key, when
            // it does not.
            let quoted = shell_quote(&path.display().to_string());
            let remedy = match held {
                Some(held) => format!(
                    "cannot be reported until that config is replaced: run `ironmem autopilot \
                     onboard {held} --path . --db {quoted}` from that repo's checkout, then ask \
                     again"
                ),
                None => format!(
                    "cannot be reported until that config is replaced: run `ironmem autopilot \
                     onboard` with the name it was onboarded under and `--db {quoted}` (its key \
                     is `gate-config:` followed by that name with each `/` written as `-`), \
                     then ask again"
                ),
            };
            assert!(message.ends_with(&remedy), "{message}");
            let shown = MemoryError::from(err);
            assert!(matches!(shown, MemoryError::Json(_)), "{shown:?}");
            assert_eq!(shown.to_string(), format!("JSON error: {message}"));
        }
    }

    #[test]
    fn two_broken_rows_under_other_spellings_are_both_listed_with_a_quoted_database() {
        // A database path a shell would split, so the `--db` in the remedy
        // has to be quoted.
        let (_dir, path) = temp_path("two words/memory.sqlite3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let db = Database::open(&path).unwrap();
            db.migrate().unwrap();
            store_raw_gate_config(&db, "Owner/Repo", &no_gate("Owner/Repo"));
            store_raw_gate_config(&db, "github.com/owner/repo", "not json");
        }

        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();

        let mut keys: Vec<&str> = match &err {
            StatusError::BrokenUnderAnotherName { rows, .. } => {
                rows.iter().map(BrokenGateConfig::storage_key).collect()
            }
            other => panic!("expected the broken rows, got {other:?}"),
        };
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "gate-config:Owner-Repo",
                "gate-config:github.com-owner-repo"
            ]
        );
        let message = err.to_string();
        let shown_db = path.display().to_string();
        assert!(shown_db.contains(' '), "{shown_db}");
        for listed in [
            "the config stored under the key 'gate-config:Owner-Repo', whose `repo` is \
             \"Owner/Repo\", does not parse: ",
            "the config stored under the key 'gate-config:github.com-owner-repo' does not parse: ",
        ] {
            assert!(message.contains(listed), "{message}");
        }
        assert!(message.contains("; the config stored under"), "{message}");
        for remedy in [
            format!("run `ironmem autopilot onboard Owner/Repo --path . --db '{shown_db}'` from"),
            format!("with the name it was onboarded under and `--db '{shown_db}'` (its key is"),
        ] {
            assert!(message.contains(&remedy), "{message}");
        }
        assert!(
            message.contains("cannot be reported until those configs are replaced: "),
            "{message}"
        );
        assert!(message.ends_with(", then ask again"), "{message}");
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
                if rows.len() == 1 && rows[0].storage_key() == "gate-config:foo-bar-baz"),
            "{err:?}"
        );

        // A `repo` that names no owner cannot be compared as a name either:
        // one with no `/`, and one whose owner or name part is empty.
        for held in ["repo", "/Repo", "Owner/", "/", "a//Repo"] {
            let (_dir, path) = db_with_raw_config("Owner/Repo", &no_gate(held));
            let err = match read_gate_config_status(&path, &repo("owner/repo")) {
                Err(err) => err,
                Ok(read) => panic!("{held:?}: expected the broken row, got {read:?}"),
            };
            assert!(
                matches!(&err, StatusError::BrokenUnderAnotherName { rows, .. }
                    if rows.len() == 1 && rows[0].repo() == Some(held)),
                "{held:?}: {err:?}"
            );
        }
    }

    /// Row content that parses as `repo`'s pending gate config.
    fn parsing(repo: &str) -> String {
        serde_json::json!({
            "repo": repo,
            "state": "pending",
            "gate_commands": ["cargo test"],
            "proposed_at": "2026-01-01T00:00:00+00:00",
            "approved_at": null,
        })
        .to_string()
    }

    /// Rewrite the one stored gate config row in place, as a hand edit or an
    /// import could leave it: under `source_file`, and under `id` when one is
    /// given.
    fn rekey_the_gate_config_row(path: &Path, source_file: &str, id: Option<&str>) {
        let conn = rusqlite::Connection::open(path).unwrap();
        let changed = conn
            .execute(
                "UPDATE drawers SET source_file = ?1, id = COALESCE(?2, id) \
                 WHERE wing = 'autopilot' AND room = 'backlog-lineage' \
                 AND source_file LIKE 'logical:gate-config:%'",
                rusqlite::params![source_file, id],
            )
            .unwrap();
        assert_eq!(changed, 1, "the UPDATE must hit the gate config row");
    }

    #[test]
    fn a_row_no_read_by_name_can_return_is_not_a_stored_gate_config() {
        // A read by name looks a row up by the id of `gate-config:<slug>`.
        // The listing asks for rows by `source_file`, with a `LIKE` that
        // ignores letter case, so it also returns a row under another id:
        // one keyed `Gate-Config:` (the id of that key is not the id of any
        // name's key), and one whose id is not its key's at all. Neither is
        // a config the Lead can read, so neither makes a name a near-miss.
        for (source_file, id) in [
            ("logical:Gate-Config:Owner-Repo", "the-id-of-another-key"),
            ("logical:gate-config:Owner-Repo", "not-the-id-of-its-key"),
        ] {
            for content in [parsing("Owner/Repo"), "not json".to_string()] {
                let (_dir, path) = db_with_raw_config("Owner/Repo", &content);
                rekey_the_gate_config_row(&path, source_file, Some(id));
                for asked in ["owner/repo", "Owner/Repo", "github.com/owner/repo"] {
                    let read = read_gate_config_status(&path, &repo(asked));
                    assert!(
                        matches!(&read, Ok(read) if read.config.is_none()),
                        "{source_file} / {content} / {asked}: {read:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_row_under_its_keys_id_is_listed_whatever_the_letter_case_of_its_key_prefix() {
        // The id is what a read by name finds the row by, so this is still
        // the stored config of `Owner/Repo`, and still a near-miss of
        // `owner/repo`, with the key a read by name uses.
        let (_dir, path) = db_with_raw_config("Owner/Repo", "not json");
        rekey_the_gate_config_row(&path, "Logical:Gate-Config:Owner-Repo", None);

        let err = read_gate_config_status(&path, &repo("Owner/Repo")).unwrap_err();
        assert!(
            matches!(&err, StatusError::StoredConfigDoesNotParse { .. }),
            "{err:?}"
        );
        let err = match read_gate_config_status(&path, &repo("owner/repo")) {
            Err(err) => err,
            Ok(read) => panic!("expected the broken row, got {read:?}"),
        };
        assert!(
            matches!(&err, StatusError::BrokenUnderAnotherName { rows, .. }
                if rows.len() == 1 && rows[0].storage_key() == "gate-config:Owner-Repo"),
            "{err:?}"
        );

        let (_dir, path) = db_with_raw_config("Owner/Repo", &parsing("Owner/Repo"));
        rekey_the_gate_config_row(&path, "Logical:Gate-Config:Owner-Repo", None);
        let near = near_miss_at(&path, "owner/repo");
        assert_eq!(near.stored, vec!["Owner/Repo".to_string()]);
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
                assert_eq!(db.as_path(), path);
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
        message: String,
    }

    impl NearMiss {
        /// Whether the message says what is true if the asked name is a
        /// repository on another host.
        fn says_it_may_be_another_host(&self) -> bool {
            self.message.contains("another host")
        }
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
            } => {
                assert_eq!(named.as_str(), asked);
                NearMiss { stored, message }
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
        let near = near_miss(&["owner/repo", "github.com/owner/repo"], "OWNER/REPO");
        // One of the two stored names has a host and the asked one has none,
        // so the message says what is true if that is another repository,
        // whichever of the two rows is listed last.
        assert!(near.says_it_may_be_another_host(), "{}", near.message);
        let NearMiss {
            mut stored,
            message,
        } = near;
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
        assert!(!near.says_it_may_be_another_host(), "{}", near.message);
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
            let near = near_miss(&[stored], asked);
            assert!(
                near.says_it_may_be_another_host(),
                "{stored} / {asked}: {}",
                near.message
            );
            assert!(
                near.message.contains(&format!(
                    "if '{asked}' is a different repository on another host, it is not onboarded"
                )),
                "{}",
                near.message
            );
        }
        // Letter case alone is never another host.
        for (stored, asked) in [
            ("owner/repo", "Owner/Repo"),
            ("github.com/owner/repo", "github.com/Owner/Repo"),
        ] {
            let near = near_miss(&[stored], asked);
            assert!(
                !near.says_it_may_be_another_host(),
                "{stored} / {asked}: {}",
                near.message
            );
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
        let err = gate_config_status(&ro, &db_at(&path), &repo("foo/bar-baz")).unwrap_err();

        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "the gate config stored under the key 'gate-config:foo-bar-baz' in {} belongs to \
                 'foo-bar/baz', not 'foo/bar-baz'",
                path.display()
            )),
            "{message}"
        );
        // What the reader must not do next, and no command that does it.
        assert!(
            message.ends_with(
                "Do not onboard 'foo/bar-baz' into this database: that would replace the config \
                 of 'foo-bar/baz' (#421); use a separate database (`--db`) for one of the two"
            ),
            "{message}"
        );
        assert!(!message.contains("onboard foo/bar-baz"), "{message}");
        match err {
            StatusError::KeyCollision { asked, stored, .. } => {
                assert_eq!(asked.as_str(), "foo/bar-baz");
                assert_eq!(stored, "foo-bar/baz");
            }
            other => panic!("expected the key collision, got {other:?}"),
        }
    }

    #[test]
    fn risk_labels_sort_case_insensitively_where_a_byte_sort_would_differ() {
        // A byte sort would put `Risk:Zeta` first, on its capital `R`.
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
        let status = status_unread("/tmp/mem.db", None, "boom");
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
            &status_from("/tmp/mem.db", None, Ok(GhOutput::failed("", "HTTP 401"))),
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
            text.contains(
                "Labels: could not be read — gh label list on owner/repo failed (exit 1): HTTP 401"
            ),
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
                &["agent:ready", "agent:blocked", "risk:logic"],
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
        let status = gate_config_status(&ro, &db_at(&path), &repo("owner/repo"))
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
                &[],
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
                &[],
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
                &[],
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
            &["agent:ready", "risk:logic"],
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
        // `serde_json::Value` equality ignores key order, so the order of
        // the keys is pinned here, on the compact form. The layout the
        // command prints (indented, one trailing newline) is pinned from
        // the binary, in `an_approved_repo_with_every_label_reports_the_pinned_json`.
        let reported = status_of(
            "/tmp/mem.db",
            Some(config_status(
                GateConfigState::Approved,
                vec!["w".into()],
                Some("2026-02-02T00:00:00+00:00"),
            )),
            &["agent:ready", "Risk:Logic", "risk:documentation"],
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
        let unread = status_unread("/tmp/mem.db", None, "boom");
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
                &[],
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
        let text = render_text(&status_of("/tmp/mem.db", None, &[]), false);
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
            &[],
        );
        let text = render_text(&status, true);
        assert!(
            text.contains("ironmem autopilot approve owner/repo --db '/tmp/x y.sqlite3'"),
            "{text}"
        );

        let not_onboarded = status_of("/tmp/x y.sqlite3", None, &[]);
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
        // The helper names the database the one way a read does, through
        // `absolute_db_path`, so this pins that function and the renderer.
        // That a real read goes through it is pinned from the command line
        // (`hints_name_the_absolute_database_when_db_was_given_relative`).
        let status = status_of("rel.sqlite3", None, &[]);
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
        let status = status_of("/tmp/mem.db", None, &[]);
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
        let status = status_of("/tmp/mem.db", None, &["agent:ready"]);
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
            &["agent:ready", "agent:blocked", "agent:exhausted"],
        );
        assert!(!render_text(&all, false).contains("create the missing"));
        let failed = status_unread("/tmp/mem.db", None, "boom");
        assert!(!render_text(&failed, false).contains("create the missing"));
    }

    #[test]
    fn the_label_lines_follow_the_map_and_never_invent_a_missing_entry() {
        // A one-entry map is one no read can produce, built here by literal
        // to show the renderer prints the map and adds nothing to it.
        let status = read_of("/tmp/mem.db", None).reported_with(LabelStatus::Ok(LabelSet {
            agent: BTreeMap::from([("agent:ready", true)]),
            risk: vec![],
        }));
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

        let text = render_text(&status_of("/tmp/mem.db", Some(config), &[]), true);

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

        let text = render_text(&status_of("/tmp/mem.db", Some(config), &[]), false);

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
    fn an_empty_database_path_is_an_error_that_names_both_places_it_can_come_from() {
        // An empty path is the one input `std::path::absolute` refuses
        // whatever the working directory is, and it is what an empty `--db`
        // and an empty `IRONMEM_DB_PATH` both arrive as.
        let err = read_gate_config_status(Path::new(""), &repo("owner/repo")).unwrap_err();
        let message = err.to_string();
        assert_eq!(
            message,
            "the database path is empty — check `--db` and `IRONMEM_DB_PATH`"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Validation(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("Validation error: {message}"));
    }

    #[test]
    fn a_relative_database_path_with_no_working_directory_says_what_is_missing() {
        // What `std::path::absolute` returns for a relative path when the
        // working directory has been removed.
        let err = StatusError::DbPathNotAbsolute {
            given: PathBuf::from("memory.sqlite3"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let message = err.to_string();
        assert!(
            message.starts_with("could not make the database path 'memory.sqlite3' absolute: "),
            "{message}"
        );
        // Not a missing database: the working directory is what could not
        // be read.
        assert!(
            message.ends_with(
                " — a relative path is read against the working directory, which could not be \
                 read; pass an absolute `--db`"
            ),
            "{message}"
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Io(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("IO error: {message}"));
    }

    #[test]
    fn a_database_path_that_is_a_directory_is_refused_as_one_before_sqlite_reads_it() {
        let (_dir, path) = temp_path("a-directory");
        std::fs::create_dir(&path).unwrap();

        let err = read_gate_config_status(&path, &repo("owner/repo")).unwrap_err();

        let message = err.to_string();
        assert_eq!(
            message,
            format!(
                "{} is a directory, not a database file — name the database file with `--db \
                 <path>` or `IRONMEM_DB_PATH`",
                path.display()
            )
        );
        let shown = MemoryError::from(err);
        assert!(matches!(shown, MemoryError::Validation(_)), "{shown:?}");
        assert_eq!(shown.to_string(), format!("Validation error: {message}"));
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
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
            matches!(&err, StatusError::DbPathNotUtf8 { db } if db == path),
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
        let err = StatusError::from_read(&db_at("/x/m.db"), MemoryError::Db(busy));
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
            db: db_at("/x/m.db"),
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
        let err = StatusError::from_read(&db_at("/x/m.db"), MemoryError::Db(no_code));
        assert!(
            matches!(
                &err,
                StatusError::Database {
                    source: source @ rusqlite::Error::InvalidColumnType(..),
                    ..
                } if DbHint::for_error(source).is_none()
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
            },
            StatusError::KeyCollision {
                asked: repo("foo/bar-baz"),
                stored: "foo-bar/baz".into(),
                storage_key: "gate-config:foo-bar-baz".into(),
                db: db_at("/x/m.db"),
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
