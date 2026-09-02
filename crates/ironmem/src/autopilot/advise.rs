//! The advisor — build-ladder rung 9, and the ladder's stated residual.
//!
//! Rung 8 closed the spec's open question 9 with a Rust-native mechanical
//! supervisor and left one thing explicitly unbuilt:
//!
//! > **Residual, stated not hidden.** The three one-shot calls are
//! > *specified and located*, not built — no rung has yet made an LLM call
//! > from the supervisor.
//!
//! This module builds them. Three short, bounded, **toolless** `claude -p`
//! invocations, one per judgment-shaped step the spec named:
//!
//! | Call | Judgment | Mechanical answer it improves on |
//! |---|---|---|
//! | [`advise_risk_class`] | Reading an issue's prose to route it | [`super::lead::class_for`]'s fallback, which routes nothing |
//! | [`advise_strategy_redirect`] | Proposing a *different* approach | [`super::supervise::redirect_text`], which can only forbid the old one |
//! | [`advise_human_question`] | Naming *what* is unclear | Operator-supplied text, i.e. nobody is told at all |
//!
//! # The property that matters, and how it is kept
//!
//! OQ9's close-out does not rest on these calls being good. It rests on
//! this:
//!
//! > **The loop does not depend on them being available.**
//!
//! So every path here fails toward the pre-rung-9 behaviour rather than
//! toward a stop or a guess. A disabled advisor, an unresolvable binary, a
//! spawn failure, a non-zero exit, unparseable stdout, a missing
//! `structured_output`, an out-of-enum answer, an exhausted budget and an
//! exhausted call count all produce the same thing: an [`Advice`] carrying
//! no answer, and a caller that proceeds exactly as rung 8 did. Nothing in
//! this module can stop a dispatch, hold a merge, or block a tick.
//! `a_failing_advisor_changes_nothing_about_the_tick` in
//! [`super::lead`]'s tests is the regression guard for that, and it is the
//! most important test in the rung.
//!
//! The corollary is that an advisor is never consulted for a fact that is
//! already mechanically known. An issue carrying a `risk:*` label is
//! classified by the label; the call is not made "to check". Spending money
//! to re-derive a known value is the failure mode this whole ladder has
//! avoided by construction.
//!
//! # Every schema has a way to say "I don't know"
//!
//! Each of the three schemas carries an explicit declining member —
//! `unclear`, `no_proposal`, `no_question`. This is not politeness. A
//! constrained enum with no decline member converts *uncertainty* into a
//! *confident wrong answer*, and for [`advise_risk_class`] a confident wrong
//! answer is the one that can auto-merge: rung 5's `decide_merge` treats
//! `documentation` as low-risk and merges it on a reviewer PASS. Declining
//! routes to `unclassified`, which holds for a human. The schema, the prompt
//! and the parser all agree that declining is the cheap outcome and guessing
//! is the expensive one.
//!
//! # The one Autopilot `claude` call with no bypass
//!
//! Every IC dispatch carries `--dangerously-skip-permissions`, by design and
//! by measurement (the spec's push-delivery A/B depends on it). An advisor
//! call carries **none of that**, and must not:
//!
//! - `--tools ""` — no tools at all. The call reads the prompt and answers.
//! - `--permission-prompts none` — anything that *would* prompt is denied
//!   automatically rather than waiting for a human who is not there. With no
//!   tools nothing should prompt; this is what makes "should" into "cannot
//!   hang", which matters because the caller is a cron-restarted supervisor.
//! - `--max-turns 1` — one turn. There is no loop to supervise.
//! - `--no-session-persistence` — no session is created, so none can be
//!   resumed, adopted, or mistaken for an IC by rung 7's registry read.
//!
//! Granting bypass to a call that cannot use a tool would hand a judgment
//! step the IC's blast radius in exchange for nothing.
//!
//! # Budget: priced in dollars, because `claude` reports dollars
//!
//! Rung 5's lesson 17 — *a ceiling denominated in units the thing being
//! bounded never reports is not a bound* — is the reason the Codex reviewer
//! needed an invocation-count ceiling. It does not apply here: this is the
//! `claude` CLI, whose `--output-format json` envelope was measured in rungs
//! 0 and 2 to carry an exact `total_cost_usd`, and which enforces
//! `--max-budget-usd` itself. So an advisor call is bounded three ways: per
//! call by `--max-budget-usd`, per day in dollars by the same
//! pre-authorization predicate [`super::run::run_issue`] applies, and per
//! day by count via [`AdviceConfig::max_calls_per_day`].
//!
//! The count ceiling is not redundant with the dollar one. A Lead tick can
//! make several advisor calls, and a cron entry can tick often; the dollar
//! ceiling is shared with IC dispatches, so without a count ceiling a chatty
//! advisor could consume the day's dispatch budget in cent-sized pieces and
//! the ledger would look healthy the whole way down.
//!
//! Unpriced calls get their **own** counter
//! ([`super::budget::BudgetLedgerEntry::unpriced_advice_count`]) rather than
//! rung 5's shared `unpriced_dispatch_count`. See that field's doc: it is
//! rung 7's lesson 26 read correctly — inherit the earlier mechanism's fix,
//! not its counter.
//!
//! # Storage: a tenth drawer kind
//!
//! [`AdviceRecord`], kind 1's shape — append-only, no `logical_key`, a
//! `has_advice` edge. Recorded because rung 9 introduces the first model
//! judgment inside the loop, and one of the three
//! ([`advise_risk_class`]) produces a value that gates auto-merge. "Which
//! model said `documentation`, when, at what price, and why" has to be
//! answerable after the fact for the same reason rung 6 records every merge.
//!
//! # What this module deliberately does not do
//!
//! It does not classify a diff — that is the Reviewer's job (rung 5), from
//! fresh context, in a different model family, and the dispatch-time class
//! this module may produce is *checked against* it. An advisor that
//! misclassifies an issue does not merge anything; it produces a mismatch,
//! and a mismatch holds.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::lineage::MAX_LINEAGE_FIELD_CHARS;
use super::merge::serialize_issue;
use super::review::RiskClass;
use super::scrub::scrub_and_bound;
use super::{
    today_utc, validate_repo, zero_embedding, IssueRef, ADDED_BY, ISSUE_ENTITY_TYPE,
    MAX_ISSUE_EDGES, ROOM, WING,
};

// ── the three schemas ───────────────────────────────────────────────────

/// Forced onto [`AdviceKind::RiskClass`] via `--json-schema`.
///
/// The eight classes are [`RiskClass`]'s own schema spellings, verbatim, plus
/// `unclear`. Inline JSON, never a file path — rung 0 measured a file-path
/// attempt fail with "not valid JSON", and rung 5 measured Codex to be the
/// exact reverse. This is a `claude` call, so it is the inline form.
pub const RISK_CLASS_JSON_SCHEMA: &str = r#"{"type":"object","properties":{"risk_class":{"type":"string","enum":["documentation","dependency_bump","mechanical_rename","test_only","logic","protocol","security","public_api","unclear"]},"reason":{"type":"string"}},"required":["risk_class","reason"],"additionalProperties":false}"#;

/// Forced onto [`AdviceKind::StrategyRedirect`].
pub const REDIRECT_JSON_SCHEMA: &str = r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["proposal","no_proposal"]},"proposal":{"type":"string"},"reason":{"type":"string"}},"required":["verdict","proposal","reason"],"additionalProperties":false}"#;

/// Forced onto [`AdviceKind::HumanQuestion`].
pub const QUESTION_JSON_SCHEMA: &str = r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["question","no_question"]},"question":{"type":"string"},"reason":{"type":"string"}},"required":["verdict","question","reason"],"additionalProperties":false}"#;

