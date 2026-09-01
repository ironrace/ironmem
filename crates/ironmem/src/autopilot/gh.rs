//! The GitHub CLI surface — build-ladder rung 6.
//!
//! Every GitHub write Autopilot performs goes through `gh`, and every one of
//! them goes through this module. It is deliberately thin: argv construction
//! and response parsing, both pure and both unit-tested, behind one
//! [`GhRunner`] trait so [`super::merge`] and [`super::labels`] can be tested
//! end-to-end against a real database without touching GitHub.
//!
//! # Why `gh` and not the REST API directly
//!
//! The spec settles this: "**Auto-merge is the Lead merging that PR** via `gh
//! pr merge` after reviewer PASS + matching low-risk classification. A GitHub
//! API merge, not a local push, so the deny-list stays absolute." The deny-list
//! forbids every IC from pushing a default branch without exception; routing
//! the merge through GitHub's API rather than a local `git push` is what keeps
//! that absolute rather than "absolute except for the Lead". `gh` also carries
//! the human's existing authentication, so Autopilot never handles a token.
//!
//! # The error contract
//!
//! Mirrors [`super::run::Dispatcher`]'s and [`super::review::ReviewRunner`]'s,
//! for the same reason: a failure to **start** `gh` is
//! [`MemoryError::NotFound`], and a `gh` that ran and exited non-zero is
//! **not** an `Err` at all — it is a [`GhOutput`] with `success: false`. That
//! split is load-bearing here. "The label already exists" and "HTTP 403" both
//! arrive as a non-zero exit, and only the caller knows which of them is
//! benign; collapsing them into `Err` would make [`super::labels::ensure_labels`]
//! usable exactly once per repo.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

use super::labels::AgentLabel;
use super::IssueRef;
use crate::error::MemoryError;

/// Lowercased substrings that identify `gh label create`'s "this label is
/// already here" refusal, as opposed to a real failure.
///
/// Matched against the *lowercased* concatenation of stdout and stderr, and
/// deliberately more than one phrasing: `gh` has surfaced this as both its
/// own message and a passed-through `HTTP 422` from the API, and a future
/// version could reword either. Missing a phrasing degrades safely — the
/// label is reported as an error rather than as already-present, which is
/// noisy but never wrong in the dangerous direction.
pub(crate) const LABEL_ALREADY_EXISTS_MARKERS: [&str; 3] =
    ["already exists", "already been taken", "http 422"];

/// Lowercased substrings that identify `gh api .../protection`'s "this branch
/// has no protection rules" answer, as opposed to a real failure.
///
/// Anchored to the status phrase (`http 404`) rather than to a bare `404`:
/// this is the single place in the module where a *failed* `gh` call is
/// allowed to mean "proceed with the merge", so a `404` appearing anywhere in
/// an unrelated error — a docs URL, a request id, a rate-limit body — must not
/// be enough to unlock it.
const BRANCH_UNPROTECTED_MARKERS: [&str; 2] = ["http 404", "branch not protected"];

/// Which `MemoryError` a refused `gh` call becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GhFailure {
    /// A read whose subject could not be found.
    NotFound,
    /// A write GitHub declined.
    Refused,
}

/// One `gh` invocation's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

impl GhOutput {
    /// The stdout of a successful call, or a `MemoryError` naming what failed.
    ///
    /// One wording for one failure shape. Four call sites across this module
    /// and [`super::labels`] each hand-rolled the same
    /// `"gh X failed (exit {:?}): {}"` string, which is four places for the
    /// exit code or the stderr trim to be forgotten.
    ///
    /// `kind` picks the variant because the distinction is real: a read that
    /// cannot find its subject is [`MemoryError::NotFound`], while a refused
    /// *write* is [`MemoryError::Validation`].
    pub(crate) fn require_success(&self, what: &str, kind: GhFailure) -> Result<&str, MemoryError> {
        if self.success {
            return Ok(&self.stdout);
        }
        let msg = format!(
            "{what} failed (exit {:?}): {}",
            self.code,
            self.stderr.trim()
        );
        Err(match kind {
            GhFailure::NotFound => MemoryError::NotFound(msg),
            GhFailure::Refused => MemoryError::Validation(msg),
        })
    }

    /// Whether stdout or stderr mentions any of `markers`.
    ///
    /// `markers` must already be lowercase — the haystack is lowercased here,
    /// and a mixed-case marker would silently never match. The two constants
    /// that feed this ([`LABEL_ALREADY_EXISTS_MARKERS`] and
    /// [`BRANCH_UNPROTECTED_MARKERS`]) are both asserted lowercase by test.
    pub(crate) fn mentions_any(&self, markers: &[&str]) -> bool {
        let haystack = format!("{} {}", self.stdout, self.stderr).to_lowercase();
        markers.iter().any(|marker| haystack.contains(marker))
    }

