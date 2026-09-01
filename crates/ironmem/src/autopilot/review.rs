//! The Reviewer — build-ladder rung 5.
//!
//! Implements the spec's *Roles* entry: "a short-lived, fresh-context,
//! read-only agent the Lead dispatches once an IC's PR is open. It performs
//! **both** merge-time jobs: re-classify the diff's risk, and review the diff
//! for correctness/security, returning `PASS` or `NEEDS CHANGES`. Routed to
//! **Codex** ... giving cross-model adversarial review rather than same-model
//! self-agreement. Not a tier — it supervises nothing and holds no state."
//!
//! This module builds that invocation, runs it, parses its verdict, banks its
//! spend, records it to lineage, and computes the **merge decision**. It does
//! not *execute* a merge: `gh pr merge`, label flips, and the human
//! notification are rung 6's, and the spec makes merge authority the Lead's
//! alone. Rung 5 answers "may this merge?"; rung 6 acts on the answer.
//!
//! # Fail closed, in one function
//!
//! Four separate rows of the spec's error table all end at "no merge", and
//! [`decide_merge`] is the single place they are enforced:
//!
//! | Spec row | Enforced by |
//! |---|---|
//! | "Reviewer itself fails to run → treated as NOT reviewed. Infrastructure failure never becomes implicit approval." | [`HoldReason::ReviewerDidNotRun`] |
//! | "Reviewer uncertain, or returns NEEDS CHANGES" | [`HoldReason::NeedsChanges`] (the prompt routes uncertainty here — see [`super::review_prompt`]) |
//! | "Everything touching logic, protocol, security, or public API opens a PR and waits for a human regardless of reviewer verdict." | [`HoldReason::HighRiskClass`] |
//! | "Diff's risk class ≠ dispatch-time class → Fail closed. Never merge on the stale class." | [`HoldReason::ClassMismatch`] |
//!
//! [`MergeDecision::EligibleForMerge`] is reachable only by falling through
//! every one of them, so a new hazard is added by inserting a guard rather
//! than by remembering to check something at each call site.
//!
//! # Two asymmetries with the IC dispatch primitive
//!
//! Rung 2's [`super::dispatch`] and this module both spawn a headless agent
//! and parse a schema-forced verdict, but the harnesses differ in two ways
//! that are easy to get backwards:
//!
//! 1. **Claude wants the schema inline; Codex wants a file path.** Rung 0
//!    measured `claude --json-schema <path>` failing with "not valid JSON",
//!    which is why [`super::dispatch::IC_VERDICT_JSON_SCHEMA`] is passed as a
//!    literal. `codex exec --output-schema` documents the exact opposite:
//!    "Path to a JSON Schema file". [`run_review`] therefore writes
//!    [`REVIEW_VERDICT_JSON_SCHEMA`] to a scratch file. Same guarantee, two
//!    opposite spellings — see [`build_argv`].
//! 2. **Codex reports no dollar figure.** `claude --output-format json`
//!    returns `total_cost_usd`, which is what makes rung 4's budget
//!    *pre-authorization* exact. `codex exec --json` emits token counts and
//!    no price, and `codex exec` has no `--max-budget-usd` equivalent at all.
//!    See *Reviewer spend is unpriced* below.
//!
//! # Reviewer spend is unpriced, and that is recorded rather than hidden
//!
//! The spec's authoritative meter is "the sum of `total_cost_usd` across IC
//! **and Reviewer** invocations". For a Codex reviewer that sum is not
//! available, and the tempting move — bank `0.0` — would make the ledger
//! quietly wrong in the one direction that matters, exactly the failure the
//! spec's *Budget accounting* section rejects transcript ingestion for
//! ("best-effort by design ... silently under-counts").
//!
//! So: [`ReviewOutcome::total_cost_usd`] is an `Option`, `None` means
//! *unknown, not free*, and [`super::budget::record_unpriced_dispatch`]
//! increments a distinct `unpriced_dispatch_count` on the day's ledger. A
//! reader of that drawer sees a total that is explicitly a floor.
//!
//! Two consequences are accepted deliberately for rung 5, both worth
//! revisiting when rung 6 gives the Lead real merge authority:
//!
//! - **The reviewer is bounded by invocation count, not by dollars.** This
//!   is not a stylistic choice. A dollar ceiling is *inert* against an
//!   unpriced invocation: it never moves `total_cost_usd`, so a
//!   reviewer-only workload runs forever under a daily budget it can never
//!   reach. (Rung 5's end-to-end smoke test caught exactly this — six
//!   reviews ran, the ledger read `$0.00`, and a `--daily-budget-usd 0.01`
//!   refusal never fired.) So [`review_pr`] checks two ceilings: the day's
//!   priced spend, and [`ReviewRequest::max_unpriced_reviews_per_day`].
//!   Spend that cannot be measured in dollars has to be bounded in
//!   invocations, or it is not bounded at all.
//! - **Pre-authorization is still weaker than the IC's**, even with that
//!   bound. `codex exec` exposes no per-invocation ceiling, so an individual
//!   review's cost is unbounded — rung 4 could promise the day's total was
//!   bounded *by construction*, and this cannot. What it can promise is that
//!   the *number* of unbounded invocations is capped.
//! - **Token usage is captured, price is not.** [`ReviewTokenUsage`] is
//!   persisted with the review record so a later rung can price it
//!   retroactively without re-running anything — at which point the dollar
//!   ceiling starts working and the count becomes a backstop rather than the
//!   only bound.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::lineage::MAX_LINEAGE_FIELD_CHARS;
use super::scrub::scrub_and_bound;
use super::{validate_repo, zero_embedding, IssueRef, ADDED_BY, ROOM, WING};

/// The schema forced onto every Reviewer invocation via `--output-schema`.
///
/// Mirrors [`super::dispatch::IC_VERDICT_JSON_SCHEMA`]'s role — without it,
/// rung 0 measured that a result carries no verdict field the caller can
/// trust — but adds `risk_class`, because the spec gives the Reviewer two
/// jobs and a merge decision needs both answers. `additionalProperties:
/// false` and all three fields `required`, so a partial answer is a parse
/// failure (→ [`HoldReason::NoVerdict`]) rather than a half-trusted one.
///
/// **Passed to Codex as a file path, not inline** — the opposite of the IC
/// schema. See the module doc's *Two asymmetries*.
pub const REVIEW_VERDICT_JSON_SCHEMA: &str = r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["pass","needs_changes"]},"risk_class":{"type":"string","enum":["documentation","dependency_bump","mechanical_rename","test_only","logic","protocol","security","public_api"]},"reason":{"type":"string"}},"required":["verdict","risk_class","reason"],"additionalProperties":false}"#;

/// How many *unpriced* reviewer invocations may run in one day before
/// [`review_pr`] refuses.
///
/// **No spec basis** — documented here as an operator-tunable placeholder, in
/// the same spirit as rung 4's `DEFAULT_N_TURNS`/`DEFAULT_ATTEMPT_CAP`. It is
/// the *only* bound on reviewer spend that actually holds today (see
/// [`ReviewRefusal::UnpricedReviewCapReached`]), so it is set at roughly twice
/// the number of IC dispatches rung 4's own defaults allow in a day
/// (`daily_budget_usd / max_budget_usd` = 10): a reviewer runs about once per
/// PR, and a PR can be reviewed more than once when the first pass returns
/// NEEDS CHANGES.
pub const DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY: u32 = 20;

/// The eight risk classes the spec's *Who classifies, and when* section
/// names, in two groups.
///
/// The split is the spec's, verbatim: "Low-risk classes eligible for
/// auto-merge on green *and* reviewer PASS: documentation, dependency bumps,
/// mechanical renames, test-only changes. Everything touching logic,
/// protocol, security, or public API opens a PR and waits for a human
/// regardless of reviewer verdict."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Documentation,
    DependencyBump,
    MechanicalRename,
    TestOnly,
    Logic,
    Protocol,
    Security,
    PublicApi,
}

impl RiskClass {
    /// Whether this class is eligible for auto-merge at all (given a PASS and
    /// a matching dispatch-time class).
    ///
    /// Written as an exhaustive `match` with no wildcard arm on purpose: a
    /// ninth class added later must be classified deliberately at this line
    /// rather than defaulting into either group by omission. Defaulting the
    /// wrong way here ships unreviewed code.
    pub fn is_low_risk(self) -> bool {
        match self {
            RiskClass::Documentation
            | RiskClass::DependencyBump
            | RiskClass::MechanicalRename
            | RiskClass::TestOnly => true,
            RiskClass::Logic | RiskClass::Protocol | RiskClass::Security | RiskClass::PublicApi => {
                false
            }
        }
    }

    /// The schema's own lowercase spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RiskClass::Documentation => "documentation",
            RiskClass::DependencyBump => "dependency_bump",
            RiskClass::MechanicalRename => "mechanical_rename",
            RiskClass::TestOnly => "test_only",
            RiskClass::Logic => "logic",
            RiskClass::Protocol => "protocol",
            RiskClass::Security => "security",
            RiskClass::PublicApi => "public_api",
        }
    }

    /// Parse one of the schema's enum values.
    ///
    /// Deliberately exact-match and case-sensitive: this parses two very
    /// different things — the Reviewer's schema-constrained answer (which is
    /// already restricted to these spellings) and the Lead's free-form
    /// `--class` string (which is not). For the latter, an unrecognized
    /// value must land at [`HoldReason::ClassMismatch`], and normalizing
    /// near-misses would quietly turn a typo into an auto-merge.
    pub fn from_schema_str(s: &str) -> Option<Self> {
        match s {
            "documentation" => Some(RiskClass::Documentation),
            "dependency_bump" => Some(RiskClass::DependencyBump),
            "mechanical_rename" => Some(RiskClass::MechanicalRename),
            "test_only" => Some(RiskClass::TestOnly),
            "logic" => Some(RiskClass::Logic),
            "protocol" => Some(RiskClass::Protocol),
            "security" => Some(RiskClass::Security),
            "public_api" => Some(RiskClass::PublicApi),
            _ => None,
        }
    }
}