/// The enum member every schema carries for "I cannot judge this".
const UNCLEAR: &str = "unclear";

// ── defaults, all placeholders with no spec basis ───────────────────────

/// Model for advisor calls.
///
/// The spec's *Model routing* table assigns **Opus** to the Lead, and these
/// three calls are precisely the Lead's judgment — the only part of it that
/// was ever judgment-shaped. Each is one turn against a short prompt with no
/// tools, so the per-call cost is cents even at the top of the range, and
/// under-powering the one step whose whole purpose is reading prose would be
/// the same mistake the spec warns against for the goal evaluator, in
/// reverse.
pub const DEFAULT_ADVICE_MODEL: &str = "claude-opus-5";

/// Per-call spend ceiling, passed as `--max-budget-usd`.
///
/// No spec basis. One toolless turn against a bounded prompt should cost
/// well under this; it is a backstop against a pathological answer, not a
/// budget.
pub const DEFAULT_MAX_ADVICE_BUDGET_USD: f64 = 0.25;

/// How many advisor calls one day may make, priced or not.
///
/// No spec basis. Sized so that a day of ticks cannot consume a meaningful
/// share of the default `$25.00` daily ledger even if every call ran to its
/// ceiling: 20 × `$0.25` = `$5.00` worst case, and the realistic figure is
/// an order of magnitude below that.
pub const DEFAULT_MAX_ADVICE_CALLS_PER_DAY: u32 = 20;

/// Bound on the prompt text handed to an advisor call.
///
/// Issue bodies and failure signatures are attacker-adjacent text (anyone
/// who can open an issue can write one), so they are scrubbed and bounded on
/// the way *in*, not only on the way to storage.
pub const MAX_ADVICE_PROMPT_CHARS: usize = 6_000;

/// Bound on any one field quoted into a prompt.
pub const MAX_ADVICE_FIELD_CHARS: usize = 2_000;

/// Bound on the advisor's own answer, applied before it is used or stored.
pub const MAX_ADVICE_ANSWER_CHARS: usize = 1_200;

/// How many prior approaches are quoted into a redirect or question prompt.
///
/// Newest first. The whole lineage would blow the prompt bound on a
/// long-running issue, and the newest few are what "you keep doing this"
/// actually refers to.
pub const MAX_QUOTED_APPROACHES: usize = 5;

/// `LIMIT` on every current edge of the issue entity, matching
/// `MAX_REVIEWS_PER_ISSUE` / `MAX_MERGES_PER_ISSUE` for the reason rung 5's
/// review finding #4 established: the traversal is not predicate-filtered, so
/// a smaller limit here would silently drop *other* kinds of record.
pub const MAX_ADVICE_PER_ISSUE: usize = 10_000;

const ADVICE_ENTITY_TYPE: &str = "advice";
const HAS_ADVICE_PREDICATE: &str = "has_advice";

// ── what is being asked ─────────────────────────────────────────────────

/// Which of the spec's three judgment-shaped steps a call is making.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdviceKind {
    RiskClass,
    StrategyRedirect,
    HumanQuestion,
}

impl AdviceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AdviceKind::RiskClass => "risk_class",
            AdviceKind::StrategyRedirect => "strategy_redirect",
            AdviceKind::HumanQuestion => "human_question",
        }
    }

    /// The schema forced onto this kind's answer.
    pub fn schema(self) -> &'static str {
        match self {
            AdviceKind::RiskClass => RISK_CLASS_JSON_SCHEMA,
            AdviceKind::StrategyRedirect => REDIRECT_JSON_SCHEMA,
            AdviceKind::HumanQuestion => QUESTION_JSON_SCHEMA,
        }
    }
}

/// Everything [`build_argv`] needs for one advisor invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct AdviceSpec {
    pub kind: AdviceKind,
    pub model: String,
    /// The rendered prompt, already scrubbed and bounded.
    pub prompt: String,
    pub max_budget_usd: f64,
}

/// Build the argv for one advisor call.
///
/// Pure, so the flags that make this call safe are unit-testable without
/// spawning anything — and they are tested, individually, because each is
/// load-bearing and their absence is silent. See the module doc.
///
/// `--tools` goes **last on purpose**: it is variadic (`--tools <tools...>`),
/// so a following bare value could be swallowed as another tool name. Nothing
/// follows it here, and nothing should be added after it.
pub fn build_argv(spec: &AdviceSpec) -> Vec<String> {
    vec![
        "-p".to_string(),
        spec.prompt.clone(),
        "--output-format".to_string(),
        "json".to_string(),
        "--model".to_string(),
        spec.model.clone(),
        "--max-turns".to_string(),
        "1".to_string(),
        "--max-budget-usd".to_string(),
        spec.max_budget_usd.to_string(),
        "--json-schema".to_string(),
        spec.kind.schema().to_string(),
        "--no-session-persistence".to_string(),
        "--permission-prompts".to_string(),
        "none".to_string(),
        "--tools".to_string(),
        String::new(),
    ]
}

// ── the runner ──────────────────────────────────────────────────────────

/// One raw advisor invocation's result.
///
/// A non-zero exit is reported here rather than as an `Err`, matching
/// [`super::run::Dispatcher`], [`super::review::ReviewRunner`],
/// [`super::gh::GhRunner`] and [`super::registry::AgentRegistry`] — the
/// fifth and last copy of rung 2's pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct AdviceOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// How [`run_advice`] makes the call.
///
/// A trait for the reason the four before it are: it makes the policy layer
/// — authorization, ledger accounting, the audit record, and every
/// degradation path — testable against a real database without spawning
/// `claude` and **without spending real money**.
pub trait Advisor {
    /// Run one advisor call. A failure to **spawn** must be
    /// [`MemoryError::NotFound`]; a non-zero exit is a successful call
    /// returning `success: false`.
    fn advise(&mut self, cwd: &Path, spec: &AdviceSpec) -> Result<AdviceOutput, MemoryError>;
}

/// The real advisor: a toolless one-shot `claude -p`.
pub struct ClaudeAdvisor {
    bin: std::path::PathBuf,
}

impl ClaudeAdvisor {
    /// Resolve `claude` on PATH, reusing `launcher`'s binary validation
    /// exactly as every other runner in this subsystem does.
    pub fn resolve() -> Result<Self, MemoryError> {
        Ok(Self {
            bin: super::dispatch::resolve_claude_binary()?,
        })
    }
}

impl Advisor for ClaudeAdvisor {
    fn advise(&mut self, cwd: &Path, spec: &AdviceSpec) -> Result<AdviceOutput, MemoryError> {
        let output = std::process::Command::new(&self.bin)
            .args(build_argv(spec))
            .current_dir(cwd)
            .output()
            .map_err(|e| MemoryError::NotFound(format!("failed to launch advisor: {e}")))?;
        Ok(AdviceOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        })
    }
}

// ── the envelope ────────────────────────────────────────────────────────

/// The measured `--output-format json` envelope, as much of it as an advisor
/// call needs.
///
/// The same envelope [`super::dispatch::parse_dispatch_output`] reads —
/// measured in rung 0 and re-measured in rung 2 — narrowed to the three
/// fields that matter here. Every field is optional, because the only thing
/// worth erroring on is stdout that is not JSON at all;
/// `envelope_agrees_with_the_dispatch_parser` pins the two readings against
/// one sample so this narrowing cannot drift away from the measured shape.
#[derive(Debug, Deserialize)]
struct AdviceEnvelope {
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    structured_output: Option<serde_json::Value>,
}

// ── configuration ───────────────────────────────────────────────────────