    #[cfg(test)]
    pub(crate) fn ok(stdout: &str) -> Self {
        Self {
            stdout: stdout.to_string(),
            stderr: String::new(),
            success: true,
            code: Some(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn failed(stdout: &str, stderr: &str) -> Self {
        Self {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            success: false,
            code: Some(1),
        }
    }
}

/// How the merge and label paths reach GitHub.
///
/// A trait for the same reason [`super::run::Dispatcher`] and
/// [`super::review::ReviewRunner`] are: it makes the policy layer — the merge
/// guards, the label transitions, the exhaustion summary — testable against a
/// real database without performing an irreversible GitHub write. Rung 6's
/// actions are the first in the whole ladder that a test cannot undo, so this
/// is the rung where that pattern stops being a convenience.
pub trait GhRunner {
    /// Run `gh` with `args`. A failure to **spawn** is
    /// [`MemoryError::NotFound`]; a non-zero exit is a successful call
    /// returning `success: false`.
    fn run(&mut self, args: &[String]) -> Result<GhOutput, MemoryError>;
}

/// Locate the `gh` binary on `PATH`, reusing `launcher`'s own binary
/// validation exactly as [`super::review::resolve_codex_binary`] does.
pub fn resolve_gh_binary() -> Result<PathBuf, MemoryError> {
    crate::launcher::find_on_path("gh")
}

/// The real `gh`, run in a working directory.
pub struct GhCli {
    bin: PathBuf,
    cwd: PathBuf,
}

impl GhCli {
    /// Resolve `gh` on PATH and pin the directory it runs in.
    ///
    /// The cwd matters even though every argv passes `--repo` explicitly:
    /// `gh` reads its configuration and host resolution relative to where it
    /// runs. `--repo` is still passed everywhere so the target can never be
    /// inferred from whatever happens to be checked out — Autopilot works
    /// several repos, and a cwd-inferred target would be the one mistake in
    /// this module that merges the wrong thing.
    pub fn resolve(cwd: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        Ok(Self {
            bin: resolve_gh_binary()?,
            cwd: cwd.into(),
        })
    }
}

impl GhRunner for GhCli {
    fn run(&mut self, args: &[String]) -> Result<GhOutput, MemoryError> {
        let output = Command::new(&self.bin)
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| {
                MemoryError::NotFound(format!(
                    "failed to start {} {}: {e}",
                    self.bin.display(),
                    args.join(" ")
                ))
            })?;
        Ok(GhOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
            code: output.status.code(),
        })
    }
}

// ── argv builders ───────────────────────────────────────────────────────
//
// All pure, all returning owned `Vec<String>` so a test can assert on the
// exact command line without running it. Every one passes `--repo` rather
// than relying on the working directory; see `GhCli::resolve`.

/// `gh label create <name> --repo R --color C --description D`.
pub fn label_create_argv(repo: &str, label: AgentLabel) -> Vec<String> {
    vec![
        "label".into(),
        "create".into(),
        label.as_str().into(),
        "--repo".into(),
        repo.into(),
        "--color".into(),
        label.color().into(),
        "--description".into(),
        label.description().into(),
    ]
}

/// `gh issue view N --repo R --json labels`.
pub fn issue_view_labels_argv(issue: &IssueRef) -> Vec<String> {
    vec![
        "issue".into(),
        "view".into(),
        issue.number.to_string(),
        "--repo".into(),
        issue.repo.clone(),
        "--json".into(),
        "labels".into(),
    ]
}

/// `gh issue edit N --repo R [--add-label L]... [--remove-label L]...`.
pub fn issue_edit_labels_argv(issue: &IssueRef, add: &[String], remove: &[String]) -> Vec<String> {
    let mut argv = vec![
        "issue".into(),
        "edit".into(),
        issue.number.to_string(),
        "--repo".into(),
        issue.repo.clone(),
    ];
    for label in add {
        argv.push("--add-label".into());
        argv.push(label.clone());
    }
    for label in remove {
        argv.push("--remove-label".into());
        argv.push(label.clone());
    }
    argv
}

/// `gh issue comment N --repo R --body <body>`.
///
/// The body is a single argv element rather than a here-doc on stdin because
/// [`GhRunner`] deliberately has no stdin channel — one input surface is one
/// thing to get wrong. Every body this module sends is rendered by
/// [`super::merge`] with a fixed literal prefix and bounded length, so it is
/// neither large enough to approach an argv limit nor able to begin with a
/// `-`. That is a property of the callers, not a guarantee of `--body`.
pub fn issue_comment_argv(issue: &IssueRef, body: &str) -> Vec<String> {
    vec![
        "issue".into(),
        "comment".into(),
        issue.number.to_string(),
        "--repo".into(),
        issue.repo.clone(),
        "--body".into(),
        body.into(),
    ]
}

/// The `--json` field list [`parse_pr_view`] expects, as one comma-separated
/// value. Named so the builder and the parser cannot drift apart.
pub const PR_VIEW_FIELDS: &str =
    "state,isDraft,mergeable,mergeStateStatus,baseRefName,headRefName,headRefOid,reviewDecision,url";

/// `gh pr view N --repo R --json <PR_VIEW_FIELDS>`.
pub fn pr_view_argv(repo: &str, pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        PR_VIEW_FIELDS.into(),
    ]
}

/// How a merge is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    Squash,
    Merge,
    Rebase,
}

impl MergeStrategy {
    /// The `gh pr merge` flag selecting this strategy. `gh` requires exactly
    /// one of them and errors if none is given, so this is never optional.
    pub fn as_flag(self) -> &'static str {
        match self {
            MergeStrategy::Squash => "--squash",
            MergeStrategy::Merge => "--merge",
            MergeStrategy::Rebase => "--rebase",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MergeStrategy::Squash => "squash",
            MergeStrategy::Merge => "merge",
            MergeStrategy::Rebase => "rebase",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "squash" => Some(MergeStrategy::Squash),
            "merge" => Some(MergeStrategy::Merge),
            "rebase" => Some(MergeStrategy::Rebase),
            _ => None,
        }
    }
}

impl Default for MergeStrategy {
    /// Squash, because an Autopilot PR's intermediate commits are one IC's
    /// dispatch-by-dispatch working history — lineage already records that
    /// history in a form built for reading, and replaying it onto the default
    /// branch would be the second copy.
    fn default() -> Self {
        MergeStrategy::Squash
    }
}

