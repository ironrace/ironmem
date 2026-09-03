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

/// `gh issue view N --repo R --json title,body`.
///
/// Rung 9 only. The queue already carries every backlog issue's title and
/// body from `gh issue list`, so this exists for the one case that listing
/// cannot serve: an issue that is *in flight* rather than in a backlog, and
/// which rung 9 is about to draft a human question for.
pub fn issue_view_brief_argv(issue: &IssueRef) -> Vec<String> {
    vec![
        "issue".into(),
        "view".into(),
        issue.number.to_string(),
        "--repo".into(),
        issue.repo.clone(),
        "--json".into(),
        "title,body".into(),
    ]
}

/// One issue's title and body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct IssueBriefJson {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// Parse `gh issue view --json title,body`.
///
/// Both fields default to empty, and empty is the *degraded* value
/// everywhere it is used: rung 9's question prompt renders it as
/// "(not available)" and the drafted question is worse, rather than the call
/// failing. Rung 6's lesson 18 — schema drift degrades, it does not break.
pub fn parse_issue_brief(stdout: &str) -> Result<IssueBriefJson, MemoryError> {
    serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!("could not parse `gh issue view --json`: {e}"))
    })
}

/// Read one issue's title and body.
pub fn issue_brief(gh: &mut dyn GhRunner, issue: &IssueRef) -> Result<IssueBriefJson, MemoryError> {
    let out = gh.run(&issue_view_brief_argv(issue))?;
    parse_issue_brief(out.require_success(
        &format!("gh issue view {}", issue.canonical()),
        GhFailure::NotFound,
    )?)
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

/// `owner/name` with each half encoded as its own path segment.
///
/// `validate_repo` is deliberately permissive on character set — it rejects
/// only empty, over-long and control-character strings — so `#`, `?` and `%`
/// all reach these format strings. Encoding the branch and interpolating the
/// repo raw would leave the same truncation hole open one segment to the
/// left, and these builders are `pub`: their safety must not rest on which
/// caller happens to have validated what first.
fn encode_repo_path(repo: &str) -> String {
    match repo.split_once('/') {
        Some((owner, name)) => format!(
            "{}/{}",
            encode_path_segment(owner),
            encode_path_segment(name)
        ),
        None => encode_path_segment(repo),
    }
}

/// `gh api repos/{repo}/branches/{branch}/protection`.
pub fn branch_protection_argv(repo: &str, branch: &str) -> Vec<String> {
    vec![
        "api".into(),
        format!(
            "repos/{}/branches/{}/protection",
            encode_repo_path(repo),
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
            "repos/{}/rules/branches/{}",
            encode_repo_path(repo),
            encode_path_segment(branch)
        ),
        // **A list endpoint, and it pages at 30.** It returns one element
        // per *rule*, not per ruleset, and a single ruleset routinely
        // contributes a dozen (`creation`, `update`, `deletion`,
        // `non_fast_forward`, `required_signatures`, …). Two rulesets on a
        // branch clear 30 easily, and an unpaginated call would then return
        // a truncated first page in which the `pull_request` rule simply is
        // not present — indistinguishable, in shape, from a branch that
        // requires no review. That reads as `NoHumanApprovalRequired` and
        // merges into a protected branch: the exact failure this endpoint
        // was added to prevent, reintroduced one layer down.
        "--paginate".into(),
        // Without `--slurp`, `--paginate` concatenates one JSON array per
        // page back to back, which is not valid JSON. With it the pages
        // arrive as an array *of* arrays — hence `parse_branch_rules`
        // flattening rather than parsing a flat list.
        //
        // `--slurp` needs gh ≥ 2.55. An older gh rejects the flag, the call
        // fails, and `branch_rules` answers `Unknown`, which holds the PR —
        // the safe direction, and loudly enough to diagnose.
        "--slurp".into(),
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
    // `gh api --paginate --slurp` wraps each page in the outer array, so
    // this is a list of *pages*, not a list of rules. Verified against a
    // live ruleset-bearing repository: the unpaginated call returns `[…]`
    // and this one returns `[[…]]`.
    let pages: Vec<Vec<BranchRuleJson>> = serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!("could not parse branch rules response: {e}"))
    })?;
    let mut count = 0;
    let mut code_owners = false;
    for rule in pages.into_iter().flatten() {
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

/// Read a base branch's review requirement, from both places GitHub keeps
/// one.
///
/// # Why two endpoints, always
///
/// The classic `.../branches/{b}/protection` endpoint and the newer
/// `.../rules/branches/{b}` endpoint describe *different* mechanisms, and a
/// branch can be governed by either or both. Neither is a superset:
///
/// - Classic protection is invisible to the rules endpoint.
/// - Rulesets — repository- and organization-level — are invisible to the
///   classic endpoint, which answers `404` as though nothing protected the
///   branch.
///
/// So asking only the classic one reads a ruleset-protected branch as open,
/// and asking it *first and stopping when it answers* reads a branch with
/// status-check-only classic protection plus an approval-requiring ruleset
/// as open too. Both are the same bug. The only correct question is the
/// conjunction, and the strictest answer wins — which is how GitHub itself
/// composes them.
///
/// The classic endpoint additionally requires **admin** rights, while the
/// rules endpoint needs only read. A token without admin gets `403` from the
/// first and a perfectly good answer from the second, so a `403` must not
/// end the enquiry either: doing so stalled every PR on every poll, forever,
/// for the commonest credential configuration there is.
///
/// # What still holds
///
/// [`BranchProtection::Unknown`] whenever a requirement *might* exist and
/// could not be read: the classic endpoint failed for any reason other than
/// "no classic protection", and the rules endpoint did not independently
/// find a requirement. An unreadable answer is never "proceed".
pub fn branch_protection(
    gh: &mut dyn GhRunner,
    repo: &str,
    branch: &str,
) -> Result<BranchProtection, MemoryError> {
    let classic = classic_protection(gh, repo, branch)?;

    // A classic requirement is already the strictest answer available: the
    // rules endpoint can only agree, and asking it would buy nothing but an
    // API call.
    if let ClassicProtection::Answered(p @ BranchProtection::HumanApprovalRequired { .. }) =
        &classic
    {
        return Ok(p.clone());
    }

    let rules = branch_rules(gh, repo, branch)?;
    if matches!(rules, BranchProtection::HumanApprovalRequired { .. }) {
        return Ok(rules);
    }

    Ok(match (classic, rules) {
        // An unreadable classic answer holds, wherever it came from. This
        // arm is unreachable today — `parse_branch_protection` never returns
        // `Unknown` — but it is what actually stops the catch-all below from
        // handing back `NoHumanApprovalRequired` if that ever changes.
        // Spelling the *permitting* arm precisely is not enough on its own,
        // because the catch-all returns `other`, and `other` can be
        // `NoHumanApprovalRequired`.
        (ClassicProtection::Answered(unknown @ BranchProtection::Unknown { .. }), _) => unknown,
        // Spelled out rather than `Answered(_)`, so the one classic answer
        // that permits a merge has to be named to get one.
        (
            ClassicProtection::Answered(BranchProtection::NoHumanApprovalRequired)
            | ClassicProtection::Absent,
            BranchProtection::NoHumanApprovalRequired,
        ) => BranchProtection::NoHumanApprovalRequired,
        // The rules endpoint could not answer either.
        (_, unknown @ BranchProtection::Unknown { .. }) => unknown,
        // The rules endpoint found nothing, but the classic one was never
        // readable — so a classic requirement may exist and be invisible to
        // us. Not "unprotected".
        (ClassicProtection::Unreadable(detail), _) => BranchProtection::Unknown { detail },
        (_, other) => other,
    })
}

/// What the classic protection endpoint said.
///
/// Three states, not two: "no classic protection exists" (`404`) and "I
/// could not read whether classic protection exists" (`403`, a network
/// failure, an unreadable body) are the same *failure* to `gh` and opposite
/// *facts* to this module.
enum ClassicProtection {
    Answered(BranchProtection),
    /// `404` — the branch has no classic protection. A real answer.
    Absent,
    Unreadable(String),
}

fn classic_protection(
    gh: &mut dyn GhRunner,
    repo: &str,
    branch: &str,
) -> Result<ClassicProtection, MemoryError> {
    let out = gh.run(&branch_protection_argv(repo, branch))?;
    if out.success {
        // Unparseable is `Unknown`, not `Err`, and the difference is the
        // whole notification path: `Unknown` becomes a hold, which comments
        // on the issue and labels it, whereas an `Err` propagates out of
        // `execute_merge` and leaves the issue with no trace that anything
        // was attempted. A human learns about every refusal from the issue
        // itself, or the refusal may as well not have happened.
        return Ok(match parse_branch_protection(&out.stdout) {
            Ok(p) => ClassicProtection::Answered(p),
            Err(e) => ClassicProtection::Unreadable(format!(
                "{repo}@{branch} protection response was unreadable: {e}"
            )),
        });
    }
    if out.mentions_any(&BRANCH_UNPROTECTED_MARKERS) {
        return Ok(ClassicProtection::Absent);
    }
    Ok(ClassicProtection::Unreadable(format!(
        "gh api branch protection for {repo}@{branch} exited {:?}: {}",
        out.code,
        out.stderr.trim()
    )))
}

/// Read the rulesets in force on a branch.
///
/// A failure here is [`BranchProtection::Unknown`]: a ruleset that cannot be
/// read may require anything.
fn branch_rules(
    gh: &mut dyn GhRunner,
    repo: &str,
    branch: &str,
) -> Result<BranchProtection, MemoryError> {
    let out = gh.run(&branch_rules_argv(repo, branch))?;
    if out.success {
        return Ok(
            parse_branch_rules(&out.stdout).unwrap_or_else(|e| BranchProtection::Unknown {
                detail: format!("{repo}@{branch} ruleset response was unreadable: {e}"),
            }),
        );
    }
    Ok(BranchProtection::Unknown {
        detail: format!(
            "the ruleset lookup for {repo}@{branch} exited {:?}: {}",
            out.code,
            out.stderr.trim()
        ),
    })
}

// ── rung 8: the two reads the Lead's queue is built from ────────────────

/// The `--json` field list [`parse_issue_list`] expects. Named for the same
/// reason [`PR_VIEW_FIELDS`] is: the builder and the parser cannot drift.
///
/// `body` is fetched here rather than with a second per-issue `gh issue view`
/// because [`super::run::IssueBrief`] needs it for the turn prompt, and one
/// list call per repo is the difference between a Lead tick costing one API
/// round trip per repo and one per *issue*.
pub const ISSUE_LIST_FIELDS: &str = "number,title,body,labels,updatedAt";

/// `gh issue list --repo R --label L --state open --json <ISSUE_LIST_FIELDS> --limit N`.
///
/// `--state open` is explicit rather than relying on `gh`'s default: a closed
/// issue that still carries `agent:ready` is not work, and depending on a CLI
/// default to exclude it puts a correctness property in someone else's
/// changelog.
pub fn issue_list_argv(repo: &str, label: &str, limit: u32) -> Vec<String> {
    vec![
        "issue".into(),
        "list".into(),
        "--repo".into(),
        repo.into(),
        "--label".into(),
        label.into(),
        "--state".into(),
        "open".into(),
        "--json".into(),
        ISSUE_LIST_FIELDS.into(),
        "--limit".into(),
        limit.to_string(),
    ]
}

/// One issue as `gh issue list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueListing {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, deserialize_with = "label_names")]
    pub labels: Vec<String>,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
}