/// Whether, and how, the Lead may ask a model for judgment.
#[derive(Debug, Clone, PartialEq)]
pub struct AdviceConfig {
    /// **Off by default.** Every other bound in this subsystem limits how
    /// much of an already-authorized activity may happen; this one decides
    /// whether a new kind of paid call happens at all, so it is the
    /// operator's explicit choice rather than a default that spends.
    pub enabled: bool,
    pub model: String,
    pub max_budget_usd: f64,
    pub max_calls_per_day: u32,
    /// The day's dollar ceiling, shared with IC dispatches and reviews.
    pub daily_budget_usd: f64,
}

impl Default for AdviceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: DEFAULT_ADVICE_MODEL.to_string(),
            max_budget_usd: DEFAULT_MAX_ADVICE_BUDGET_USD,
            max_calls_per_day: DEFAULT_MAX_ADVICE_CALLS_PER_DAY,
            daily_budget_usd: super::run::DEFAULT_DAILY_BUDGET_USD,
        }
    }
}

impl AdviceConfig {
    /// Reject a configuration that cannot produce a usable call.
    ///
    /// Validated even when disabled: a misconfigured advisor should be named
    /// at the point it is configured, not discovered the day someone turns it
    /// on.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.model.trim().is_empty() {
            return Err(MemoryError::Validation(
                "advisor model must not be empty".into(),
            ));
        }
        if !self.max_budget_usd.is_finite() || self.max_budget_usd <= 0.0 {
            return Err(MemoryError::Validation(
                "advisor max_budget_usd must be a finite, positive number".into(),
            ));
        }
        if !self.daily_budget_usd.is_finite() || self.daily_budget_usd <= 0.0 {
            return Err(MemoryError::Validation(
                "advisor daily_budget_usd must be a finite, positive number".into(),
            ));
        }
        if self.max_calls_per_day == 0 {
            return Err(MemoryError::Validation(
                "advisor max_calls_per_day must be at least 1 — zero is spelled \
                 `enabled: false`, which says the same thing without pretending a \
                 call could ever be authorized"
                    .into(),
            ));
        }
        if self.max_budget_usd > self.daily_budget_usd {
            return Err(MemoryError::Validation(format!(
                "advisor max_budget_usd ({}) exceeds daily_budget_usd ({}) — no advisor \
                 call could ever be authorized",
                self.max_budget_usd, self.daily_budget_usd
            )));
        }
        Ok(())
    }
}

// ── what came back ──────────────────────────────────────────────────────

/// Why an advisor call produced no usable answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AdviceStatus {
    /// The model answered, and the answer parsed.
    Answered,
    /// The model ran and explicitly declined — `unclear`, `no_proposal`,
    /// `no_question`. **A real answer**, and the cheap one: it means the
    /// caller keeps rung 8's mechanical behaviour, which is always safe.
    Declined { why: String },
    /// The call was never made: the advisor is disabled, the day's dollar
    /// ceiling cannot authorize it, or the day's call count is spent.
    /// **Nothing was spent.**
    Refused { why: String },
    /// The call was attempted and produced nothing usable — spawn failure,
    /// non-zero exit, unparseable stdout, missing `structured_output`, or an
    /// answer outside the schema's enum. Money may have been spent, and the
    /// ledger says so.
    Unavailable { why: String },
}

impl AdviceStatus {
    /// Whether a process actually ran, and therefore whether the ledger
    /// should have been touched.
    pub fn ran(&self) -> bool {
        !matches!(self, AdviceStatus::Refused { .. })
    }
}

/// One advisor call's outcome.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Advice {
    pub kind: AdviceKind,
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub status: AdviceStatus,
    /// The schema's payload — a risk class, a proposed approach, a drafted
    /// question — scrubbed and bounded. `None` for every status but
    /// [`AdviceStatus::Answered`].
    pub answer: Option<String>,
    /// The model's stated reason, scrubbed and bounded.
    pub reason: Option<String>,
    /// `None` means **unknown**, never free. See rung 5's lesson 18.
    pub total_cost_usd: Option<f64>,
}

impl Advice {
    /// The answered risk class, if this is an answered [`AdviceKind::RiskClass`].
    ///
    /// Parsed rather than trusted: the enum constraint lives in the model's
    /// output schema, and a guard that assumes the schema was honoured is a
    /// guard that reports rather than binds.
    pub fn risk_class(&self) -> Option<RiskClass> {
        if self.kind != AdviceKind::RiskClass {
            return None;
        }
        self.answer.as_deref().and_then(RiskClass::from_schema_str)
    }

    /// The usable answer, if there is one.
    pub fn answered(&self) -> Option<&str> {
        match self.status {
            AdviceStatus::Answered => self.answer.as_deref(),
            _ => None,
        }
    }

    fn refused(kind: AdviceKind, issue: &IssueRef, why: impl Into<String>) -> Self {
        Self {
            kind,
            issue: issue.clone(),
            status: AdviceStatus::Refused { why: why.into() },
            answer: None,
            reason: None,
            total_cost_usd: None,
        }
    }
}

// ── the three prompts ───────────────────────────────────────────────────

/// Quote a caller-supplied string into a prompt: scrubbed for secrets and
/// bounded, because issue text is written by anyone who can open an issue.
fn quote(text: &str) -> String {
    scrub_and_bound(text.trim(), MAX_ADVICE_FIELD_CHARS).text
}

