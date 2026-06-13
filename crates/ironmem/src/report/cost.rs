//! METRICS_SPEC §7 cost table (revision 2026-06-11) + per-row cost derivation.
//!
//! `ironmem report` derives cost from this embedded table rather than summing
//! the stored `token_usage.cost_usd` column, because that column is populated
//! only by the Claude-CLI backend (sparse). See the dated §12 amendment in
//! `docs/METRICS_SPEC.md`. Token aggregations remain verbatim §10; the stored
//! provider cost is surfaced separately as `provider_reported_cost_usd`.

// Interim: the production caller (`super::cost::row_cost_usd`, invoked from
// `run_report`) lands in Task 3 of this plan. Until then these items are
// exercised only by the unit tests below. CI runs `clippy -- -D warnings`, so
// suppress dead-code here for this skeleton commit; removed once Task 3 wires
// the caller. Mirrors the existing `#[allow(dead_code)]` precedent in db/wal.rs.
#![allow(dead_code)]

/// METRICS_SPEC §7.1 pinned rates: `(model_id, input $/MTok, output $/MTok)`.
/// `claude-fable-5` is retained as frozen reference data for pricing historical
/// rows even though the model is no longer dispatched.
pub(crate) const MODEL_RATES: &[(&str, f64, f64)] = &[
    ("claude-fable-5", 10.00, 50.00),
    ("claude-opus-4-8", 5.00, 25.00),
    ("claude-opus-4-7", 5.00, 25.00),
    ("claude-sonnet-4-6", 3.00, 15.00),
    ("claude-haiku-4-5", 1.00, 5.00),
];

/// §7.1 cache multipliers, applied to the row model's input rate.
pub(crate) const CACHE_READ_MULT: f64 = 0.1;
pub(crate) const CACHE_CREATE_MULT: f64 = 1.25;

const PER_MTOK: f64 = 1_000_000.0;

fn rate_for(model: &str) -> Option<(f64, f64)> {
    MODEL_RATES
        .iter()
        .find(|(id, _, _)| *id == model)
        .map(|(_, i, o)| (*i, *o))
}

/// METRICS_SPEC §7 per-row cost in USD, or `None` when the row is not priceable:
/// - `harness == "codex"` → `None` (§7.2: Codex cost is outside this table),
///   regardless of the model string.
/// - unknown / unpinned model → `None` (§7.3: a new model is a dated table row).
///
/// Never returns `Some(0.0)` as a stand-in for "unknown"; callers flag `None`
/// rows in `unpriced_models` and still count their tokens.
pub(crate) fn row_cost_usd(
    harness: &str,
    model: Option<&str>,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
) -> Option<f64> {
    if harness == "codex" {
        return None;
    }
    let (r_in, r_out) = rate_for(model?)?;
    Some(
        input_tokens as f64 * r_in / PER_MTOK
            + output_tokens as f64 * r_out / PER_MTOK
            + cache_creation_input_tokens as f64 * (CACHE_CREATE_MULT * r_in) / PER_MTOK
            + cache_read_input_tokens as f64 * (CACHE_READ_MULT * r_in) / PER_MTOK,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // METRICS_SPEC §7.1 pinned rates — every row, exact.
    #[test]
    fn model_rates_match_spec_7_1() {
        let expect = [
            ("claude-fable-5", 10.00, 50.00),
            ("claude-opus-4-8", 5.00, 25.00),
            ("claude-opus-4-7", 5.00, 25.00),
            ("claude-sonnet-4-6", 3.00, 15.00),
            ("claude-haiku-4-5", 1.00, 5.00),
        ];
        assert_eq!(MODEL_RATES.len(), expect.len());
        for (id, i, o) in expect {
            let got = MODEL_RATES
                .iter()
                .find(|r| r.0 == id)
                .expect("model present");
            assert!((got.1 - i).abs() < 1e-9, "{id} input rate");
            assert!((got.2 - o).abs() < 1e-9, "{id} output rate");
        }
    }

    #[test]
    fn input_and_output_priced_per_mtok() {
        // 1M input @ opus-4-8 ($5/MTok) = $5.00; 1M output ($25/MTok) = $25.00.
        let c = row_cost_usd(
            "claude",
            Some("claude-opus-4-8"),
            1_000_000,
            1_000_000,
            0,
            0,
        )
        .unwrap();
        assert!((c - 30.00).abs() < 1e-9);
    }

    #[test]
    fn cache_multipliers_are_point1_and_1point25_of_input() {
        // opus-4-8 input $5/MTok. cache_read = 0.1x = $0.50/MTok; cache_create = 1.25x = $6.25/MTok.
        let read = row_cost_usd("claude", Some("claude-opus-4-8"), 0, 0, 0, 1_000_000).unwrap();
        assert!((read - 0.50).abs() < 1e-9);
        let create = row_cost_usd("claude", Some("claude-opus-4-8"), 0, 0, 1_000_000, 0).unwrap();
        assert!((create - 6.25).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_is_none_not_zero() {
        assert_eq!(
            row_cost_usd("claude", Some("claude-future-9"), 1_000_000, 0, 0, 0),
            None
        );
        assert_eq!(row_cost_usd("claude", None, 1_000_000, 0, 0, 0), None);
    }

    #[test]
    fn codex_harness_is_none_even_with_anthropic_model() {
        // §7.2: Codex cost is outside this table, regardless of the model string.
        assert_eq!(
            row_cost_usd("codex", Some("claude-opus-4-8"), 1_000_000, 0, 0, 0),
            None
        );
    }
}
