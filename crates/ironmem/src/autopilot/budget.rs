//! Daily budget ledger — `logical_key` per date, accumulated from each
//! invocation's `total_cost_usd` (spec's *Budget accounting* section).
//!
//! This module owns the accumulation (read-current, add, overwrite) because
//! the spec is explicit that the ledger is a running sum across invocations,
//! not a bag of independent samples a caller would have to total up itself.
//! Every writer's read-modify-write runs in one `BEGIN IMMEDIATE`
//! transaction, so concurrent writers on one database (a Codex Lead and a
//! Muse Lead sharing the default DB, say) serialize rather than lose each
//! other's update, and the unpriced cap is checked in the same transaction
//! that reserves against it.

use serde::{Deserialize, Serialize};

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::{read_current, write_current_tx};

/// One reserved dollar ceiling, held against a date's total from before
/// its dispatch launches until it settles. Identified, and tied to its
/// issue, so a reservation its controller died holding can be released
/// by the run that next takes the issue over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendReservationRecord {
    pub id: String,
    /// The issue's canonical `owner/repo#n`.
    pub issue: String,
    pub amount_usd: f64,
}

/// One date's accumulated spend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetLedgerEntry {
    /// Dollar ceilings currently reserved (see [`SpendReservationRecord`]);
    /// their amounts are already inside `total_cost_usd`.
    #[serde(default)]
    pub spend_reservations: Vec<SpendReservationRecord>,
    /// Durable invocation identities reserved before launching unpriced implementations.
    #[serde(default)]
    pub reserved_unpriced_dispatch_ids: Vec<String>,
    /// `YYYY-MM-DD`.
    pub date: String,
    pub total_cost_usd: f64,
    /// Number of invocations (IC + Reviewer dispatches) folded into
    /// `total_cost_usd` so far today.
    pub dispatch_count: u32,
    /// IC dispatches (fix/remediation runs included) that happened today
    /// but reported **no price**, so their spend is missing from
    /// `total_cost_usd`. Codex and Muse invocations reserve this counter before launch;
    /// Claude outcomes with no meter (including wall-clock kills) also land here.
    ///
    /// Non-zero (or any other unpriced counter non-zero — see
    /// [`Self::has_unpriced_spend`]) means
    /// `total_cost_usd` is a **floor, not a total**. Banking `0.0` for
    /// those would satisfy the type and quietly under-report the day —
    /// the exact failure mode the spec's *Budget accounting* section
    /// rejects transcript ingestion for. So an unpriced dispatch
    /// increments this instead, and a reader can see the ledger is
    /// incomplete rather than trusting a wrong total.
    ///
    /// Bounded by
    /// [`super::run::RunConfig::max_unpriced_dispatches_per_day`]. Reviews
    /// have their own counter ([`Self::unpriced_review_count`]) so they
    /// cannot consume this headroom.
    ///
    /// `#[serde(default)]` so ledger drawers written before rung 5 read
    /// back as "nothing unpriced", which is true of them: every writer that
    /// existed then reported a price.
    #[serde(default)]
    pub unpriced_dispatch_count: u32,
    /// Codex/Muse reviews that happened today but reported **no price**,
    /// so their spend is missing from `total_cost_usd`.
    ///
    /// Its own field for the same reason the advisor got one at rung 9:
    /// inherit the earlier mechanism's *fix* (bound unpriced spend by
    /// count) rather than its *shape* (the same counter). Until this
    /// split, every review incremented `unpriced_dispatch_count`, so five
    /// reviews exhausted the IC dispatch headroom
    /// (`max_unpriced_dispatches_per_day`, default 5) and the Lead stopped
    /// dispatching with `UnpricedDispatchesExhausted`. Bounded by
    /// `max_unpriced_reviews_per_day` (default 20).
    ///
    /// `#[serde(default)]` so ledger drawers written before this split
    /// read back as "no unpriced reviews", and existing fields are kept.
    #[serde(default)]
    pub unpriced_review_count: u32,
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
    /// **Deliberately not `unpriced_dispatch_count`.**
    /// [`super::run::RunConfig::max_unpriced_dispatches_per_day`] bounds
    /// that counter at a value sized for `$2.50` dispatches. Folding
    /// another writer into it — one whose calls are cheap but frequent —
    /// would let a flaky advisor stop every IC dispatch on any repo with
    /// a wall-clock bound. Reviews learned the same lesson and now have
    /// `unpriced_review_count`. Inherit the earlier mechanism's *fix*
    /// (bound unpriced spend by count) rather than its *shape* (the same
    /// counter).
    #[serde(default)]
    pub unpriced_advice_count: u32,
}