/// The Reviewer's review verdict — the spec's `PASS` / `NEEDS CHANGES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Pass,
    NeedsChanges,
}

impl ReviewVerdict {
    fn from_schema_str(s: &str) -> Option<Self> {
        match s {
            "pass" => Some(ReviewVerdict::Pass),
            "needs_changes" => Some(ReviewVerdict::NeedsChanges),
            _ => None,
        }
    }
}

/// Everything [`build_argv`] needs to construct one Reviewer invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewSpec {
    /// `-m`. `None` leaves Codex on its configured default rather than
    /// pinning a model this repo has no measurement for — the spec's *Model
    /// routing* table says only "Codex", naming no specific model.
    pub model: Option<String>,
    /// The rendered prompt from [`super::review_prompt::render`].
    pub prompt: String,
    /// Where [`run_review`] wrote [`REVIEW_VERDICT_JSON_SCHEMA`], for
    /// `--output-schema`.
    pub schema_path: PathBuf,
    /// Where Codex should write the agent's final message, for `-o`. Read
    /// back by [`parse_review_message`].
    pub last_message_path: PathBuf,
}

/// Build the argv passed to the `codex` binary for one review. Pure and
/// unit-testable without spawning a process, mirroring
/// [`super::dispatch::build_argv`].
///
/// Every flag here is load-bearing:
///
/// - `exec` — the non-interactive form. The bare `codex` entry point forwards
///   to the interactive CLI, which would hang headless.
/// - `-s read-only` — the spec's "read-only" is a sandbox property here, not
///   just a prompt instruction. This is the flag that makes it true.
/// - `--ephemeral` — "holds no state", enforced: no session file is
///   persisted for a role that has nothing to resume.
/// - `--json` — JSONL events on stdout, which is where the token counts come
///   from ([`parse_codex_token_usage`]).
/// - `--output-schema` / `-o` — the verdict guard: a schema-constrained final
///   message, written where it can be read back deterministically instead of
///   scraped out of the event stream.
/// - `-C` — the working root. The reviewer reads a checkout it does not own.
///
/// There is deliberately no `--name`: `codex exec` has no such flag, and
/// unlike an IC (which rung 2 gave a deterministic address for `ListAgents`
/// and the abort path) a reviewer is short-lived, holds no state, and is
/// never messaged, so there is nothing to address.
pub fn build_argv(spec: &ReviewSpec, repo_dir: &Path) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "-s".to_string(),
        "read-only".to_string(),
        "--ephemeral".to_string(),
        "--json".to_string(),
        "--output-schema".to_string(),
        spec.schema_path.to_string_lossy().to_string(),
        "-o".to_string(),
        spec.last_message_path.to_string_lossy().to_string(),
        "-C".to_string(),
        repo_dir.to_string_lossy().to_string(),
    ];
    if let Some(model) = &spec.model {
        args.push("-m".to_string());
        args.push(model.clone());
    }
    // The prompt is `codex exec`'s single trailing positional, so it is
    // pushed after every flag and its value.
    //
    // Note what this does *not* buy: being last does not protect a prompt
    // that begins with `-`, because clap dispatches on the leading dash, not
    // on position — such a prompt would be rejected as an unknown flag, and a
    // prompt of exactly `-` means "read the instructions from stdin". Neither
    // is reachable today ([`super::review_prompt::render`] always opens with
    // "You are a fresh-context..."), and both fail closed if it ever changes,
    // so no `--` escape is spelled here; the guarantee just isn't the one the
    // ordering suggests.
    args.push(spec.prompt.clone());
    args
}

/// Cumulative token usage for one review, as reported by Codex.
///
/// Not a price. See the module doc's *Reviewer spend is unpriced*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

/// One review's outcome: the verdict, the class, and whatever accounting the
/// harness was able to give.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewOutcome {
    /// `None` when the final message was absent, unparseable, or carried a
    /// value outside the schema's enum — never silently a pass.
    pub verdict: Option<ReviewVerdict>,
    /// `None` on the same conditions as `verdict`.
    pub risk_class: Option<RiskClass>,
    pub reason: Option<String>,
    /// `None` for a Codex reviewer, which reports no dollar figure. **Not
    /// zero** — see the module doc.
    pub total_cost_usd: Option<f64>,
    /// `None` when the event stream carried no token accounting at all.
    pub token_usage: Option<ReviewTokenUsage>,
    /// Whether the reviewer process itself exited zero.
    pub process_success: bool,
}

impl ReviewOutcome {
    /// The outcome of a reviewer that never produced anything usable —
    /// the spec's "Reviewer itself fails to run ... treated as NOT
    /// reviewed".
    ///
    /// Exists as a named constructor so a caller handling a spawn error has
    /// an obvious, correct value to record and route through
    /// [`decide_merge`], rather than assembling one field-by-field and
    /// getting `process_success` backwards.
    pub fn did_not_run() -> Self {
        Self {
            verdict: None,
            risk_class: None,
            reason: None,
            total_cost_usd: None,
            token_usage: None,
            process_success: false,
        }
    }
}

/// The verdict object the schema forces, as parsed from the final message.
#[derive(Debug, Clone, Deserialize)]
struct RawReviewMessage {
    #[serde(default)]
    verdict: Option<String>,
    #[serde(default)]
    risk_class: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Parse the Reviewer's final message into a verdict, class, and reason.
///
/// Tolerates surrounding whitespace and a markdown code fence, because those
/// are formatting noise around an otherwise-conforming answer. It tolerates
/// **nothing else**: a message that is prose, truncated, or carries a value
/// outside the schema's enums yields `(None, None, ...)`, which
/// [`decide_merge`] turns into [`HoldReason::NoVerdict`]. There is
/// deliberately no path that infers a verdict from prose — that would
/// reintroduce exactly the guess the `--output-schema` flag exists to
/// eliminate.
pub fn parse_review_message(
    message: &str,
) -> (Option<ReviewVerdict>, Option<RiskClass>, Option<String>) {
    let trimmed = strip_code_fence(message.trim());
    let Ok(raw) = serde_json::from_str::<RawReviewMessage>(trimmed) else {
        return (None, None, None);
    };
    let verdict = raw
        .verdict
        .as_deref()
        .and_then(ReviewVerdict::from_schema_str);
    let risk_class = raw
        .risk_class
        .as_deref()
        .and_then(RiskClass::from_schema_str);
    (verdict, risk_class, raw.reason)
}

/// Strip a single ```/```json fence wrapping the whole message, if present.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // Drop the optional language tag on the opening fence line.
    let after_tag = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => return s,
    };
    match after_tag.rfind("```") {
        Some(idx) => after_tag[..idx].trim(),
        None => s,
    }
}

/// Extract cumulative token usage from a `codex exec --json` event stream.
///
/// Returns the **last** usage object seen, which is the cumulative total for
/// the session (matching `metrics::transcript`'s Codex rollout parser).
///
/// # Why this searches by shape rather than by a fixed path
///
/// The exact event envelope `codex exec --json` emits was **not verified
/// against a live run** in rung 5 — running Codex costs money, and rung 4's
/// lesson on that is explicit. What is known from this repo's own Codex
/// rollout parser is the inner shape: an object carrying
/// `total_token_usage: {input_tokens, cached_input_tokens, output_tokens}`.
/// So this walks each JSON line looking for that object anywhere in the tree
/// rather than asserting a path (`event_msg.payload.info....`) that a version
/// bump could move.
///
/// Absence yields `None` — *unknown*, never zero. A caller must not read a
/// missing count as a free review; [`review_pr`] records it as unpriced
/// either way.
pub fn parse_codex_token_usage(stdout: &str) -> Option<ReviewTokenUsage> {
    let mut last = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Non-JSON lines are tolerated individually: a Codex stream can carry
        // plain diagnostics alongside its events, and one of them must not
        // discard the accounting from the rest.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(usage) = find_total_token_usage(&value) {
            last = Some(usage);
        }
    }
    last
}

fn find_total_token_usage(value: &serde_json::Value) -> Option<ReviewTokenUsage> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(usage) = map.get("total_token_usage") {
                let g = |k: &str| usage.get(k).and_then(|v| v.as_i64());
                // Require the field to actually be present and numeric.
                // Defaulting a missing component to 0 would silently report a
                // partial count as a complete one.
                if let (Some(input), Some(cached), Some(output)) = (
                    g("input_tokens"),
                    g("cached_input_tokens"),
                    g("output_tokens"),
                ) {
                    return Some(ReviewTokenUsage {
                        input_tokens: input,
                        cached_input_tokens: cached,
                        output_tokens: output,
                    });
                }
            }
            map.values().find_map(find_total_token_usage)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_total_token_usage),
        _ => None,
    }
}

/// Locate the `codex` binary on `PATH`, reusing `launcher`'s own binary
/// validation — the spec's *Reuse* section: "this is what makes a Codex
/// reviewer nearly free".
pub fn resolve_codex_binary() -> Result<PathBuf, MemoryError> {
    crate::launcher::find_on_path(crate::launcher::Harness::Codex.binary())
}