/// `gh pr merge N --repo R <strategy> [--delete-branch]`.
///
/// `--match-head-commit` is passed by [`super::merge`] separately, not here,
/// because it needs the SHA the reviewer actually read; see
/// [`pr_merge_argv_at`].
pub fn pr_merge_argv(
    repo: &str,
    pr_number: u64,
    strategy: MergeStrategy,
    delete_branch: bool,
) -> Vec<String> {
    let mut argv = vec![
        "pr".into(),
        "merge".into(),
        pr_number.to_string(),
        "--repo".into(),
        repo.into(),
        strategy.as_flag().into(),
    ];
    if delete_branch {
        argv.push("--delete-branch".into());
    }
    argv
}

/// [`pr_merge_argv`] plus `--match-head-commit <sha>`.
///
/// This is the merge argv Autopilot actually uses. The flag makes GitHub
/// itself refuse the merge if the PR's head has moved since `sha`, which
/// closes the window between [`super::merge`]'s own head check and the merge
/// call: a push landing in that window would otherwise merge a commit no
/// reviewer ever read, which is the one thing the spec's goal 5 forbids
/// outright. Our own check is still performed first, because it produces a
/// hold reason a human can act on rather than a `gh` error string.
pub fn pr_merge_argv_at(
    repo: &str,
    pr_number: u64,
    strategy: MergeStrategy,
    delete_branch: bool,
    head_sha: &str,
) -> Vec<String> {
    let mut argv = pr_merge_argv(repo, pr_number, strategy, delete_branch);
    argv.push("--match-head-commit".into());
    argv.push(head_sha.into());
    argv
}

/// Percent-encode one path segment of a REST URL.
///
/// Branch names are not URL-safe. Git forbids spaces, `?` and control
/// characters in a ref, but permits `/`, `#`, `%` and most punctuation — and
/// `/` is not exotic, it is `release/1.0`, which the GitHub API requires as
/// `release%2F1.0` because the endpoint takes the branch as a single
/// segment. Interpolating raw sent a *different*, well-formed request: a `#`
/// truncates the path at the fragment, so `.../branches/a#b/protection`
/// becomes a plain "get branch" call that returns 200 with an unrelated
/// body — and every field of [`ProtectionJson`] is optional, so that body
/// parses cleanly as [`BranchProtection::NoHumanApprovalRequired`]. A
/// protected branch would read as open.
///
/// Encodes everything outside RFC 3986's unreserved set rather than a
/// hand-picked list, because the set of characters git permits and URLs
/// reserve overlaps in more places than is comfortable to enumerate.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `gh api repos/{repo}/branches/{branch}/protection`.
pub fn branch_protection_argv(repo: &str, branch: &str) -> Vec<String> {
    vec![
        "api".into(),
        format!(
            "repos/{repo}/branches/{}/protection",
            encode_path_segment(branch)
        ),
    ]
}

/// `gh api repos/{repo}/rules/branches/{branch}` — the rules that actually
/// apply to a branch, repository- and organization-level alike.
///
/// The endpoint [`branch_protection_argv`]'s does not replace but must be
/// asked alongside: a branch governed only by a **ruleset** has no classic
/// protection, so the classic endpoint 404s while the branch is in fact
/// protected. It also needs only read access to the repository, where the
/// classic one needs admin.
pub fn branch_rules_argv(repo: &str, branch: &str) -> Vec<String> {
    vec![
        "api".into(),
        format!(
            "repos/{repo}/rules/branches/{}",
            encode_path_segment(branch)
        ),
    ]
}

// ── responses ───────────────────────────────────────────────────────────

/// A pull request as GitHub currently sees it.
///
/// Deserialized straight from `gh pr view --json`'s camelCase, and serialized
/// back out in snake_case for this crate's own `--json` output — hence the
/// direction-scoped `rename_all`. It previously had a field-for-field shadow
/// struct doing the rename by hand, which was a second place to forget a
/// field when the schema grows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct PrSnapshot {
    /// `OPEN`, `CLOSED` or `MERGED`, verbatim. The one required field: a
    /// response without it is not a PR view at all.
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`, verbatim.
    #[serde(default)]
    pub mergeable: String,
    /// `CLEAN`, `BLOCKED`, `BEHIND`, `DIRTY`, `UNSTABLE`, `HAS_HOOKS`,
    /// `DRAFT` or `UNKNOWN`, verbatim.
    #[serde(default)]
    pub merge_state_status: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub head_ref_name: String,
    /// The commit at the head of the PR right now. The single most important
    /// field in this struct: it is what a recorded review's head SHA is
    /// compared against.
    #[serde(default)]
    pub head_ref_oid: String,
    /// GitHub's own answer to *"does this PR satisfy the base branch's review
    /// requirement?"* — `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`,
    /// or empty when the base requires no review at all.
    ///
    /// The distinction [`BranchProtection`] cannot make. Protection describes
    /// the *rule*; this describes whether *this* PR meets it. Reading only
    /// the rule is why an approved PR on a protected branch would otherwise
    /// be refused forever — see [`super::merge`]'s protection guard.
    #[serde(default)]
    pub review_decision: String,
    #[serde(default)]
    pub url: String,
}

impl PrSnapshot {
    /// Whether GitHub reports this PR as carrying the approvals its base
    /// branch requires.
    ///
    /// Only the literal `APPROVED` counts. An empty string — schema drift, a
    /// `gh` too old to emit the field, a base with no requirement — is not an
    /// approval, so a merge that needs one holds.
    pub fn human_approved(&self) -> bool {
        self.review_decision.eq_ignore_ascii_case("approved")
    }
}

/// Parse `gh pr view --json <PR_VIEW_FIELDS>` output.
///
/// `state` is the only required field: every other one has a `#[serde(default)]`
/// so a `gh` that drops or renames a field yields an empty string rather than
/// an error. Empty is the safe value throughout — an empty `head_ref_oid`
/// matches no recorded review, and an empty `merge_state_status` is not
/// `CLEAN` — so a schema drift degrades to "hold for a human", never to a
/// merge.
pub fn parse_pr_view(stdout: &str) -> Result<PrSnapshot, MemoryError> {
    serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!("could not parse `gh pr view --json` output: {e}"))
    })
}

