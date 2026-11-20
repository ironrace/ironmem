//! The `mined_rows` doctor check (#377): how old the newest mined drawer is.
//!
//! MCP writes keep a store looking alive while hook mining has stopped, so the
//! age of the newest `added_by = 'mine'` row (a chunk of a workspace file) is
//! the direct evidence of whether files are still being mined. Those rows are
//! written by a trusted Stop or PreCompact hook (each run that has a
//! workspace root, which Claude Code always passes as `cwd`), by a workspace's
//! first mine (a trusted SessionStart hook, or a trusted MCP server or daemon
//! start with `IRONMEM_WORKSPACE_ROOT` set, as the plugin wrapper sets both;
//! see `bootstrap::ensure_bootstrapped`), by the `ironmem claude` / `codex`
//! launchers' warm-up, and by `ironmem mine`. Mining stores only
//! files whose content changed, so an old row means either that mining stopped
//! or that no mined workspace changed; this check cannot tell which. It is also
//! the backstop for the `hook_mining` check, which infers a hook's mode from
//! config and can be wrong about it.

use std::path::Path;

use chrono::{NaiveDateTime, TimeDelta};

use super::hooks::miner_events;
use super::{is_db_busy, Check, CheckStatus, DOCTOR_DB_BUSY_TIMEOUT};
use crate::db::schema::Database;
use crate::MemoryError;

/// A newest mined row older than this many days is reported as a `Warn`.
const MINED_STALE_AFTER_DAYS: i64 = 7;

/// `filed_at` is SQLite `datetime('now')`: UTC, `YYYY-MM-DD HH:MM:SS`.
const FILED_AT_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

const KEY: &str = "mined_rows";

/// Read the newest mined row from the store at `db_path` and report its age.
/// `database` is the `database` line's status. Always returns a check, so
/// doctor's key set stays stable: a missing database, or one that line
/// reports as blocking, is an `Info` skip, because that line already
/// reports it.
pub(super) fn check_mined_rows(db_path: &Path, database: CheckStatus, now: NaiveDateTime) -> Check {
    if database.is_blocking() || !db_path.exists() {
        return Check::new(
            KEY,
            CheckStatus::Info,
            "mined-row age: skipped (no usable database)",
        );
    }
    // Same opener as `check_database`: no create, no migration, short busy wait.
    let newest = Database::open_with_busy_timeout(db_path, DOCTOR_DB_BUSY_TIMEOUT)
        .and_then(|db| db.newest_mined_filed_at());
    mined_rows_check(newest, now)
}

/// Decide the `mined_rows` check from the newest mined `filed_at` and `now`
/// (both UTC). Pure, so the thresholds are unit-tested without a clock.
fn mined_rows_check(newest: Result<Option<String>, MemoryError>, now: NaiveDateTime) -> Check {
    let filed_at = match newest {
        // The `database` line usually already reports a busy store; a second
        // hintless warning here would just repeat it.
        Err(e) if is_db_busy(&e) => {
            return Check::new(
                KEY,
                CheckStatus::Info,
                "mined-row age: skipped (database busy; re-run when idle)",
            )
        }
        Err(e) => {
            return Check::new(
                KEY,
                CheckStatus::Warn,
                format!("cannot read the newest mined row: {e}"),
            )
            .with_hint(
                "see the database line; if it is OK, re-run doctor, and if this persists the \
                 store may be damaged (restore it from a backup)",
            )
        }
        Ok(None) => {
            return Check::new(
                KEY,
                CheckStatus::Info,
                "no mined rows yet: no workspace file has been mined into this store",
            )
            .with_hint(format!(
                "trusted {} hooks mine changed workspace files on every run (see the \
                 hook_mining line); ironmem mine <dir> and the ironmem claude and ironmem codex \
                 launchers mine too, and a trusted SessionStart hook or a trusted MCP server \
                 start with IRONMEM_WORKSPACE_ROOT set mines a workspace the first time",
                miner_events(" and ")
            ))
        }
        Ok(Some(s)) => s,
    };
    let Ok(mined_at) = NaiveDateTime::parse_from_str(&filed_at, FILED_AT_FORMAT) else {
        return Check::new(
            KEY,
            CheckStatus::Warn,
            format!("newest mined row has an unreadable timestamp: {filed_at}"),
        );
    };
    let age = now.signed_duration_since(mined_at);
    // A row dated after now: the clock moved back, or the row's clock was
    // ahead. Its age says nothing, so say that instead.
    if age < TimeDelta::zero() {
        return Check::new(
            KEY,
            CheckStatus::Warn,
            format!("newest mined row is dated in the future ({filed_at} UTC)"),
        )
        .with_hint("check the system clock; the row's age cannot be judged until it is right");
    }
    if age > TimeDelta::days(MINED_STALE_AFTER_DAYS) {
        Check::new(
            KEY,
            CheckStatus::Warn,
            format!(
                "newest mined row is {} old ({filed_at} UTC): nothing new has been mined since",
                describe_age(age)
            ),
        )
        .with_hint(
            "only changed files are mined, so a workspace left unchanged that long looks the \
             same; MCP writes do not count; the hook_mining line says whether the hooks mine",
        )
    } else {
        Check::new(
            KEY,
            CheckStatus::Ok,
            format!(
                "newest mined row is {} old ({filed_at} UTC)",
                describe_age(age)
            ),
        )
    }
}