fn label_names<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<LabelJson> = Vec::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|l| l.name)
        .filter(|n| !n.is_empty())
        .collect())
}

/// Parse `gh issue list --json` output.
///
/// An issue numbered 0 is dropped rather than carried: GitHub never issues
/// one, so its only source is a malformed or truncated response, and a
/// `repo#0` reaching the queue would key a dispatch-state drawer that no
/// human command can name.
pub fn parse_issue_list(stdout: &str) -> Result<Vec<IssueListing>, MemoryError> {
    let listings: Vec<IssueListing> = serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!("could not parse `gh issue list --json`: {e}"))
    })?;
    Ok(listings.into_iter().filter(|i| i.number != 0).collect())
}

/// List one repo's issues carrying `label`.
///
/// A failure is an `Err`, never an empty list. Rung 7's lesson: an unreadable
/// collection must not degrade to an empty one — an empty backlog is a
/// confident claim that a repo has no work, and the Lead would act on it by
/// giving that repo's slots to another repo.
pub fn list_labeled_issues(
    gh: &mut dyn GhRunner,
    repo: &str,
    label: &str,
    limit: u32,
) -> Result<Vec<IssueListing>, MemoryError> {
    let out = gh.run(&issue_list_argv(repo, label, limit))?;
    parse_issue_list(out.require_success(
        &format!("gh issue list --label {label} on {repo}"),
        GhFailure::NotFound,
    )?)
}