/// A scratch directory holding the schema and last-message files for one
/// review, removed on drop.
///
/// Hand-rolled rather than `tempfile::TempDir` because `tempfile` is a
/// dev-dependency here (see `Cargo.toml`'s note on keeping it out of release
/// builds), and pulling it into `[dependencies]` for two files would change
/// what every release build compiles.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn create() -> Result<Self, MemoryError> {
        let path = std::env::temp_dir().join(format!("ironmem-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).map_err(|e| {
            MemoryError::Config(format!(
                "cannot create reviewer scratch dir {}: {e}",
                path.display()
            ))
        })?;
        Ok(Self { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best-effort: a leftover scratch dir in the system temp dir is
        // harmless, and failing a completed review over cleanup would be
        // worse than leaking it.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Run one review: write the schema, spawn `bin` with [`build_argv`]'s argv
/// in `repo_dir`, capture stdout, and read the verdict back out of the
/// last-message file.
///
/// # Error contract
///
/// Mirrors [`super::run::Dispatcher`]'s: a failure to **start** the process is
/// [`MemoryError::NotFound`]; everything else is a different variant. A
/// process that ran but produced no usable verdict is **not** an error — it
/// returns an outcome with `verdict: None`, which [`decide_merge`] holds on.
/// That distinction matters because a spawn failure and an unparseable
/// verdict warrant different operator responses, and collapsing both into
/// `Err` would lose the accounting (token usage) from the second.
pub fn run_review(
    bin: &Path,
    repo_dir: &Path,
    model: Option<String>,
    prompt: String,
) -> Result<ReviewOutcome, MemoryError> {
    let scratch = ScratchDir::create()?;
    let schema_path = scratch.path.join("verdict-schema.json");
    let last_message_path = scratch.path.join("last-message.txt");
    std::fs::write(&schema_path, REVIEW_VERDICT_JSON_SCHEMA).map_err(|e| {
        MemoryError::Config(format!(
            "cannot write reviewer schema to {}: {e}",
            schema_path.display()
        ))
    })?;

    let spec = ReviewSpec {
        model,
        prompt,
        schema_path,
        last_message_path: last_message_path.clone(),
    };
    let args = build_argv(&spec, repo_dir);

    let output = std::process::Command::new(bin)
        .args(&args)
        .current_dir(repo_dir)
        .output()
        .map_err(|e| MemoryError::NotFound(format!("failed to launch reviewer: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let token_usage = parse_codex_token_usage(&stdout);

    // A missing last-message file means the reviewer produced no final
    // message — same fail-closed treatment as an unparseable one.
    let message = std::fs::read_to_string(&last_message_path).unwrap_or_default();
    let (verdict, risk_class, reason) = parse_review_message(&message);

    Ok(ReviewOutcome {
        verdict,
        risk_class,
        reason,
        // Codex reports no price. See the module doc.
        total_cost_usd: None,
        token_usage,
        process_success: output.status.success(),
    })
}

/// Why a PR is being held for a human instead of auto-merged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum HoldReason {
    /// The gate was not green. A reviewer PASS never substitutes for it: the
    /// spec's low-risk auto-merge is "on green **and** reviewer PASS".
    GateNotGreen,
    /// The reviewer process failed to run, or ran and exited non-zero.
    /// "Infrastructure failure never becomes implicit approval."
    ReviewerDidNotRun,
    /// The reviewer ran but produced no schema-valid verdict and class.
    NoVerdict,
    /// The reviewer found something, or was uncertain.
    NeedsChanges,
    /// The diff's own class is one that always waits for a human.
    HighRiskClass { class: RiskClass },
    /// The class derived from the diff disagrees with the Lead's
    /// dispatch-time class — including the case where the dispatch-time
    /// class was never a recognized class at all.
    ClassMismatch {
        dispatch_class: String,
        diff_class: RiskClass,
    },
}

/// Whether this PR may be auto-merged, and if not, why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum MergeDecision {
    /// Every guard passed. Rung 6 executes the merge; rung 5 only says it is
    /// permitted.
    EligibleForMerge {
        class: RiskClass,
    },
    HoldForHuman(HoldReason),
}

impl MergeDecision {
    pub fn is_eligible(&self) -> bool {
        matches!(self, MergeDecision::EligibleForMerge { .. })
    }
}

/// Decide whether a reviewed PR may be auto-merged.
///
/// `gate_green` is an explicit parameter rather than something inferred here
/// because this module never runs a gate; making the caller state it keeps
/// the function total and makes "we merged without checking" impossible to
/// express.
///
/// # Guard order
///
/// The guards are ordered by how much a human reading the hold reason
/// learns from it, not by severity — every one of them is equally
/// disqualifying, and a PR can trip several at once:
///
/// 1. `gate_green` — if the gate is red, the review is moot.
/// 2. The reviewer did not run, or did not answer. These say nothing about
///    the diff at all.
/// 3. `needs_changes` — a concrete finding, the most actionable thing a
///    human can be handed.
/// 4. High-risk class, then class mismatch — properties of the change
///    itself, reported only once the review is known to be clean.
pub fn decide_merge(
    gate_green: bool,
    dispatch_class: &str,
    outcome: &ReviewOutcome,
) -> MergeDecision {
    if !gate_green {
        return MergeDecision::HoldForHuman(HoldReason::GateNotGreen);
    }
    if !outcome.process_success {
        return MergeDecision::HoldForHuman(HoldReason::ReviewerDidNotRun);
    }
    let (Some(verdict), Some(diff_class)) = (outcome.verdict, outcome.risk_class) else {
        return MergeDecision::HoldForHuman(HoldReason::NoVerdict);
    };
    if verdict == ReviewVerdict::NeedsChanges {
        return MergeDecision::HoldForHuman(HoldReason::NeedsChanges);
    }
    if !diff_class.is_low_risk() {
        return MergeDecision::HoldForHuman(HoldReason::HighRiskClass { class: diff_class });
    }
    if RiskClass::from_schema_str(dispatch_class.trim()) != Some(diff_class) {
        return MergeDecision::HoldForHuman(HoldReason::ClassMismatch {
            dispatch_class: dispatch_class.to_string(),
            diff_class,
        });
    }
    MergeDecision::EligibleForMerge { class: diff_class }
}

const REVIEW_ENTITY_TYPE: &str = "review";
const HAS_REVIEW_PREDICATE: &str = "has_review";
const ISSUE_ENTITY_TYPE: &str = "issue";

/// One review, as the caller supplies it. `reason` is scrubbed and
/// length-bounded by [`record_review`] before it is persisted — a review
/// reason quotes the diff and can carry anything the diff carried.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRecord {
    pub issue: IssueRef,
    pub pr_number: u64,
    pub dispatch_class: String,
    /// The commit the reviewer actually read, if the caller could resolve
    /// it. Rung 6 refuses to merge a PR whose head has moved since this
    /// SHA, so a record without one cannot authorize a merge at all — see
    /// [`RecordedReviewSummary::head_sha`].
    pub head_sha: Option<String>,
    /// The base branch the review was taken against, if the caller knows it.
    /// Rung 6 refuses to merge a PR that has since been retargeted, because
    /// the reviewed diff is not then the diff that would land — see
    /// [`RecordedReviewSummary::base_branch`].
    pub base_branch: Option<String>,
    pub outcome: ReviewOutcome,
    pub decision: MergeDecision,
}

/// The JSON actually written to the drawer.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewBody {
    issue: String,
    repo: String,
    issue_number: u64,
    pr_number: u64,
    dispatch_class: String,
    /// `#[serde(default)]` so every review recorded before rung 6 existed
    /// reads back as `None` — which rung 6 treats as "the reviewed commit is
    /// unknown" and holds on, rather than as "the head has not moved".
    #[serde(default)]
    head_sha: Option<String>,
    /// `#[serde(default)]` for the same reason `head_sha` is: a review
    /// recorded before the field existed reads back as `None`, which rung 6
    /// treats as "the reviewed base is unknown" and skips the comparison on,
    /// rather than as a mismatch.
    #[serde(default)]
    base_branch: Option<String>,
    verdict: Option<ReviewVerdict>,
    risk_class: Option<RiskClass>,
    reason: Option<String>,
    total_cost_usd: Option<f64>,
    token_usage: Option<ReviewTokenUsage>,
    process_success: bool,
    decision: serde_json::Value,
    /// Same role as [`super::lineage::AttemptRecord`]'s: guarantees this
    /// record's content — and therefore its content-derived drawer id — is
    /// unique even when two reviews of the same PR agree in every field, so
    /// the second cannot silently overwrite the first via
    /// `insert_drawer`'s `ON CONFLICT(id) DO UPDATE`. Reviews repeat: the
    /// spec re-dispatches the IC on NEEDS CHANGES and reviews again.
    record_id: String,
    recorded_at: String,
    reason_redacted: bool,
    reason_truncated: bool,
}

/// What [`record_review`] persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedReview {
    pub drawer_id: String,
    pub redacted: bool,
    pub truncated: bool,
}