fn quote_approaches(approaches: &[String]) -> String {
    if approaches.is_empty() {
        return "(none recorded)".to_string();
    }
    approaches
        .iter()
        .rev()
        .take(MAX_QUOTED_APPROACHES)
        .map(|approach| format!("- {}", quote(approach)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The risk-classification prompt.
///
/// It states what a wrong answer costs, because that is the fact that makes
/// declining the right move: the caller's fallback holds the work for a
/// human, whereas a confident `documentation` on a logic change is the one
/// answer that can reach `gh pr merge`.
pub fn render_risk_prompt(issue: &IssueRef, title: &str, body: &str) -> String {
    let prompt = format!(
        "You are classifying one GitHub issue for an autonomous backlog runner. Answer only \
         with the structured verdict the schema requires.\n\n\
         Classify the change this issue asks for into exactly one risk class:\n\
         - documentation, dependency_bump, mechanical_rename, test_only: low risk. A change in \
         one of these classes may be merged automatically once a separate reviewer agrees.\n\
         - logic, protocol, security, public_api: everything that changes behaviour, an \
         interface, or a security boundary. These always wait for a human.\n\
         - unclear: you cannot tell from what you have been given.\n\n\
         Answer `unclear` unless the issue text itself settles it. Declining is cheap: the work \
         is routed to a human and nothing is lost. Guessing is not: a wrong low-risk answer is \
         the one path by which an unreviewed change can be merged without a human. Do not infer \
         a class from the issue's tone, its author, or how small it sounds.\n\n\
         Issue: {issue}\n\
         Title: {title}\n\
         Body:\n{body}\n",
        issue = issue.canonical(),
        title = quote(title),
        body = quote(body),
    );
    scrub_and_bound(&prompt, MAX_ADVICE_PROMPT_CHARS).text
}

/// The strategy-redirect prompt.
///
/// The mechanical redirect rung 7 generates is quoted in, because the
/// proposal is *added to* it and must not restate it.
pub fn render_redirect_prompt(issue: &IssueRef, signature: &str, approaches: &[String]) -> String {
    let prompt = format!(
        "An autonomous coding agent is stuck on one GitHub issue. Its last few attempts all \
         failed the same way. Answer only with the structured verdict the schema requires.\n\n\
         Propose ONE genuinely different approach it has not already tried — a different cause \
         to investigate, a different place to look, a different order of work. Two or three \
         sentences, addressed to the agent, concrete enough to act on without further \
         explanation.\n\n\
         Answer `no_proposal` if what you have does not support a real alternative. It is \
         already being told not to repeat itself; a restatement of that, or a generic \
         suggestion that would apply to any failure, is worse than nothing, because it reads \
         as new information and is not.\n\n\
         Issue: {issue}\n\
         The repeated failure: {signature}\n\
         Approaches already tried (newest first):\n{approaches}\n",
        issue = issue.canonical(),
        signature = quote(signature),
        approaches = quote_approaches(approaches),
    );
    scrub_and_bound(&prompt, MAX_ADVICE_PROMPT_CHARS).text
}

/// The human-escalation prompt.
pub fn render_question_prompt(
    issue: &IssueRef,
    title: &str,
    body: &str,
    signature: &str,
    approaches: &[String],
) -> String {
    let prompt = format!(
        "An autonomous coding agent has been stopped on one GitHub issue: it tried a redirected \
         approach and failed the same way regardless. A human is about to be asked for help. \
         Answer only with the structured verdict the schema requires.\n\n\
         Draft ONE question for that human. It must name the specific thing that is unclear or \
         undecided — an ambiguity in the issue, a missing decision, a constraint nobody stated \
         — and be answerable in a sentence or two by someone who knows this codebase. Do not \
         ask for permission, do not ask the human to debug it, and do not summarise what \
         happened: they can read the attempts.\n\n\
         Answer `no_question` if nothing in what you have been given is genuinely unclear. A \
         spurious question costs a human's attention and buries the real ones.\n\n\
         Issue: {issue}\n\
         Title: {title}\n\
         Body:\n{body}\n\
         The repeated failure: {signature}\n\
         Approaches already tried (newest first):\n{approaches}\n",
        issue = issue.canonical(),
        title = quote(title),
        body = quote(body),
        signature = quote(signature),
        approaches = quote_approaches(approaches),
    );
    scrub_and_bound(&prompt, MAX_ADVICE_PROMPT_CHARS).text
}

// ── authorization ───────────────────────────────────────────────────────

/// Why an advisor call may not be made today, or `None` if it may.
///
/// The dollar predicate is spelled **exactly** as
/// [`super::run::run_issue`]'s pre-authorization —
/// `spent + max_budget_usd > daily` — rather than approximated with
/// `spent >= daily`. Rung 7's lesson 26 and rung 8's queue guard, a third
/// time: two spellings of one ceiling let one caller authorize what the next
/// refuses.
fn refusal(db: &Database, config: &AdviceConfig) -> Result<Option<String>, MemoryError> {
    if !config.enabled {
        return Ok(Some("the advisor is disabled".to_string()));
    }
    let ledger = super::budget::get_daily_spend(db, &today_utc())?;
    let spent = ledger.as_ref().map(|e| e.total_cost_usd).unwrap_or(0.0);
    let calls = ledger.as_ref().map(|e| e.advice_call_count).unwrap_or(0);
    if calls >= config.max_calls_per_day {
        return Ok(Some(format!(
            "the day's advisor call ceiling is spent ({calls}/{})",
            config.max_calls_per_day
        )));
    }
    if spent + config.max_budget_usd > config.daily_budget_usd {
        return Ok(Some(format!(
            "the daily budget cannot authorize another ${:.2} call (${:.4} of ${:.2} spent)",
            config.max_budget_usd, spent, config.daily_budget_usd
        )));
    }
    Ok(None)
}

// ── the call ────────────────────────────────────────────────────────────

/// Make one advisor call: authorize, run, bill, interpret, record.
///
/// # Ordering
///
/// The ledger is written **immediately after the process returns**, before
/// the answer is interpreted or recorded. Rung 6's lesson 15 — order the
/// write against the half you cannot afford to lose — points that way
/// unambiguously: money is already spent by then, and an unparseable answer
/// must not be able to lose the accounting for it. The audit record is
/// written last, because losing it costs an audit trail rather than a
/// budget.
///
/// Returns `Err` only for a failure of *this crate's* storage. Every failure
/// of the advisor itself is an [`AdviceStatus`], never an `Err` — a
/// supervisor must not be stoppable by the thing it is optional about.
pub fn run_advice(
    db: &Database,
    advisor: &mut dyn Advisor,
    cwd: &Path,
    issue: &IssueRef,
    kind: AdviceKind,
    prompt: &str,
    config: &AdviceConfig,
) -> Result<Advice, MemoryError> {
    validate_repo(&issue.repo)?;
    config.validate()?;

    if let Some(why) = refusal(db, config)? {
        let advice = Advice::refused(kind, issue, why);
        // Not recorded: nothing happened, and a drawer per tick saying "the
        // advisor is off" would bury the records of calls that did happen.
        return Ok(advice);
    }

    let spec = AdviceSpec {
        kind,
        model: config.model.clone(),
        prompt: prompt.to_string(),
        max_budget_usd: config.max_budget_usd,
    };

    let (cost, status, answer, reason) = match advisor.advise(cwd, &spec) {
        // A call that never launched spent nothing, and banks nothing —
        // rung 5's HIGH finding #1, which rung 4 had already established and
        // rung 5 failed to carry over. Every `Err` from a runner in this
        // subsystem happens before the process exists.
        Err(e) => {
            let advice = Advice {
                kind,
                issue: issue.clone(),
                status: AdviceStatus::Unavailable {
                    why: format!("the advisor did not run: {e}"),
                },
                answer: None,
                reason: None,
                total_cost_usd: None,
            };
            record_advice(db, &advice)?;
            return Ok(advice);
        }
        Ok(output) => {
            let envelope: Option<AdviceEnvelope> = serde_json::from_str(&output.stdout).ok();
            let cost = envelope.as_ref().and_then(|e| e.total_cost_usd);
            // Bill first. Everything below this line can fail without
            // costing the ledger a call that really happened.
            super::budget::record_advice_call(db, &today_utc(), cost)?;
            let (status, answer, reason) = interpret(kind, &output, envelope.as_ref());
            (cost, status, answer, reason)
        }
    };

    let advice = Advice {
        kind,
        issue: issue.clone(),
        status,
        answer,
        reason,
        total_cost_usd: cost,
    };
    record_advice(db, &advice)?;
    Ok(advice)
}

/// Read one call's result into a status and, if there is one, an answer.
///
/// Every degradation lands on [`AdviceStatus::Unavailable`] or
/// [`AdviceStatus::Declined`], and both mean the caller keeps its mechanical
/// behaviour. Nothing here can produce an answer the schema did not allow.
fn interpret(
    kind: AdviceKind,
    output: &AdviceOutput,
    envelope: Option<&AdviceEnvelope>,
) -> (AdviceStatus, Option<String>, Option<String>) {
    let unavailable = |why: String| (AdviceStatus::Unavailable { why }, None, None);

    // A non-zero exit refuses the answer even when stdout parsed, exactly as
    // `DispatchOutcome::is_met` refuses a "met" from a failed process: a
    // process can flush a complete, schema-valid answer and then die for an
    // unrelated reason.
    if !output.success {
        return unavailable(format!(
            "the advisor exited non-zero: {}",
            first_line(&output.stderr)
        ));
    }
    let Some(envelope) = envelope else {
        return unavailable("the advisor produced no parseable result JSON".to_string());
    };
    if envelope.is_error {
        return unavailable("the advisor reported an error result".to_string());
    }
    let Some(structured) = envelope.structured_output.as_ref() else {
        return unavailable(
            "the advisor returned no structured_output — the 6a guard: an unschema'd \
             answer is never read as one"
                .to_string(),
        );
    };

    let field = |name: &str| -> Option<String> {
        structured
            .get(name)
            .and_then(|v| v.as_str())
            .map(|s| scrub_and_bound(s.trim(), MAX_ADVICE_ANSWER_CHARS).text)
            .filter(|s| !s.is_empty())
    };
    let reason = field("reason");

    match kind {
        AdviceKind::RiskClass => match field("risk_class") {
            None => unavailable("the advisor returned no risk_class".to_string()),
            Some(class) if class == UNCLEAR => (
                AdviceStatus::Declined {
                    why: "the advisor could not judge the issue's risk class".to_string(),
                },
                None,
                reason,
            ),
            Some(class) => match RiskClass::from_schema_str(&class) {
                // Out of enum: the schema said it could not happen, so
                // treating it as an answer would be trusting the schema
                // rather than checking it. Declined, not Unavailable — the
                // call worked, its answer is simply not usable.
                None => (
                    AdviceStatus::Declined {
                        why: format!("the advisor returned an unknown risk class '{class}'"),
                    },
                    None,
                    reason,
                ),
                Some(_) => (AdviceStatus::Answered, Some(class), reason),
            },
        },
        AdviceKind::StrategyRedirect => {
            interpret_verdict(structured, &field, reason, "proposal", "no_proposal")
        }
        AdviceKind::HumanQuestion => {
            interpret_verdict(structured, &field, reason, "question", "no_question")
        }
    }
}

/// The shared shape of the two `verdict` + payload schemas.
fn interpret_verdict(
    structured: &serde_json::Value,
    field: &dyn Fn(&str) -> Option<String>,
    reason: Option<String>,
    payload: &str,
    declined: &str,
) -> (AdviceStatus, Option<String>, Option<String>) {
    let verdict = structured.get("verdict").and_then(|v| v.as_str());
    match verdict {
        Some(v) if v == declined => (
            AdviceStatus::Declined {
                why: format!("the advisor had no {payload} to offer"),
            },
            None,
            reason,
        ),
        Some(v) if v == payload => match field(payload) {
            // The verdict claims a payload and there is none. Declined
            // rather than Unavailable, and empty rather than half-answered:
            // an empty redirect appended to rung 7's text would read as an
            // instruction with no content.
            None => (
                AdviceStatus::Declined {
                    why: format!("the advisor's {payload} was empty"),
                },
                None,
                reason,
            ),
            Some(text) => (AdviceStatus::Answered, Some(text), reason),
        },
        _ => (
            AdviceStatus::Unavailable {
                why: "the advisor returned no recognizable verdict".to_string(),
            },
            None,
            reason,
        ),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    scrub_and_bound(line.trim(), 200).text
}

// ── the three entry points ──────────────────────────────────────────────

/// Ask for an issue's dispatch-time risk class.
///
/// **Only ever called for an issue with no `risk:*` label.** Where the label
/// exists it is the answer, and paying a model to re-derive a fact the repo
/// already states would be the ladder's own anti-pattern. The caller is
/// [`super::lead::resolve_class`], which enforces that.
pub fn advise_risk_class(
    db: &Database,
    advisor: &mut dyn Advisor,
    cwd: &Path,
    issue: &IssueRef,
    title: &str,
    body: &str,
    config: &AdviceConfig,
) -> Result<Advice, MemoryError> {
    let prompt = render_risk_prompt(issue, title, body);
    run_advice(
        db,
        advisor,
        cwd,
        issue,
        AdviceKind::RiskClass,
        &prompt,
        config,
    )
}

/// Ask for a genuinely different approach to a thrashing issue.
///
/// The answer is *added to* rung 7's mechanical redirect by
/// [`super::supervise::set_redirect_proposal`], never substituted for it:
/// the mechanical text carries the binding instruction ("do not repeat this;
/// report `impossible` if you cannot name something different"), and a
/// proposal that replaced it would trade a guaranteed floor for an
/// unguaranteed improvement.
pub fn advise_strategy_redirect(
    db: &Database,
    advisor: &mut dyn Advisor,
    cwd: &Path,
    issue: &IssueRef,
    signature: &str,
    approaches: &[String],
    config: &AdviceConfig,
) -> Result<Advice, MemoryError> {
    let prompt = render_redirect_prompt(issue, signature, approaches);
    run_advice(
        db,
        advisor,
        cwd,
        issue,
        AdviceKind::StrategyRedirect,
        &prompt,
        config,
    )
}

/// Ask what to put to a human about an escalated issue.
#[allow(clippy::too_many_arguments)]
pub fn advise_human_question(
    db: &Database,
    advisor: &mut dyn Advisor,
    cwd: &Path,
    issue: &IssueRef,
    title: &str,
    body: &str,
    signature: &str,
    approaches: &[String],
    config: &AdviceConfig,
) -> Result<Advice, MemoryError> {
    let prompt = render_question_prompt(issue, title, body, signature, approaches);
    run_advice(
        db,
        advisor,
        cwd,
        issue,
        AdviceKind::HumanQuestion,
        &prompt,
        config,
    )
}

// ── storage: the tenth drawer kind ──────────────────────────────────────

/// The JSON actually written to the drawer.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdviceBody {
    issue: String,
    repo: String,
    issue_number: u64,
    kind: AdviceKind,
    status: AdviceStatus,
    answer: Option<String>,
    reason: Option<String>,
    total_cost_usd: Option<f64>,
    model: Option<String>,
    record_id: String,
    recorded_at: String,
}

/// One recorded advisor call, read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedAdvice {
    pub issue: String,
    pub kind: AdviceKind,
    pub status: AdviceStatus,
    pub answer: Option<String>,
    pub reason: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub recorded_at: String,
}

/// Append one advisor call to the issue's lineage.
///
/// **Append-only, no `logical_key`** — the module doc's `logical_key` hazard,
/// for the reason attempts, reviews and merges are append-only: two
/// classifications of the same issue are two facts, and the earlier one is
/// what a later disagreement is read against.
pub fn record_advice(db: &Database, advice: &Advice) -> Result<String, MemoryError> {
    validate_repo(&advice.issue.repo)?;

    let body = AdviceBody {
        issue: advice.issue.canonical(),
        repo: advice.issue.repo.clone(),
        issue_number: advice.issue.number,
        kind: advice.kind,
        status: advice.status.clone(),
        answer: advice
            .answer
            .as_deref()
            .map(|text| scrub_and_bound(text, MAX_LINEAGE_FIELD_CHARS).text),
        reason: advice
            .reason
            .as_deref()
            .map(|text| scrub_and_bound(text, MAX_LINEAGE_FIELD_CHARS).text),
        total_cost_usd: advice.total_cost_usd,
        model: None,
        record_id: uuid::Uuid::new_v4().to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
    };

    let content = serde_json::to_string(&body)?;
    let drawer_id = crate::db::drawers::generate_id(&content, WING, ROOM);
    let issue_entity = advice.issue.entity_name();
    let embedding = zero_embedding();

    db.with_transaction(|tx| {
        Database::insert_drawer_tx(
            tx, &drawer_id, &content, &embedding, WING, ROOM, "", ADDED_BY,
        )?;
        KnowledgeGraph::add_triple_tx(
            tx,
            &issue_entity,
            ISSUE_ENTITY_TYPE,
            HAS_ADVICE_PREDICATE,
            &drawer_id,
            ADVICE_ENTITY_TYPE,
            None,
            1.0,
            None,
        )?;
        Database::wal_log_tx(
            tx,
            "autopilot_record_advice",
            &json!({
                "drawer_id": &drawer_id,
                "issue": &body.issue,
                "kind": body.kind.as_str(),
            }),
            None,
        )?;
        Ok(())
    })?;

    Ok(drawer_id)
}

/// Every advisor call recorded for an issue, oldest first.
pub fn advice_for_issue(
    db: &Database,
    issue: &IssueRef,
) -> Result<Vec<RecordedAdvice>, MemoryError> {
    let kg = KnowledgeGraph::new(db);
    let entity = match kg.resolve_entity(&issue.entity_name(), Some(ISSUE_ENTITY_TYPE)) {
        Ok(entity) => entity,
        Err(MemoryError::NotFound(_)) => return Ok(Vec::new()),
        Err(other) => return Err(other),
    };

    let triples =
        kg.query_entity_current_with_predicate(&entity.id, HAS_ADVICE_PREDICATE, MAX_ISSUE_EDGES)?;
    let mut records = Vec::new();
    for triple in triples {
        let Some(object_entity) = kg.get_entity(&triple.object)? else {
            continue;
        };
        let Some(drawer) = db.get_drawer(&object_entity.name)? else {
            continue;
        };
        let body: AdviceBody = serde_json::from_str(&drawer.content)?;
        records.push(RecordedAdvice {
            issue: body.issue,
            kind: body.kind,
            status: body.status,
            answer: body.answer,
            reason: body.reason,
            total_cost_usd: body.total_cost_usd,
            recorded_at: body.recorded_at,
        });
    }
    records.sort_by(|a, b| a.recorded_at.cmp(&b.recorded_at));
    Ok(records)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// An [`Advisor`] that replays a fixed script and records what it was
    /// asked. The whole reason [`Advisor`] is a trait: every test in this
    /// rung runs the real policy layer against a real database and spends
    /// nothing.
    pub(crate) struct ScriptedAdvisor {
        pub(crate) seen: Vec<AdviceSpec>,
        responses: std::collections::VecDeque<Result<AdviceOutput, MemoryError>>,
    }

    impl ScriptedAdvisor {
        pub(crate) fn new(responses: Vec<Result<AdviceOutput, MemoryError>>) -> Self {
            Self {
                seen: Vec::new(),
                responses: responses.into(),
            }
        }

        /// An advisor that always fails to launch — the "not available"
        /// world every caller must survive unchanged.
        pub(crate) fn broken() -> Self {
            Self::new(Vec::new())
        }

        /// One successful call returning `structured_output`.
        pub(crate) fn answering(structured: &str, cost: f64) -> Self {
            Self::new(vec![Ok(AdviceOutput {
                stdout: envelope_json(structured, cost),
                stderr: String::new(),
                success: true,
            })])
        }
    }

    pub(crate) fn envelope_json(structured: &str, cost: f64) -> String {
        format!(
            r#"{{"type":"result","subtype":"success","is_error":false,
                "duration_ms":900,"num_turns":1,"total_cost_usd":{cost},
                "session_id":"11111111-2222-3333-4444-555555555555",
                "structured_output":{structured}}}"#
        )
    }

    impl Advisor for ScriptedAdvisor {
        fn advise(&mut self, _cwd: &Path, spec: &AdviceSpec) -> Result<AdviceOutput, MemoryError> {
            self.seen.push(spec.clone());
            match self.responses.pop_front() {
                Some(response) => response,
                None => Err(MemoryError::NotFound(
                    "failed to launch advisor: no such file".to_string(),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{envelope_json, ScriptedAdvisor};
    use super::*;

    const REPO: &str = "ironrace/ironmem";

    fn issue() -> IssueRef {
        IssueRef::new(REPO, 42)
    }

    fn enabled() -> AdviceConfig {
        AdviceConfig {
            enabled: true,
            ..AdviceConfig::default()
        }
    }

    fn cwd() -> &'static Path {
        Path::new(".")
    }

    // ── argv: every flag here is load-bearing and silent if absent ──────

    fn argv_for(kind: AdviceKind) -> Vec<String> {
        build_argv(&AdviceSpec {
            kind,
            model: "claude-opus-5".to_string(),
            prompt: "classify this".to_string(),
            max_budget_usd: 0.25,
        })
    }

    #[test]
    fn an_advisor_call_never_carries_bypass_permissions() {
        // The one Autopilot `claude` invocation without it. A toolless call
        // cannot use a permission, so granting bypass would buy nothing and
        // hand a judgment step the IC's blast radius.
        let argv = argv_for(AdviceKind::RiskClass);
        assert!(!argv.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn an_advisor_call_has_no_tools_and_cannot_hang_on_a_prompt() {
        let argv = argv_for(AdviceKind::RiskClass);
        let tools = argv.iter().position(|a| a == "--tools").expect("--tools");
        assert_eq!(argv[tools + 1], "", "\"\" disables every built-in tool");
        assert_eq!(
            tools + 2,
            argv.len(),
            "--tools is variadic, so nothing may follow it"
        );

        let prompts = argv
            .iter()
            .position(|a| a == "--permission-prompts")
            .expect("--permission-prompts");
        assert_eq!(argv[prompts + 1], "none");
    }

    #[test]
    fn an_advisor_call_is_one_turn_and_leaves_no_session() {
        let argv = argv_for(AdviceKind::StrategyRedirect);
        let turns = argv.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(argv[turns + 1], "1");
        assert!(argv.iter().any(|a| a == "--no-session-persistence"));
        // No session id and no name: rung 7's registry read must never see
        // an advisor call and mistake it for an IC.
        assert!(!argv.iter().any(|a| a == "--session-id" || a == "--resume"));
        assert!(!argv.iter().any(|a| a == "--name"));
    }

    #[test]
    fn each_kind_carries_its_own_schema_inline() {
        for kind in [
            AdviceKind::RiskClass,
            AdviceKind::StrategyRedirect,
            AdviceKind::HumanQuestion,
        ] {
            let argv = argv_for(kind);
            let at = argv.iter().position(|a| a == "--json-schema").unwrap();
            assert_eq!(argv[at + 1], kind.schema());
            // Inline JSON, not a path — rung 0's measurement.
            assert!(argv[at + 1].starts_with('{'));
            assert!(
                serde_json::from_str::<serde_json::Value>(&argv[at + 1]).is_ok(),
                "{} schema must be valid JSON",
                kind.as_str()
            );
        }
    }

    #[test]
    fn every_schema_offers_a_way_to_decline() {
        // Without one, a constrained enum turns uncertainty into a
        // confident wrong answer — and for the risk class that is the one
        // answer that can auto-merge.
        assert!(RISK_CLASS_JSON_SCHEMA.contains("\"unclear\""));
        assert!(REDIRECT_JSON_SCHEMA.contains("\"no_proposal\""));
        assert!(QUESTION_JSON_SCHEMA.contains("\"no_question\""));
    }

    #[test]
    fn the_risk_schema_lists_exactly_the_reviewers_own_classes() {
        // If these ever drift, the advisor answers in a vocabulary
        // `decide_merge` cannot read, and every classification silently
        // becomes a hold. Pinned against `RiskClass` itself.
        let schema: serde_json::Value = serde_json::from_str(RISK_CLASS_JSON_SCHEMA).unwrap();
        let listed: Vec<String> = schema["properties"]["risk_class"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        for class in listed.iter().filter(|c| c.as_str() != UNCLEAR) {
            assert!(
                RiskClass::from_schema_str(class).is_some(),
                "{class} is not a RiskClass"
            );
        }
        assert_eq!(listed.len(), 9, "eight classes plus `unclear`");
    }

    // ── the envelope ────────────────────────────────────────────────────

    #[test]
    fn envelope_agrees_with_the_dispatch_parser() {
        // This module reads a narrowed view of the same measured envelope
        // rung 2 parses. One sample, both readings, so the narrowing cannot
        // drift away from the shape that was actually measured.
        let sample = envelope_json(r#"{"verdict":"met","reason":"done"}"#, 0.042);
        let dispatch = super::super::dispatch::parse_dispatch_output(&sample).unwrap();
        let envelope: AdviceEnvelope = serde_json::from_str(&sample).unwrap();

        assert_eq!(envelope.total_cost_usd, Some(dispatch.total_cost_usd));
        assert_eq!(envelope.is_error, dispatch.is_error);
        assert!(envelope.structured_output.is_some());
    }

    // ── configuration ───────────────────────────────────────────────────

    #[test]
    fn the_advisor_is_off_by_default() {
        assert!(!AdviceConfig::default().enabled);
    }

    #[test]
    fn a_config_that_could_never_authorize_a_call_is_refused() {
        let mut cfg = enabled();
        cfg.max_budget_usd = 100.0;
        cfg.daily_budget_usd = 25.0;
        assert!(cfg.validate().is_err());

        let mut cfg = enabled();
        cfg.max_calls_per_day = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = enabled();
        cfg.model = "  ".to_string();
        assert!(cfg.validate().is_err());

        let mut cfg = enabled();
        cfg.max_budget_usd = f64::NAN;
        assert!(cfg.validate().is_err(), "NaN would make every check false");
    }

    #[test]
    fn a_disabled_advisor_is_validated_anyway() {
        let cfg = AdviceConfig {
            max_calls_per_day: 0,
            ..Default::default()
        };
        assert!(!cfg.enabled);
        assert!(cfg.validate().is_err());
    }

    // ── authorization ───────────────────────────────────────────────────

    #[test]
    fn a_disabled_advisor_refuses_without_spending_or_recording() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::broken();
        let advice = run_advice(
            &db,
            &mut advisor,
            cwd(),
            &issue(),
            AdviceKind::RiskClass,
            "prompt",
            &AdviceConfig::default(),
        )
        .unwrap();

        assert!(matches!(advice.status, AdviceStatus::Refused { .. }));
        assert!(advisor.seen.is_empty(), "nothing was launched");
        assert!(super::super::budget::get_daily_spend(&db, &today_utc())
            .unwrap()
            .is_none());
        assert!(advice_for_issue(&db, &issue()).unwrap().is_empty());
    }

    #[test]
    fn the_dollar_predicate_is_spelled_exactly_as_the_runners() {
        // `spent + max_budget_usd > daily`, not `spent >= daily` — otherwise
        // the advisor authorizes a call the ledger cannot actually afford.
        let db = Database::open_in_memory().unwrap();
        let mut cfg = enabled();
        cfg.max_budget_usd = 0.25;
        cfg.daily_budget_usd = 1.00;

        super::super::budget::accumulate_daily_spend(&db, &today_utc(), 0.80).unwrap();
        assert!(
            refusal(&db, &cfg).unwrap().is_some(),
            "0.80 + 0.25 > 1.00 refuses, even though 0.80 < 1.00"
        );
    }

    #[test]
    fn the_call_ceiling_stops_a_chatty_advisor_the_dollar_ceiling_would_not_see() {
        let db = Database::open_in_memory().unwrap();
        let mut cfg = enabled();
        cfg.max_calls_per_day = 2;

        for _ in 0..2 {
            super::super::budget::record_advice_call(&db, &today_utc(), Some(0.001)).unwrap();
        }
        let why = refusal(&db, &cfg).unwrap().expect("refused");
        assert!(why.contains("call ceiling"), "{why}");
    }

    // ── the three answers ───────────────────────────────────────────────

    #[test]
    fn an_answered_risk_class_parses_and_is_billed_in_dollars() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::answering(
            r#"{"risk_class":"documentation","reason":"README only"}"#,
            0.03,
        );
        let advice = advise_risk_class(
            &db,
            &mut advisor,
            cwd(),
            &issue(),
            "Fix a typo",
            "The README says teh",
            &enabled(),
        )
        .unwrap();

        assert_eq!(advice.status, AdviceStatus::Answered);
        assert_eq!(advice.risk_class(), Some(RiskClass::Documentation));
        assert_eq!(advice.total_cost_usd, Some(0.03));

        let ledger = super::super::budget::get_daily_spend(&db, &today_utc())
            .unwrap()
            .unwrap();
        assert!((ledger.total_cost_usd - 0.03).abs() < 1e-9);
        assert_eq!(ledger.advice_call_count, 1);
        assert_eq!(ledger.unpriced_advice_count, 0);
    }

    #[test]
    fn an_unclear_risk_class_declines_rather_than_guessing() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::answering(
            r#"{"risk_class":"unclear","reason":"the issue is two sentences"}"#,
            0.01,
        );
        let advice = advise_risk_class(
            &db,
            &mut advisor,
            cwd(),
            &issue(),
            "do the thing",
            "",
            &enabled(),
        )
        .unwrap();

        assert!(matches!(advice.status, AdviceStatus::Declined { .. }));
        assert_eq!(advice.risk_class(), None);
        assert_eq!(advice.answered(), None);
        assert!(advice.reason.is_some(), "the stated reason is kept");
    }

    #[test]
    fn a_risk_class_outside_the_enum_is_never_taken_as_an_answer() {
        // The enum lives in the model's output schema. A guard that assumes
        // the schema was honoured reports rather than binds — rung 5's
        // lesson 17.
        let db = Database::open_in_memory().unwrap();
        let mut advisor =
            ScriptedAdvisor::answering(r#"{"risk_class":"banana","reason":"why not"}"#, 0.01);
        let advice =
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();

        assert!(matches!(advice.status, AdviceStatus::Declined { .. }));
        assert_eq!(advice.answered(), None);
        assert_eq!(advice.risk_class(), None);
    }

    #[test]
    fn a_redirect_proposal_and_its_decline_both_parse() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::answering(
            r#"{"verdict":"proposal","proposal":"The failing assertion is in the fixture, not the code under test. Read the fixture first.","reason":"same error three times"}"#,
            0.02,
        );
        let advice = advise_strategy_redirect(
            &db,
            &mut advisor,
            cwd(),
            &issue(),
            "assertion failed: left == right",
            &["rewrote the parser".to_string()],
            &enabled(),
        )
        .unwrap();
        assert_eq!(advice.status, AdviceStatus::Answered);
        assert!(advice.answered().unwrap().contains("fixture"));

        let mut advisor = ScriptedAdvisor::answering(
            r#"{"verdict":"no_proposal","proposal":"","reason":"nothing to go on"}"#,
            0.01,
        );
        let advice = advise_strategy_redirect(
            &db,
            &mut advisor,
            cwd(),
            &issue(),
            "assertion failed",
            &[],
            &enabled(),
        )
        .unwrap();
        assert!(matches!(advice.status, AdviceStatus::Declined { .. }));
        assert_eq!(advice.answered(), None);
    }

    #[test]
    fn a_verdict_claiming_a_payload_that_is_empty_declines() {
        // An empty proposal appended to rung 7's redirect would read as an
        // instruction with no content.
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::answering(
            r#"{"verdict":"proposal","proposal":"   ","reason":"oops"}"#,
            0.01,
        );
        let advice =
            advise_strategy_redirect(&db, &mut advisor, cwd(), &issue(), "sig", &[], &enabled())
                .unwrap();
        assert!(matches!(advice.status, AdviceStatus::Declined { .. }));
    }

    #[test]
    fn a_drafted_question_parses() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::answering(
            r#"{"verdict":"question","question":"Should the migration keep soft-deleted rows?","reason":"the issue does not say"}"#,
            0.02,
        );
        let advice = advise_human_question(
            &db,
            &mut advisor,
            cwd(),
            &issue(),
            "Migrate the table",
            "Move rows to the new schema",
            "migration failed: constraint violation",
            &["dropped the constraint".to_string()],
            &enabled(),
        )
        .unwrap();

        assert_eq!(advice.status, AdviceStatus::Answered);
        assert!(advice.answered().unwrap().starts_with("Should"));
    }

    // ── every degradation ───────────────────────────────────────────────

    #[test]
    fn a_spawn_failure_banks_nothing_and_answers_nothing() {
        // Rung 4 drew this line and rung 5 failed to carry it over: an
        // `Err` from a runner happens before the process exists, so nothing
        // was spent.
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::broken();
        let advice =
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();

        assert!(matches!(advice.status, AdviceStatus::Unavailable { .. }));
        assert!(super::super::budget::get_daily_spend(&db, &today_utc())
            .unwrap()
            .is_none());
        // Recorded, though: an advisor that cannot launch is worth seeing.
        assert_eq!(advice_for_issue(&db, &issue()).unwrap().len(), 1);
    }

    #[test]
    fn a_non_zero_exit_refuses_the_answer_even_when_stdout_parsed() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::new(vec![Ok(AdviceOutput {
            stdout: envelope_json(r#"{"risk_class":"documentation","reason":"sure"}"#, 0.02),
            stderr: "killed\n".to_string(),
            success: false,
        })]);
        let advice =
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();

        assert!(matches!(advice.status, AdviceStatus::Unavailable { .. }));
        assert_eq!(advice.risk_class(), None);
        // It ran, so its price is banked even though its answer is refused.
        let ledger = super::super::budget::get_daily_spend(&db, &today_utc())
            .unwrap()
            .unwrap();
        assert!((ledger.total_cost_usd - 0.02).abs() < 1e-9);
    }

    #[test]
    fn unparseable_stdout_is_unavailable_and_banks_an_unpriced_call() {
        // The process ran. Its price is unknown, and unknown is never $0.00.
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::new(vec![Ok(AdviceOutput {
            stdout: "not json at all".to_string(),
            stderr: String::new(),
            success: true,
        })]);
        let advice =
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();

        assert!(matches!(advice.status, AdviceStatus::Unavailable { .. }));
        assert_eq!(advice.total_cost_usd, None);
        let ledger = super::super::budget::get_daily_spend(&db, &today_utc())
            .unwrap()
            .unwrap();
        assert_eq!(ledger.unpriced_advice_count, 1);
        assert_eq!(ledger.total_cost_usd, 0.0);
        assert_eq!(
            ledger.unpriced_dispatch_count, 0,
            "a flaky advisor must never stop IC dispatches"
        );
    }

    #[test]
    fn a_missing_structured_output_is_never_read_as_an_answer() {
        // The 6a guard, third module to apply it: rung 0 measured that the
        // base envelope carries no verdict of its own.
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::new(vec![Ok(AdviceOutput {
            stdout: r#"{"is_error":false,"total_cost_usd":0.01,"result":"documentation"}"#
                .to_string(),
            stderr: String::new(),
            success: true,
        })]);
        let advice =
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();

        assert!(matches!(advice.status, AdviceStatus::Unavailable { .. }));
        assert_eq!(advice.risk_class(), None);
    }

    #[test]
    fn an_error_result_is_unavailable() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::new(vec![Ok(AdviceOutput {
            stdout: r#"{"is_error":true,"total_cost_usd":0.01,
                "structured_output":{"risk_class":"documentation","reason":"x"}}"#
                .to_string(),
            stderr: String::new(),
            success: true,
        })]);
        let advice =
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();
        assert!(matches!(advice.status, AdviceStatus::Unavailable { .. }));
    }

    #[test]
    fn an_unrecognized_verdict_is_unavailable() {
        let db = Database::open_in_memory().unwrap();
        let mut advisor =
            ScriptedAdvisor::answering(r#"{"verdict":"maybe","proposal":"try harder"}"#, 0.01);
        let advice =
            advise_strategy_redirect(&db, &mut advisor, cwd(), &issue(), "sig", &[], &enabled())
                .unwrap();
        assert!(matches!(advice.status, AdviceStatus::Unavailable { .. }));
        assert_eq!(advice.answered(), None);
    }

    // ── prompts ─────────────────────────────────────────────────────────

    #[test]
    fn prompts_are_scrubbed_and_bounded() {
        let long = "x".repeat(50_000);
        let prompt = render_risk_prompt(&issue(), "title", &long);
        assert!(prompt.chars().count() <= MAX_ADVICE_PROMPT_CHARS);

        let secret = "here is my key sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKK";
        let prompt = render_risk_prompt(&issue(), "title", secret);
        assert!(
            !prompt.contains("sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKK"),
            "issue text is written by anyone who can open an issue"
        );
    }

    #[test]
    fn the_risk_prompt_says_what_a_wrong_answer_costs() {
        // The fact that makes declining the right move. If this sentence
        // goes, the model has no reason to prefer `unclear` to a guess.
        let prompt = render_risk_prompt(&issue(), "t", "b");
        assert!(prompt.contains("unclear"));
        assert!(prompt.contains("merged"));
    }

    #[test]
    fn only_the_newest_approaches_are_quoted_newest_first() {
        let approaches: Vec<String> = (1..=8).map(|n| format!("approach {n}")).collect();
        let quoted = quote_approaches(&approaches);
        assert!(quoted.starts_with("- approach 8"));
        assert!(!quoted.contains("approach 3"), "bounded to the newest few");
        assert_eq!(quoted.lines().count(), MAX_QUOTED_APPROACHES);

        assert_eq!(quote_approaches(&[]), "(none recorded)");
    }

    // ── storage ─────────────────────────────────────────────────────────

    #[test]
    fn two_calls_on_one_issue_produce_two_drawers() {
        // Append-only, no `logical_key` — the module doc's hazard. A later
        // classification must not destroy the earlier one it disagrees with.
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::new(vec![
            Ok(AdviceOutput {
                stdout: envelope_json(r#"{"risk_class":"documentation","reason":"one"}"#, 0.01),
                stderr: String::new(),
                success: true,
            }),
            Ok(AdviceOutput {
                stdout: envelope_json(r#"{"risk_class":"logic","reason":"two"}"#, 0.01),
                stderr: String::new(),
                success: true,
            }),
        ]);
        for _ in 0..2 {
            advise_risk_class(&db, &mut advisor, cwd(), &issue(), "t", "b", &enabled()).unwrap();
        }

        let records = advice_for_issue(&db, &issue()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].answer.as_deref(), Some("documentation"));
        assert_eq!(records[1].answer.as_deref(), Some("logic"));
    }

    #[test]
    fn an_issue_with_no_advice_reads_back_empty_rather_than_erroring() {
        let db = Database::open_in_memory().unwrap();
        assert!(advice_for_issue(&db, &issue()).unwrap().is_empty());
    }

    #[test]
    fn a_bad_repo_is_refused_before_anything_runs() {
        // `validate_repo` checks the string is storable, not that it looks
        // like `owner/repo` — the same contract every other module in this
        // subsystem validates against. What matters here is the *ordering*:
        // it runs before the advisor is launched, so a call that cannot be
        // recorded is never paid for.
        let db = Database::open_in_memory().unwrap();
        let mut advisor = ScriptedAdvisor::broken();
        let bad = IssueRef::new("owner/repo\u{0}", 1);
        assert!(run_advice(
            &db,
            &mut advisor,
            cwd(),
            &bad,
            AdviceKind::RiskClass,
            "prompt",
            &enabled(),
        )
        .is_err());
        assert!(advisor.seen.is_empty());
    }
}