/// `gh issue view N --repo R --json comments`.
pub fn issue_comments_argv(issue: &IssueRef) -> Vec<String> {
    vec![
        "issue".into(),
        "view".into(),
        issue.number.to_string(),
        "--repo".into(),
        issue.repo.clone(),
        "--json".into(),
        "comments".into(),
    ]
}

/// One comment on an issue.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueComment {
    #[serde(default)]
    pub body: String,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
    #[serde(default, deserialize_with = "author_login")]
    pub author: String,
}

fn author_login<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Author {
        #[serde(default)]
        login: String,
    }
    // `author` is null on a comment from a deleted account. That is a
    // rendering detail, not a reason to fail the whole poll.
    let raw: Option<Author> = Option::deserialize(deserializer)?;
    Ok(raw.map(|a| a.login).unwrap_or_default())
}

#[derive(Deserialize)]
struct IssueCommentsJson {
    #[serde(default)]
    comments: Vec<IssueComment>,
}

/// Parse `gh issue view --json comments` output.
pub fn parse_issue_comments(stdout: &str) -> Result<Vec<IssueComment>, MemoryError> {
    let raw: IssueCommentsJson = serde_json::from_str(stdout.trim()).map_err(|e| {
        MemoryError::Validation(format!(
            "could not parse `gh issue view --json comments`: {e}"
        ))
    })?;
    Ok(raw.comments)
}