/// Append a review record to the issue's lineage.
///
/// **Append-only, no `logical_key`** — the same hazard the module doc's
/// *`logical_key` hazard* section describes for attempts applies here for the
/// same reason: a NEEDS CHANGES review followed by a PASS review of the same
/// PR are two facts, and keying them would destroy the first.
pub fn record_review(db: &Database, record: &ReviewRecord) -> Result<RecordedReview, MemoryError> {
    validate_repo(&record.issue.repo)?;

    let reason_scrub = record
        .outcome
        .reason
        .as_deref()
        .map(|text| scrub_and_bound(text, MAX_LINEAGE_FIELD_CHARS));

    let body = ReviewBody {
        issue: record.issue.canonical(),
        repo: record.issue.repo.clone(),
        issue_number: record.issue.number,
        pr_number: record.pr_number,
        dispatch_class: record.dispatch_class.clone(),
        head_sha: record.head_sha.clone(),
        base_branch: record.base_branch.clone(),
        verdict: record.outcome.verdict,
        risk_class: record.outcome.risk_class,
        reason: reason_scrub.as_ref().map(|o| o.text.clone()),
        total_cost_usd: record.outcome.total_cost_usd,
        token_usage: record.outcome.token_usage,
        process_success: record.outcome.process_success,
        decision: serde_json::to_value(&record.decision)?,
        record_id: uuid::Uuid::new_v4().to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
        reason_redacted: reason_scrub.as_ref().is_some_and(|o| o.redacted),
        reason_truncated: reason_scrub.as_ref().is_some_and(|o| o.truncated),
    };

    let content = serde_json::to_string(&body)?;
    let drawer_id = crate::db::drawers::generate_id(&content, WING, ROOM);
    let issue_entity = record.issue.entity_name();
    let embedding = zero_embedding();

    // Drawer and edge in one transaction, matching
    // `lineage::record_attempt`: a crash between the two would leave a
    // review drawer no traversal can reach.
    db.with_transaction(|tx| {
        Database::insert_drawer_tx(
            tx, &drawer_id, &content, &embedding, WING, ROOM, "", ADDED_BY,
        )?;
        KnowledgeGraph::add_triple_tx(
            tx,
            &issue_entity,
            ISSUE_ENTITY_TYPE,
            HAS_REVIEW_PREDICATE,
            &drawer_id,
            REVIEW_ENTITY_TYPE,
            None,
            1.0,
            None,
        )?;
        Database::wal_log_tx(
            tx,
            "autopilot_record_review",
            &json!({
                "drawer_id": &drawer_id,
                "issue": &body.issue,
                "pr_number": body.pr_number,
            }),
            None,
        )?;
        Ok(())
    })?;

    Ok(RecordedReview {
        drawer_id,
        redacted: body.reason_redacted,
        truncated: body.reason_truncated,
    })
}

/// The most triples [`reviews_for_issue`] will walk for one issue. Matches
/// `lineage::MAX_ATTEMPTS_PER_ISSUE`: a bound on a traversal that is
/// otherwise unbounded by anything but retention.
///
/// It has to be generous, because it is a `LIMIT` on **every** current edge
/// hanging off the issue entity — `has_attempt` and `has_review` alike, plus
/// whatever a later rung adds — not on reviews alone, and
/// `query_entity_current` orders newest-first. A tight cap would therefore
/// let a long-running issue's attempt edges crowd its reviews out of the
/// result set entirely, and rung 6 asking "has this PR already been
/// reviewed, and what did it say?" would read an empty history and
/// re-review — or lose a `needs_changes`.
const MAX_REVIEWS_PER_ISSUE: usize = 10_000;

/// One recorded review, as read back from storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedReviewSummary {
    pub pr_number: u64,
    pub dispatch_class: String,
    /// The commit this review read. `None` for a review recorded before the
    /// field existed, and for one whose caller could not resolve the head
    /// branch. Both are "unknown", and rung 6's
    /// [`super::merge::execute_merge`] holds on unknown: a PASS that cannot
    /// be tied to a specific commit is not evidence about the commit being
    /// merged.
    #[serde(default)]
    pub head_sha: Option<String>,
    /// The base branch this review was taken against. `None` for a review
    /// recorded before the field existed. Rung 6's
    /// [`super::merge::execute_merge`] holds when it disagrees with the PR's
    /// current base: a retargeted PR was reviewed against a diff that no
    /// longer exists.
    #[serde(default)]
    pub base_branch: Option<String>,
    pub verdict: Option<ReviewVerdict>,
    pub risk_class: Option<RiskClass>,
    pub reason: Option<String>,
    pub process_success: bool,
    pub recorded_at: String,
}

/// Every recorded review for an issue, oldest first.
///
/// This is the review half of the spec's "exact issue→attempt traversal":
/// semantic `search` cannot reliably enumerate an issue's records, so
/// [`record_review`] writes a `has_review` edge and this walks it. Without
/// it a review would be persisted and then effectively unfindable — the
/// exact problem the knowledge-graph edges exist to prevent, and the reason
/// rung 6 can ask "has this PR already been reviewed, and what did it say?"
/// rather than re-reviewing on every poll.
///
/// Returns an empty vec (not an error) for an issue with no reviews yet.
pub fn reviews_for_issue(
    db: &Database,
    issue: &IssueRef,
) -> Result<Vec<RecordedReviewSummary>, MemoryError> {
    let kg = KnowledgeGraph::new(db);
    let entity = match kg.resolve_entity(&issue.entity_name(), Some(ISSUE_ENTITY_TYPE)) {
        Ok(entity) => entity,
        Err(MemoryError::NotFound(_)) => return Ok(Vec::new()),
        Err(other) => return Err(other),
    };

    let triples = kg.query_entity_current(&entity.id, MAX_REVIEWS_PER_ISSUE)?;
    let mut records = Vec::new();
    for triple in triples {
        if triple.predicate != HAS_REVIEW_PREDICATE {
            continue;
        }
        // `triple.object` is the *entity* id, not the drawer id stored as
        // that entity's `name` — the same indirection
        // `lineage::attempts_for_issue` documents.
        let Some(object_entity) = kg.get_entity(&triple.object)? else {
            continue;
        };
        let Some(drawer) = db.get_drawer(&object_entity.name)? else {
            continue;
        };
        let body: ReviewBody = serde_json::from_str(&drawer.content)?;
        records.push(RecordedReviewSummary {
            pr_number: body.pr_number,
            dispatch_class: body.dispatch_class,
            head_sha: body.head_sha,
            base_branch: body.base_branch,
            verdict: body.verdict,
            risk_class: body.risk_class,
            reason: body.reason,
            process_success: body.process_success,
            recorded_at: body.recorded_at,
        });
    }
    records.sort_by(|a, b| a.recorded_at.cmp(&b.recorded_at));
    Ok(records)
}

/// How [`review_pr`] runs the reviewer.
///
/// A trait for the same reason [`super::run::Dispatcher`] is one: it makes
/// the whole policy layer — budget pre-authorization, ledger accounting,
/// lineage writes, the merge decision — testable against a real database
/// without spawning `codex` and spending real money.
///
/// # Error contract
///
/// A failure to **start** the process must be [`MemoryError::NotFound`];
/// anything else must be a different variant. Both are routed to
/// [`HoldReason::ReviewerDidNotRun`] by [`review_pr`], so the distinction is
/// for the operator reading the error, not for control flow.
pub trait ReviewRunner {
    fn review(&mut self, repo_dir: &Path, prompt: &str) -> Result<ReviewOutcome, MemoryError>;
}

/// The production [`ReviewRunner`]: a real `codex exec` invocation.
pub struct CodexReviewer {
    bin: PathBuf,
    model: Option<String>,
}

impl CodexReviewer {
    /// Resolve `codex` on `PATH` now, so a missing binary is reported before
    /// any state is written rather than at the moment of dispatch.
    pub fn resolve(model: Option<String>) -> Result<Self, MemoryError> {
        Ok(Self {
            bin: resolve_codex_binary()?,
            model,
        })
    }
}

impl ReviewRunner for CodexReviewer {
    fn review(&mut self, repo_dir: &Path, prompt: &str) -> Result<ReviewOutcome, MemoryError> {
        run_review(&self.bin, repo_dir, self.model.clone(), prompt.to_string())
    }
}

/// Inputs to [`review_pr`].
pub struct ReviewRequest<'a> {
    pub issue: &'a IssueRef,
    pub pr_number: u64,
    pub base_branch: &'a str,
    pub head_branch: &'a str,
    /// The commit `head_branch` resolves to in `repo_dir` right now, if the
    /// caller could resolve it. Recorded verbatim so rung 6 can tell whether
    /// the PR it is about to merge is still the change this review read.
    pub head_sha: Option<String>,
    /// The Lead's dispatch-time class, compared against the reviewer's.
    pub dispatch_class: &'a str,
    /// The repo's approved gate commands.
    pub gate_commands: &'a [String],
    /// Whether the gate was green when the PR was opened. The Lead knows
    /// this; this module never runs a gate.
    pub gate_green: bool,
    /// Checkout the reviewer reads. Read-only to it.
    pub repo_dir: &'a Path,
    /// The day's ledger ceiling, in the same units as
    /// [`super::run::RunConfig::daily_budget_usd`].
    ///
    /// **Inert for a Codex reviewer on its own.** A reviewer that reports no
    /// price never moves the ledger's total, so this ceiling can only ever be
    /// reached by the IC dispatches sharing the day with it —
    /// `max_unpriced_reviews_per_day` is what bounds the reviewer itself.
    pub daily_budget_usd: f64,
    /// How many unpriced reviewer invocations may run today. See
    /// [`DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY`].
    pub max_unpriced_reviews_per_day: u32,
}

/// The result of reviewing one PR.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrReview {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub pr_number: u64,
    pub outcome: ReviewOutcome,
    pub decision: MergeDecision,
    /// The drawer id of the appended review record, or `None` when the
    /// review was refused before it ran (see [`PrReview::dispatched`]).
    pub record_drawer_id: Option<String>,
    /// Why no reviewer was launched, or `None` when one was.
    ///
    /// A *launch failure* is not a refusal: it produced a recorded, held
    /// review, so it reports `None` here and `ReviewerDidNotRun` as its
    /// decision. This field exists so a Lead can tell "retry when the day
    /// rolls over" from "something is broken".
    pub refusal: Option<ReviewRefusal>,
}

/// Why [`review_pr`] declined to launch a reviewer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRefusal {
    /// The day's priced spend has already reached the ceiling.
    DailyBudgetExhausted,
    /// The day's *unpriced* invocation count has reached its ceiling.
    ///
    /// This is the bound that actually holds for a Codex reviewer, and it
    /// exists because the dollar one does not: an unpriced invocation never
    /// moves `total_cost_usd`, so a reviewer-only workload could run
    /// forever under a dollar cap that can never be reached. A spend that
    /// cannot be measured in dollars has to be bounded in invocations or it
    /// is not bounded at all. See
    /// [`ReviewRequest::max_unpriced_reviews_per_day`].
    UnpricedReviewCapReached,
}

impl PrReview {
    /// Whether a reviewer process was actually launched.
    pub fn dispatched(&self) -> bool {
        self.refusal.is_none()
    }
}