impl BudgetLedgerEntry {
    /// Whether the day's dollar total is a floor, not a total: any
    /// unpriced counter (dispatch, review, or advice) is non-zero.
    pub fn has_unpriced_spend(&self) -> bool {
        self.unpriced_dispatch_count > 0
            || self.unpriced_review_count > 0
            || self.unpriced_advice_count > 0
    }
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
/// persist the new total. Returns the ledger entry after the update. Atomic
/// across concurrent callers; see the module doc.
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
pub fn record_unpriced_dispatch(
    db: &Database,
    date: &str,
) -> Result<BudgetLedgerEntry, MemoryError> {
    validate_date(date)?;
    update_ledger(db, date, |entry| {
        entry.unpriced_dispatch_count += 1;
    })
}

/// Record that a review happened today whose cost could not be
/// determined, without moving `total_cost_usd` or the dispatch counter.
/// See [`BudgetLedgerEntry::unpriced_review_count`].
pub fn record_unpriced_review(db: &Database, date: &str) -> Result<BudgetLedgerEntry, MemoryError> {
    validate_date(date)?;
    update_ledger(db, date, |entry| {
        entry.unpriced_review_count += 1;
    })
}

/// Bill one advisor one-shot call (rung 9) to `date`.
///
/// One ledger write, not two: the call's price and the fact that a call
/// happened at all are two fields of the same fact, so they are recorded in
/// one transaction rather than two a crash could split.
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

/// Reserve an unpriced invocation before launch, with no cap. Repeated
/// recovery of the same identity cannot bill it twice. Every dispatch path
/// must use [`reserve_unpriced_dispatch_if_below`] instead, which respects
/// the daily cap; this one is for tests and for recording a reservation
/// already made.
pub fn reserve_unpriced_dispatch(
    db: &Database,
    date: &str,
    invocation_id: &str,
) -> Result<BudgetLedgerEntry, MemoryError> {
    reserve(db, date, invocation_id, None).map(|(entry, _)| entry)
}

/// What [`reserve_unpriced_dispatch_if_below`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpricedReservation {
    /// A new reservation was written; the invocation may launch.
    Reserved,
    /// This invocation was already reserved, so nothing changed. Recovery
    /// re-reserving what it already paid for is never a new dispatch.
    AlreadyReserved,
    /// The day's cap is reached and nothing was written; do not launch.
    CapReached { unpriced_today: u32 },
}

/// Reserve an unpriced invocation before launch only while the day has fewer
/// than `cap` reserved, checking and reserving in one `BEGIN IMMEDIATE`
/// transaction. Two Leads racing for the last unit therefore admit exactly
/// one, where a separate check then reserve would admit both.
pub fn reserve_unpriced_dispatch_if_below(
    db: &Database,
    date: &str,
    invocation_id: &str,
    cap: u32,
) -> Result<UnpricedReservation, MemoryError> {
    reserve(db, date, invocation_id, Some(cap)).map(|(_, decision)| decision)
}

/// The one reservation both public forms share: `invocation_id` once, and
/// only below `cap` when there is one.
fn reserve(
    db: &Database,
    date: &str,
    invocation_id: &str,
    cap: Option<u32>,
) -> Result<(BudgetLedgerEntry, UnpricedReservation), MemoryError> {
    validate_reservation(date, invocation_id)?;
    transact_ledger(db, date, |entry| {
        if entry
            .reserved_unpriced_dispatch_ids
            .iter()
            .any(|id| id == invocation_id)
        {
            return (false, UnpricedReservation::AlreadyReserved);
        }
        if cap.is_some_and(|cap| entry.unpriced_dispatch_count >= cap) {
            return (
                false,
                UnpricedReservation::CapReached {
                    unpriced_today: entry.unpriced_dispatch_count,
                },
            );
        }
        entry
            .reserved_unpriced_dispatch_ids
            .push(invocation_id.to_string());
        entry.unpriced_dispatch_count += 1;
        (true, UnpricedReservation::Reserved)
    })
}