/// Read an issue's comment thread, oldest first (the order `gh` returns).
pub fn issue_comments(
    gh: &mut dyn GhRunner,
    issue: &IssueRef,
) -> Result<Vec<IssueComment>, MemoryError> {
    let out = gh.run(&issue_comments_argv(issue))?;
    parse_issue_comments(out.require_success(
        &format!("gh issue view {} --json comments", issue.canonical()),
        GhFailure::NotFound,
    )?)
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

    // ── rung 8: the queue's two reads ───────────────────────────────────

    #[test]
    fn the_issue_list_argv_names_the_repo_the_label_and_the_open_state() {
        let argv = issue_list_argv("owner/repo", "agent:ready", 50);
        assert_eq!(argv[0], "issue");
        assert_eq!(argv[1], "list");
        assert!(argv.windows(2).any(|w| w == ["--repo", "owner/repo"]));
        assert!(argv.windows(2).any(|w| w == ["--label", "agent:ready"]));
        // Explicit, not inherited from a CLI default: a closed issue that
        // still carries `agent:ready` is not work.
        assert!(argv.windows(2).any(|w| w == ["--state", "open"]));
        assert!(argv.windows(2).any(|w| w == ["--limit", "50"]));
        assert!(argv.windows(2).any(|w| w == ["--json", ISSUE_LIST_FIELDS]));
    }

    #[test]
    fn the_issue_list_field_list_covers_everything_the_parser_reads() {
        for field in ["number", "title", "body", "labels", "updatedAt"] {
            assert!(
                ISSUE_LIST_FIELDS.split(',').any(|f| f == field),
                "{field} is read by IssueListing but not requested"
            );
        }
    }

    #[test]
    fn parse_issue_list_reads_numbers_titles_bodies_and_label_names() {
        let listings = parse_issue_list(
            r#"[{"number":7,"title":"T","body":"B",
                 "labels":[{"name":"agent:ready"},{"name":"priority:high"}],
                 "updatedAt":"2026-09-02T00:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].number, 7);
        assert_eq!(listings[0].title, "T");
        assert_eq!(listings[0].body, "B");
        assert_eq!(listings[0].labels, vec!["agent:ready", "priority:high"]);
    }

    #[test]
    fn parse_issue_list_accepts_an_empty_backlog() {
        assert!(parse_issue_list("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_issue_list_tolerates_missing_optional_fields() {
        // Degrades to empty strings, never to an error: a missing body is a
        // thin turn prompt, not a reason to skip a repo.
        let listings = parse_issue_list(r#"[{"number":7}]"#).unwrap();
        assert_eq!(listings[0].number, 7);
        assert!(listings[0].body.is_empty());
        assert!(listings[0].labels.is_empty());
    }

    #[test]
    fn parse_issue_list_drops_an_issue_numbered_zero() {
        // GitHub never issues one, so its only source is a malformed
        // response — and `repo#0` would key a drawer no command can name.
        let listings = parse_issue_list(r#"[{"number":0,"title":"junk"},{"number":5}]"#).unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].number, 5);
    }

    #[test]
    fn an_unparseable_listing_is_an_error_never_an_empty_backlog() {
        // Rung 7's lesson 21: an empty backlog is a confident claim that a
        // repo has no work, and the Lead acts on it by giving the repo's
        // slots away.
        assert!(parse_issue_list("not json").is_err());
        assert!(parse_issue_list("{}").is_err());
    }

    #[test]
    fn a_failed_listing_call_is_an_error_never_an_empty_backlog() {
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed("", "HTTP 403"))]);
        assert!(list_labeled_issues(&mut gh, "owner/repo", "agent:ready", 10).is_err());
    }

    #[test]
    fn parse_issue_comments_reads_body_author_and_creation_time_in_order() {
        let comments = parse_issue_comments(
            r#"{"comments":[
                {"author":{"login":"a"},"body":"first","createdAt":"2026-09-01T00:00:00Z"},
                {"author":{"login":"b"},"body":"second","createdAt":"2026-09-02T00:00:00Z"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].body, "first");
        assert_eq!(comments[0].author, "a");
        assert_eq!(comments[1].created_at, "2026-09-02T00:00:00Z");
    }

    #[test]
    fn a_comment_from_a_deleted_account_does_not_fail_the_whole_poll() {
        let comments =
            parse_issue_comments(r#"{"comments":[{"author":null,"body":"x","createdAt":"t"}]}"#)
                .unwrap();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].author.is_empty());
    }

    #[test]
    fn an_issue_with_no_comments_parses_as_an_empty_thread() {
        assert!(parse_issue_comments(r#"{"comments":[]}"#)
            .unwrap()
            .is_empty());
        assert!(parse_issue_comments("{}").unwrap().is_empty());
    }

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
            parse_branch_rules("[[]]").unwrap(),
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
            Ok(GhOutput::ok("[[]]")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert_eq!(p, BranchProtection::NoHumanApprovalRequired);
        assert_eq!(
            gh.seen[1],
            branch_rules_argv("owner/repo", "main"),
            "the 404 must be followed by the ruleset lookup"
        );
        assert_eq!(gh.seen.len(), 2);
    }

    #[test]
    fn a_ruleset_requiring_an_approval_is_protection_the_classic_endpoint_cannot_see() {
        // The finding this test exists for: a repo that adopted rulesets has
        // no classic protection, so reading the 404 as "unprotected" sent
        // Autopilot to `gh pr merge` against a branch requiring a human.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "gh: Branch not protected (HTTP 404)")),
            Ok(GhOutput::ok(
                r#"[[{"type":"creation"},
                     {"type":"pull_request","parameters":{
                         "required_approving_review_count":1,
                         "require_code_owner_review":false}}]]"#,
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
            r#"[[{"type":"pull_request","parameters":{
                 "required_approving_review_count":0,
                 "require_code_owner_review":true}}]]"#,
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
    fn rules_split_across_pages_compose_to_the_strictest_of_them() {
        let p = parse_branch_rules(
            r#"[[{"type":"pull_request","parameters":{
                     "required_approving_review_count":1,
                     "require_code_owner_review":false}}],
                 [{"type":"pull_request","parameters":{
                     "required_approving_review_count":2,
                     "require_code_owner_review":true}}]]"#,
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
    fn the_rules_lookup_asks_for_every_page() {
        // The endpoint returns one element per *rule*, not per ruleset, and
        // pages at 30. A truncated first page missing the `pull_request`
        // rule is shape-identical to a branch that requires no review, so an
        // unpaginated call would read a protected branch as open — the very
        // failure the ruleset lookup was added to prevent.
        let argv = branch_rules_argv("owner/repo", "main");
        assert!(argv.contains(&"--paginate".to_string()), "{argv:?}");
        assert!(
            argv.contains(&"--slurp".to_string()),
            "without --slurp the pages are concatenated into invalid JSON: {argv:?}"
        );
    }

    #[test]
    fn an_unreadable_protection_body_holds_instead_of_erroring_out() {
        // An `Err` propagates out of `execute_merge` and leaves the issue
        // with no comment, no label and no record — from the issue's point
        // of view nothing happened. `Unknown` becomes a hold, which tells
        // the human.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::ok("not json at all")),
            Ok(GhOutput::ok("[[]]")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }), "{p:?}");
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn an_unreadable_ruleset_body_holds_too() {
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "gh: Branch not protected (HTTP 404)")),
            Ok(GhOutput::ok("{}")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }), "{p:?}");
    }

    #[test]
    fn the_repo_is_encoded_into_the_path_just_as_the_branch_is() {
        // `validate_repo` is permissive on character set, so a `?` here
        // would truncate the path to `repos/owner/repo` — a plain repository
        // object, which every field of `ProtectionJson` being optional would
        // parse cleanly as "no approval required".
        let argv = branch_protection_argv("own?er/re#po", "main");
        assert_eq!(argv[1], "repos/own%3Fer/re%23po/branches/main/protection");
        assert!(!argv[1].contains('?') && !argv[1].contains('#'));
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
            branch_rules_argv("owner/repo", "release/1.0")[1],
            "repos/owner/repo/rules/branches/release%2F1.0"
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
            branch_rules_argv("owner/repo", "main")[1],
            "repos/owner/repo/rules/branches/main"
        );
    }

    #[test]
    fn a_stray_404_in_an_unrelated_failure_does_not_unlock_a_merge() {
        // The one place a failed `gh` call means "proceed", so the match is
        // anchored to the status phrase: a request id, a docs URL or a
        // rate-limit body that merely contains the digits must still hold.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed(
                "",
                "HTTP 500: server error (request id 404abc, see https://docs.github.com/rest/404)",
            )),
            // the rules endpoint is still consulted — it needs no admin —
            // but finding no ruleset cannot vouch for the classic
            // protection we were unable to read
            Ok(GhOutput::ok("[[]]")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }), "{p:?}");
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_403_is_unknown_and_therefore_blocks() {
        // The inversion that would merge into a protected branch: a token
        // without admin scope cannot read protection, and "cannot read" is
        // not "not protected".
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
            Ok(GhOutput::ok("[[]]")),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::Unknown { .. }));
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn no_combination_of_the_two_endpoints_reads_as_open_unless_both_said_so() {
        // The safety property the whole lattice exists for, checked
        // exhaustively rather than by reading the match arms. `classic ×
        // rules`, every combination, with the one permitted route to
        // `NoHumanApprovalRequired` named explicitly.
        let requires = r#"{"required_pull_request_reviews":
            {"required_approving_review_count":1}}"#;
        let permits = r#"{"required_status_checks":{"strict":true}}"#;
        let rules_require = r#"[[{"type":"pull_request","parameters":{
            "required_approving_review_count":1,
            "require_code_owner_review":false}}]]"#;

        let classics = [
            ("answered: requires", GhOutput::ok(requires)),
            ("answered: permits", GhOutput::ok(permits)),
            ("absent (404)", GhOutput::failed("", "HTTP 404")),
            ("unreadable (403)", GhOutput::failed("", "HTTP 403")),
            ("unreadable (bad body)", GhOutput::ok("{{{")),
        ];
        let rules = [
            ("requires", GhOutput::ok(rules_require)),
            ("none", GhOutput::ok("[[]]")),
            ("unreadable", GhOutput::failed("", "HTTP 500")),
            ("bad body", GhOutput::ok("not json")),
        ];

        for (cname, c) in &classics {
            for (rname, r) in &rules {
                let mut gh = ScriptedGh::new(vec![Ok(c.clone()), Ok(r.clone())]);
                let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
                let classic_permits = *cname == "answered: permits";
                let classic_absent = *cname == "absent (404)";
                let rules_none = *rname == "none";
                let may_merge = (classic_permits || classic_absent) && rules_none;
                assert_eq!(
                    p.permits_autopilot_merge(),
                    may_merge,
                    "classic={cname}, rules={rname} gave {p:?}"
                );
            }
        }
    }

    #[test]
    fn a_403_still_consults_the_rules_endpoint_which_needs_no_admin() {
        // The classic endpoint requires admin rights. A token without them
        // gets 403 on every poll forever — so ending the enquiry there
        // stalled every PR permanently for the commonest credential setup
        // there is. The rules endpoint needs only read access and can still
        // find a requirement.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "HTTP 403: Must have admin rights")),
            Ok(GhOutput::ok(
                r#"[[{"type":"pull_request","parameters":{
                     "required_approving_review_count":1,
                     "require_code_owner_review":false}}]]"#,
            )),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 1,
                require_code_owner_reviews: false,
                enforce_admins: false,
            },
            "an unreadable classic endpoint must not hide a ruleset requirement"
        );
    }

    #[test]
    fn classic_protection_without_a_review_requirement_still_asks_about_rulesets() {
        // A branch can carry classic protection for status checks only *and*
        // an organization ruleset requiring an approval. Stopping as soon as
        // the classic endpoint answered read that branch as open.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::ok(
                r#"{"required_status_checks":{"strict":true},"enforce_admins":{"enabled":false}}"#,
            )),
            Ok(GhOutput::ok(
                r#"[[{"type":"pull_request","parameters":{
                     "required_approving_review_count":2,
                     "require_code_owner_review":false}}]]"#,
            )),
        ]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert_eq!(
            p,
            BranchProtection::HumanApprovalRequired {
                required_approving_review_count: 2,
                require_code_owner_reviews: false,
                enforce_admins: false,
            }
        );
        assert!(!p.permits_autopilot_merge());
    }

    #[test]
    fn a_classic_requirement_is_the_strictest_answer_and_costs_no_second_call() {
        // The rules endpoint can only agree with a classic requirement, so
        // asking it would buy nothing but an API call.
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok(
            r#"{"required_pull_request_reviews":{"required_approving_review_count":1},
                "enforce_admins":{"enabled":true}}"#,
        ))]);
        let p = branch_protection(&mut gh, "owner/repo", "main").unwrap();
        assert!(matches!(p, BranchProtection::HumanApprovalRequired { .. }));
        assert_eq!(gh.seen.len(), 1, "{:?}", gh.seen);
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

    #[test]
    fn an_issue_brief_parses_and_degrades_to_empty_rather_than_failing() {
        let brief =
            parse_issue_brief(r#"{"title":"Fix the parser","body":"It drops rows"}"#).unwrap();
        assert_eq!(brief.title, "Fix the parser");
        assert_eq!(brief.body, "It drops rows");

        // A missing field is the *degraded* value, not an error: rung 9
        // renders an empty body as "(not available)" and drafts a worse
        // question, rather than skipping the notice a human needs.
        let brief = parse_issue_brief("{}").unwrap();
        assert!(brief.title.is_empty() && brief.body.is_empty());

        assert!(parse_issue_brief("not json").is_err());
    }

    #[test]
    fn the_issue_brief_argv_requests_exactly_the_fields_it_reads() {
        let argv = issue_view_brief_argv(&IssueRef::new("ironrace/ironmem", 42));
        assert_eq!(argv[0], "issue");
        assert_eq!(argv[1], "view");
        assert_eq!(argv[2], "42");
        assert!(argv.contains(&"title,body".to_string()));
    }
}