/// Fetch a PR's current state.
pub fn pr_snapshot(
    gh: &mut dyn GhRunner,
    repo: &str,
    pr_number: u64,
) -> Result<PrSnapshot, MemoryError> {
    let out = gh.run(&pr_view_argv(repo, pr_number))?;
    parse_pr_view(out.require_success(
        &format!("gh pr view {pr_number} on {repo}"),
        GhFailure::NotFound,
    )?)
}

#[derive(Deserialize)]
struct IssueLabelsJson {
    #[serde(default)]
    labels: Vec<LabelJson>,
}

#[derive(Deserialize)]
struct LabelJson {
    #[serde(default)]
    name: String,
}

/// Parse `gh issue view --json labels` output into bare label names.
pub fn parse_issue_labels(stdout: &str) -> Result<Vec<String>, MemoryError> {
    let raw: IssueLabelsJson = serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!(
            "could not parse `gh issue view --json labels`: {e}"
        ))
    })?;
    Ok(raw
        .labels
        .into_iter()
        .map(|l| l.name)
        .filter(|n| !n.is_empty())
        .collect())
}

/// Read an issue's current label names.
pub fn issue_labels(gh: &mut dyn GhRunner, issue: &IssueRef) -> Result<Vec<String>, MemoryError> {
    let out = gh.run(&issue_view_labels_argv(issue))?;
    parse_issue_labels(out.require_success(
        &format!("gh issue view {}", issue.canonical()),
        GhFailure::NotFound,
    )?)
}

/// Post a comment on an issue.
pub fn comment_on_issue(
    gh: &mut dyn GhRunner,
    issue: &IssueRef,
    body: &str,
) -> Result<(), MemoryError> {
    let out = gh.run(&issue_comment_argv(issue, body))?;
    out.require_success(
        &format!("gh issue comment on {}", issue.canonical()),
        GhFailure::Refused,
    )?;
    Ok(())
}

/// What a base branch's protection rules say about human approval.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "protection", rename_all = "snake_case")]
pub enum BranchProtection {
    /// The branch has no protection rules, or none that require an approving
    /// review. A merge may proceed on Autopilot's own authority.
    NoHumanApprovalRequired,
    /// The branch requires an approving review — either a count of them, or
    /// a code-owner's, or both. Autopilot cannot supply one — a review it
    /// wrote would not be independent even if the API allowed it — so the PR
    /// waits.
    HumanApprovalRequired {
        required_approving_review_count: u64,
        /// A CODEOWNERS approval is required. Independent of the count: a
        /// branch can require a code owner's review with a count of zero,
        /// and keying only on the count would read that as unprotected.
        require_code_owner_reviews: bool,
        /// Whether the rule also applies to administrators. When true there
        /// is no bypass at all, which is the case for this very repository.
        enforce_admins: bool,
    },
    /// The protection rules could not be read. **Not** the same as "no
    /// protection": the commonest cause is a token without admin scope on the
    /// repo, and treating an unreadable rule as an absent one is exactly the
    /// inversion that would merge into a protected branch.
    Unknown { detail: String },
}

impl BranchProtection {
    /// Whether a merge may proceed. `Unknown` answers `false` — the whole
    /// point of the variant.
    pub fn permits_autopilot_merge(&self) -> bool {
        matches!(self, BranchProtection::NoHumanApprovalRequired)
    }
}

#[derive(Deserialize)]
struct ProtectionJson {
    #[serde(default)]
    required_pull_request_reviews: Option<RequiredReviewsJson>,
    #[serde(default)]
    enforce_admins: Option<EnabledJson>,
}

#[derive(Deserialize)]
struct RequiredReviewsJson {
    #[serde(default)]
    required_approving_review_count: u64,
    #[serde(default)]
    require_code_owner_reviews: bool,
}

#[derive(Deserialize)]
struct EnabledJson {
    #[serde(default)]
    enabled: bool,
}

/// Parse a successful `gh api .../protection` response.
pub fn parse_branch_protection(stdout: &str) -> Result<BranchProtection, MemoryError> {
    let raw: ProtectionJson = serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!("could not parse branch protection response: {e}"))
    })?;
    let reviews = raw.required_pull_request_reviews;
    let count = reviews
        .as_ref()
        .map(|r| r.required_approving_review_count)
        .unwrap_or(0);
    let code_owners = reviews
        .as_ref()
        .map(|r| r.require_code_owner_reviews)
        .unwrap_or(false);
    // Either condition alone requires a human. They are independent in the
    // API: a branch may require a code owner's approval with a count of
    // zero, and keying only on the count would read that as unprotected —
    // Autopilot would then attempt a merge GitHub refuses, and report the
    // accurate `HumanApprovalRequired` as a bare `MergeCommandFailed`.
    if count == 0 && !code_owners {
        return Ok(BranchProtection::NoHumanApprovalRequired);
    }
    Ok(BranchProtection::HumanApprovalRequired {
        required_approving_review_count: count,
        require_code_owner_reviews: code_owners,
        enforce_admins: raw.enforce_admins.map(|e| e.enabled).unwrap_or(false),
    })
}

