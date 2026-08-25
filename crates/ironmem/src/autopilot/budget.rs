//! Daily budget ledger — `logical_key` per date, accumulated from each
//! invocation's `total_cost_usd` (spec's *Budget accounting* section).
//!
//! This module owns the accumulation (read-current, add, overwrite) because
//! the spec is explicit that the ledger is a running sum across invocations,
//! not a bag of independent samples a caller would have to total up itself.
//! It is not fully race-safe under concurrent writers (see
//! [`accumulate_daily_spend`]'s doc) — acceptable for v1, whose design has
//! exactly one Lead process at a time (see the spec's *Non-goals*: "No
//! second peer Lead").

use serde::{Deserialize, Serialize};

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::{read_current, write_current};

/// One date's accumulated spend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetLedgerEntry {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub total_cost_usd: f64,
    /// Number of invocations (IC + Reviewer dispatches) folded into
    /// `total_cost_usd` so far today.
    pub dispatch_count: u32,
}

fn budget_key(date: &str) -> String {
    format!("budget-ledger:{date}")
}

fn validate_date(date: &str) -> Result<(), MemoryError> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| MemoryError::Validation(format!("date must be YYYY-MM-DD, got '{date}'")))
}

/// Add `delta_usd` (one invocation's `total_cost_usd`) to `date`'s ledger and
/// persist the new total. Returns the ledger entry after the update.
///
/// **Not atomic across concurrent callers**: this reads the current entry,
/// adds in-process, and writes it back as two separate database
/// round-trips. Two callers racing on the same date could both read the same
/// starting total and one update would be lost. Fine for a single Lead
/// process serializing its own dispatch accounting (the only writer v1's
/// design has); would need to move the read+add+write inside one
/// `Database::with_transaction` if a second concurrent writer is ever
/// introduced.
pub fn accumulate_daily_spend(
    db: &Database,
    date: &str,
    delta_usd: f64,
) -> Result<BudgetLedgerEntry, MemoryError> {
    validate_date(date)?;
    if !delta_usd.is_finite() || delta_usd < 0.0 {
        return Err(MemoryError::Validation(
            "delta_usd must be a finite, non-negative number".into(),
        ));
    }

    let key = budget_key(date);
    let mut entry = match read_current(db, &key)? {
        Some(drawer) => serde_json::from_str::<BudgetLedgerEntry>(&drawer.content)?,
        None => BudgetLedgerEntry {
            date: date.to_string(),
            total_cost_usd: 0.0,
            dispatch_count: 0,
        },
    };
    entry.total_cost_usd += delta_usd;
    entry.dispatch_count += 1;

    let content = serde_json::to_string(&entry)?;
    write_current(db, &key, &content)?;
    Ok(entry)
}

/// Read a date's ledger entry, if anything has been billed to it yet.
pub fn get_daily_spend(
    db: &Database,
    date: &str,
) -> Result<Option<BudgetLedgerEntry>, MemoryError> {
    validate_date(date)?;
    match read_current(db, &budget_key(date))? {
        None => Ok(None),
        Some(drawer) => Ok(Some(serde_json::from_str(&drawer.content)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_across_calls_for_the_same_date() {
        let db = Database::open_in_memory().unwrap();
        accumulate_daily_spend(&db, "2026-08-25", 0.187).unwrap();
        let entry = accumulate_daily_spend(&db, "2026-08-25", 0.036).unwrap();

        assert!((entry.total_cost_usd - 0.223).abs() < 1e-9);
        assert_eq!(entry.dispatch_count, 2);

        // Still one row, not two — this is a logical_key overwrite of a
        // running total, not an append-only ledger of samples.
        let db_rows = db
            .get_drawers(
                Some(super::super::WING),
                Some(super::super::ROOM),
                usize::MAX,
            )
            .unwrap();
        assert_eq!(db_rows.len(), 1);
    }

    #[test]
    fn separate_dates_do_not_share_a_ledger() {
        let db = Database::open_in_memory().unwrap();
        accumulate_daily_spend(&db, "2026-08-25", 1.0).unwrap();
        accumulate_daily_spend(&db, "2026-08-26", 2.0).unwrap();

        assert_eq!(
            get_daily_spend(&db, "2026-08-25")
                .unwrap()
                .unwrap()
                .total_cost_usd,
            1.0
        );
        assert_eq!(
            get_daily_spend(&db, "2026-08-26")
                .unwrap()
                .unwrap()
                .total_cost_usd,
            2.0
        );
    }

    #[test]
    fn missing_date_is_none_not_an_error() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(get_daily_spend(&db, "2026-01-01").unwrap(), None);
    }

    #[test]
    fn rejects_malformed_date_and_negative_delta() {
        let db = Database::open_in_memory().unwrap();
        assert!(accumulate_daily_spend(&db, "not-a-date", 1.0).is_err());
        assert!(accumulate_daily_spend(&db, "2026-08-25", -1.0).is_err());
        assert!(accumulate_daily_spend(&db, "2026-08-25", f64::NAN).is_err());
    }
}