fn serialize_issue<S>(issue: &IssueRef, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&issue.canonical())
}

/// Review one PR end to end: pre-authorize against the daily ledger, run the
/// reviewer, bank its (unpriced) spend, append a lineage record, and return
/// the merge decision.
///
/// # Budget refusal is not a hold reason
///
/// When either ceiling is reached, no reviewer runs and the returned
/// decision is [`HoldReason::ReviewerDidNotRun`] — correctly, since nothing
/// reviewed the diff — with [`PrReview::refusal`] distinguishing it from a
/// reviewer that launched and failed. The spec's "reviewer fails to run ⇒
/// not reviewed ⇒ no auto-merge" holds either way; the field exists so a
/// Lead can tell "retry when the day rolls over" from "something is broken".
///
/// Unlike an IC dispatch, this cannot *pre*-authorize the invocation's own
/// ceiling, because `codex exec` has none to set. See the module doc's
/// *Reviewer spend is unpriced*.
pub fn review_pr(
    db: &Database,
    runner: &mut dyn ReviewRunner,
    request: &ReviewRequest,
) -> Result<PrReview, MemoryError> {
    validate_repo(&request.issue.repo)?;
    // Same check `run::RunConfig::validate` applies, for the same reason: a
    // NaN ceiling makes every `spent_today >= daily_budget_usd` comparison
    // false, so the dollar bound silently disappears instead of failing
    // closed. A caller must not be able to spell "no ceiling" by accident.
    if !request.daily_budget_usd.is_finite() || request.daily_budget_usd <= 0.0 {
        return Err(MemoryError::Config(
            "daily_budget_usd must be a finite, positive number".into(),
        ));
    }

    let today = super::today_utc();
    let ledger = super::budget::get_daily_spend(db, &today)?;
    let spent_today = ledger.as_ref().map(|e| e.total_cost_usd).unwrap_or(0.0);
    let unpriced_today = ledger
        .as_ref()
        .map(|e| e.unpriced_dispatch_count)
        .unwrap_or(0);

    // Two ceilings, because one of them cannot see the reviewer at all: an
    // unpriced invocation never moves `total_cost_usd`, so a dollar cap alone
    // would let a reviewer-only workload run unbounded forever under a budget
    // it can never reach.
    let refusal = if spent_today >= request.daily_budget_usd {
        Some(ReviewRefusal::DailyBudgetExhausted)
    } else if unpriced_today >= request.max_unpriced_reviews_per_day {
        Some(ReviewRefusal::UnpricedReviewCapReached)
    } else {
        None
    };
    if let Some(refusal) = refusal {
        let outcome = ReviewOutcome::did_not_run();
        let decision = decide_merge(request.gate_green, request.dispatch_class, &outcome);
        return Ok(PrReview {
            issue: request.issue.clone(),
            pr_number: request.pr_number,
            outcome,
            decision,
            record_drawer_id: None,
            refusal: Some(refusal),
        });
    }

    let prompt = super::review_prompt::render(&super::review_prompt::ReviewPromptInputs {
        issue: request.issue,
        pr_number: request.pr_number,
        base_branch: request.base_branch,
        head_branch: request.head_branch,
        dispatch_class: request.dispatch_class,
        gate_commands: request.gate_commands,
    });

    // A reviewer that fails to launch is not an error for this function: it
    // is the spec's "treated as NOT reviewed", which must still be recorded
    // and held on rather than propagated as an `Err` a caller might retry
    // into an unbounded loop.
    let (outcome, launched) = match runner.review(request.repo_dir, &prompt) {
        Ok(outcome) => (outcome, true),
        Err(e) => {
            tracing::warn!(error = %e, "autopilot reviewer failed to run; treating as NOT reviewed");
            (ReviewOutcome::did_not_run(), false)
        }
    };

    // Bank the invocation before deciding anything with it: a review that
    // ran cost money whatever its verdict, and rung 4 banks failed dispatches
    // for the same reason.
    //
    // An `Err` from the runner is the one case that banks *nothing*. Per the
    // [`ReviewRunner`] error contract every such failure happens before the
    // process is spawned, so no tokens were spent — and the unpriced counter
    // is the only bound on reviewer spend that actually holds, so charging a
    // never-launched reviewer against it would let a broken `codex` exhaust
    // the day's reviews for free. Rung 4 draws the same line for the same
    // reason (`run::run_issue`'s dispatch-error arm banks no cost), and the
    // `refusal` field's "retry tomorrow vs. something is broken" distinction
    // only survives if a broken reviewer never turns into a budget refusal.
    if launched {
        match outcome.total_cost_usd {
            // A price only counts as a price if it is one. A harness that
            // reports a negative, NaN, or infinite figure has told us nothing
            // usable, and `accumulate_daily_spend` rejects all three — which
            // would turn a review that actually ran into an `Err` that
            // discards its record *and* its merge decision. Routing it to the
            // unpriced counter instead keeps this module's whole thesis
            // intact: unknown is recorded as unknown, never as free.
            Some(cost) if cost.is_finite() && cost >= 0.0 => {
                super::budget::accumulate_daily_spend(db, &today, cost)?;
            }
            Some(cost) => {
                tracing::warn!(
                    cost,
                    "autopilot reviewer reported an unusable price; banking it as unpriced"
                );
                super::budget::record_unpriced_dispatch(db, &today)?;
            }
            None => {
                super::budget::record_unpriced_dispatch(db, &today)?;
            }
        }
    }

    let decision = decide_merge(request.gate_green, request.dispatch_class, &outcome);
    let recorded = record_review(
        db,
        &ReviewRecord {
            issue: request.issue.clone(),
            pr_number: request.pr_number,
            dispatch_class: request.dispatch_class.to_string(),
            head_sha: request.head_sha.clone(),
            base_branch: Some(request.base_branch.to_string()),
            outcome: outcome.clone(),
            decision: decision.clone(),
        },
    )?;

    Ok(PrReview {
        issue: request.issue.clone(),
        pr_number: request.pr_number,
        outcome,
        decision,
        record_drawer_id: Some(recorded.drawer_id),
        refusal: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── risk classes ────────────────────────────────────────────────────

    #[test]
    fn the_spec_s_four_low_risk_classes_are_the_only_low_risk_ones() {
        for class in [
            RiskClass::Documentation,
            RiskClass::DependencyBump,
            RiskClass::MechanicalRename,
            RiskClass::TestOnly,
        ] {
            assert!(class.is_low_risk(), "{} must be low risk", class.as_str());
        }
        for class in [
            RiskClass::Logic,
            RiskClass::Protocol,
            RiskClass::Security,
            RiskClass::PublicApi,
        ] {
            assert!(
                !class.is_low_risk(),
                "{} must never be auto-mergeable",
                class.as_str()
            );
        }
    }

    #[test]
    fn every_schema_enum_value_round_trips_through_the_class_parser() {
        // The schema string and the enum are two independent lists; a value
        // in one and not the other means the Reviewer can return something
        // that parses to `None` and silently becomes a hold.
        let schema: serde_json::Value = serde_json::from_str(REVIEW_VERDICT_JSON_SCHEMA).unwrap();
        let values = schema["properties"]["risk_class"]["enum"]
            .as_array()
            .expect("schema must constrain risk_class to an enum");
        assert_eq!(values.len(), 8);
        for value in values {
            let s = value.as_str().unwrap();
            let parsed = RiskClass::from_schema_str(s)
                .unwrap_or_else(|| panic!("schema value {s} does not parse to a RiskClass"));
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn every_schema_verdict_value_round_trips_through_the_verdict_parser() {
        let schema: serde_json::Value = serde_json::from_str(REVIEW_VERDICT_JSON_SCHEMA).unwrap();
        let values = schema["properties"]["verdict"]["enum"].as_array().unwrap();
        assert_eq!(values.len(), 2);
        for value in values {
            assert!(ReviewVerdict::from_schema_str(value.as_str().unwrap()).is_some());
        }
    }

    #[test]
    fn the_schema_requires_all_three_fields_and_forbids_extras() {
        let schema: serde_json::Value = serde_json::from_str(REVIEW_VERDICT_JSON_SCHEMA).unwrap();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"verdict"));
        assert!(required.contains(&"risk_class"));
        assert!(required.contains(&"reason"));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    // ── argv ────────────────────────────────────────────────────────────

    fn sample_spec() -> ReviewSpec {
        ReviewSpec {
            model: None,
            prompt: "review this".to_string(),
            schema_path: PathBuf::from("/tmp/scratch/verdict-schema.json"),
            last_message_path: PathBuf::from("/tmp/scratch/last-message.txt"),
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let idx = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("missing flag {flag} in argv: {args:?}"));
        &args[idx + 1]
    }

    #[test]
    fn argv_uses_the_non_interactive_exec_subcommand() {
        // A bare `codex` forwards to the interactive CLI, which would hang
        // with no TTY. `exec` must be the first argument.
        let args = build_argv(&sample_spec(), Path::new("/repo"));
        assert_eq!(args[0], "exec");
    }

    #[test]
    fn argv_sandboxes_the_reviewer_read_only() {
        // The spec's "read-only" is a sandbox property, not a prompt
        // suggestion. This is the flag that enforces it.
        let args = build_argv(&sample_spec(), Path::new("/repo"));
        assert_eq!(value_after(&args, "-s"), "read-only");
        assert!(
            !args
                .iter()
                .any(|a| a.contains("bypass") || a.contains("danger")),
            "reviewer must never be granted write or bypass access: {args:?}"
        );
    }

    #[test]
    fn argv_holds_no_state_and_emits_json_events() {
        let args = build_argv(&sample_spec(), Path::new("/repo"));
        assert!(args.contains(&"--ephemeral".to_string()));
        assert!(args.contains(&"--json".to_string()));
    }

    #[test]
    fn output_schema_is_passed_as_a_file_path_never_inline() {
        // The exact opposite of the IC dispatch schema, which rung 0
        // measured must be inline. Passing this one inline would be read as
        // a (nonexistent) path.
        let args = build_argv(&sample_spec(), Path::new("/repo"));
        let value = value_after(&args, "--output-schema");
        assert_eq!(value, "/tmp/scratch/verdict-schema.json");
        assert!(
            serde_json::from_str::<serde_json::Value>(value).is_err(),
            "schema must be a path here, not inline JSON"
        );
    }

    #[test]
    fn argv_points_the_reviewer_at_the_supplied_checkout() {
        let args = build_argv(&sample_spec(), Path::new("/repo/checkout"));
        assert_eq!(value_after(&args, "-C"), "/repo/checkout");
        assert_eq!(value_after(&args, "-o"), "/tmp/scratch/last-message.txt");
    }

    #[test]
    fn the_prompt_is_the_trailing_positional() {
        // The prompt lands after every flag *and every flag's value* — a
        // prompt emitted between `-m` and the model name would be consumed as
        // the model. Note this says nothing about a prompt that begins with
        // `-`; see `build_argv`'s note on why that is not a guarantee here.
        let mut spec = sample_spec();
        spec.model = Some("gpt-5-codex".to_string());
        spec.prompt = "review this".to_string();
        let args = build_argv(&spec, Path::new("/repo"));
        assert_eq!(args.last().unwrap(), "review this");
        assert_eq!(value_after(&args, "-m"), "gpt-5-codex");
    }

    #[test]
    fn a_model_is_passed_only_when_one_was_chosen() {
        let args = build_argv(&sample_spec(), Path::new("/repo"));
        assert!(!args.contains(&"-m".to_string()));

        let mut spec = sample_spec();
        spec.model = Some("gpt-5-codex".to_string());
        let args = build_argv(&spec, Path::new("/repo"));
        assert_eq!(value_after(&args, "-m"), "gpt-5-codex");
    }

    // ── parsing the verdict ─────────────────────────────────────────────

    #[test]
    fn parses_a_conforming_message() {
        let (verdict, class, reason) = parse_review_message(
            r#"{"verdict":"pass","risk_class":"documentation","reason":"docs only"}"#,
        );
        assert_eq!(verdict, Some(ReviewVerdict::Pass));
        assert_eq!(class, Some(RiskClass::Documentation));
        assert_eq!(reason.as_deref(), Some("docs only"));
    }

    #[test]
    fn tolerates_whitespace_and_a_code_fence() {
        let (verdict, class, _) = parse_review_message(
            "\n```json\n{\"verdict\":\"needs_changes\",\"risk_class\":\"logic\",\"reason\":\"off by one\"}\n```\n",
        );
        assert_eq!(verdict, Some(ReviewVerdict::NeedsChanges));
        assert_eq!(class, Some(RiskClass::Logic));
    }

    #[test]
    fn prose_never_becomes_a_verdict() {
        // The whole point of --output-schema: no path infers a verdict from
        // text. Each of these must be a NoVerdict hold, not a guess.
        for message in [
            "",
            "Looks good to me, I'd merge it.",
            "PASS",
            "{\"verdict\":\"pass\"}", // no class
            "{\"risk_class\":\"documentation\",\"reason\":\"x\"}", // no verdict
            "{\"verdict\":\"looks_fine\",\"risk_class\":\"documentation\",\"reason\":\"x\"}",
            "{\"verdict\":\"pass\",\"risk_class\":\"trivial\",\"reason\":\"x\"}",
            "{\"verdict\":\"pass\",\"risk_class\":\"Documentation\",\"reason\":\"x\"}",
            "{truncated",
        ] {
            let (verdict, class, _) = parse_review_message(message);
            assert!(
                verdict.is_none() || class.is_none(),
                "message must not yield a complete verdict: {message}"
            );
        }
    }

    // ── token accounting ────────────────────────────────────────────────

    #[test]
    fn reads_the_last_cumulative_token_count_from_the_event_stream() {
        let stdout = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20}}}}
not json at all
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":40,"output_tokens":90}}}}
"#;
        let usage = parse_codex_token_usage(stdout).expect("usage must be found");
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.cached_input_tokens, 40);
        assert_eq!(usage.output_tokens, 90);
    }

    #[test]
    fn token_usage_is_found_regardless_of_envelope_depth() {
        // The parser searches by shape rather than by a fixed path, because
        // the `codex exec --json` envelope was not verified against a live
        // run. A version bump that moves the object must not silently zero
        // the accounting.
        let stdout = r#"{"msg":{"total_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}"#;
        let usage = parse_codex_token_usage(stdout).unwrap();
        assert_eq!(usage.input_tokens, 5);
    }

    #[test]
    fn absent_or_partial_token_accounting_is_unknown_not_zero() {
        assert_eq!(parse_codex_token_usage(""), None);
        assert_eq!(parse_codex_token_usage("{\"type\":\"other\"}\n"), None);
        // A partial object must not be reported as a complete count with
        // zeroed components.
        assert_eq!(
            parse_codex_token_usage(r#"{"total_token_usage":{"input_tokens":5}}"#),
            None
        );
    }

    // ── the merge decision ──────────────────────────────────────────────

    fn passing_outcome(class: RiskClass) -> ReviewOutcome {
        ReviewOutcome {
            verdict: Some(ReviewVerdict::Pass),
            risk_class: Some(class),
            reason: Some("looks correct".to_string()),
            total_cost_usd: None,
            token_usage: None,
            process_success: true,
        }
    }

    #[test]
    fn pass_plus_low_risk_plus_matching_class_is_the_only_way_to_merge() {
        let decision = decide_merge(
            true,
            "documentation",
            &passing_outcome(RiskClass::Documentation),
        );
        assert_eq!(
            decision,
            MergeDecision::EligibleForMerge {
                class: RiskClass::Documentation
            }
        );
        assert!(decision.is_eligible());
    }

    #[test]
    fn reviewer_needs_changes_means_no_merge() {
        // Spec Testing table: "Reviewer returns NEEDS CHANGES → no merge
        // occurs" — Goal 5, gates alone never authorize a merge.
        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.verdict = Some(ReviewVerdict::NeedsChanges);
        assert_eq!(
            decide_merge(true, "documentation", &outcome),
            MergeDecision::HoldForHuman(HoldReason::NeedsChanges)
        );
    }

    #[test]
    fn a_reviewer_that_never_ran_means_no_merge() {
        // Spec Testing table: "Reviewer process fails to start → no merge
        // occurs. Infrastructure failure ≠ approval."
        assert_eq!(
            decide_merge(true, "documentation", &ReviewOutcome::did_not_run()),
            MergeDecision::HoldForHuman(HoldReason::ReviewerDidNotRun)
        );
    }

    #[test]
    fn a_reviewer_that_exited_non_zero_means_no_merge_even_with_a_pass_on_stdout() {
        // A process can flush a complete, schema-valid "pass" and then die.
        // Same guard as `DispatchOutcome::process_success` in rung 2.
        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.process_success = false;
        assert_eq!(
            decide_merge(true, "documentation", &outcome),
            MergeDecision::HoldForHuman(HoldReason::ReviewerDidNotRun)
        );
    }

    #[test]
    fn a_reviewer_that_answered_nothing_usable_means_no_merge() {
        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.verdict = None;
        assert_eq!(
            decide_merge(true, "documentation", &outcome),
            MergeDecision::HoldForHuman(HoldReason::NoVerdict)
        );

        // A verdict with no class is equally unusable: the class half of the
        // decision would have nothing to check.
        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.risk_class = None;
        assert_eq!(
            decide_merge(true, "documentation", &outcome),
            MergeDecision::HoldForHuman(HoldReason::NoVerdict)
        );
    }

    #[test]
    fn a_high_risk_diff_waits_for_a_human_regardless_of_the_verdict() {
        // Spec: "Everything touching logic, protocol, security, or public
        // API opens a PR and waits for a human regardless of reviewer
        // verdict" — note this holds even when the Lead *also* classified it
        // that way, so the classes agree.
        for class in [
            RiskClass::Logic,
            RiskClass::Protocol,
            RiskClass::Security,
            RiskClass::PublicApi,
        ] {
            assert_eq!(
                decide_merge(true, class.as_str(), &passing_outcome(class)),
                MergeDecision::HoldForHuman(HoldReason::HighRiskClass { class }),
                "{} must never auto-merge",
                class.as_str()
            );
        }
    }

    #[test]
    fn a_docs_dispatch_whose_diff_touches_logic_opens_a_pr_instead_of_merging() {
        // Spec Testing table, verbatim: "Dispatch class `docs`, diff touches
        // logic → PR, not merge. Double classification fails closed."
        let decision = decide_merge(true, "documentation", &passing_outcome(RiskClass::Logic));
        assert_eq!(
            decision,
            MergeDecision::HoldForHuman(HoldReason::HighRiskClass {
                class: RiskClass::Logic
            })
        );
        assert!(!decision.is_eligible());
    }

    #[test]
    fn two_disagreeing_low_risk_classes_still_fail_closed() {
        // The subtler half of the same rule: both classes are low risk, so
        // neither guard above fires, but the spec says "Any mismatch between
        // the two ... fails closed to a human PR. Never merge on the stale
        // class."
        assert_eq!(
            decide_merge(true, "documentation", &passing_outcome(RiskClass::TestOnly)),
            MergeDecision::HoldForHuman(HoldReason::ClassMismatch {
                dispatch_class: "documentation".to_string(),
                diff_class: RiskClass::TestOnly,
            })
        );
    }

    #[test]
    fn an_unrecognized_dispatch_class_is_a_mismatch_not_a_wildcard() {
        // `ironmem autopilot run` defaults `--class` to "unclassified",
        // which is not a risk class at all. The safe reading is "we never
        // said what this was", so it can never match — a Lead that forgot to
        // classify cannot auto-merge by omission.
        assert_eq!(
            decide_merge(
                true,
                "unclassified",
                &passing_outcome(RiskClass::Documentation)
            ),
            MergeDecision::HoldForHuman(HoldReason::ClassMismatch {
                dispatch_class: "unclassified".to_string(),
                diff_class: RiskClass::Documentation,
            })
        );
        assert!(!decide_merge(true, "", &passing_outcome(RiskClass::Documentation)).is_eligible());
        assert!(
            !decide_merge(true, "docs", &passing_outcome(RiskClass::Documentation)).is_eligible()
        );
    }

    #[test]
    fn surrounding_whitespace_on_the_dispatch_class_is_not_a_mismatch() {
        // A CLI-supplied class can carry stray whitespace; that is a
        // formatting artifact, not a different class.
        assert!(decide_merge(
            true,
            "  documentation  ",
            &passing_outcome(RiskClass::Documentation)
        )
        .is_eligible());
    }

    #[test]
    fn a_red_gate_is_never_rescued_by_a_reviewer_pass() {
        // Auto-merge is "on green *and* reviewer PASS" — both, not either.
        assert_eq!(
            decide_merge(
                false,
                "documentation",
                &passing_outcome(RiskClass::Documentation)
            ),
            MergeDecision::HoldForHuman(HoldReason::GateNotGreen)
        );
    }

    // ── storage ─────────────────────────────────────────────────────────

    fn sample_record(issue: &IssueRef, reason: &str) -> ReviewRecord {
        let outcome = ReviewOutcome {
            verdict: Some(ReviewVerdict::NeedsChanges),
            risk_class: Some(RiskClass::Logic),
            reason: Some(reason.to_string()),
            total_cost_usd: None,
            token_usage: Some(ReviewTokenUsage {
                input_tokens: 1200,
                cached_input_tokens: 300,
                output_tokens: 400,
            }),
            process_success: true,
        };
        let decision = decide_merge(true, "documentation", &outcome);
        ReviewRecord {
            issue: issue.clone(),
            pr_number: 322,
            dispatch_class: "documentation".to_string(),
            head_sha: Some("0".repeat(40)),
            base_branch: Some("main".to_string()),
            outcome,
            decision,
        }
    }

    #[test]
    fn two_reviews_of_the_same_pr_produce_two_drawers() {
        // The `logical_key` hazard, in this module's shape: a NEEDS CHANGES
        // review and a later PASS on the same PR are two facts. Even two
        // *identical* reviews must not collapse, since the drawer id is
        // content-derived.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);

        let first = record_review(&db, &sample_record(&issue, "same reason")).unwrap();
        let second = record_review(&db, &sample_record(&issue, "same reason")).unwrap();
        assert_ne!(first.drawer_id, second.drawer_id);

        let rows = db.get_drawers(Some(WING), Some(ROOM), usize::MAX).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_review_is_reachable_from_its_issue_by_graph_traversal() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        record_review(&db, &sample_record(&issue, "off by one")).unwrap();

        let reviews = reviews_for_issue(&db, &issue).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].pr_number, 322);
        assert_eq!(reviews[0].verdict, Some(ReviewVerdict::NeedsChanges));
        assert_eq!(reviews[0].risk_class, Some(RiskClass::Logic));

        // ...and via the raw KG primitive, the same one
        // `mcp__ironmem__kg_query` calls, to prove the traversal is not
        // hiding behind this module's own convenience wrapper.
        let kg = KnowledgeGraph::new(&db);
        let entity = kg
            .resolve_entity(&issue.entity_name(), Some(ISSUE_ENTITY_TYPE))
            .expect("issue entity must exist after a review is recorded");
        let edges = kg
            .query_entity_current(&entity.id, 50)
            .unwrap()
            .into_iter()
            .filter(|t| t.predicate == HAS_REVIEW_PREDICATE)
            .count();
        assert_eq!(edges, 1);
    }

    #[test]
    fn every_review_of_an_issue_is_enumerable_oldest_first() {
        // Reviews repeat: NEEDS CHANGES → re-dispatch → review again. Rung 6
        // needs to read what the last one said rather than re-reviewing on
        // every poll, so all of them must be reachable, not just the latest.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        for reason in ["first finding", "second finding", "third finding"] {
            record_review(&db, &sample_record(&issue, reason)).unwrap();
        }

        let reviews = reviews_for_issue(&db, &issue).unwrap();
        assert_eq!(reviews.len(), 3);
        let reasons: Vec<&str> = reviews.iter().filter_map(|r| r.reason.as_deref()).collect();
        assert_eq!(
            reasons,
            vec!["first finding", "second finding", "third finding"]
        );
    }

    #[test]
    fn an_issue_with_no_reviews_enumerates_empty_rather_than_erroring() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 999);
        assert_eq!(reviews_for_issue(&db, &issue).unwrap(), Vec::new());
    }

    #[test]
    fn an_issue_s_attempts_and_reviews_do_not_leak_into_each_other() {
        // Both kinds hang off the same issue entity in the same room; only
        // the predicate separates them. A traversal that ignored it would
        // try to deserialize an attempt as a review.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        super::super::lineage::record_attempt(
            &db,
            &super::super::lineage::AttemptRecord {
                issue: issue.clone(),
                attempt_n: 1,
                approach: "tried the obvious thing".to_string(),
                verdict: super::super::lineage::AttemptOutcome::Failed,
                why_failed: Some("gate red".to_string()),
                commit_sha: None,
            },
        )
        .unwrap();
        record_review(&db, &sample_record(&issue, "off by one")).unwrap();

        assert_eq!(reviews_for_issue(&db, &issue).unwrap().len(), 1);
        assert_eq!(
            super::super::lineage::attempts_for_issue(&db, &issue)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_review_reason_is_scrubbed_before_it_is_persisted() {
        // A review reason quotes the diff, so it can carry anything the diff
        // carried. Same write-path guarantee as `lineage::record_attempt`.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let recorded = record_review(
            &db,
            &sample_record(&issue, &format!("hardcoded key {secret} in the diff")),
        )
        .unwrap();

        let drawer = db.get_drawer(&recorded.drawer_id).unwrap().unwrap();
        assert!(
            !drawer.content.contains(secret),
            "review reason reached storage unscrubbed"
        );
        assert!(recorded.redacted);
    }

    #[test]
    fn the_persisted_record_keeps_the_decision_and_the_token_usage() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let recorded = record_review(&db, &sample_record(&issue, "off by one")).unwrap();

        let drawer = db.get_drawer(&recorded.drawer_id).unwrap().unwrap();
        let body: serde_json::Value = serde_json::from_str(&drawer.content).unwrap();
        assert_eq!(body["pr_number"], 322);
        assert_eq!(body["decision"]["decision"], "hold_for_human");
        assert_eq!(body["decision"]["reason"], "needs_changes");
        assert_eq!(body["dispatch_class"], "documentation");
        assert_eq!(body["risk_class"], "logic");
        // Unpriced, and stored as null rather than 0.0 — see the module doc.
        assert!(body["total_cost_usd"].is_null());
        assert_eq!(body["token_usage"]["input_tokens"], 1200);
    }

    #[test]
    fn a_malformed_repo_is_rejected_before_anything_is_written() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("", 283);
        assert!(record_review(&db, &sample_record(&issue, "x")).is_err());
        assert_eq!(
            db.get_drawers(Some(WING), Some(ROOM), usize::MAX)
                .unwrap()
                .len(),
            0
        );
    }

    // ── review_pr, the policy layer ─────────────────────────────────────

    /// A [`ReviewRunner`] that returns a canned outcome without spawning
    /// anything — the same reason rung 4's `Dispatcher` is a trait: the
    /// policy layer must be testable against a real database without
    /// spending real money.
    ///
    /// The failure case is configured as a message rather than a
    /// `MemoryError` because `MemoryError` is not `Clone`, and the stub has
    /// to be able to answer more than once.
    struct StubRunner {
        outcome: Result<ReviewOutcome, String>,
        calls: usize,
        last_prompt: String,
    }

    impl StubRunner {
        fn returning(outcome: ReviewOutcome) -> Self {
            Self {
                outcome: Ok(outcome),
                calls: 0,
                last_prompt: String::new(),
            }
        }

        /// A runner whose process never starts — `NotFound`, per the
        /// [`ReviewRunner`] error contract.
        fn failing_to_launch(message: &str) -> Self {
            Self {
                outcome: Err(message.to_string()),
                calls: 0,
                last_prompt: String::new(),
            }
        }
    }

    impl ReviewRunner for StubRunner {
        fn review(&mut self, _repo_dir: &Path, prompt: &str) -> Result<ReviewOutcome, MemoryError> {
            self.last_prompt = prompt.to_string();
            self.calls += 1;
            match &self.outcome {
                Ok(outcome) => Ok(outcome.clone()),
                Err(message) => Err(MemoryError::NotFound(message.clone())),
            }
        }
    }

    fn sample_request<'a>(
        issue: &'a IssueRef,
        gates: &'a [String],
        repo_dir: &'a Path,
    ) -> ReviewRequest<'a> {
        ReviewRequest {
            issue,
            pr_number: 322,
            base_branch: "main",
            head_branch: "autopilot/ironrace-ironmem-283",
            head_sha: Some("0".repeat(40)),
            dispatch_class: "documentation",
            gate_commands: gates,
            gate_green: true,
            repo_dir,
            daily_budget_usd: 25.0,
            max_unpriced_reviews_per_day: DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY,
        }
    }

    #[test]
    fn a_clean_low_risk_review_is_recorded_and_eligible() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test --workspace".to_string()];
        let mut runner = StubRunner::returning(passing_outcome(RiskClass::Documentation));

        let review = review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        assert!(review.dispatched());
        assert!(review.decision.is_eligible());
        assert!(review.record_drawer_id.is_some());
        assert_eq!(runner.calls, 1);
        // The prompt the reviewer saw came from the approved gate config,
        // never authored separately.
        assert!(runner.last_prompt.contains("cargo test --workspace"));
    }

    #[test]
    fn a_reviewer_launch_failure_is_recorded_and_held_not_propagated() {
        // The spec's "Reviewer itself fails to run ⇒ treated as NOT
        // reviewed". Returning `Err` here would let a caller retry it into a
        // loop, and would leave no record that the PR was ever looked at.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut runner = StubRunner::failing_to_launch("no codex on PATH");

        let review = review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        assert!(
            review.dispatched(),
            "a launch failure is not a refusal — it produced a recorded, held review"
        );
        assert_eq!(review.refusal, None);
        assert_eq!(
            review.decision,
            MergeDecision::HoldForHuman(HoldReason::ReviewerDidNotRun)
        );
        assert!(review.record_drawer_id.is_some());
    }

    #[test]
    fn an_unpriced_review_is_banked_as_unpriced_never_as_free() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut runner = StubRunner::returning(passing_outcome(RiskClass::Documentation));

        review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        let today = super::super::today_utc();
        let ledger = super::super::budget::get_daily_spend(&db, &today)
            .unwrap()
            .expect("the review must appear on the day's ledger");
        assert_eq!(ledger.unpriced_dispatch_count, 1);
        assert_eq!(ledger.dispatch_count, 0);
        assert_eq!(ledger.total_cost_usd, 0.0);
    }

    #[test]
    fn a_reviewer_that_never_launched_does_not_burn_an_unpriced_slot() {
        // The unpriced counter is the *only* bound on reviewer spend that
        // holds today, so charging it for an invocation that never spawned
        // would let a broken `codex` exhaust the day's reviews for free —
        // and would then report the breakage as `UnpricedReviewCapReached`,
        // destroying the "retry tomorrow vs. something is broken" split that
        // `PrReview::refusal` exists to preserve. Rung 4 draws the same line:
        // a dispatch that errored banks no cost.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut runner = StubRunner::failing_to_launch("no codex on PATH");

        for _ in 0..5 {
            let review = review_pr(
                &db,
                &mut runner,
                &sample_request(&issue, &gates, Path::new("/repo")),
            )
            .unwrap();
            assert_eq!(review.refusal, None, "a launch failure is never a refusal");
        }

        let today = super::super::today_utc();
        match super::super::budget::get_daily_spend(&db, &today).unwrap() {
            None => {}
            Some(ledger) => {
                assert_eq!(
                    ledger.unpriced_dispatch_count, 0,
                    "a reviewer that never spawned spent nothing"
                );
                assert_eq!(ledger.dispatch_count, 0);
            }
        }
    }

    #[test]
    fn a_nonsensical_reported_price_does_not_discard_a_completed_review() {
        // A harness reporting a negative, NaN, or infinite price must not
        // turn a review that actually ran into an `Err` that throws away its
        // record and its merge decision — and must not be believed either.
        // It lands on the unpriced counter: unknown, never free.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let bogus_prices = [-1.0_f64, f64::NAN, f64::INFINITY];
        for bogus in bogus_prices {
            let mut outcome = passing_outcome(RiskClass::Documentation);
            outcome.total_cost_usd = Some(bogus);
            let mut runner = StubRunner::returning(outcome);
            let review = review_pr(
                &db,
                &mut runner,
                &sample_request(&issue, &gates, Path::new("/repo")),
            )
            .expect("a bogus price must not sink the review");
            assert!(review.record_drawer_id.is_some());
        }

        let ledger = super::super::budget::get_daily_spend(&db, &super::super::today_utc())
            .unwrap()
            .unwrap();
        assert_eq!(ledger.total_cost_usd, 0.0);
        assert_eq!(ledger.dispatch_count, 0);
        assert_eq!(
            ledger.unpriced_dispatch_count,
            bogus_prices.len() as u32,
            "an unusable price is unknown spend, not zero spend"
        );
    }

    #[test]
    fn a_ceiling_that_cannot_bind_is_rejected_rather_than_silently_ignored() {
        // `spent_today >= NaN` is always false, so a NaN ceiling would make
        // the dollar bound vanish instead of failing closed. Same check
        // `RunConfig::validate` applies to the IC path.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut runner = StubRunner::returning(passing_outcome(RiskClass::Documentation));

        for bad in [f64::NAN, 0.0, -1.0, f64::INFINITY] {
            let request = ReviewRequest {
                daily_budget_usd: bad,
                ..sample_request(&issue, &gates, Path::new("/repo"))
            };
            assert!(
                review_pr(&db, &mut runner, &request).is_err(),
                "daily_budget_usd {bad} must be rejected"
            );
        }
        assert_eq!(
            runner.calls, 0,
            "no reviewer may run on an unusable ceiling"
        );
    }

    #[test]
    fn a_priced_review_is_banked_against_the_running_total() {
        // No harness reports a price today, but the accounting path must
        // already be right for the one that eventually does.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.total_cost_usd = Some(0.04);
        let mut runner = StubRunner::returning(outcome);

        review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        let today = super::super::today_utc();
        let ledger = super::super::budget::get_daily_spend(&db, &today)
            .unwrap()
            .unwrap();
        assert!((ledger.total_cost_usd - 0.04).abs() < 1e-9);
        assert_eq!(ledger.dispatch_count, 1);
        assert_eq!(ledger.unpriced_dispatch_count, 0);
    }

    #[test]
    fn an_exhausted_daily_budget_refuses_to_dispatch_a_reviewer() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let today = super::super::today_utc();
        super::super::budget::accumulate_daily_spend(&db, &today, 25.0).unwrap();

        let mut runner = StubRunner::returning(passing_outcome(RiskClass::Documentation));
        let review = review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        assert_eq!(runner.calls, 0, "no reviewer may run past the daily cap");
        assert!(!review.dispatched());
        assert_eq!(review.refusal, Some(ReviewRefusal::DailyBudgetExhausted));
        assert_eq!(
            review.decision,
            MergeDecision::HoldForHuman(HoldReason::ReviewerDidNotRun),
            "an unreviewed PR is held whatever the reason it went unreviewed"
        );
        // Nothing ran, so nothing is recorded: a refusal is not a review.
        assert_eq!(review.record_drawer_id, None);
        assert_eq!(
            db.get_drawers(Some(WING), Some(ROOM), usize::MAX)
                .unwrap()
                .len(),
            1,
            "only the pre-existing ledger drawer should be present"
        );
    }

    #[test]
    fn unpriced_reviews_are_bounded_by_count_because_dollars_cannot_bound_them() {
        // Regression guard for the defect rung 5's end-to-end smoke test
        // caught: six reviews had run, every one unpriced, so the ledger
        // still read $0.00 and a `--daily-budget-usd 0.01` refusal never
        // fired. A dollar ceiling is inert against an invocation that
        // reports no dollars, so the bound has to be a count.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let today = super::super::today_utc();

        let mut runner = StubRunner::returning(passing_outcome(RiskClass::Documentation));
        for _ in 0..3 {
            let request = ReviewRequest {
                max_unpriced_reviews_per_day: 3,
                ..sample_request(&issue, &gates, Path::new("/repo"))
            };
            let review = review_pr(&db, &mut runner, &request).unwrap();
            assert!(review.dispatched());
        }

        // The dollar ledger has not moved at all — which is exactly why it
        // could never have stopped this on its own.
        assert_eq!(
            super::super::budget::get_daily_spend(&db, &today)
                .unwrap()
                .unwrap()
                .total_cost_usd,
            0.0
        );

        let request = ReviewRequest {
            max_unpriced_reviews_per_day: 3,
            ..sample_request(&issue, &gates, Path::new("/repo"))
        };
        let review = review_pr(&db, &mut runner, &request).unwrap();
        assert_eq!(runner.calls, 3, "the fourth review must not have launched");
        assert!(!review.dispatched());
        assert_eq!(
            review.refusal,
            Some(ReviewRefusal::UnpricedReviewCapReached)
        );
        assert_eq!(review.record_drawer_id, None);
        assert_eq!(
            review.decision,
            MergeDecision::HoldForHuman(HoldReason::ReviewerDidNotRun)
        );
    }

    #[test]
    fn a_priced_reviewer_is_still_bounded_by_the_dollar_ceiling() {
        // The count cap must not have displaced the dollar one: a harness
        // that does report a price stays bounded by spend, as the spec's
        // ledger intends.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let today = super::super::today_utc();
        super::super::budget::accumulate_daily_spend(&db, &today, 25.0).unwrap();

        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.total_cost_usd = Some(0.04);
        let mut runner = StubRunner::returning(outcome);
        let review = review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        assert_eq!(runner.calls, 0);
        assert_eq!(review.refusal, Some(ReviewRefusal::DailyBudgetExhausted));
    }

    #[test]
    fn a_needs_changes_review_still_banks_its_spend() {
        // The review cost money whatever it concluded — the same reason rung
        // 4 banks failed dispatches.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut outcome = passing_outcome(RiskClass::Documentation);
        outcome.verdict = Some(ReviewVerdict::NeedsChanges);
        let mut runner = StubRunner::returning(outcome);

        let review = review_pr(
            &db,
            &mut runner,
            &sample_request(&issue, &gates, Path::new("/repo")),
        )
        .unwrap();

        assert_eq!(
            review.decision,
            MergeDecision::HoldForHuman(HoldReason::NeedsChanges)
        );
        let today = super::super::today_utc();
        assert_eq!(
            super::super::budget::get_daily_spend(&db, &today)
                .unwrap()
                .unwrap()
                .unpriced_dispatch_count,
            1
        );
    }
}
