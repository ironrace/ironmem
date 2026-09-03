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
    /// Invocations that happened today but reported **no price**, so their
    /// spend is missing from `total_cost_usd`.
    ///
    /// Non-zero means `total_cost_usd` is a **floor, not a total**. The
    /// Codex reviewer (rung 5) lands here: `codex exec --json` reports token
    /// counts and no dollar figure, and the spec's authoritative meter is
    /// "the sum of `total_cost_usd` across IC **and Reviewer**
    /// invocations". Banking `0.0` for those would satisfy the type and
    /// quietly under-report the day — the exact failure mode the spec's
    /// *Budget accounting* section rejects transcript ingestion for. So an
    /// unpriced invocation increments this instead, and a reader can see
    /// the ledger is incomplete rather than trusting a wrong total.
    ///
    /// `#[serde(default)]` so ledger drawers written before rung 5 read
    /// back as "nothing unpriced", which is true of them: every writer that
    /// existed then reported a price.
    #[serde(default)]
    pub unpriced_dispatch_count: u32,
    /// Advisor one-shot calls (rung 9) billed to this date, priced or not.
    ///
    /// Its **own field** because it is the quantity
    /// [`super::advise::AdviceConfig::max_calls_per_day`] bounds, and an
    /// advisor call is a different *kind* of thing from an IC dispatch: one
    /// bounded turn with no tools, capped at cents, potentially made several
    /// times per Lead tick.
    ///
    /// It is **not** carved out of `dispatch_count`: a priced advisor call
    /// increments that too, because `dispatch_count` counts what has been
    /// folded into `total_cost_usd` and a priced call has been. An *unpriced*
    /// call is the mirror image: it moves this field and
    /// `unpriced_advice_count`, and deliberately neither `dispatch_count` nor
    /// `unpriced_dispatch_count`. So the two counters **overlap on priced
    /// calls** and only there — this is neither a subset of `dispatch_count`
    /// nor disjoint from it, and summing them double-counts.
    #[serde(default)]
    pub advice_call_count: u32,
    /// Advisor calls whose price could not be read, so their spend is missing
    /// from `total_cost_usd`.
    ///
    /// **Deliberately not `unpriced_dispatch_count`.** Rung 7 already shares
    /// that counter between IC dispatches and Codex reviews, and
    /// [`super::run::RunConfig::max_unpriced_dispatches_per_day`] bounds it
    /// at a value sized for `$2.50` dispatches. Folding a third writer into
    /// it — one whose calls are cheap but frequent — would let a flaky
    /// advisor stop every IC dispatch on any repo with a wall-clock bound.
    /// That is rung 7's lesson 26 read correctly: inherit the earlier
    /// mechanism's *fix* (bound unpriced spend by count) rather than its
    /// *shape* (the same counter).
    #[serde(default)]
    pub unpriced_advice_count: u32,
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
    update_ledger(db, date, |entry| {
        entry.total_cost_usd += delta_usd;
        entry.dispatch_count += 1;
    })
}

/// Record that an invocation happened today whose cost could not be
/// determined, without moving `total_cost_usd`.
///
/// Deliberately **not** `accumulate_daily_spend(db, date, 0.0)`: that would
/// count the invocation as costing nothing, which is a claim, whereas this
/// records the truth — an invocation whose price is unknown. See
/// [`BudgetLedgerEntry::unpriced_dispatch_count`].
///
/// Shares [`accumulate_daily_spend`]'s non-atomicity caveat.
pub fn record_unpriced_dispatch(
    db: &Database,
    date: &str,
) -> Result<BudgetLedgerEntry, MemoryError> {
    validate_date(date)?;
    update_ledger(db, date, |entry| {
        entry.unpriced_dispatch_count += 1;
    })
}

/// Bill one advisor one-shot call (rung 9) to `date`.
///
/// One ledger write, not two: the call's price and the fact that a call
/// happened at all are two fields of the same fact, and
/// [`accumulate_daily_spend`]'s read-modify-write is not atomic, so
/// recording them separately would give a concurrent writer a window to lose
/// one of them.
///
/// `cost_usd` is `None` when the call's price could not be read — the same
/// distinction [`record_unpriced_dispatch`] draws, and for the same reason:
/// `Some(0.0)` is a claim that it was free, which is never known to be true
/// of a process that ran.
pub fn record_advice_call(
    db: &Database,
    date: &str,
    cost_usd: Option<f64>,
) -> Result<BudgetLedgerEntry, MemoryError> {
    validate_date(date)?;
    // A non-finite or negative price is not a price. Routed to the unpriced
    // counter rather than rejected, because rejecting here would discard the
    // record of a call that really happened — rung 5's review finding #2.
    let cost_usd = cost_usd.filter(|c| c.is_finite() && *c >= 0.0);
    update_ledger(db, date, |entry| {
        entry.advice_call_count += 1;
        match cost_usd {
            Some(cost) => {
                entry.total_cost_usd += cost;
                entry.dispatch_count += 1;
            }
            None => entry.unpriced_advice_count += 1,
        }
    })
}