/// What [`reserve_spend_if_within`] decided.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpendReservation {
    /// `amount_usd` is added to the day's total; settle it with
    /// [`settle_reserved_spend`] once the dispatch's cost is known.
    Reserved,
    /// The day's total plus `amount_usd` would pass the budget; nothing was
    /// written, and nothing may launch.
    BudgetReached { spent_usd: f64 },
}

/// Reserve a dispatch's dollar ceiling (`amount_usd`) against `date`'s
/// `budget_usd` before it launches, checking and adding in one
/// `BEGIN IMMEDIATE` transaction. Two Leads that would together pass the
/// budget therefore admit only what fits, where reading the total then
/// adding after the dispatch would admit both.
pub fn reserve_spend_if_within(
    db: &Database,
    date: &str,
    amount_usd: f64,
    budget_usd: f64,
    issue: &str,
    id: &str,
) -> Result<SpendReservation, MemoryError> {
    validate_date(date)?;
    validate_amount(amount_usd)?;
    let (_, decision) = transact_ledger(db, date, |entry| {
        if entry.total_cost_usd + amount_usd > budget_usd {
            return (
                false,
                SpendReservation::BudgetReached {
                    spent_usd: entry.total_cost_usd,
                },
            );
        }
        entry.total_cost_usd += amount_usd;
        entry.spend_reservations.push(SpendReservationRecord {
            id: id.to_string(),
            issue: issue.to_string(),
            amount_usd,
        });
        (true, SpendReservation::Reserved)
    })?;
    Ok(decision)
}

/// Settle the [`reserve_spend_if_within`] reservation `id` on `date` (the
/// day it was reserved on): the measured `cost_usd` replaces it and
/// counts as a dispatch, or, with no price, the reservation is only
/// released (the caller records the unpriced dispatch on its own
/// counter). A reservation already released (its controller died and
/// the next run released it) subtracts nothing; its cost still counts.
pub fn settle_reserved_spend(
    db: &Database,
    date: &str,
    id: &str,
    cost_usd: Option<f64>,
) -> Result<BudgetLedgerEntry, MemoryError> {
    validate_date(date)?;
    if let Some(cost) = cost_usd {
        validate_amount(cost)?;
    }
    update_ledger(db, date, |entry| {
        if let Some(at) = entry.spend_reservations.iter().position(|r| r.id == id) {
            let record = entry.spend_reservations.remove(at);
            entry.total_cost_usd = (entry.total_cost_usd - record.amount_usd).max(0.0);
        }
        if let Some(cost) = cost_usd {
            entry.total_cost_usd += cost;
            entry.dispatch_count += 1;
        }
    })
}

/// Release every spend reservation held for `issue` on `date`, returning
/// what it gave back: a reservation its controller died holding (killed
/// or crashed between reserving and settling) otherwise inflates the
/// day's total until midnight, however idle the issue now is.
pub fn release_issue_spend_reservations(
    db: &Database,
    date: &str,
    issue: &str,
) -> Result<f64, MemoryError> {
    validate_date(date)?;
    let (_, released) = transact_ledger(db, date, |entry| {
        let released: f64 = entry
            .spend_reservations
            .iter()
            .filter(|r| r.issue == issue)
            .map(|r| r.amount_usd)
            .sum();
        if released == 0.0 {
            return (false, 0.0);
        }
        entry.spend_reservations.retain(|r| r.issue != issue);
        entry.total_cost_usd = (entry.total_cost_usd - released).max(0.0);
        (true, released)
    })?;
    Ok(released)
}