/// Coarse human age: whole days, else whole hours, else "under an hour".
/// A negative age reads as "under an hour"; `mined_rows_check` never passes
/// one.
fn describe_age(age: TimeDelta) -> String {
    let (days, hours) = (age.num_days(), age.num_hours());
    match (days, hours) {
        (d, _) if d >= 2 => format!("{d} days"),
        (1, _) => "1 day".to_string(),
        (_, h) if h >= 2 => format!("{h} hours"),
        (_, 1) => "1 hour".to_string(),
        _ => "under an hour".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn recent_row_is_ok_and_shows_its_age() {
        let c = mined_rows_check(
            Ok(Some("2026-11-12 10:00:00".into())),
            at("2026-11-14 12:00:00"),
        );
        assert_eq!(c.name, "mined_rows");
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.summary.contains("2 days old"), "{}", c.summary);
        assert!(
            c.summary.contains("2026-11-12 10:00:00 UTC"),
            "{}",
            c.summary
        );
    }

    #[test]
    fn row_older_than_seven_days_warns_and_says_it_may_be_an_unchanged_workspace() {
        let c = mined_rows_check(
            Ok(Some("2026-07-18 06:10:11".into())),
            at("2026-11-14 12:00:00"),
        );
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.summary.contains("119 days old"), "{}", c.summary);
        assert!(
            c.summary
                .ends_with("(2026-07-18 06:10:11 UTC): nothing new has been mined since"),
            "{}",
            c.summary
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("hook_mining"), "{hint}");
        // Mining stores only changed files, so the warning cannot tell a
        // stopped miner from an unchanged workspace; the hint must say so.
        assert!(hint.contains("workspace left unchanged"), "{hint}");
    }

    #[test]
    fn exactly_seven_days_is_still_ok_one_second_more_warns() {
        let now = at("2026-11-14 12:00:00");
        let ok = mined_rows_check(Ok(Some("2026-11-07 12:00:00".into())), now);
        assert_eq!(ok.status, CheckStatus::Ok, "{}", ok.summary);
        let stale = mined_rows_check(Ok(Some("2026-11-07 11:59:59".into())), now);
        assert_eq!(stale.status, CheckStatus::Warn, "{}", stale.summary);
    }

    #[test]
    fn no_mined_rows_is_info() {
        let c = mined_rows_check(Ok(None), at("2026-11-14 12:00:00"));
        assert_eq!(c.status, CheckStatus::Info);
        assert_eq!(
            c.summary,
            "no mined rows yet: no workspace file has been mined into this store"
        );
    }

    #[test]
    fn no_mined_rows_hint_names_every_writer() {
        let c = mined_rows_check(Ok(None), at("2026-11-14 12:00:00"));
        assert_eq!(
            c.hint.as_deref(),
            Some(
                "trusted Stop and PreCompact hooks mine changed workspace files on every run \
                 (see the hook_mining line); ironmem mine <dir> and the ironmem claude and \
                 ironmem codex launchers mine too, and a trusted SessionStart hook or a trusted \
                 MCP server start with IRONMEM_WORKSPACE_ROOT set mines a workspace the first \
                 time"
            )
        );
    }

    #[test]
    fn a_row_written_by_the_real_insert_path_reads_as_fresh() {
        // Pins FILED_AT_FORMAT to the schema's own `filed_at` default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let db = Database::open(&path).unwrap();
        db.migrate().unwrap();
        db.insert_drawer(
            &"a".repeat(32),
            "mined chunk",
            &vec![0.1; ironrace_embed::embedder::EMBED_DIM],
            "w",
            "r",
            "src/lib.rs",
            crate::db::drawers::MINED_ADDED_BY,
        )
        .unwrap();
        drop(db);
        let c = check_mined_rows(&path, CheckStatus::Ok, chrono::Utc::now().naive_utc());
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
        assert!(
            c.summary
                .starts_with("newest mined row is under an hour old ("),
            "{}",
            c.summary
        );
    }

    #[test]
    fn read_error_and_bad_timestamp_warn() {
        let now = at("2026-11-14 12:00:00");
        // A non-busy Db error (SQLITE_CORRUPT) must still warn, not skip.
        let corrupt = MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(11),
            None,
        ));
        let text = corrupt.to_string();
        let e = mined_rows_check(Err(corrupt), now);
        assert_eq!(e.status, CheckStatus::Warn);
        assert!(e.summary.contains(&text), "{}", e.summary);
        assert_eq!(
            e.hint.as_deref(),
            Some(
                "see the database line; if it is OK, re-run doctor, and if this persists the \
                 store may be damaged (restore it from a backup)"
            )
        );
        let bad = mined_rows_check(Ok(Some("yesterday".into())), now);
        assert_eq!(bad.status, CheckStatus::Warn);
        assert!(bad.summary.contains("yesterday"));
    }

    #[test]
    fn age_wording() {
        assert_eq!(describe_age(TimeDelta::minutes(5)), "under an hour");
        assert_eq!(describe_age(TimeDelta::minutes(61)), "1 hour");
        assert_eq!(describe_age(TimeDelta::hours(5)), "5 hours");
        assert_eq!(describe_age(TimeDelta::hours(30)), "1 day");
        assert_eq!(describe_age(TimeDelta::days(9)), "9 days");
        assert_eq!(describe_age(TimeDelta::minutes(-5)), "under an hour");
        // The boundaries.
        assert_eq!(describe_age(TimeDelta::minutes(59)), "under an hour");
        assert_eq!(describe_age(TimeDelta::minutes(119)), "1 hour");
        assert_eq!(describe_age(TimeDelta::minutes(120)), "2 hours");
        assert_eq!(describe_age(TimeDelta::hours(23)), "23 hours");
        assert_eq!(describe_age(TimeDelta::hours(24)), "1 day");
        assert_eq!(describe_age(TimeDelta::hours(47)), "1 day");
        assert_eq!(describe_age(TimeDelta::hours(48)), "2 days");
        assert_eq!(describe_age(TimeDelta::days(-3)), "under an hour");
    }

    #[test]
    fn a_row_dated_in_the_future_warns_about_the_clock() {
        let c = mined_rows_check(
            Ok(Some("2026-11-17 12:00:00".into())),
            at("2026-11-14 12:00:00"),
        );
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(
            c.summary,
            "newest mined row is dated in the future (2026-11-17 12:00:00 UTC)"
        );
        assert!(c.hint.as_deref().unwrap().contains("system clock"));
        // The same second is not the future.
        let same = mined_rows_check(
            Ok(Some("2026-11-14 12:00:00".into())),
            at("2026-11-14 12:00:00"),
        );
        assert_eq!(same.status, CheckStatus::Ok, "{}", same.summary);
    }

    #[test]
    fn a_schema_behind_database_is_still_read() {
        // The `database` line warns for it; `drawers.added_by` and
        // `filed_at` exist since the first migration, so the row is read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let db = Database::open(&path).unwrap();
        db.migrate().unwrap();
        db.insert_drawer(
            &"b".repeat(32),
            "mined chunk",
            &vec![0.1; ironrace_embed::embedder::EMBED_DIM],
            "w",
            "r",
            "src/lib.rs",
            crate::db::drawers::MINED_ADDED_BY,
        )
        .unwrap();
        drop(db);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "DELETE FROM schema_version WHERE version = ?1",
            [crate::db::schema::LATEST_SCHEMA_VERSION],
        )
        .unwrap();
        drop(conn);
        let behind = Database::open(&path).unwrap().schema_version().unwrap();
        assert_eq!(behind, crate::db::schema::LATEST_SCHEMA_VERSION - 1);
        let c = check_mined_rows(&path, CheckStatus::Warn, chrono::Utc::now().naive_utc());
        assert_eq!(c.status, CheckStatus::Ok, "{}", c.summary);
    }

    #[test]
    fn missing_database_is_skipped_as_info() {
        let dir = tempfile::tempdir().unwrap();
        let now = at("2026-11-14 12:00:00");
        let missing = check_mined_rows(&dir.path().join("absent.sqlite3"), CheckStatus::Ok, now);
        assert_eq!(missing.status, CheckStatus::Info);
        assert!(missing.summary.contains("skipped"), "{}", missing.summary);
    }

    #[test]
    fn unusable_database_is_skipped_even_when_the_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.sqlite3");
        std::fs::write(&path, b"not a database").unwrap();
        let unusable = check_mined_rows(&path, CheckStatus::Error, at("2026-11-14 12:00:00"));
        assert_eq!(unusable.status, CheckStatus::Info);
        assert!(unusable.summary.contains("skipped"), "{}", unusable.summary);
    }

    #[test]
    fn busy_or_locked_database_is_an_info_skip_not_a_second_warning() {
        let now = at("2026-11-14 12:00:00");
        for code in [5, 6] {
            let err = MemoryError::Db(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            let c = mined_rows_check(Err(err), now);
            assert_eq!(c.status, CheckStatus::Info, "code {code}");
            assert!(c.summary.contains("database busy"), "{}", c.summary);
        }
    }
}