/// Read the day's ledger, apply `mutate`, write it back.
fn update_ledger(
    db: &Database,
    date: &str,
    mutate: impl FnOnce(&mut BudgetLedgerEntry),
) -> Result<BudgetLedgerEntry, MemoryError> {
    let key = budget_key(date);
    let mut entry = match read_current(db, &key)? {
        Some(drawer) => serde_json::from_str::<BudgetLedgerEntry>(&drawer.content)?,
        None => BudgetLedgerEntry {
            date: date.to_string(),
            total_cost_usd: 0.0,
            dispatch_count: 0,
            unpriced_dispatch_count: 0,
            advice_call_count: 0,
            unpriced_advice_count: 0,
        },
    };
    mutate(&mut entry);

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
    fn an_unpriced_dispatch_does_not_move_the_total() {
        let db = Database::open_in_memory().unwrap();
        accumulate_daily_spend(&db, "2026-08-31", 1.25).unwrap();
        let entry = record_unpriced_dispatch(&db, "2026-08-31").unwrap();

        // The whole point: the invocation is visible, but it did not get
        // counted as costing $0.
        assert_eq!(entry.total_cost_usd, 1.25);
        assert_eq!(entry.dispatch_count, 1);
        assert_eq!(entry.unpriced_dispatch_count, 1);
    }

    #[test]
    fn unpriced_dispatches_accumulate_on_their_own_counter() {
        let db = Database::open_in_memory().unwrap();
        record_unpriced_dispatch(&db, "2026-08-31").unwrap();
        let entry = record_unpriced_dispatch(&db, "2026-08-31").unwrap();
        assert_eq!(entry.unpriced_dispatch_count, 2);
        assert_eq!(entry.dispatch_count, 0);
        assert_eq!(entry.total_cost_usd, 0.0);
    }

    #[test]
    fn a_pre_rung_5_ledger_drawer_still_deserializes() {
        // Ledger drawers written before `unpriced_dispatch_count` existed
        // carry no such field. They must read back as "nothing unpriced"
        // rather than failing the whole ledger read — which would make the
        // budget check error out and stop every dispatch.
        let db = Database::open_in_memory().unwrap();
        write_current(
            &db,
            &budget_key("2026-08-25"),
            r#"{"date":"2026-08-25","total_cost_usd":0.22,"dispatch_count":2}"#,
        )
        .unwrap();

        let entry = get_daily_spend(&db, "2026-08-25").unwrap().unwrap();
        assert_eq!(entry.unpriced_dispatch_count, 0);
        assert_eq!(entry.dispatch_count, 2);

        // And a later write from the new code path preserves the old fields.
        let entry = record_unpriced_dispatch(&db, "2026-08-25").unwrap();
        assert!((entry.total_cost_usd - 0.22).abs() < 1e-9);
        assert_eq!(entry.dispatch_count, 2);
        assert_eq!(entry.unpriced_dispatch_count, 1);
    }

    #[test]
    fn rejects_malformed_date_and_negative_delta() {
        let db = Database::open_in_memory().unwrap();
        assert!(accumulate_daily_spend(&db, "not-a-date", 1.0).is_err());
        assert!(record_unpriced_dispatch(&db, "not-a-date").is_err());
        assert!(accumulate_daily_spend(&db, "2026-08-25", -1.0).is_err());
        assert!(accumulate_daily_spend(&db, "2026-08-25", f64::NAN).is_err());
        assert!(record_advice_call(&db, "not-a-date", Some(0.01)).is_err());
    }

    #[test]
    fn a_priced_advice_call_moves_the_dollar_total_and_both_counts() {
        let db = Database::open_in_memory().unwrap();
        let entry = record_advice_call(&db, "2026-09-02", Some(0.02)).unwrap();

        assert!((entry.total_cost_usd - 0.02).abs() < 1e-9);
        assert_eq!(entry.dispatch_count, 1, "it is billed spend like any other");
        assert_eq!(entry.advice_call_count, 1);
        assert_eq!(entry.unpriced_advice_count, 0);
    }

    #[test]
    fn an_unpriced_advice_call_never_reports_as_free_and_never_touches_the_dispatch_counter() {
        // The load-bearing half: `unpriced_dispatch_count` gates IC
        // dispatches through `max_unpriced_dispatches_per_day`, so a flaky
        // advisor must not be able to stop real work.
        let db = Database::open_in_memory().unwrap();
        record_advice_call(&db, "2026-09-02", None).unwrap();
        let entry = record_advice_call(&db, "2026-09-02", Some(f64::NAN)).unwrap();

        assert_eq!(entry.total_cost_usd, 0.0);
        assert_eq!(entry.dispatch_count, 0);
        assert_eq!(entry.advice_call_count, 2);
        assert_eq!(entry.unpriced_advice_count, 2, "NaN is not a price");
        assert_eq!(entry.unpriced_dispatch_count, 0);
    }

    #[test]
    fn a_pre_rung_9_ledger_drawer_still_deserializes() {
        let db = Database::open_in_memory().unwrap();
        write_current(
            &db,
            &budget_key("2026-09-01"),
            r#"{"date":"2026-09-01","total_cost_usd":1.5,"dispatch_count":3,
                "unpriced_dispatch_count":1}"#,
        )
        .unwrap();

        let entry = get_daily_spend(&db, "2026-09-01").unwrap().unwrap();
        assert_eq!(entry.advice_call_count, 0);
        assert_eq!(entry.unpriced_advice_count, 0);

        let entry = record_advice_call(&db, "2026-09-01", Some(0.5)).unwrap();
        assert!((entry.total_cost_usd - 2.0).abs() < 1e-9);
        assert_eq!(entry.unpriced_dispatch_count, 1, "old fields preserved");
    }
}