fn validate_amount(amount_usd: f64) -> Result<(), MemoryError> {
    if !amount_usd.is_finite() || amount_usd < 0.0 {
        return Err(MemoryError::Validation(
            "an amount in USD must be a finite, non-negative number".into(),
        ));
    }
    Ok(())
}

fn validate_reservation(date: &str, invocation_id: &str) -> Result<(), MemoryError> {
    validate_date(date)?;
    if invocation_id.trim().is_empty() || invocation_id.trim() != invocation_id {
        return Err(MemoryError::Validation(
            "invocation_id must be nonempty without surrounding whitespace".into(),
        ));
    }
    Ok(())
}

/// Whether this exact invocation was durably reserved. Check every retained
/// daily ledger: an invocation may cross midnight or remain held for several
/// days, and the issue's start date is not its per-invocation reservation date.
/// This is read-only and never infers spend from prepared artifact files alone.
pub(crate) fn has_unpriced_dispatch_reservation(
    db: &Database,
    invocation_id: &str,
) -> Result<bool, MemoryError> {
    let prefix = format!(
        "{}{}",
        crate::mcp::tools::LOGICAL_KEY_SOURCE_PREFIX,
        budget_key("")
    );
    let drawers =
        db.get_drawers_by_source_prefix(super::WING, super::ROOM, &prefix, i64::MAX as usize)?;
    for drawer in drawers {
        // One corrupt day must not fail every recovery: it is named, and the
        // other days are still read.
        let entry: BudgetLedgerEntry = match serde_json::from_str(&drawer.content) {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    ledger = %drawer.source_file,
                    "autopilot skips a budget ledger it cannot read while looking for an \
                     invocation's reservation"
                );
                continue;
            }
        };
        if entry
            .reserved_unpriced_dispatch_ids
            .iter()
            .any(|id| id == invocation_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Read the day's ledger, apply `mutate`, write it back, atomically.
fn update_ledger(
    db: &Database,
    date: &str,
    mutate: impl FnOnce(&mut BudgetLedgerEntry),
) -> Result<BudgetLedgerEntry, MemoryError> {
    transact_ledger(db, date, |entry| {
        mutate(entry);
        (true, ())
    })
    .map(|(entry, ())| entry)
}

/// The one read-modify-write every ledger writer goes through: read the
/// day's entry, let `decide` change it, and write it back only when `decide`
/// says to, all inside one `BEGIN IMMEDIATE` transaction. Concurrent writers
/// (two Leads, or a Lead and a reviewer, on one database) therefore
/// serialize instead of overwriting each other's update. Returns the entry
/// as it stands after the transaction.
fn transact_ledger<T>(
    db: &Database,
    date: &str,
    decide: impl FnOnce(&mut BudgetLedgerEntry) -> (bool, T),
) -> Result<(BudgetLedgerEntry, T), MemoryError> {
    let key = budget_key(date);
    db.with_immediate_transaction(|tx| {
        // `read_current` goes through `db`, the same connection as `tx`, so
        // it reads inside this transaction.
        let mut entry = match read_current(db, &key)? {
            Some(drawer) => serde_json::from_str::<BudgetLedgerEntry>(&drawer.content)?,
            None => BudgetLedgerEntry {
                spend_reservations: Vec::new(),
                reserved_unpriced_dispatch_ids: Vec::new(),
                date: date.to_string(),
                total_cost_usd: 0.0,
                dispatch_count: 0,
                unpriced_dispatch_count: 0,
                unpriced_review_count: 0,
                advice_call_count: 0,
                unpriced_advice_count: 0,
            },
        };
        let (write, value) = decide(&mut entry);
        if write {
            write_current_tx(tx, &key, &serde_json::to_string(&entry)?)?;
        }
        Ok((entry, value))
    })
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
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::autopilot::write_current;

    /// A migrated database file that each racing thread opens on its own
    /// connection, as two Lead processes would.
    fn shared_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        Database::open(&path).unwrap().migrate().unwrap();
        (dir, path)
    }

    /// Run `work(thread_index, db)` on `threads` connections released at once.
    fn race<T: Send + 'static>(
        path: &std::path::Path,
        threads: usize,
        work: impl Fn(usize, &Database) -> T + Send + Sync + 'static,
    ) -> Vec<T> {
        let barrier = Arc::new(Barrier::new(threads));
        let work = Arc::new(work);
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (path, barrier, work) = (path.to_path_buf(), barrier.clone(), work.clone());
                std::thread::spawn(move || {
                    let db = Database::open(&path).unwrap();
                    barrier.wait();
                    work(t, &db)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    }

    #[test]
    fn concurrent_ledger_writers_lose_no_reservation_and_no_dollar() {
        const THREADS: usize = 4;
        const EACH: usize = 25;
        let (_dir, path) = shared_db();
        race(&path, THREADS, |t, db| {
            for i in 0..EACH {
                reserve_unpriced_dispatch(db, "2026-09-03", &format!("t{t}-{i}")).unwrap();
                accumulate_daily_spend(db, "2026-09-03", 0.25).unwrap();
            }
        });
        let db = Database::open(&path).unwrap();
        let entry = get_daily_spend(&db, "2026-09-03").unwrap().unwrap();
        let ids: std::collections::BTreeSet<_> =
            entry.reserved_unpriced_dispatch_ids.iter().collect();
        assert_eq!(ids.len(), THREADS * EACH, "every reservation kept");
        assert_eq!(entry.unpriced_dispatch_count as usize, THREADS * EACH);
        assert_eq!(entry.dispatch_count as usize, THREADS * EACH);
        assert!((entry.total_cost_usd - 0.25 * (THREADS * EACH) as f64).abs() < 1e-9);
    }

    #[test]
    fn two_leads_racing_for_the_last_unit_admit_exactly_one() {
        const CAP: u32 = 3;
        let (_dir, path) = shared_db();
        for round in 1..=20u32 {
            let date = format!("2026-09-{round:02}");
            {
                let db = Database::open(&path).unwrap();
                for n in 0..CAP - 1 {
                    reserve_unpriced_dispatch(&db, &date, &format!("earlier-{n}")).unwrap();
                }
            }
            let racing_date = date.clone();
            let results = race(&path, 2, move |t, db| {
                reserve_unpriced_dispatch_if_below(db, &racing_date, &format!("racer-{t}"), CAP)
                    .unwrap()
            });
            let reserved = results
                .iter()
                .filter(|r| **r == UnpricedReservation::Reserved)
                .count();
            assert_eq!(reserved, 1, "round {round}: {results:?}");
            assert!(
                results.contains(&UnpricedReservation::CapReached {
                    unpriced_today: CAP
                }),
                "round {round}: {results:?}"
            );
            let db = Database::open(&path).unwrap();
            let entry = get_daily_spend(&db, &date).unwrap().unwrap();
            assert_eq!(entry.unpriced_dispatch_count, CAP, "round {round}");
            assert_eq!(entry.reserved_unpriced_dispatch_ids.len(), CAP as usize);
        }
    }

    #[test]
    fn a_reservation_at_the_cap_writes_nothing_and_a_known_one_is_never_refused() {
        let db = Database::open_in_memory().unwrap();
        accumulate_daily_spend(&db, "2026-09-03", 1.25).unwrap();
        assert_eq!(
            reserve_unpriced_dispatch_if_below(&db, "2026-09-03", "first", 1).unwrap(),
            UnpricedReservation::Reserved
        );
        let before = get_daily_spend(&db, "2026-09-03").unwrap();
        assert_eq!(
            reserve_unpriced_dispatch_if_below(&db, "2026-09-03", "second", 1).unwrap(),
            UnpricedReservation::CapReached { unpriced_today: 1 }
        );
        assert_eq!(get_daily_spend(&db, "2026-09-03").unwrap(), before);
        // Recovery re-reserving an invocation it already paid for is not a
        // new dispatch, so the cap never refuses it.
        assert_eq!(
            reserve_unpriced_dispatch_if_below(&db, "2026-09-03", "first", 1).unwrap(),
            UnpricedReservation::AlreadyReserved
        );
        assert_eq!(get_daily_spend(&db, "2026-09-03").unwrap(), before);
        assert!(reserve_unpriced_dispatch_if_below(&db, "bad", "id", 1).is_err());
        assert!(reserve_unpriced_dispatch_if_below(&db, "2026-09-03", " ", 1).is_err());
    }

    #[test]
    fn reservation_is_idempotent_and_preserves_known_dollars() {
        let db = Database::open_in_memory().unwrap();
        accumulate_daily_spend(&db, "2026-09-03", 1.25).unwrap();
        reserve_unpriced_dispatch(&db, "2026-09-03", "invocation-one").unwrap();
        let entry = reserve_unpriced_dispatch(&db, "2026-09-03", "invocation-one").unwrap();
        assert_eq!(entry.unpriced_dispatch_count, 1);
        assert_eq!(entry.reserved_unpriced_dispatch_ids, vec!["invocation-one"]);
        assert_eq!(entry.total_cost_usd, 1.25);
        assert_eq!(entry.dispatch_count, 1);
        assert!(reserve_unpriced_dispatch(&db, "bad", "id").is_err());
        assert!(reserve_unpriced_dispatch(&db, "2026-09-03", " ").is_err());
    }

    /// F95: the dollar ceiling is reserved in the transaction that checks
    /// it, so two dispatches that together pass the budget admit one.
    #[test]
    fn a_spend_reservation_past_the_budget_is_refused_and_settles_to_the_measured_cost() {
        let db = Database::open_in_memory().unwrap();
        let date = "2026-09-03";
        accumulate_daily_spend(&db, date, 22.0).unwrap();
        assert_eq!(
            reserve_spend_if_within(&db, date, 2.5, 25.0, "owner/repo#1", "r1").unwrap(),
            SpendReservation::Reserved
        );
        assert_eq!(
            reserve_spend_if_within(&db, date, 2.5, 25.0, "owner/repo#1", "r1").unwrap(),
            SpendReservation::BudgetReached { spent_usd: 24.5 }
        );
        let entry = settle_reserved_spend(&db, date, "r1", Some(1.0)).unwrap();
        assert!((entry.total_cost_usd - 23.0).abs() < 1e-9, "{entry:?}");
        assert_eq!(entry.dispatch_count, 2);
        reserve_spend_if_within(&db, date, 2.0, 25.0, "owner/repo#1", "r2").unwrap();
        let entry = settle_reserved_spend(&db, date, "r2", None).unwrap();
        assert!((entry.total_cost_usd - 23.0).abs() < 1e-9, "{entry:?}");
        assert_eq!(
            entry.dispatch_count, 2,
            "an unpriced dispatch is not counted here"
        );
    }

    /// A reservation whose controller died is released by issue, and
    /// its late settle still counts the measured cost exactly once.
    #[test]
    fn an_orphaned_spend_reservation_is_released_by_issue() {
        let db = Database::open_in_memory().unwrap();
        let date = "2026-09-03";
        assert_eq!(
            reserve_spend_if_within(&db, date, 2.5, 25.0, "owner/repo#1", "r1").unwrap(),
            SpendReservation::Reserved
        );
        assert_eq!(
            reserve_spend_if_within(&db, date, 2.5, 25.0, "owner/repo#2", "r2").unwrap(),
            SpendReservation::Reserved
        );
        let released = release_issue_spend_reservations(&db, date, "owner/repo#1").unwrap();
        assert!((released - 2.5).abs() < 1e-9, "{released}");
        let entry = get_daily_spend(&db, date).unwrap().unwrap();
        assert!((entry.total_cost_usd - 2.5).abs() < 1e-9, "{entry:?}");
        assert_eq!(entry.spend_reservations.len(), 1);
        // The dead controller's settle, arriving late, counts its cost
        // without subtracting the released ceiling a second time.
        let entry = settle_reserved_spend(&db, date, "r1", Some(1.0)).unwrap();
        assert!((entry.total_cost_usd - 3.5).abs() < 1e-9, "{entry:?}");
        assert_eq!(entry.dispatch_count, 1);
    }

    #[test]
    fn concurrent_spend_reservations_never_pass_the_budget() {
        let (_dir, path) = shared_db();
        accumulate_daily_spend(&Database::open(&path).unwrap(), "2026-09-03", 22.0).unwrap();
        let reserved = race(&path, 4, |_, db| {
            reserve_spend_if_within(db, "2026-09-03", 2.5, 25.0, "owner/repo#1", "race").unwrap()
                == SpendReservation::Reserved
        });
        assert_eq!(reserved.iter().filter(|r| **r).count(), 1, "{reserved:?}");
    }

    /// F5: one corrupt day's ledger does not hide a reservation on another.
    #[test]
    fn a_corrupt_ledger_does_not_fail_the_reservation_lookup() {
        let db = Database::open_in_memory().unwrap();
        write_current(&db, &budget_key("2026-09-03"), "{").unwrap();
        reserve_unpriced_dispatch(&db, "2026-09-04", "inv").unwrap();
        assert!(has_unpriced_dispatch_reservation(&db, "inv").unwrap());
        assert!(!has_unpriced_dispatch_reservation(&db, "other").unwrap());
    }

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
    fn an_unpriced_review_moves_the_review_counter_not_the_dispatch_counter() {
        let db = Database::open_in_memory().unwrap();
        record_unpriced_dispatch(&db, "2026-09-03").unwrap();
        let entry = record_unpriced_review(&db, "2026-09-03").unwrap();
        assert_eq!(entry.unpriced_review_count, 1);
        assert_eq!(entry.unpriced_dispatch_count, 1);
        assert_eq!(entry.total_cost_usd, 0.0);
        assert_eq!(entry.dispatch_count, 0);
        assert!(entry.has_unpriced_spend());
    }

    #[test]
    fn floor_predicate_fires_on_any_single_unpriced_counter() {
        // Mutation guard for `||`: each counter alone must mark the total
        // a floor. An `&&` (or a dropped advice arm) stays red here.
        let mut entry = BudgetLedgerEntry {
            spend_reservations: Vec::new(),
            reserved_unpriced_dispatch_ids: Vec::new(),
            date: "2026-09-03".to_string(),
            total_cost_usd: 0.0,
            dispatch_count: 0,
            unpriced_dispatch_count: 0,
            unpriced_review_count: 0,
            advice_call_count: 0,
            unpriced_advice_count: 0,
        };
        assert!(!entry.has_unpriced_spend());
        entry.unpriced_review_count = 1;
        assert!(entry.has_unpriced_spend(), "review alone is a floor");
        entry.unpriced_review_count = 0;
        entry.unpriced_dispatch_count = 1;
        assert!(entry.has_unpriced_spend(), "dispatch alone is a floor");
        entry.unpriced_dispatch_count = 0;
        entry.unpriced_advice_count = 1;
        assert!(entry.has_unpriced_spend(), "advice alone is a floor");
    }

    #[test]
    fn a_pre_split_ledger_drawer_deserializes_with_zero_review_count() {
        let db = Database::open_in_memory().unwrap();
        write_current(
            &db,
            &budget_key("2026-09-01"),
            r#"{"date":"2026-09-01","total_cost_usd":1.5,"dispatch_count":3,
                "unpriced_dispatch_count":2,"advice_call_count":1,"unpriced_advice_count":1}"#,
        )
        .unwrap();
        let entry = get_daily_spend(&db, "2026-09-01").unwrap().unwrap();
        assert_eq!(entry.unpriced_review_count, 0);
        assert!(entry.reserved_unpriced_dispatch_ids.is_empty());
        assert_eq!(entry.unpriced_dispatch_count, 2, "old fields preserved");
        assert_eq!(entry.dispatch_count, 3);
        assert!((entry.total_cost_usd - 1.5).abs() < 1e-9);
        let entry = record_unpriced_review(&db, "2026-09-01").unwrap();
        assert_eq!(entry.unpriced_review_count, 1);
        assert_eq!(entry.unpriced_dispatch_count, 2);
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