/// One entry of `GET /repos/{repo}/rules/branches/{branch}`'s flat array.
#[derive(Deserialize)]
struct BranchRuleJson {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    parameters: Option<PullRequestRuleJson>,
}

/// The `parameters` of a `pull_request` rule.
///
/// **`require_code_owner_review`, singular** — the rulesets API spells it
/// differently from the classic API's `require_code_owner_reviews`, so
/// reusing [`RequiredReviewsJson`] here would deserialize a required
/// code-owner review as `false` and read a protected branch as open.
#[derive(Deserialize)]
struct PullRequestRuleJson {
    #[serde(default)]
    required_approving_review_count: u64,
    #[serde(default)]
    require_code_owner_review: bool,
}

/// Parse a successful `gh api .../rules/branches/...` response.
///
/// An empty array, or one with no `pull_request` rule, is a real answer:
/// nothing requires a human review. `enforce_admins` has no analogue here —
/// rulesets express exemptions as per-ruleset bypass actors, which this does
/// not read — so it is reported `false`, which understates the constraint and
/// never overstates Autopilot's authority.
pub fn parse_branch_rules(stdout: &str) -> Result<BranchProtection, MemoryError> {
    let rules: Vec<BranchRuleJson> = serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!("could not parse branch rules response: {e}"))
    })?;
    let mut count = 0;
    let mut code_owners = false;
    for rule in rules {
        if !rule.r#type.eq_ignore_ascii_case("pull_request") {
            continue;
        }
        let Some(params) = rule.parameters else {
            continue;
        };
        // Several rulesets can apply at once; the strictest wins, which is
        // how GitHub itself composes them.
        count = count.max(params.required_approving_review_count);
        code_owners |= params.require_code_owner_review;
    }
    if count == 0 && !code_owners {
        return Ok(BranchProtection::NoHumanApprovalRequired);
    }
    Ok(BranchProtection::HumanApprovalRequired {
        required_approving_review_count: count,
        require_code_owner_reviews: code_owners,
        enforce_admins: false,
    })
}

/// Read a base branch's protection rules.
///
/// # Why a 404 is not an answer on its own
///
/// GitHub returns `404 Not Found` from the *classic* protection endpoint for
/// a branch with no classic protection — and also for a branch protected
/// entirely by a **ruleset**, which is the modern mechanism and invisible to
/// that endpoint. Reading the 404 as "unprotected", as this once did, is
/// therefore wrong in exactly the repositories that adopted the newer
/// feature: Autopilot would proceed to `gh pr merge` against a branch
/// requiring a human approval.
///
/// So a 404 asks the second question rather than concluding. The rules
/// endpoint reports every rule in force, from repository and organization
/// rulesets alike, and needs only read access where the classic endpoint
/// needs admin — so it is both the more accurate answer and the one more
/// tokens can obtain.
///
/// Everything else holds. A `403` from a token without admin scope, a
/// network failure, an unparseable body, or a rules lookup that itself fails
/// all become [`BranchProtection::Unknown`]. No failed `gh` call means
/// "proceed" any more: the only route to
/// [`BranchProtection::NoHumanApprovalRequired`] is an endpoint that
/// answered.
pub fn branch_protection(
    gh: &mut dyn GhRunner,
    repo: &str,
    branch: &str,
) -> Result<BranchProtection, MemoryError> {
    let out = gh.run(&branch_protection_argv(repo, branch))?;
    if out.success {
        return parse_branch_protection(&out.stdout);
    }
    if out.mentions_any(&BRANCH_UNPROTECTED_MARKERS) {
        return branch_rules(gh, repo, branch);
    }
    Ok(BranchProtection::Unknown {
        detail: format!(
            "gh api branch protection for {repo}@{branch} exited {:?}: {}",
            out.code,
            out.stderr.trim()
        ),
    })
}

