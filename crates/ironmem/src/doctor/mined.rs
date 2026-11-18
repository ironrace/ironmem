//! The `mined_rows` doctor check (#377): how old the newest mined drawer is.
//!
//! MCP writes keep a store looking alive while hook mining has stopped, so the
//! age of the newest `added_by = 'mine'` row (a chunk of a workspace file,
//! written by a trusted hook, the launcher's warm-up or `ironmem mine`) is the
//! direct evidence of whether files are still being mined. Mining stores only
//! files whose content changed, so an old row means either that mining stopped
//! or that no mined workspace changed; this check cannot tell which. It is also
//! the backstop for the `hook_mining` check, which infers a hook's mode from
//! config and can be wrong about it.

use std::path::Path;
use std::time::Duration;

use chrono::{NaiveDateTime, TimeDelta};

use super::{is_db_busy, Check, CheckStatus};
use crate::db::schema::Database;
use crate::MemoryError;

/// A newest mined row older than this many days is reported as a `Warn`.
pub(super) const MINED_STALE_AFTER_DAYS: i64 = 7;

/// `filed_at` is SQLite `datetime('now')`: UTC, `YYYY-MM-DD HH:MM:SS`.
const FILED_AT_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

const KEY: &str = "mined_rows";

/// Read the newest mined row from the store at `db_path` and report its age.
/// Always returns a check, so doctor's key set stays stable: a missing or
/// unusable database is an `Info` skip, because the `database` line already
/// reports it.
pub(super) fn check_mined_rows(db_path: &Path, database_usable: bool, now: NaiveDateTime) -> Check {
    if !database_usable || !db_path.exists() {
        return Check::new(
            KEY,
            CheckStatus::Info,
            "mined-row age: skipped (no usable database)",
        );
    }
    // Same opener as `check_database`: no create, no migration, short busy wait.
    let newest = Database::open_with_busy_timeout(db_path, Duration::from_millis(500))
        .and_then(|db| db.newest_mined_filed_at());
    mined_rows_check(newest, now)
}

/// Decide the `mined_rows` check from the newest mined `filed_at` and `now`
/// (both UTC). Pure, so the thresholds are unit-tested without a clock.
pub(super) fn mined_rows_check(
    newest: Result<Option<String>, MemoryError>,
    now: NaiveDateTime,
) -> Check {
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
        }
        Ok(None) => {
            return Check::new(
                KEY,
                CheckStatus::Info,
                "no mined rows yet: no workspace file has been mined into this store",
            )
            .with_hint("mining runs in the Stop and PreCompact hooks; see the hook_mining line")
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
/// A future timestamp (clock skew) reads as "under an hour".
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
    fn row_older_than_seven_days_warns_that_mining_stopped() {
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
    }

    #[test]
    fn missing_database_is_skipped_as_info() {
        let dir = tempfile::tempdir().unwrap();
        let now = at("2026-11-14 12:00:00");
        let missing = check_mined_rows(&dir.path().join("absent.sqlite3"), true, now);
        assert_eq!(missing.status, CheckStatus::Info);
        assert!(missing.summary.contains("skipped"), "{}", missing.summary);
    }

    #[test]
    fn unusable_database_is_skipped_even_when_the_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.sqlite3");
        std::fs::write(&path, b"not a database").unwrap();
        let unusable = check_mined_rows(&path, false, at("2026-11-14 12:00:00"));
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