/// Read the rulesets in force on a branch.
///
/// Reached only when the classic endpoint reported no classic protection.
/// A failure here is [`BranchProtection::Unknown`] and holds the PR: the
/// classic 404 has already told us nothing, so an unanswered rules lookup
/// leaves the question genuinely open.
fn branch_rules(
    gh: &mut dyn GhRunner,
    repo: &str,
    branch: &str,
) -> Result<BranchProtection, MemoryError> {
    let out = gh.run(&branch_rules_argv(repo, branch))?;
    if out.success {
        return parse_branch_rules(&out.stdout);
    }
    Ok(BranchProtection::Unknown {
        detail: format!(
            "{repo}@{branch} has no classic branch protection, and the ruleset lookup \
exited {:?}: {}",
            out.code,
            out.stderr.trim()
        ),
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A [`GhRunner`] that replays a fixed script and records what it was
    /// asked to run. The whole reason [`GhRunner`] is a trait.
    pub(crate) struct ScriptedGh {
        pub(crate) seen: Vec<Vec<String>>,
        responses: std::collections::VecDeque<Result<GhOutput, MemoryError>>,
    }

    impl ScriptedGh {
        pub(crate) fn new(responses: Vec<Result<GhOutput, MemoryError>>) -> Self {
            Self {
                seen: Vec::new(),
                responses: responses.into(),
            }
        }
    }

    impl GhRunner for ScriptedGh {
        fn run(&mut self, args: &[String]) -> Result<GhOutput, MemoryError> {
            self.seen.push(args.to_vec());
            self.responses.pop_front().unwrap_or_else(|| {
                panic!(
                    "ScriptedGh ran out of responses on call {}: {}",
                    self.seen.len(),
                    args.join(" ")
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::ScriptedGh;
    use super::*;

    fn issue() -> IssueRef {
        IssueRef::new("owner/repo", 42)
    }

    // ── argv ────────────────────────────────────────────────────────────

    #[test]
    fn every_argv_targets_the_repo_explicitly() {
        // A cwd-inferred target is the one mistake in this module that merges
        // the wrong thing.
        let argvs = vec![
            label_create_argv("owner/repo", AgentLabel::Ready),
            issue_view_labels_argv(&issue()),
            issue_edit_labels_argv(&issue(), &["agent:ready".into()], &[]),
            issue_comment_argv(&issue(), "hello"),
            pr_view_argv("owner/repo", 7),
            pr_merge_argv("owner/repo", 7, MergeStrategy::Squash, true),
        ];
        for argv in argvs {
            assert!(
                argv.contains(&"--repo".to_string()),
                "argv must name the repo: {argv:?}"
            );
            assert!(
                argv.contains(&"owner/repo".to_string()),
                "argv must name the repo: {argv:?}"
            );
        }
    }

    #[test]
    fn the_pr_view_field_list_covers_every_field_the_parser_reads() {
        // The builder and the parser drift apart silently otherwise: a field
        // dropped from the request parses as `default` rather than erroring.
        for field in [
            "state",
            "isDraft",
            "mergeable",
            "mergeStateStatus",
            "baseRefName",
            "headRefName",
            "headRefOid",
            "url",
        ] {
            assert!(
                PR_VIEW_FIELDS.split(',').any(|f| f == field),
                "{field} must be requested"
            );
        }
    }

    #[test]
    fn a_merge_argv_always_names_exactly_one_strategy() {
        for strategy in [
            MergeStrategy::Squash,
            MergeStrategy::Merge,
            MergeStrategy::Rebase,
        ] {
            let argv = pr_merge_argv("owner/repo", 7, strategy, false);
            let flags = argv
                .iter()
                .filter(|a| ["--squash", "--merge", "--rebase"].contains(&a.as_str()))
                .count();
            assert_eq!(flags, 1, "gh requires exactly one: {argv:?}");
            assert!(!argv.contains(&"--delete-branch".to_string()));
        }
    }

    #[test]
    fn the_merge_argv_pins_the_head_commit() {
        // Closes the window between our head check and the merge call: a push
        // landing in it would merge a commit no reviewer read.
        let argv = pr_merge_argv_at("owner/repo", 7, MergeStrategy::Squash, true, "abc123");
        let idx = argv
            .iter()
            .position(|a| a == "--match-head-commit")
            .expect("the head commit must be pinned");
        assert_eq!(argv[idx + 1], "abc123");
        assert!(argv.contains(&"--delete-branch".to_string()));
    }

    #[test]
    fn merge_strategy_parses_case_insensitively_and_rejects_junk() {
        assert_eq!(MergeStrategy::parse("SQUASH"), Some(MergeStrategy::Squash));
        assert_eq!(
            MergeStrategy::parse(" rebase "),
            Some(MergeStrategy::Rebase)
        );
        assert_eq!(MergeStrategy::parse("fast-forward"), None);
        assert_eq!(MergeStrategy::default(), MergeStrategy::Squash);
    }

    #[test]
    fn label_edit_argv_emits_one_flag_per_label() {
        let argv = issue_edit_labels_argv(
            &issue(),
            &["agent:exhausted".into()],
            &["agent:ready".into(), "agent:blocked".into()],
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--add-label").count(),
            1,
            "{argv:?}"
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--remove-label").count(),
            2,
            "{argv:?}"
        );
    }

    #[test]
    fn the_protection_argv_is_the_rest_path_for_the_base_branch() {
        assert_eq!(
            branch_protection_argv("owner/repo", "main"),
            vec![
                "api".to_string(),
                "repos/owner/repo/branches/main/protection".to_string()
            ]
        );
    }

    // ── parsing ─────────────────────────────────────────────────────────

    #[test]
    fn a_pr_view_response_parses_every_field() {
        let snap = parse_pr_view(
            r#"{"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE",
                "mergeStateStatus":"CLEAN","baseRefName":"main",
                "headRefName":"autopilot/owner-repo-42","headRefOid":"deadbeef",
                "reviewDecision":"APPROVED",
                "url":"https://github.com/owner/repo/pull/7"}"#,
        )
        .unwrap();
        assert_eq!(snap.state, "OPEN");
        assert!(!snap.is_draft);
        assert_eq!(snap.head_ref_oid, "deadbeef");
        assert_eq!(snap.base_ref_name, "main");
        assert!(snap.human_approved());
    }

    #[test]
    fn a_missing_field_degrades_to_empty_not_to_an_error() {
        // Schema drift in `gh` must fail toward "hold", not toward a merge:
        // an empty head oid matches no recorded review and an empty merge
        // state is not CLEAN.
        let snap = parse_pr_view(r#"{"state":"OPEN"}"#).unwrap();
        assert_eq!(snap.head_ref_oid, "");
        assert_eq!(snap.merge_state_status, "");
        assert_eq!(snap.mergeable, "");
        assert!(
            !snap.human_approved(),
            "a missing reviewDecision is not an approval"
        );
    }

    #[test]
    fn only_the_literal_approved_counts_as_an_approval() {
        for decision in ["REVIEW_REQUIRED", "CHANGES_REQUESTED", ""] {
            let snap = parse_pr_view(&format!(
                r#"{{"state":"OPEN","reviewDecision":"{decision}"}}"#
            ))
            .unwrap();
            assert!(!snap.human_approved(), "{decision:?}");
        }
        let snap = parse_pr_view(r#"{"state":"OPEN","reviewDecision":"approved"}"#).unwrap();
        assert!(snap.human_approved(), "GitHub's casing is not a contract");
    }

    #[test]
    fn the_pr_view_asks_for_the_review_decision() {
        // Without the field the snapshot's `reviewDecision` is always empty,
        // and an approved PR on a protected branch would hold forever.
        assert!(
            PR_VIEW_FIELDS.contains("reviewDecision"),
            "{PR_VIEW_FIELDS}"
        );
    }

    #[test]
    fn unparseable_pr_view_output_is_an_error() {
        assert!(parse_pr_view("not json").is_err());
    }

    #[test]
    fn issue_labels_parse_to_bare_names() {
        let names = parse_issue_labels(
            r#"{"labels":[{"name":"agent:ready","color":"0e8a16"},{"name":"bug"}]}"#,
        )
        .unwrap();
        assert_eq!(names, vec!["agent:ready".to_string(), "bug".to_string()]);
    }

    #[test]
    fn an_issue_with_no_labels_parses_to_an_empty_list() {
        assert!(parse_issue_labels(r#"{"labels":[]}"#).unwrap().is_empty());
        assert!(parse_issue_labels(r#"{}"#).unwrap().is_empty());
    }

    // ── branch protection ───────────────────────────────────────────────

    #[test]
    fn a_branch_requiring_an_approval_blocks_autopilot() {
        let p = parse_branch_protection(
            r#"{"required_pull_request_reviews":{"required_approving_review_count":1},
                "enforce_admins":{"enabled":true}}"#,
        )
        .unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 1,
                require_code_owner_reviews: false,
                enforce_admins: true,
            }
        );
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn this_repositorys_own_protection_body_parses_as_requiring_a_human() {
        // Captured verbatim from `gh api repos/ironrace/ironmem/branches/
        // main/protection` on 2026-08-31, trimmed of the `url` fields. A
        // hand-written fixture proves the parser handles what its author
        // imagined; this proves it handles what GitHub actually sends to the
        // one repository this code has to be right about.
        let p = parse_branch_protection(
            r#"{"required_pull_request_reviews":{"dismiss_stale_reviews":true,
                "require_code_owner_reviews":false,"require_last_push_approval":false,
                "required_approving_review_count":1},
                "required_signatures":{"enabled":false},
                "enforce_admins":{"enabled":true},
                "required_linear_history":{"enabled":false},
                "allow_force_pushes":{"enabled":false},
                "allow_deletions":{"enabled":false},
                "block_creations":{"enabled":false},
                "required_conversation_resolution":{"enabled":false},
                "lock_branch":{"enabled":false},
                "allow_fork_syncing":{"enabled":false}}"#,
        )
        .unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 1,
                require_code_owner_reviews: false,
                enforce_admins: true,
            }
        );
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn this_repositorys_own_pr_view_body_parses_as_awaiting_a_review() {
        // Captured verbatim from `gh pr view 324 --json …` on 2026-08-31,
        // with gh 2.96.0. The state that defeated the old guard ordering:
        // BLOCKED *because* the approval is missing, not because anything is
        // wrong with the branch.
        let snap = parse_pr_view(
            r#"{"baseRefName":"main","mergeStateStatus":"BLOCKED",
                "reviewDecision":"REVIEW_REQUIRED","state":"OPEN"}"#,
        )
        .unwrap();
        assert_eq!(snap.merge_state_status, "BLOCKED");
        assert!(!snap.human_approved(), "REVIEW_REQUIRED is not an approval");
    }

    #[test]
    fn an_empty_ruleset_array_is_an_answer_not_a_failure() {
        // What `gh api repos/ironrace/ironmem/rules/branches/main` returns
        // today. Reached only when the classic endpoint 404s, but it must
        // parse rather than error when it is.
        assert_eq!(
            parse_branch_rules("[]").unwrap(),
            BranchProtection::NoHumanApprovalRequired
        );
    }

    #[test]
    fn protection_without_a_review_requirement_permits_a_merge() {
        let p = parse_branch_protection(
            r#"{"required_status_checks":{"strict":true},"enforce_admins":{"enabled":false}}"#,
        )
        .unwrap();
        assert_eq!(p, BranchProtection::NoHumanApprovalRequired);
        assert!(p.permits_autopilot_merge());
    }

    #[test]
    fn a_code_owner_requirement_counts_even_with_a_zero_review_count() {
        // Independent fields in the API. Keying only on the count would read
        // this branch as unprotected and turn an accurate
        // `HumanApprovalRequired` into a bare `MergeCommandFailed`.
        let p = parse_branch_protection(
            r#"{"required_pull_request_reviews":{"required_approving_review_count":0,
                "require_code_owner_reviews":true}}"#,
        )
        .unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 0,
                require_code_owner_reviews: true,
                enforce_admins: false,
            }
        );
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_zero_approval_requirement_is_not_a_requirement() {
        let p = parse_branch_protection(
            r#"{"required_pull_request_reviews":{"required_approving_review_count":0,
                "require_code_owner_reviews":false}}"#,
        )
        .unwrap();
        assert_eq!(p, BranchProtection::NoHumanApprovalRequired);
    }

    #[test]
    fn a_404_asks_the_rulesets_endpoint_before_concluding_anything() {
        // The classic endpoint 404s both for an unprotected branch and for
        // one protected entirely by a ruleset. Only the second endpoint
        // tells them apart.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "gh: Branch not protected (HTTP 404)")),
            Ok(GhOutput::ok("[]")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert_eq!(p, BranchProtection::NoHumanApprovalRequired);
        assert_eq!(
            gh.seen[1],
            branch_rules_argv("owner/repo", "main"),
            "the 404 must be followed by the ruleset lookup"
        );
    }

    #[test]
    fn a_ruleset_requiring_an_approval_is_protection_the_classic_endpoint_cannot_see() {
        // The finding this test exists for: a repo that adopted rulesets has
        // no classic protection, so reading the 404 as "unprotected" sent
        // Autopilot to `gh pr merge` against a branch requiring a human.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "gh: Branch not protected (HTTP 404)")),
            Ok(GhOutput::ok(
                r#"[{"type":"creation"},
                    {"type":"pull_request","parameters":{
                        "required_approving_review_count":1,
                        "require_code_owner_review":false}}]"#,
            )),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 1,
                require_code_owner_reviews: false,
                // No analogue in the rulesets API — understated, never
                // overstated.
                enforce_admins: false,
            }
        );
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_ruleset_code_owner_requirement_is_read_from_the_singular_field_name() {
        // The rulesets API spells it `require_code_owner_review`; the
        // classic one spells it `..._reviews`. Reusing the classic parser
        // here would read a required code-owner review as `false`.
        let p = parse_branch_rules(
            r#"[{"type":"pull_request","parameters":{
                "required_approving_review_count":0,
                "require_code_owner_review":true}}]"#,
        )
        .unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 0,
                require_code_owner_reviews: true,
                enforce_admins: false,
            }
        );
    }

    #[test]
    fn several_rulesets_compose_to_the_strictest_of_them() {
        let p = parse_branch_rules(
            r#"[{"type":"pull_request","parameters":{
                    "required_approving_review_count":1,
                    "require_code_owner_review":false}},
                {"type":"pull_request","parameters":{
                    "required_approving_review_count":2,
                    "require_code_owner_review":true}}]"#,
        )
        .unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 2,
                require_code_owner_reviews: true,
                enforce_admins: false,
            }
        );
    }

    #[test]
    fn a_ruleset_lookup_that_fails_holds_rather_than_reading_as_unprotected() {
        // The classic 404 answered nothing, so an unanswered ruleset lookup
        // leaves the question genuinely open.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "gh: Branch not protected (HTTP 404)")),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }), "{p:?}");
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_branch_name_is_encoded_as_one_path_segment() {
        // `release/1.0` is the ordinary case: the endpoint takes the branch
        // as a single segment, so an unencoded slash addresses a different
        // resource entirely.
        assert_eq!(
            branch_protection_argv("owner/repo", "release/1.0"),
            vec![
                "api".to_string(),
                "repos/owner/repo/branches/release%2F1.0/protection".to_string()
            ]
        );
        assert_eq!(
            branch_rules_argv("owner/repo", "release/1.0"),
            vec![
                "api".to_string(),
                "repos/owner/repo/rules/branches/release%2F1.0".to_string()
            ]
        );
    }

    #[test]
    fn a_fragment_character_cannot_truncate_the_protection_path() {
        // The dangerous one: an unencoded `#` ends the path at the fragment,
        // so the request becomes a plain "get branch" that answers 200 —
        // and `ProtectionJson`'s fields are all optional, so that unrelated
        // body parses as "no approval required".
        let argv = branch_protection_argv("owner/repo", "feat#1");
        assert_eq!(argv[1], "repos/owner/repo/branches/feat%231/protection");
        assert!(!argv[1].contains('#'));
    }

    #[test]
    fn an_ordinary_branch_name_is_left_alone() {
        assert_eq!(
            branch_protection_argv("owner/repo", "main")[1],
            "repos/owner/repo/branches/main/protection"
        );
    }

    #[test]
    fn the_rules_argv_is_the_rest_path_for_the_branch() {
        assert_eq!(
            branch_rules_argv("owner/repo", "main"),
            vec![
                "api".to_string(),
                "repos/owner/repo/rules/branches/main".to_string()
            ]
        );
    }

    #[test]
    fn a_stray_404_in_an_unrelated_failure_does_not_unlock_a_merge() {
        // The one place a failed `gh` call means "proceed", so the match is
        // anchored to the status phrase: a request id, a docs URL or a
        // rate-limit body that merely contains the digits must still hold.
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed(
            "",
            "HTTP 500: server error (request id 404abc, see https://docs.github.com/rest/404)",
        ))]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }), "{p:?}");
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_403_is_unknown_and_therefore_blocks() {
        // The inversion that would merge into a protected branch: a token
        // without admin scope cannot read protection, and "cannot read" is
        // not "not protected".
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed("", "HTTP 403: Forbidden"))]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }));
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_spawn_failure_propagates_rather_than_reading_as_unprotected() {
        let mut gh = ScriptedGh::new(vec![Err(MemoryError::NotFound("no gh".into()))]);
        assert!(branch_protection(&mut gh, "owner/repo", "main").is_err());
    }

    // ── runner plumbing ─────────────────────────────────────────────────

    #[test]
    fn a_failed_pr_view_is_an_error_not_an_empty_snapshot() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed("", "no such PR"))]);
        assert!(pr_snapshot(&mut gh, "owner/repo", 7).is_err());
    }

    #[test]
    fn a_failed_comment_is_an_error() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed("", "HTTP 403"))]);
        assert!(comment_on_issue(&mut gh, &issue(), "body").is_err());
    }

    #[test]
    fn the_already_exists_markers_are_matched_lowercased() {
        for marker in LABEL_ALREADY_EXISTS_MARKERS
            .iter()
            .chain(BRANCH_UNPROTECTED_MARKERS.iter())
        {
            assert_eq!(
                *marker,
                marker.to_lowercase(),
                "markers are compared against a lowercased haystack"
            );
        }
    }
}
