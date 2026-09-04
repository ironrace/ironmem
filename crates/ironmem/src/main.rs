use clap::{Parser, Subcommand};
use std::process;

use ironmem::review_diff::{build_review_diff, expand_review_diff, ReviewDiffRequest};
use ironmem::MemoryError;
use ironmem::{
    bootstrap, config, context, dashboard, ingest, launcher, mcp, migrate, reembed, report,
    symbol_graph,
};

#[derive(Parser)]
#[command(
    name = "ironmem",
    version = env!("IRONMEM_VERSION"),
    about = "AI memory — semantic search + knowledge graph"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (JSON-RPC over stdio)
    Serve {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Run as the shared daemon bound to this Unix socket path
        #[arg(long)]
        listen: Option<String>,
        /// Run as a thin proxy connecting to this Unix socket path
        #[arg(long, conflicts_with = "listen")]
        connect: Option<String>,
        /// Disable daemon auto-spawn from proxy (--connect) mode
        #[arg(long)]
        no_autospawn: bool,
    },
    /// Initialize a new memory store
    Init,
    /// Download the embedding model
    Setup,
    /// Mine files into memory
    Mine {
        /// Directory to mine
        path: String,
    },
    /// Migrate from a ChromaDB store
    Migrate {
        /// Path to existing ChromaDB directory
        #[arg(long)]
        from: String,
    },
    /// Re-embed all drawers using the current model (run after a model upgrade)
    Reembed {
        /// Only re-embed drawers in this wing
        #[arg(long)]
        wing: Option<String>,
    },
    /// Run a hook (called by Claude Code / Codex)
    Hook {
        /// Hook name: stop, precompact, session-start, user-prompt-submit
        name: String,
        /// Harness: claude-code, codex
        #[arg(long, default_value = "claude-code")]
        harness: String,
    },
    /// Render the metrics report (METRICS_SPEC §10 + §7 cost)
    Report {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Only this task (collab_session_id or task_tag)
        #[arg(long)]
        task: Option<String>,
        /// Only rows at/after this RFC3339 instant or YYYY-MM-DD date (inclusive)
        #[arg(long)]
        since: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Diagnose the local installation (binary, db, model, MCP mode, harnesses)
    Doctor {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Assemble a compact context pack for a task (memory + decisions + code maps)
    Context {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repository root for code-map lookup
        #[arg(long, default_value = ".")]
        repo: String,
        /// Task description driving memory recall
        #[arg(long)]
        task: String,
        /// Code-map area to include (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Approximate output token budget
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Write the ironmem memory-protocol managed block into rules file(s) (explicit opt-in)
    WriteRules {
        /// Target rules file. Omit to write all default harness rules files.
        /// Validated against registered harness rules files. Non-native selections
        /// also ensure the canonical AGENTS.md dependency is updated.
        #[arg(long, conflicts_with = "harness")]
        target: Option<String>,
        /// Write this harness's rules file (e.g. claude → CLAUDE.md, codex → AGENTS.md).
        /// Non-native harnesses also update the canonical AGENTS.md dependency.
        #[arg(long, conflicts_with = "target")]
        harness: Option<String>,
        /// Directory containing the target file(s)
        #[arg(long, default_value = ".")]
        workspace: String,
    },
    /// Launch Claude Code in a repo with the ironmem MCP server attached
    Claude {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
    /// Start a local read-only dashboard server for inspecting memory, code maps, sessions, and metrics
    Dashboard {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Host to bind (default: 127.0.0.1 loopback only)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind (0 = ephemeral)
        #[arg(long, default_value_t = 7384)]
        port: u16,
        /// Allow binding to a non-loopback address (WARNING: exposes dashboard to the network)
        #[arg(long)]
        allow_non_loopback: bool,
        /// Emit startup metadata as JSON instead of prose
        #[arg(long)]
        json: bool,
        /// Internal test hook: exit when stdin closes
        #[arg(long, hide = true)]
        exit_on_stdin_close: bool,
    },
    /// Build a compressed Git diff artifact or expand one of its indexed hunks
    ReviewDiff {
        /// Git repository containing the diff source
        #[arg(long, default_value = ".")]
        repo: String,
        /// Diff uncommitted worktree changes relative to HEAD
        #[arg(long, conflicts_with_all = ["base", "head"])]
        worktree: bool,
        /// Base revision for a merge-base range
        #[arg(long, conflicts_with = "worktree")]
        base: Option<String>,
        /// Head revision for a merge-base range
        #[arg(long, conflicts_with = "worktree")]
        head: Option<String>,
        /// Expand this indexed file instead of printing the compressed artifact
        #[arg(long)]
        expand_file: Option<String>,
        /// One-based hunk ordinal within --expand-file
        #[arg(long, requires = "expand_file")]
        hunk: Option<usize>,
    },
    /// Build or query the local symbol/import graph index
    Symbols {
        #[command(subcommand)]
        cmd: SymbolsCmd,
    },
    /// Inspect or prune memory lifecycle artifacts
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Onboard or approve a repo's autopilot gate config (build-ladder rung 3)
    Autopilot {
        #[command(subcommand)]
        cmd: AutopilotCmd,
    },
    /// List registered harnesses (dev/CI helper for packaging scripts)
    Harnesses {
        /// Output format
        #[arg(long, default_value = "text", value_parser = ["json", "text"])]
        format: String,
    },
    /// Launch Codex in a repo with the ironmem MCP server attached
    Codex {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
    /// Launch Grok in a repo with the ironmem MCP server attached (scaffolding — #190 Task 13)
    Grok {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
    /// Launch Gemini CLI in a repo with the ironmem MCP server attached
    Gemini {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
}

/// Subcommands nested under `ironmem memory`.
#[derive(Subcommand)]
enum MemoryCmd {
    /// Dry-run or apply conservative stale-drawer garbage collection
    Gc {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Show candidates without deleting anything (the default)
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Actually delete candidates. Omit for dry-run.
        #[arg(long)]
        apply: bool,
        /// Retention for collab-checkpoints drawers
        #[arg(long, default_value_t = ironmem::db::retention::DEFAULT_COLLAB_CHECKPOINT_RETENTION_DAYS)]
        collab_checkpoint_days: i64,
        /// Retention for collab-plans and collab-task-lists drawers
        #[arg(long, default_value_t = ironmem::db::retention::DEFAULT_COLLAB_ARTIFACT_RETENTION_DAYS)]
        collab_artifact_days: i64,
        /// Maximum candidates to inspect
        #[arg(long, default_value_t = ironmem::db::retention::DEFAULT_GC_LIMIT)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

/// Subcommands nested under `ironmem autopilot` (spec's *Repo onboarding*
/// section, steps 1 and 3 — step 2, gate inference, is
/// `ironmem::autopilot::onboard::infer_gate_commands`; `Run` is rung 4's
/// end-to-end single-issue loop).
///
/// `large_enum_variant` is allowed rather than satisfied: `Run` carries the
/// whole `RunConfig` surface as individual clap arguments, which makes it
/// far larger than its two siblings. The lint's concern — an oversized enum
/// copied around a hot path — does not apply to a command enum that is
/// parsed exactly once at startup and immediately destructured, and clippy's
/// suggested `Box<String>` fields would obscure the argument definitions
/// without changing anything that runs.
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum AutopilotCmd {
    /// Infer gate commands for a repo checkout and write a pending proposal
    Onboard {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// Local checkout to inspect for build manifests
        #[arg(long, default_value = ".")]
        path: String,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Approve a repo's pending gate-config proposal
    Approve {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Drive one issue end to end against its approved gate config (rung 4)
    Run {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// GitHub issue number
        issue: u64,
        /// Issue title, shown to the IC in its turn prompt
        #[arg(long)]
        title: String,
        /// Issue body text. Mutually exclusive with --body-file.
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the issue body from a file instead of the command line
        #[arg(long)]
        body_file: Option<String>,
        /// Local checkout of `repo` that worktrees are cut from
        #[arg(long, default_value = ".")]
        path: String,
        /// Directory that per-issue worktrees are created under
        #[arg(long)]
        worktree_root: Option<String>,
        /// Committish new issue branches are cut from
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Model for the IC dispatch
        #[arg(long, default_value = "claude-sonnet-5")]
        model: String,
        /// Dispatch-time risk class, recorded for the Reviewer to compare against
        #[arg(long = "class", default_value = ironmem::autopilot::lead::UNCLASSIFIED)]
        dispatch_class: String,
        /// Turns per dispatch (the N in "or stop after N turns")
        #[arg(long)]
        n_turns: Option<u32>,
        /// Hard --max-turns ceiling; must clear --n-turns with headroom
        #[arg(long)]
        max_turns: Option<u32>,
        /// Per-dispatch spend ceiling, passed through to --max-budget-usd
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Per-issue attempt cap, cumulative across runs
        #[arg(long)]
        attempt_cap: Option<u32>,
        /// Daily ledger ceiling across all dispatches
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Review an issue's open PR with a fresh-context Codex reviewer (rung 5)
    Review {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// GitHub issue number the PR closes
        issue: u64,
        /// Pull request number to review
        #[arg(long)]
        pr: u64,
        /// The dispatch-time risk class the Lead assigned, compared against
        /// the class the reviewer derives from the diff
        #[arg(long = "class")]
        dispatch_class: String,
        /// Checkout the reviewer reads (read-only to it)
        #[arg(long, default_value = ".")]
        path: String,
        /// Base branch the PR merges into
        #[arg(long, default_value = "main")]
        base: String,
        /// Head branch the IC pushed. Defaults to this issue's autopilot branch.
        #[arg(long)]
        head: Option<String>,
        /// Model for the reviewer; defaults to Codex's own configured model
        #[arg(long)]
        model: Option<String>,
        /// Assert the repo's gate was green when the PR was opened.
        ///
        /// Off by default, and deliberately an *opt-in assertion* rather than
        /// a `--gate-red` opt-out: auto-merge requires green **and** a
        /// reviewer PASS, so a caller who forgets the flag gets a hold, not a
        /// merge. The failure mode of the inverse spelling is shipping
        /// unreviewed code on a red gate.
        #[arg(long)]
        gate_green: bool,
        /// Daily ledger ceiling.
        ///
        /// Note this cannot bound a Codex reviewer on its own: Codex reports
        /// no price, so a review never moves the ledger's dollar total. Use
        /// --max-unpriced-reviews-per-day for that.
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// How many unpriced reviewer invocations may run today. The only
        /// bound on reviewer spend that actually holds today.
        #[arg(long)]
        max_unpriced_reviews_per_day: Option<u32>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Execute the merge decision for a reviewed PR (build-ladder rung 6)
    Merge {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// GitHub issue number the PR closes
        issue: u64,
        /// Pull request number to merge
        #[arg(long)]
        pr: u64,
        /// Directory `gh` runs in. Only affects `gh`'s own configuration
        /// resolution — every command names --repo explicitly.
        #[arg(long, default_value = ".")]
        path: String,
        /// Assert the repo's gate is green **right now**.
        ///
        /// Opt-in for the same reason `review --gate-green` is: a caller who
        /// forgets the flag gets a hold, never a merge.
        #[arg(long)]
        gate_green: bool,
        /// Merge strategy: squash (default), merge, or rebase
        #[arg(long, default_value = "squash")]
        strategy: String,
        /// Delete the head branch after a successful merge
        #[arg(long)]
        delete_branch: bool,
        /// Run every check and every read, but write nothing to GitHub
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Close out an issue that hit its attempt cap: summary comment plus
    /// `agent:exhausted` (build-ladder rung 6)
    Exhaust {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// GitHub issue number to exhaust
        issue: u64,
        /// Directory `gh` runs in
        #[arg(long, default_value = ".")]
        path: String,
        /// Render the comment and the label plan, but write nothing
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Create the three `agent:*` labels in a repo if they are missing
    /// (build-ladder rung 6)
    Labels {
        /// Repo identity (e.g. "owner/repo")
        repo: String,
        /// Directory `gh` runs in
        #[arg(long, default_value = ".")]
        path: String,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Run both supervision checks against one in-flight issue (rung 7)
    Supervise {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity (e.g. "owner/repo")
        repo: String,
        /// Issue number
        issue: u64,
        /// Seconds a session must be missing from the registry before its
        /// absence counts toward death (the spec's "short timeout")
        #[arg(long)]
        liveness_grace_secs: Option<u64>,
        /// Seconds the observable lineage/dispatch state must sit unchanged
        /// before staleness counts toward death (the "longer window")
        #[arg(long)]
        progress_window_secs: Option<u64>,
        /// Consecutive identical failures that count as thrashing
        #[arg(long)]
        thrash_threshold: Option<u32>,
        /// Clear a strategy escalation so the issue can be dispatched again.
        ///
        /// An escalation never self-resumes, exactly as `agent:exhausted`
        /// does not — this is the human re-label. Clears the redirect with
        /// it, since resuming while still carrying the redirect would
        /// re-escalate on the very next attempt.
        #[arg(long)]
        clear_escalation: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Rebuild the Lead's picture of in-flight work after a restart (rung 7)
    Reconcile {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Run one Lead tick: reconcile, unblock, supervise, choose, dispatch
    /// (build-ladder rung 8)
    Lead {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// A repo and its local checkout, as `owner/repo=/path/to/checkout`.
        /// Repeat for each repo this Lead works.
        #[arg(long = "repo", value_name = "OWNER/REPO=PATH", required = true)]
        repos: Vec<String>,
        /// Committish new issue branches are cut from, for every repo
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Directory that per-issue worktrees are created under
        #[arg(long)]
        worktree_root: Option<String>,
        /// Model for IC dispatches
        #[arg(long, default_value = "claude-sonnet-5")]
        model: String,
        /// Risk class for issues carrying no `risk:*` label.
        ///
        /// Defaults to `unclassified`, which **fails closed**: rung 5's
        /// merge decision cannot parse it, so such a PR holds for a human
        /// rather than auto-merging. Overriding this is a decision to let
        /// unjudged issues route as something specific.
        #[arg(long = "fallback-class", default_value = ironmem::autopilot::lead::UNCLASSIFIED)]
        fallback_class: String,
        /// Let the Lead make rung 9's three one-shot judgment calls:
        /// classify an unlabeled issue's risk, propose an alternative
        /// approach for a thrashing one, and draft the question an
        /// escalation puts to a human.
        ///
        /// **Off by default, and it spends money.** Every one of the three
        /// degrades to the rung-8 behaviour when it is off, refused, or
        /// fails: an unlabeled issue dispatches as `--fallback-class`, a
        /// redirect keeps its mechanical text, and an escalation is still
        /// reported on the issue, just without a drafted question.
        #[arg(long)]
        advisor: bool,
        /// Model for advisor calls
        #[arg(long, default_value = ironmem::autopilot::advise::DEFAULT_ADVICE_MODEL)]
        advisor_model: String,
        /// Per-call spend ceiling for advisor calls
        #[arg(long)]
        advice_budget_usd: Option<f64>,
        /// How many advisor calls one day may make, priced or not
        #[arg(long)]
        max_advice_calls: Option<u32>,
        /// How many issues one tick may dispatch
        #[arg(long)]
        max_dispatches: Option<usize>,
        /// Ceiling on concurrently in-flight ICs across every repo
        #[arg(long)]
        concurrency_cap: Option<usize>,
        /// Turns per dispatch (the N in "or stop after N turns")
        #[arg(long)]
        n_turns: Option<u32>,
        /// Per-dispatch spend ceiling
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Per-issue attempt cap, cumulative across runs
        #[arg(long)]
        attempt_cap: Option<u32>,
        /// Daily ledger ceiling across all dispatches
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// Read everything, plan everything, write nothing and spend nothing
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Show what the Lead would work on next, without touching anything
    /// (build-ladder rung 8)
    Queue {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo to include. Repeat for each one.
        #[arg(long = "repo", value_name = "OWNER/REPO", required = true)]
        repos: Vec<String>,
        /// Directory `gh` runs in
        #[arg(long, default_value = ".")]
        path: String,
        /// Ceiling on concurrently in-flight ICs across every repo
        #[arg(long)]
        concurrency_cap: Option<usize>,
        /// Per-issue attempt cap, cumulative across runs
        #[arg(long)]
        attempt_cap: Option<u32>,
        /// Per-dispatch spend ceiling, used for the same pre-authorization
        /// check `autopilot run` applies
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Daily ledger ceiling across all dispatches
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Post a question on an issue and flip it to `agent:blocked`
    /// (build-ladder rung 8)
    Ask {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity (e.g. "owner/repo")
        repo: String,
        /// Issue number
        issue: u64,
        /// The question. A human's reply after it becomes the answer.
        #[arg(long)]
        question: String,
        /// Directory `gh` runs in
        #[arg(long, default_value = ".")]
        path: String,
        /// Render the comment and the label plan, but write nothing
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Make one of rung 9's three judgment calls and print the answer
    ///
    /// Spends money: it runs a real, toolless, one-turn `claude` call and
    /// bills it to the day's ledger. Provided so an operator can see what
    /// the advisor says about one issue before letting `autopilot lead
    /// --advisor` act on it.
    Advise {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity (e.g. "owner/repo")
        repo: String,
        /// Issue number
        issue: u64,
        /// Which judgment to ask for
        #[arg(long, value_parser = ["risk", "redirect", "question"])]
        kind: String,
        /// Directory `gh` and the advisor run in
        #[arg(long, default_value = ".")]
        path: String,
        /// The repeated failure to redirect or escalate on. Required for
        /// `--kind redirect` and `--kind question`; read from the issue's
        /// supervision record when omitted.
        #[arg(long)]
        signature: Option<String>,
        /// Model for the call
        #[arg(long, default_value = ironmem::autopilot::advise::DEFAULT_ADVICE_MODEL)]
        model: String,
        /// Per-call spend ceiling
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Daily ledger ceiling, shared with dispatches and reviews
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Carry every succeeded issue's PR forward: review it, apply rung 6's
    /// merge decision, and clean up once it lands (build-ladder rung 10)
    ///
    /// The other half of `autopilot lead`. A tick starts work and records a
    /// success; this finishes it. Run them in that order.
    ///
    /// **Reviews by default, merges only with `--merge`.** Without it every
    /// merge is rehearsed — every guard and every read runs and nothing is
    /// written to GitHub — because a merge is the one irreversible action in
    /// this subsystem. Reviewing spends money on `codex`, bounded by the same
    /// daily ceilings `autopilot review` applies.
    Advance {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// A repo and its local checkout, as `owner/repo=/path/to/checkout`.
        /// Repeat for each repo this pass covers.
        #[arg(long = "repo", value_name = "OWNER/REPO=PATH", required = true)]
        repos: Vec<String>,
        /// Directory per-issue worktrees live under. Must be the one the
        /// Lead used: a review reads the issue's worktree, and an issue
        /// whose worktree is not found is reported rather than reviewed
        /// against a checkout that cannot see its branch.
        #[arg(long)]
        worktree_root: Option<String>,
        /// Execute merges instead of rehearsing them. **Irreversible.**
        #[arg(long)]
        merge: bool,
        /// Arm a re-dispatch when a review returns NEEDS CHANGES (rung 11),
        /// instead of holding the PR for a human straight away. This pass
        /// arms it; `autopilot lead` (or `autopilot run`) is what actually
        /// dispatches, so run them in that order. Each attempt costs a
        /// dispatch and spends the issue's --attempt-cap; on exhaustion the
        /// PR is held for a human exactly as it is without this flag.
        #[arg(long)]
        remediate: bool,
        /// Per-issue attempt cap. Must match the Lead's, since a remediation
        /// armed under one cap and dispatched under another either never
        /// fires or never stops.
        #[arg(long)]
        attempt_cap: Option<u32>,
        /// Merge strategy for `--merge`
        #[arg(long, default_value = "squash")]
        strategy: String,
        /// Delete the head branch after a successful merge
        #[arg(long)]
        delete_branch: bool,
        /// Model for the Codex reviewer
        #[arg(long)]
        model: Option<String>,
        /// How many issues one pass may carry forward
        #[arg(long)]
        max_advances: Option<usize>,
        /// How many `agent:ready` issues to list per repo
        #[arg(long)]
        max_issues_per_repo: Option<u32>,
        /// Daily ledger ceiling, shared with dispatches and reviews
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// How many unpriced reviewer runs one day may make
        #[arg(long)]
        max_unpriced_reviews_per_day: Option<u32>,
        /// Read everything, write nothing, spend nothing
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Read or clear an issue's armed re-dispatch (rung 11)
    ///
    /// A remediation is armed by `autopilot advance --remediate` when a
    /// reviewer returns NEEDS CHANGES, dispatched by `autopilot lead`, and
    /// ends by itself when the IC pushes a fix that goes green — there is
    /// nothing to clear on the happy path. `--clear` is the human override: it
    /// drops the record, so the Lead stops re-dispatching and the next
    /// `advance` pass holds the PR for a human instead.
    Remediate {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity (e.g. "owner/repo")
        repo: String,
        /// Issue number
        issue: u64,
        /// Drop the armed remediation, handing the PR back to a human
        #[arg(long)]
        clear: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Set or clear a repo's per-dispatch wall-clock bound (rung 7)
    Timeout {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity (e.g. "owner/repo")
        repo: String,
        /// Seconds one dispatch may run before it is killed. Omit to read the
        /// current value; pass --clear to remove the bound entirely.
        secs: Option<u64>,
        /// Remove the bound, leaving dispatches into this repo unbounded
        #[arg(long, conflicts_with = "secs")]
        clear: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

/// Parse one `--repo owner/repo=/path/to/checkout` argument.
///
/// Split on the **first** `=`: a checkout path may legitimately contain one,
/// a repo identifier may not. Splitting on the last would turn
/// `owner/repo=/srv/a=b` into a repo named `owner/repo=/srv/a`.
fn parse_repo_target(
    spec: &str,
    base: &str,
) -> Result<ironmem::autopilot::lead::RepoTarget, MemoryError> {
    let (repo, path) = spec.split_once('=').ok_or_else(|| {
        MemoryError::Validation(format!(
            "--repo must be `owner/repo=/path/to/checkout`, got '{spec}' — the Lead cuts \
             worktrees from a local checkout and has no way to guess where one is"
        ))
    })?;
    let repo = repo.trim();
    let path = path.trim();
    if repo.is_empty() || path.is_empty() {
        return Err(MemoryError::Validation(format!(
            "--repo '{spec}' has an empty repo or path"
        )));
    }
    Ok(ironmem::autopilot::lead::RepoTarget {
        repo: repo.to_string(),
        path: std::path::PathBuf::from(path),
        base: base.to_string(),
    })
}

/// The reviewer a dry run is handed.
///
/// Never called: `advance_pass` returns before reviewing when `dry_run` is
/// set. It exists so the `codex` binary is not required to *rehearse* a
/// pass, and it fails loudly rather than silently returning a verdict, so a
/// dry run that somehow reached a review would be visible instead of
/// fabricating one.
struct DryRunReviewer;

impl ironmem::autopilot::review::ReviewRunner for DryRunReviewer {
    fn review(
        &mut self,
        _repo_dir: &std::path::Path,
        _prompt: &str,
    ) -> Result<ironmem::autopilot::review::ReviewOutcome, MemoryError> {
        Err(MemoryError::NotFound(
            "a dry run does not review: this runner should never be called".into(),
        ))
    }
}

/// Human-readable rendering of one advance pass.
fn print_advance_report(report: &ironmem::autopilot::advance::AdvanceReport) {
    use ironmem::autopilot::advance::{AdvanceStep, Stall};
    use ironmem::autopilot::merge::MergeOutcome;
    use ironmem::autopilot::remediate::ArmOutcome;
    use ironmem::autopilot::worktree::WorktreeRemoval;

    println!(
        "Advance — {} carried forward, {} skipped, {} problem(s){}{}{}",
        report.advanced.len(),
        report.skipped.len(),
        report.problems.len(),
        if report.dry_run { " [dry run]" } else { "" },
        if report.merge_enabled {
            ""
        } else {
            " [merges rehearsed — pass --merge to execute]"
        },
        if report.remediate_enabled {
            ""
        } else {
            " [NEEDS CHANGES held for a human — pass --remediate to re-dispatch]"
        }
    );

    for step in &report.advanced {
        println!(
            "  {} (class {})",
            step.issue.canonical(),
            step.dispatch_class
        );
        match &step.step {
            AdvanceStep::Stalled(Stall::NoOpenPr { branch }) => {
                println!("    STALLED: no open PR on {branch}");
            }
            AdvanceStep::Stalled(Stall::AmbiguousPr { numbers }) => {
                println!(
                    "    STALLED: {} open PRs share this branch ({}) — close all but one",
                    numbers.len(),
                    numbers
                        .iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            AdvanceStep::Stalled(Stall::WorktreeMissing { path }) => {
                println!("    STALLED: needs a review and its worktree is gone ({path})");
                println!("    a reviewer pointed at a checkout without the branch reviews the wrong thing");
            }
            AdvanceStep::Review {
                pr_number,
                head_sha,
                gate_green,
                ..
            } => {
                println!(
                    "    PR #{pr_number} at {head_sha} — reviewing (gate green: {gate_green})"
                );
            }
            AdvanceStep::Merge {
                pr_number,
                head_sha,
                gate_green,
            } => {
                println!(
                    "    PR #{pr_number} at {head_sha} — already reviewed (gate green: {gate_green})"
                );
            }
        }
        if let Some(review) = &step.review {
            match &review.refusal {
                // Named rather than folded into the verdict line: a ceiling
                // that refused the review is "retry when the day rolls over",
                // and the pass stops the issue here rather than letting the
                // merge report it as unreviewed.
                Some(refusal) => println!(
                    "    review: NOT dispatched ({refusal:?}) — the merge was not attempted"
                ),
                None => println!(
                    "    review: {:?} / {:?}",
                    review.outcome.verdict, review.decision
                ),
            }
        }
        match &step.remediation {
            None => {}
            Some(ArmOutcome::Armed {
                pr_number,
                head_sha,
                has_findings,
            }) => {
                println!(
                    "    REMEDIATION armed on PR #{pr_number} at {head_sha}{} — the Lead will re-dispatch the IC to fix it",
                    if *has_findings {
                        ""
                    } else {
                        " (the reviewer gave no reason)"
                    }
                );
            }
            Some(ArmOutcome::AlreadyArmed {
                pr_number,
                head_sha,
                dispatches_since,
            }) => {
                println!(
                    "    remediation already in force on PR #{pr_number} at {head_sha} — {dispatches_since} attempt(s) since it was armed"
                );
            }
            Some(ArmOutcome::CapReached {
                cumulative_attempt_n,
                attempt_cap,
            }) => {
                println!(
                    "    remediation EXHAUSTED ({cumulative_attempt_n}/{attempt_cap} attempts) — the PR stays open for a human"
                );
            }
        }
        if let Some(exec) = &step.merge {
            match &exec.outcome {
                MergeOutcome::Merged { strategy, head_sha } => {
                    println!("    MERGED ({} of {head_sha})", strategy.as_str())
                }
                MergeOutcome::WouldMerge { strategy, head_sha } => println!(
                    "    would merge ({} of {head_sha}) — rehearsal, nothing written",
                    strategy.as_str()
                ),
                MergeOutcome::AlreadyMerged { head_sha } => {
                    println!("    already merged ({head_sha})")
                }
                MergeOutcome::Held(hold) => println!("    HELD: {}", hold.summary()),
            }
            if let Some(err) = &exec.label_error {
                println!("    WARNING: labels were NOT updated: {err}");
            }
        }
        if let Some(cleanup) = &step.cleanup {
            match &cleanup.worktree {
                WorktreeRemoval::Removed { path } => println!("    worktree removed: {path}"),
                WorktreeRemoval::Absent => println!("    worktree: nothing to remove"),
                WorktreeRemoval::DirtyRefused { path } => println!(
                    "    worktree KEPT (uncommitted changes the merge did not include): {path}"
                ),
            }
            if cleanup.dispatch_state_cleared {
                println!("    dispatch state cleared");
            }
            if let Some(err) = &cleanup.error {
                // The merge landed regardless. Silent here, a failed cleanup
                // would be indistinguishable from a clean one.
                println!("    WARNING: cleanup did not finish: {err}");
            }
        }
    }

    for skipped in &report.skipped {
        println!(
            "  skip {} — {:?}",
            skipped.issue.canonical(),
            skipped.reason
        );
    }
    for problem in &report.problems {
        println!("  PROBLEM {}: {}", problem.what, problem.detail);
    }
}

/// Human-readable rendering of one queue plan.
fn print_queue_plan(plan: &ironmem::autopilot::queue::QueuePlan) {
    println!(
        // `occupied_slots` counts in-flight work this pass did *not* pick, so
        // it is not the number of slots in use — the selected resumes hold
        // slots too. Both are named rather than one being printed under the
        // other's label.
        "Queue — {} to dispatch, {} deferred, {}/{} slots in use after this pass \
         ({} held by work not picked), ${:.4} of ${:.2} spent today",
        plan.dispatch.len(),
        plan.deferred.len(),
        plan.occupied_slots + plan.dispatch.len(),
        plan.concurrency_cap,
        plan.occupied_slots,
        plan.spent_today_usd,
        plan.daily_budget_usd,
    );
    for (i, queued) in plan.dispatch.iter().enumerate() {
        println!(
            "  {}. {} [{:?}]{}{} — {}",
            i + 1,
            queued.issue.canonical(),
            queued.priority,
            if queued.resuming { " (resume)" } else { "" },
            queued
                .risk_label
                .as_ref()
                .map(|c| format!(" risk:{c}"))
                .unwrap_or_default(),
            queued.title,
        );
    }
    for deferred in &plan.deferred {
        println!(
            "  — {} deferred: {:?}",
            deferred.issue.canonical(),
            deferred.reason
        );
    }
}

/// Human-readable rendering of one Lead tick.
fn print_lead_report(report: &ironmem::autopilot::lead::LeadReport) {
    if report.dry_run {
        println!("DRY RUN — nothing was written and nothing was spent");
    }
    if !report.registry_available {
        println!("  registry: UNREADABLE — issues with a live session are held this tick");
    }
    for row in &report.reconciliation {
        println!(
            "  reconcile {}: {:?}",
            row.issue
                .as_ref()
                .map(|i| i.canonical())
                .unwrap_or_else(|| row.session_name.clone()),
            row.verdict
        );
    }
    for row in &report.blocked {
        println!("  blocked {}: {:?}", row.issue.canonical(), row.poll);
    }
    for row in &report.supervision {
        println!("  supervise {}: {:?}", row.issue.canonical(), row.action);
    }
    print_queue_plan(&report.plan);
    for run in &report.runs {
        println!(
            "  ran {} — {} dispatch(es), ${:.4}, terminal {:?}",
            run.issue.canonical(),
            run.dispatches.len(),
            run.total_cost_usd,
            run.terminal,
        );
    }
    for row in &report.advice {
        // The cost prints as "unknown" rather than as $0.0000 when it could
        // not be read: an operator reading a ledger needs to see a floor as
        // a floor.
        let cost = row
            .total_cost_usd
            .map(|c| format!("${c:.4}"))
            .unwrap_or_else(|| "unknown cost".to_string());
        println!(
            "  advice {} on {}: {:?} ({cost})",
            row.kind.as_str(),
            row.issue.canonical(),
            row.status,
        );
    }
    for notice in &report.escalation_notices {
        println!(
            "  escalation reported on {} — {}",
            notice.issue.canonical(),
            if notice.drafted_question {
                "with a drafted question"
            } else {
                "no question drafted"
            },
        );
    }
    for problem in &report.problems {
        println!("  PROBLEM {}: {}", problem.what, problem.detail);
    }
}

/// Where per-issue worktrees live when `--worktree-root` is not given.
///
/// Deliberately *outside* any target repo: a worktree nested inside its own
/// repo shows up in that repo's `git status` and in every glob a build script
/// runs, so Autopilot's checkouts sit beside the database instead, under the
/// same `~/.ironrace-memory` base directory `Config` already owns.
fn default_worktree_root() -> Result<std::path::PathBuf, MemoryError> {
    let home = dirs::home_dir()
        .ok_or_else(|| MemoryError::Config("Cannot determine home directory".into()))?;
    Ok(home.join(".ironrace-memory").join("autopilot-worktrees"))
}

/// Load config, open, and migrate the database — the sequence every
/// `Autopilot` subcommand handler needs before touching storage. Several
/// other command handlers in this file repeat the same three-line sequence
/// inline rather than through a shared helper; that pre-existing duplication
/// is out of scope here, but the new `Autopilot` arms at least don't add
/// to it.
fn open_migrated_db(db: Option<String>) -> Result<ironmem::db::schema::Database, MemoryError> {
    let cfg = config::Config::load(db)?;
    let database = ironmem::db::schema::Database::open(&cfg.db_path)?;
    database.migrate()?;
    Ok(database)
}

/// Subcommands nested under `ironmem symbols`.
#[derive(Subcommand)]
enum SymbolsCmd {
    /// Index a git repository (Rust + Python source files)
    Index {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// Re-index every file even if content hash is unchanged
        #[arg(long)]
        force: bool,
        /// Emit JSON instead of prose summary
        #[arg(long)]
        json: bool,
    },
    /// Look up symbol declarations by name
    Lookup {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// Name or qualified-name prefix to search for
        query: String,
        /// Filter by kind (fn, struct, enum, class, …)
        #[arg(long)]
        kind: Option<String>,
        /// Maximum number of results (capped at 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of prose
        #[arg(long)]
        json: bool,
    },
    /// Look up import statements by file path or module name
    Imports {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// File path (repo-relative) or module name prefix
        query: String,
        /// Maximum number of results (capped at 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of prose
        #[arg(long)]
        json: bool,
    },
    /// Look up symbol-graph edges (neighbors) by symbol id or file path
    Neighbors {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// Symbol id or file path prefix
        query: String,
        /// Maximum number of results (capped at 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of prose
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(
                "ironmem=info"
                    .parse()
                    .expect("static directive literal is always valid"),
            ),
        )
        .init();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), MemoryError> {
    match cli.command {
        Commands::Serve {
            db,
            listen,
            connect,
            no_autospawn,
        } => {
            let cfg = config::Config::load(db)?;

            // `--listen <socket>`: run as the shared daemon (Unix only, Task 6).
            // `run_daemon` owns its own runtime, so it must not nest inside this
            // `#[tokio::main]` runtime — run it on a dedicated std thread and
            // join. On the None path `cfg` is untouched (the taken branch
            // diverges via `return`), so the stdio fallback below still owns it.
            #[cfg(unix)]
            if let Some(sock) = listen {
                let socket_path = std::path::PathBuf::from(sock);
                return std::thread::spawn(move || mcp::daemon::run_daemon(cfg, socket_path))
                    .join()
                    .map_err(|_| MemoryError::Config("daemon thread panicked".into()))?;
            }
            // On non-unix, `--listen` has no daemon transport yet (Task 10 adds a
            // fallback); consume the flag so the in-process stdio server runs.
            #[cfg(not(unix))]
            let _ = listen;

            // `--connect <socket>`: run as a thin proxy (Unix only, Task 8).
            // `no_autospawn` (CLI flag) forces auto-spawn off regardless of the
            // `IRONMEM_NO_DAEMON` env var; otherwise the env-derived Config
            // setting decides. A successful proxy session returns straight
            // from here; `FallbackToInProcess` (no daemon + autospawn disabled)
            // falls through to the same in-process stdio server used by bare
            // `serve`, below.
            #[cfg(unix)]
            if let Some(sock) = connect {
                let socket_path = std::path::PathBuf::from(sock);
                let autospawn_enabled = !no_autospawn && cfg.daemon_autospawn_enabled();
                // M3: forward this proxy's own resolved db_path so an
                // auto-spawned daemon serves the SAME database, not the
                // default. H5: redirect an auto-spawned daemon's stderr to
                // `<state_dir>/daemon.log` instead of discarding it.
                let daemon_log_path = cfg.state_dir.join("daemon.log");
                match mcp::daemon::run_connect_mode(
                    &socket_path,
                    autospawn_enabled,
                    &cfg.db_path,
                    &daemon_log_path,
                )
                .await?
                {
                    mcp::daemon::ProxyOutcome::Proxied => return Ok(()),
                    mcp::daemon::ProxyOutcome::FallbackToInProcess => {}
                }
            }
            // On non-unix, `--connect` has no proxy transport yet (Task 10 adds
            // a fallback); consume the flags so the in-process stdio server runs.
            #[cfg(not(unix))]
            let _ = (connect, no_autospawn);

            // Phase 1: fast server-ready init (DB open + schema migrate, ~50ms).
            // App is not Sync (single-threaded stdio server, block_in_place dispatch).
            #[allow(clippy::arc_with_non_send_sync)]
            let app = std::sync::Arc::new(mcp::app::App::new_server_ready(cfg.clone())?);
            // Phase 2: model load + bootstrap run in a background thread with its own
            // DB connection (SQLite WAL handles concurrent access safely).
            bootstrap::check_and_record_version(&cfg.state_dir);
            let memory_ready = std::sync::Arc::clone(&app.memory_ready);
            bootstrap::run_background_memory_init(cfg, memory_ready);
            // MCP stdio loop starts immediately — initialize responds in <100ms.
            mcp::server::run_server(app).await
        }
        Commands::Init => {
            let cfg = config::Config::load(None)?;
            cfg.ensure_dirs()?;
            eprintln!("Memory store initialized at {}", cfg.db_path.display());
            Ok(())
        }
        Commands::Setup => {
            let cfg = config::Config::load(None)?;
            if matches!(cfg.embed_mode, config::EmbedMode::Noop) {
                eprintln!("Noop embedder mode enabled; skipping model setup.");
                return Ok(());
            }
            let allow_download = !cfg.model_dir_explicit;
            if allow_download {
                eprintln!(
                    "Preparing embedding model in {}...",
                    cfg.model_dir.display()
                );
            } else {
                eprintln!(
                    "Verifying embedding model in {}...",
                    cfg.model_dir.display()
                );
            }
            ironrace_embed::embedder::ensure_model_in_dir(&cfg.model_dir, allow_download)?;
            eprintln!("Model ready at {}.", cfg.model_dir.display());
            Ok(())
        }
        Commands::Mine { path } => {
            let cfg = config::Config::load(None)?;
            let app = mcp::app::App::new(cfg)?;
            ingest::mine_directory(&app, &path)?;
            Ok(())
        }
        Commands::Migrate { from } => {
            let cfg = config::Config::load(None)?;
            let app = mcp::app::App::new(cfg)?;
            migrate::chromadb::migrate_from_chromadb(&from, &app)?;
            Ok(())
        }
        Commands::Reembed { wing } => {
            let cfg = config::Config::load(None)?;
            let app = mcp::app::App::new(cfg)?;
            reembed::reembed_all(&app, wing.as_deref())?;
            Ok(())
        }
        Commands::Hook { name, harness } => {
            let cfg = config::Config::load(None)?;
            let response = ironmem::hook::run_hook(&name, &harness, cfg)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
        Commands::Report {
            db,
            task,
            since,
            json,
        } => {
            let cfg = config::Config::load(db)?;
            let database = ironmem::db::schema::Database::open(&cfg.db_path)?;
            database.migrate()?;
            let opts = report::ReportOptions { task, since };
            let report = report::run_report(&database, &opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report::render_text(&report));
            }
            Ok(())
        }
        Commands::Doctor { db, json } => {
            let cfg = config::Config::load(db)?;
            // #190 Task 14: extends run_doctor with the shared-daemon health
            // probe + auto-spawn config, which need an async socket connect.
            let report = ironmem::doctor::run_doctor_with_daemon(&cfg).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", ironmem::doctor::render_text(&report));
            }
            // Diagnose-only: a clean run exits 0; blocking setup failures exit
            // non-zero so scripts and CI can gate on `ironmem doctor`.
            if report.has_blocking() {
                process::exit(2);
            }
            Ok(())
        }
        Commands::Context {
            db,
            repo,
            task,
            areas,
            budget,
            json,
        } => {
            let cfg = config::Config::load(db)?;
            let app = mcp::app::App::new(cfg)?;
            let opts = context::ContextPackOptions {
                repo: std::path::PathBuf::from(repo),
                task,
                areas,
                budget_tokens: budget,
            };
            let pack = context::run_context(&app, &opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pack)?);
            } else {
                print!("{}", context::render::render_text(&pack));
            }
            Ok(())
        }
        Commands::Claude {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Claude,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Codex {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Codex,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Grok {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Grok,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Gemini {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Gemini,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Dashboard {
            db,
            host,
            port,
            allow_non_loopback,
            json,
            exit_on_stdin_close,
        } => {
            let cfg = config::Config::load(db)?;
            let host_addr: std::net::IpAddr = host.parse().map_err(|e| {
                MemoryError::Validation(format!("invalid host address {host:?}: {e}"))
            })?;
            let dash_cfg = dashboard::DashboardConfig {
                db_path: cfg.db_path.clone(),
                host: host_addr,
                port,
                allow_non_loopback,
                json_startup: json,
                exit_on_stdin_close,
                // Reuse the same model-dir resolution as the rest of the CLI so
                // warming status reflects the actual embed cache the binary uses.
                model_dir: cfg.model_dir.clone(),
            };
            dashboard::run_dashboard(dash_cfg).await
        }
        Commands::ReviewDiff {
            repo,
            worktree,
            base,
            head,
            expand_file,
            hunk,
        } => {
            let request = match (worktree, base, head) {
                (true, None, None) => ReviewDiffRequest::worktree(repo),
                (false, Some(base), Some(head)) => ReviewDiffRequest::range(repo, base, head),
                _ => {
                    return Err(MemoryError::Validation(
                        "review-diff source requires exactly --worktree or both --base <rev> --head <rev>"
                            .into(),
                    ));
                }
            };
            if hunk == Some(0) {
                return Err(MemoryError::Validation(
                    "review-diff hunk ordinal must be one-based".into(),
                ));
            }
            match expand_file {
                Some(path) => print!("{}", expand_review_diff(&request, &path, hunk)?),
                None => print!("{}", build_review_diff(&request)?.rendered),
            }
            Ok(())
        }
        Commands::Symbols { cmd } => {
            let cfg = config::Config::load(None)?;
            let db = ironmem::db::schema::Database::open(&cfg.db_path)?;
            db.migrate()?;
            match cmd {
                SymbolsCmd::Index { repo, force, json } => {
                    let result = symbol_graph::index_repo(&db, &repo, force)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else {
                        eprintln!(
                            "indexed: {} files, {} symbols, {} imports, {} edges ({} skipped, {} purged)",
                            result.files_indexed,
                            result.symbols_inserted,
                            result.imports_inserted,
                            result.edges_inserted,
                            result.files_skipped,
                            result.files_purged,
                        );
                    }
                }
                SymbolsCmd::Lookup {
                    repo,
                    query,
                    kind,
                    limit,
                    json,
                } => {
                    let canonical = symbol_graph::canonicalize_repo(&repo)?;
                    let results = symbol_graph::lookup_symbols(
                        &db,
                        &canonical,
                        &query,
                        kind.as_deref(),
                        limit,
                    )?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&results)?);
                    } else {
                        for s in &results {
                            println!(
                                "{} {} {}:{} ({})",
                                s.kind, s.qualified_name, s.path, s.start_line, s.language,
                            );
                        }
                        eprintln!("{} result(s)", results.len());
                    }
                }
                SymbolsCmd::Imports {
                    repo,
                    query,
                    limit,
                    json,
                } => {
                    let canonical = symbol_graph::canonicalize_repo(&repo)?;
                    let results = symbol_graph::lookup_imports(&db, &canonical, &query, limit)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&results)?);
                    } else {
                        for imp in &results {
                            println!("{} → {} (line {})", imp.path, imp.module, imp.line);
                        }
                        eprintln!("{} result(s)", results.len());
                    }
                }
                SymbolsCmd::Neighbors {
                    repo,
                    query,
                    limit,
                    json,
                } => {
                    let canonical = symbol_graph::canonicalize_repo(&repo)?;
                    let results = symbol_graph::lookup_neighbors(&db, &canonical, &query, limit)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&results)?);
                    } else {
                        for e in &results {
                            println!("{} --{}--> {}", e.from_id, e.edge_kind, e.to_ref);
                        }
                        eprintln!("{} result(s)", results.len());
                    }
                }
            }
            Ok(())
        }
        Commands::Memory { cmd } => match cmd {
            MemoryCmd::Gc {
                db,
                dry_run: _,
                apply,
                collab_checkpoint_days,
                collab_artifact_days,
                limit,
                json,
            } => {
                let cfg = config::Config::load(db)?;
                let database = ironmem::db::schema::Database::open(&cfg.db_path)?;
                database.migrate()?;
                let report = ironmem::db::retention::run_memory_gc(
                    &database,
                    ironmem::db::retention::MemoryGcOptions {
                        apply,
                        collab_checkpoint_days,
                        collab_artifact_days,
                        limit,
                    },
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print!(
                        "{}",
                        ironmem::db::retention::render_memory_gc_report(&report)
                    );
                }
                Ok(())
            }
        },
        Commands::Autopilot { cmd } => match cmd {
            AutopilotCmd::Onboard {
                db,
                repo,
                path,
                json,
            } => {
                let database = open_migrated_db(db)?;
                let config = ironmem::autopilot::onboard::onboard_repo(
                    &database,
                    &repo,
                    std::path::Path::new(&path),
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    println!(
                        "Proposed gate config for '{}' (pending approval):",
                        config.repo
                    );
                    for gate_command in config.gate_commands() {
                        println!("  - {gate_command}");
                    }
                    if !config.manifest_warnings.is_empty() {
                        println!("Warnings (review before approving):");
                        for warning in &config.manifest_warnings {
                            println!("  - {warning}");
                        }
                    }
                    println!("Run `ironmem autopilot approve {}` to accept.", config.repo);
                }
                Ok(())
            }
            AutopilotCmd::Approve { db, repo, json } => {
                let database = open_migrated_db(db)?;
                let config =
                    ironmem::autopilot::gate_config::approve_gate_config(&database, &repo)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    println!("Approved gate config for '{}'.", config.repo);
                }
                Ok(())
            }
            AutopilotCmd::Run {
                db,
                repo,
                issue,
                title,
                body,
                body_file,
                path,
                worktree_root,
                base,
                model,
                dispatch_class,
                n_turns,
                max_turns,
                max_budget_usd,
                attempt_cap,
                daily_budget_usd,
                json,
            } => {
                use ironmem::autopilot::run::RunConfig;

                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let body = match (body, body_file) {
                    (Some(text), _) => text,
                    (None, Some(file)) => std::fs::read_to_string(&file).map_err(|e| {
                        MemoryError::NotFound(format!("cannot read --body-file {file}: {e}"))
                    })?,
                    (None, None) => String::new(),
                };

                let mut config = RunConfig::new(model, dispatch_class);
                if let Some(n) = n_turns {
                    config.n_turns = n;
                    // Keep the documented headroom when only N was supplied,
                    // rather than silently failing validation against the
                    // default `max_turns` that was derived from the default N.
                    config.max_turns =
                        n.saturating_add(ironmem::autopilot::run::DEFAULT_MAX_TURNS_HEADROOM);
                }
                if let Some(max) = max_turns {
                    config.max_turns = max;
                }
                if let Some(budget) = max_budget_usd {
                    config.max_budget_usd = budget;
                }
                if let Some(cap) = attempt_cap {
                    config.attempt_cap = cap;
                }
                if let Some(daily) = daily_budget_usd {
                    config.daily_budget_usd = daily;
                }
                // Validate before provisioning a worktree: a bad config
                // should not leave a checkout behind.
                config.validate()?;

                let database = open_migrated_db(db)?;
                // Refuse an unapproved repo *before* creating a worktree and
                // branch for it — `run_issue` checks this too, but by then
                // the checkout already exists on disk.
                ironmem::autopilot::run::approved_gate_commands(&database, &issue_ref.repo)?;

                let worktree_root = match worktree_root {
                    Some(dir) => std::path::PathBuf::from(dir),
                    None => default_worktree_root()?,
                };
                let worktree = ironmem::autopilot::worktree::ensure_worktree(
                    std::path::Path::new(&path),
                    &worktree_root,
                    &issue_ref,
                    &base,
                )?;
                let mut dispatcher = ironmem::autopilot::run::ClaudeDispatcher::resolve()?;
                let run = ironmem::autopilot::run::run_issue(
                    &database,
                    &issue_ref,
                    &ironmem::autopilot::run::IssueBrief { title, body },
                    &worktree,
                    &config,
                    &mut dispatcher,
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&run)?);
                } else {
                    println!("Issue {}", run.issue.canonical());
                    println!("  worktree: {}", worktree.path.display());
                    if let Some(quarantined) = &worktree.quarantined_from {
                        println!("  quarantined dirty worktree to: {}", quarantined.display());
                    }
                    println!("  dispatches: {}", run.dispatches.len());
                    for (i, dispatch) in run.dispatches.iter().enumerate() {
                        println!(
                            "    {}. {:?} — {} turns, ${:.4}{}",
                            i + 1,
                            dispatch.classification,
                            dispatch.num_turns,
                            dispatch.total_cost_usd,
                            dispatch
                                .attempt_n
                                .map(|n| format!(" (attempt {n})"))
                                .unwrap_or_default()
                        );
                    }
                    println!("  spend this run: ${:.4}", run.total_cost_usd);
                    println!("  cumulative attempts: {}", run.cumulative_attempt_n);
                    println!("  terminal: {:?}", run.terminal);
                }
                Ok(())
            }
            AutopilotCmd::Review {
                db,
                repo,
                issue,
                pr,
                dispatch_class,
                path,
                base,
                head,
                model,
                gate_green,
                daily_budget_usd,
                max_unpriced_reviews_per_day,
                json,
            } => {
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let head_branch =
                    head.unwrap_or_else(|| ironmem::autopilot::worktree::branch_name(&issue_ref));

                let database = open_migrated_db(db)?;
                // Refuse an unapproved repo before spending anything: the
                // reviewer's prompt is built from the *approved* gate
                // commands, so there is nothing coherent to dispatch without
                // them.
                let gate_commands =
                    ironmem::autopilot::run::approved_gate_commands(&database, &issue_ref.repo)?;

                let mut runner = ironmem::autopilot::review::CodexReviewer::resolve(model)?;
                let mut review = ironmem::autopilot::review::review_pr(
                    &database,
                    &mut runner,
                    &ironmem::autopilot::review::ReviewRequest {
                        issue: &issue_ref,
                        pr_number: pr,
                        base_branch: &base,
                        head_branch: &head_branch,
                        // `review_pr` resolves this from `repo_dir` and
                        // `head_branch`; the field is an override, and the
                        // CLI has nothing to override it with.
                        head_sha: None,
                        dispatch_class: &dispatch_class,
                        gate_commands: &gate_commands,
                        gate_green,
                        repo_dir: std::path::Path::new(&path),
                        daily_budget_usd: daily_budget_usd
                            .unwrap_or(ironmem::autopilot::run::DEFAULT_DAILY_BUDGET_USD),
                        max_unpriced_reviews_per_day: max_unpriced_reviews_per_day.unwrap_or(
                            ironmem::autopilot::review::DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY,
                        ),
                    },
                )?;

                // `record_review` scrubs the reason on the way into storage
                // because a review reason quotes the diff and can carry
                // anything the diff carried. The *emit* path needs the same
                // guarantee: this string goes to stdout and, under --json,
                // into whatever a Lead logs. Scrubbed here rather than inside
                // `review_pr` so the drawer's `reason_redacted` flag still
                // records that a redaction happened.
                if let Some(reason) = review.outcome.reason.take() {
                    review.outcome.reason = Some(
                        ironmem::autopilot::scrub::scrub_and_bound(
                            &reason,
                            ironmem::autopilot::lineage::MAX_LINEAGE_FIELD_CHARS,
                        )
                        .text,
                    );
                }

                if json {
                    println!("{}", serde_json::to_string_pretty(&review)?);
                } else {
                    println!(
                        "Issue {} PR #{}",
                        review.issue.canonical(),
                        review.pr_number
                    );
                    if let Some(refusal) = &review.refusal {
                        println!("  reviewer: not dispatched ({refusal:?})");
                    }
                    println!("  verdict: {:?}", review.outcome.verdict);
                    println!("  diff risk class: {:?}", review.outcome.risk_class);
                    if let Some(reason) = &review.outcome.reason {
                        println!("  reason: {reason}");
                    }
                    if let Some(usage) = &review.outcome.token_usage {
                        println!(
                            "  tokens: {} in ({} cached), {} out — no price reported by Codex",
                            usage.input_tokens, usage.cached_input_tokens, usage.output_tokens
                        );
                    }
                    println!("  decision: {:?}", review.decision);
                    if let Some(id) = &review.record_drawer_id {
                        println!("  recorded: {id}");
                    }
                }
                Ok(())
            }
            AutopilotCmd::Merge {
                db,
                repo,
                issue,
                pr,
                path,
                gate_green,
                strategy,
                delete_branch,
                dry_run,
                json,
            } => {
                let strategy =
                    ironmem::autopilot::gh::MergeStrategy::parse(&strategy).ok_or_else(|| {
                        ironmem::error::MemoryError::Config(format!(
                            "unknown merge strategy {strategy:?} — expected squash, merge or rebase"
                        ))
                    })?;
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let database = open_migrated_db(db)?;
                let mut gh = ironmem::autopilot::gh::GhCli::resolve(&path)?;
                let exec = ironmem::autopilot::merge::execute_merge(
                    &database,
                    &mut gh,
                    &ironmem::autopilot::merge::MergeRequest {
                        issue: &issue_ref,
                        pr_number: pr,
                        gate_green,
                        strategy,
                        delete_branch,
                        dry_run,
                    },
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&exec)?);
                } else {
                    println!("Issue {} PR #{}", exec.issue.canonical(), exec.pr_number);
                    match &exec.outcome {
                        ironmem::autopilot::merge::MergeOutcome::Merged { strategy, head_sha } => {
                            println!("  MERGED ({} of {head_sha})", strategy.as_str());
                        }
                        ironmem::autopilot::merge::MergeOutcome::WouldMerge {
                            strategy,
                            head_sha,
                        } => {
                            println!(
                                "  would merge ({} of {head_sha}) — dry run, nothing written",
                                strategy.as_str()
                            );
                        }
                        ironmem::autopilot::merge::MergeOutcome::AlreadyMerged { head_sha } => {
                            println!("  already merged ({head_sha}) — Autopilot did not merge it");
                        }
                        ironmem::autopilot::merge::MergeOutcome::Held(hold) => {
                            println!("  HELD: {}", hold.summary());
                        }
                    }
                    if let Some(plan) = &exec.label_plan {
                        if !plan.is_noop() {
                            println!("  labels: +{:?} -{:?}", plan.add, plan.remove);
                        }
                    }
                    if let Some(err) = &exec.label_error {
                        // The merge landed and the label write did not. Silent
                        // here, this was indistinguishable from a clean run —
                        // which defeats the point of reporting it rather than
                        // erroring.
                        println!("  WARNING: labels were NOT updated: {err}");
                        println!("  the next run clears them once it sees the PR as merged");
                    }
                    if exec.commented {
                        println!("  commented on the issue");
                    }
                    println!(
                        "  {}: {}",
                        if exec.record_appended {
                            "recorded"
                        } else {
                            "unchanged since"
                        },
                        exec.record_drawer_id
                    );
                }
                Ok(())
            }
            AutopilotCmd::Exhaust {
                db,
                repo,
                issue,
                path,
                dry_run,
                json,
            } => {
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let database = open_migrated_db(db)?;
                let mut gh = ironmem::autopilot::gh::GhCli::resolve(&path)?;
                let exec = ironmem::autopilot::stagnation::exhaust_issue(
                    &mut gh, &database, &issue_ref, dry_run,
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&exec)?);
                } else {
                    println!("Issue {}", exec.issue.canonical());
                    use ironmem::autopilot::stagnation::ExhaustOutcome;
                    match &exec.outcome {
                        ExhaustOutcome::AlreadyExhausted => {
                            println!("  already agent:exhausted — nothing to do")
                        }
                        ExhaustOutcome::Exhausted {
                            label_plan,
                            attempts_summarized,
                            commented,
                        } => println!(
                            "  exhausted: {} {attempts_summarized} attempt(s), labels +{:?} -{:?}",
                            if *commented {
                                "commented on"
                            } else {
                                "summary already posted for"
                            },
                            label_plan.add,
                            label_plan.remove
                        ),
                        ExhaustOutcome::WouldExhaust {
                            label_plan,
                            attempts_summarized,
                        } => println!(
                            "  would exhaust: {attempts_summarized} attempt(s) summarized, labels +{:?} -{:?} — dry run, nothing written",
                            label_plan.add, label_plan.remove
                        ),
                    }
                }
                Ok(())
            }
            AutopilotCmd::Labels { repo, path, json } => {
                let mut gh = ironmem::autopilot::gh::GhCli::resolve(&path)?;
                let results = ironmem::autopilot::labels::ensure_labels(&mut gh, &repo)?;
                if json {
                    let rows: Vec<serde_json::Value> = results
                        .iter()
                        .map(|(label, state)| {
                            serde_json::json!({ "label": label.as_str(), "state": state })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    for (label, state) in results {
                        println!("  {} — {state:?}", label.as_str());
                    }
                }
                Ok(())
            }
            AutopilotCmd::Supervise {
                db,
                repo,
                issue,
                liveness_grace_secs,
                progress_window_secs,
                thrash_threshold,
                clear_escalation,
                json,
            } => {
                use ironmem::autopilot::supervise::{
                    ProcessHealth, StrategyHealth, SupervisionAction, SupervisionConfig,
                };
                let database = open_migrated_db(db)?;
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let defaults = SupervisionConfig::default();
                let config = SupervisionConfig {
                    liveness_grace_secs: liveness_grace_secs
                        .unwrap_or(defaults.liveness_grace_secs),
                    progress_window_secs: progress_window_secs
                        .unwrap_or(defaults.progress_window_secs),
                    thrash_threshold: thrash_threshold.unwrap_or(defaults.thrash_threshold),
                };
                config.validate()?;

                if clear_escalation {
                    let cleared =
                        ironmem::autopilot::supervise::clear_escalation(&database, &issue_ref)?;
                    if cleared {
                        println!(
                            "cleared the strategy escalation on {} — it can be dispatched again",
                            issue_ref.canonical()
                        );
                    } else {
                        println!(
                            "{} was not escalated; nothing to clear",
                            issue_ref.canonical()
                        );
                    }
                    return Ok(());
                }

                let mut registry = ironmem::autopilot::registry::ClaudeAgentRegistry::resolve()?;
                let snapshot = ironmem::autopilot::registry::snapshot(&mut registry);
                let report = ironmem::autopilot::supervise::supervise_issue(
                    &database, &issue_ref, &snapshot, &config,
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!("{}", issue_ref.canonical());
                    if !report.in_flight {
                        println!("  not in flight — no dispatch state; process-health is vacuous");
                    }
                    match &report.process {
                        ProcessHealth::Healthy => {
                            println!("  process: alive and making progress")
                        }
                        ProcessHealth::AliveButStalled { stalled_for_secs } => println!(
                            "  process: listed, but nothing observable has changed in \
                             {stalled_for_secs}s — not a death, and not restartable"
                        ),
                        ProcessHealth::SilentNotDead {
                            absent_for_secs,
                            stalled_for_secs,
                            reason,
                        } => println!(
                            "  process: silent but NOT dead ({reason:?}) — absent {absent_for_secs}s, \
                             stalled {stalled_for_secs}s"
                        ),
                        ProcessHealth::Dead {
                            absent_for_secs,
                            stalled_for_secs,
                        } => println!(
                            "  process: DEAD — absent {absent_for_secs}s and stalled \
                             {stalled_for_secs}s"
                        ),
                        ProcessHealth::Unknown { reason } => {
                            println!("  process: unknown — {reason}")
                        }
                    }
                    match &report.strategy {
                        StrategyHealth::Ok => println!("  strategy: no repeated-failure pattern"),
                        StrategyHealth::Thrashing {
                            signature,
                            consecutive,
                        } => println!(
                            "  strategy: THRASHING — {consecutive} identical failures: {signature}"
                        ),
                    }
                    match &report.action {
                        SupervisionAction::None => println!("  action: none"),
                        SupervisionAction::Hold { reason } => {
                            println!("  action: hold — {reason}")
                        }
                        SupervisionAction::RestartFromCheckpoint { .. } => println!(
                            "  action: restart from checkpoint — run `ironmem autopilot run {} {}`",
                            issue_ref.repo, issue_ref.number
                        ),
                        SupervisionAction::Redirect { .. } => println!(
                            "  action: strategy redirect issued — it is in force for the next \
                             dispatch"
                        ),
                        SupervisionAction::Escalate { reason, .. } => {
                            println!("  action: ESCALATE to a human — {reason}")
                        }
                    }
                }
                Ok(())
            }
            AutopilotCmd::Reconcile { db, json } => {
                use ironmem::autopilot::supervise::ReconcileVerdict;
                let database = open_migrated_db(db)?;
                let mut registry = ironmem::autopilot::registry::ClaudeAgentRegistry::resolve()?;
                let snapshot = ironmem::autopilot::registry::snapshot(&mut registry);
                let rows = ironmem::autopilot::supervise::reconcile(&database, &snapshot)?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else if rows.is_empty() {
                    println!("nothing in flight, and no unrecognized IC sessions");
                } else {
                    for row in &rows {
                        let who = match &row.issue {
                            Some(issue) => issue.canonical(),
                            None => "(no dispatch state)".to_string(),
                        };
                        match &row.verdict {
                            ReconcileVerdict::Adopt => {
                                println!(
                                    "  {who} [{}] — adopt, resume supervision",
                                    row.session_name
                                )
                            }
                            ReconcileVerdict::RestartFromCheckpoint { session_claimed } => {
                                let how = if *session_claimed {
                                    "resume its session from the last checkpoint"
                                } else {
                                    "no session was ever opened — the next run starts a fresh one"
                                };
                                println!("  {who} [{}] — restart: {how}", row.session_name);
                            }
                            ReconcileVerdict::Orphan => println!(
                                "  ORPHAN [{}] — a live IC session with no dispatch state. \
                                 Flagged for a human; never adopted.",
                                row.session_name
                            ),
                            ReconcileVerdict::Hold { reason } => {
                                println!("  {who} [{}] — hold: {reason}", row.session_name)
                            }
                        }
                    }
                }
                Ok(())
            }
            AutopilotCmd::Lead {
                db,
                repos,
                base,
                worktree_root,
                model,
                fallback_class,
                advisor,
                advisor_model,
                advice_budget_usd,
                max_advice_calls,
                max_dispatches,
                concurrency_cap,
                n_turns,
                max_budget_usd,
                attempt_cap,
                daily_budget_usd,
                dry_run,
                json,
            } => {
                use ironmem::autopilot::lead::LeadConfig;
                use ironmem::autopilot::queue::QueueConfig;
                use ironmem::autopilot::run::RunConfig;

                let worktree_root = match worktree_root {
                    Some(dir) => std::path::PathBuf::from(dir),
                    None => default_worktree_root()?,
                };

                let mut targets = Vec::new();
                for spec in &repos {
                    targets.push(parse_repo_target(spec, &base)?);
                }

                let mut run = RunConfig::new(model, fallback_class);
                if let Some(n) = n_turns {
                    run.n_turns = n;
                    run.max_turns =
                        n.saturating_add(ironmem::autopilot::run::DEFAULT_MAX_TURNS_HEADROOM);
                }
                if let Some(budget) = max_budget_usd {
                    run.max_budget_usd = budget;
                }
                if let Some(cap) = attempt_cap {
                    run.attempt_cap = cap;
                }
                if let Some(daily) = daily_budget_usd {
                    run.daily_budget_usd = daily;
                }

                let mut queue = QueueConfig::default();
                if let Some(cap) = concurrency_cap {
                    queue.concurrency_cap = cap;
                }
                if let Some(cap) = attempt_cap {
                    queue.attempt_cap = cap;
                }
                // Kept in step with the run config on purpose: the queue's
                // budget guard exists to apply the *same* predicate
                // `run_issue` will, so two different numbers here would make
                // the queue admit work the runner then refuses.
                queue.max_budget_usd = run.max_budget_usd;
                queue.daily_budget_usd = run.daily_budget_usd;

                let mut advice = ironmem::autopilot::advise::AdviceConfig {
                    enabled: advisor,
                    model: advisor_model,
                    // The advisor's dollar ceiling is the *same* day's
                    // ledger the runner and the queue read, for the reason
                    // the queue's is kept in step: two spellings of one
                    // ceiling let one caller authorize what the next
                    // refuses.
                    daily_budget_usd: run.daily_budget_usd,
                    ..Default::default()
                };
                if let Some(budget) = advice_budget_usd {
                    advice.max_budget_usd = budget;
                }
                if let Some(calls) = max_advice_calls {
                    advice.max_calls_per_day = calls;
                }

                let config = LeadConfig {
                    targets,
                    queue,
                    run,
                    advice,
                    supervision: ironmem::autopilot::supervise::SupervisionConfig::default(),
                    max_dispatches_per_tick: max_dispatches
                        .unwrap_or(ironmem::autopilot::lead::DEFAULT_MAX_DISPATCHES_PER_TICK),
                    worktree_root,
                    dry_run,
                };
                // Validate before resolving binaries: a bad config should
                // fail on the config, not on a missing `gh`.
                config.validate()?;

                let database = open_migrated_db(db)?;
                let mut gh_runner = ironmem::autopilot::gh::GhCli::resolve(
                    config
                        .targets
                        .first()
                        .map(|t| t.path.clone())
                        .unwrap_or_else(|| std::path::PathBuf::from(".")),
                )?;
                let mut registry = ironmem::autopilot::registry::ClaudeAgentRegistry::resolve()?;
                let mut dispatcher = ironmem::autopilot::run::ClaudeDispatcher::resolve()?;
                let mut advisor = ironmem::autopilot::advise::ClaudeAdvisor::resolve()?;

                let report = ironmem::autopilot::lead::lead_tick(
                    &database,
                    &mut gh_runner,
                    &mut registry,
                    &mut dispatcher,
                    &mut advisor,
                    &config,
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print_lead_report(&report);
                }
                Ok(())
            }
            AutopilotCmd::Advance {
                db,
                repos,
                worktree_root,
                merge,
                remediate,
                attempt_cap,
                strategy,
                delete_branch,
                model,
                max_advances,
                max_issues_per_repo,
                daily_budget_usd,
                max_unpriced_reviews_per_day,
                dry_run,
                json,
            } => {
                use ironmem::autopilot::advance::AdvanceConfig;

                let strategy =
                    ironmem::autopilot::gh::MergeStrategy::parse(&strategy).ok_or_else(|| {
                        ironmem::error::MemoryError::Config(format!(
                            "unknown merge strategy {strategy:?} — expected squash, merge or rebase"
                        ))
                    })?;
                let targets = repos
                    .iter()
                    // `base` is the committish new branches are cut from, and
                    // this pass cuts none: every branch it looks at already
                    // exists and already has a PR.
                    .map(|spec| parse_repo_target(spec, "HEAD"))
                    .collect::<Result<Vec<_>, _>>()?;
                let worktree_root = match worktree_root {
                    Some(root) => std::path::PathBuf::from(root),
                    None => default_worktree_root()?,
                };

                let config = AdvanceConfig {
                    targets,
                    max_issues_per_repo: max_issues_per_repo
                        .unwrap_or(ironmem::autopilot::queue::DEFAULT_MAX_ISSUES_PER_REPO),
                    max_advances_per_pass: max_advances
                        .unwrap_or(ironmem::autopilot::advance::DEFAULT_MAX_ADVANCES_PER_PASS),
                    merge,
                    remediate,
                    attempt_cap: attempt_cap
                        .unwrap_or(ironmem::autopilot::run::DEFAULT_ATTEMPT_CAP),
                    strategy,
                    delete_branch,
                    dry_run,
                    daily_budget_usd: daily_budget_usd
                        .unwrap_or(ironmem::autopilot::run::DEFAULT_DAILY_BUDGET_USD),
                    max_unpriced_reviews_per_day: max_unpriced_reviews_per_day.unwrap_or(
                        ironmem::autopilot::review::DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY,
                    ),
                    worktree_root,
                };
                // Validated before any binary is resolved: a bad config
                // should fail on the config, not on a missing `gh`.
                config.validate()?;

                let database = open_migrated_db(db)?;
                let mut gh_runner = ironmem::autopilot::gh::GhCli::resolve(
                    config
                        .targets
                        .first()
                        .map(|t| t.path.clone())
                        .unwrap_or_else(|| std::path::PathBuf::from(".")),
                )?;
                // `codex` is resolved only when a review can actually
                // happen. A dry run returns before reviewing anything, so
                // requiring the binary would make the one flag whose whole
                // promise is "read everything, change nothing" fail on a
                // machine that has nothing to change.
                let mut real;
                let mut refusing = DryRunReviewer;
                let reviewer: &mut dyn ironmem::autopilot::review::ReviewRunner = if dry_run {
                    &mut refusing
                } else {
                    real = ironmem::autopilot::review::CodexReviewer::resolve(model)?;
                    &mut real
                };

                let report = ironmem::autopilot::advance::advance_pass(
                    &database,
                    &mut gh_runner,
                    reviewer,
                    &config,
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print_advance_report(&report);
                }
                Ok(())
            }
            AutopilotCmd::Queue {
                db,
                repos,
                path,
                concurrency_cap,
                attempt_cap,
                max_budget_usd,
                daily_budget_usd,
                json,
            } => {
                use ironmem::autopilot::queue::{plan_queue, QueueConfig, RepoBacklog};

                let mut config = QueueConfig::default();
                if let Some(cap) = concurrency_cap {
                    config.concurrency_cap = cap;
                }
                if let Some(cap) = attempt_cap {
                    config.attempt_cap = cap;
                }
                if let Some(budget) = max_budget_usd {
                    config.max_budget_usd = budget;
                }
                if let Some(daily) = daily_budget_usd {
                    config.daily_budget_usd = daily;
                }
                config.validate()?;

                let database = open_migrated_db(db)?;
                let mut gh_runner =
                    ironmem::autopilot::gh::GhCli::resolve(std::path::PathBuf::from(&path))?;
                let mut registry = ironmem::autopilot::registry::ClaudeAgentRegistry::resolve()?;
                let snapshot = ironmem::autopilot::registry::snapshot(&mut registry);

                let mut backlogs = Vec::new();
                for repo in &repos {
                    let issues = ironmem::autopilot::gh::list_labeled_issues(
                        &mut gh_runner,
                        repo,
                        ironmem::autopilot::labels::AgentLabel::Ready.as_str(),
                        config.max_issues_per_repo,
                    )?;
                    backlogs.push(RepoBacklog {
                        repo: repo.clone(),
                        issues,
                    });
                }

                let plan = plan_queue(&database, &backlogs, &snapshot, &config)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&plan)?);
                } else {
                    print_queue_plan(&plan);
                }
                Ok(())
            }
            AutopilotCmd::Advise {
                db,
                repo,
                issue,
                kind,
                path,
                signature,
                model,
                max_budget_usd,
                daily_budget_usd,
                json,
            } => {
                use ironmem::autopilot::advise::{self, AdviceConfig};

                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let repo_path = std::path::PathBuf::from(&path);
                // Enabled unconditionally: the operator typed the command,
                // which *is* the opt-in `--advisor` expresses on a tick.
                let mut config = AdviceConfig {
                    enabled: true,
                    model,
                    ..Default::default()
                };
                if let Some(budget) = max_budget_usd {
                    config.max_budget_usd = budget;
                }
                if let Some(daily) = daily_budget_usd {
                    config.daily_budget_usd = daily;
                }
                config.validate()?;

                let database = open_migrated_db(db)?;
                let mut advisor = advise::ClaudeAdvisor::resolve()?;

                // Read the signature from supervision when it was not given,
                // so the usual case is one flag shorter and cannot disagree
                // with the record the redirect is keyed on.
                let signature = match signature {
                    Some(signature) => Some(signature),
                    None => ironmem::autopilot::supervise::get_supervision(&database, &issue_ref)?
                        .and_then(|record| record.redirect_signature),
                };
                // Terminal summaries are filtered out for the same reason the
                // Lead's own `attempt_approaches` filters them: they are the
                // run's epitaph, not an approach the IC tried, and quoting
                // them back would make this preview disagree with the prompt
                // a real tick sends.
                let approaches: Vec<String> =
                    ironmem::autopilot::lineage::attempts_for_issue(&database, &issue_ref)?
                        .into_iter()
                        .map(|a| a.approach)
                        .filter(|approach| !ironmem::autopilot::run::is_terminal_summary(approach))
                        .collect();

                let advice = match kind.as_str() {
                    "risk" => {
                        let brief = ironmem::autopilot::gh::issue_brief(
                            &mut ironmem::autopilot::gh::GhCli::resolve(repo_path.clone())?,
                            &issue_ref,
                        )?;
                        advise::advise_risk_class(
                            &database,
                            &mut advisor,
                            &repo_path,
                            &issue_ref,
                            &brief.title,
                            &brief.body,
                            &config,
                        )?
                    }
                    "redirect" | "question" => {
                        let signature = signature.ok_or_else(|| {
                            MemoryError::Validation(format!(
                                "--signature is required for --kind {kind}: {} has no redirect \
                                 on record to name the repeated failure",
                                issue_ref.canonical()
                            ))
                        })?;
                        if kind == "redirect" {
                            advise::advise_strategy_redirect(
                                &database,
                                &mut advisor,
                                &repo_path,
                                &issue_ref,
                                &signature,
                                &approaches,
                                &config,
                            )?
                        } else {
                            let brief = ironmem::autopilot::gh::issue_brief(
                                &mut ironmem::autopilot::gh::GhCli::resolve(repo_path.clone())?,
                                &issue_ref,
                            )?;
                            advise::advise_human_question(
                                &database,
                                &mut advisor,
                                &repo_path,
                                &issue_ref,
                                &brief.title,
                                &brief.body,
                                &signature,
                                &approaches,
                                &config,
                            )?
                        }
                    }
                    other => {
                        return Err(MemoryError::Validation(format!(
                            "unknown advice kind '{other}'"
                        )))
                    }
                };

                if json {
                    println!("{}", serde_json::to_string_pretty(&advice)?);
                } else {
                    println!("{} on {}", advice.kind.as_str(), issue_ref.canonical());
                    println!("  status: {:?}", advice.status);
                    match advice.total_cost_usd {
                        Some(cost) => println!("  cost: ${cost:.4}"),
                        None => println!("  cost: unknown — banked as an unpriced call"),
                    }
                    if let Some(answer) = advice.answered() {
                        println!("  answer: {answer}");
                    }
                    if let Some(reason) = &advice.reason {
                        println!("  reason: {reason}");
                    }
                }
                Ok(())
            }
            AutopilotCmd::Ask {
                db,
                repo,
                issue,
                question,
                path,
                dry_run,
                json,
            } => {
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let database = open_migrated_db(db)?;
                let mut gh_runner =
                    ironmem::autopilot::gh::GhCli::resolve(std::path::PathBuf::from(&path))?;
                let outcome = ironmem::autopilot::blocked::ask_human(
                    &database,
                    &mut gh_runner,
                    &issue_ref,
                    &question,
                    dry_run,
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else {
                    match &outcome {
                        ironmem::autopilot::blocked::AskOutcome::Asked { question } => {
                            println!("Asked on {}: {question}", issue_ref.canonical());
                            println!(
                                "  labeled agent:blocked; a reply after this comment resumes it"
                            );
                        }
                        ironmem::autopilot::blocked::AskOutcome::AlreadyWaiting { question } => {
                            println!(
                                "{} is already waiting on a human: {question}",
                                issue_ref.canonical()
                            );
                            println!("  nothing posted, no label touched");
                        }
                        ironmem::autopilot::blocked::AskOutcome::DryRun { question } => {
                            println!(
                                "DRY RUN — would ask on {}: {question}",
                                issue_ref.canonical()
                            );
                        }
                    }
                }
                Ok(())
            }
            AutopilotCmd::Remediate {
                db,
                repo,
                issue,
                clear,
                json,
            } => {
                use ironmem::autopilot::remediate;

                let database = open_migrated_db(db)?;
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);

                if clear {
                    let dropped = remediate::clear_remediation(&database, &issue_ref)?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "issue": issue_ref.canonical(),
                                "cleared": dropped,
                            }))?
                        );
                    } else if dropped {
                        println!(
                            "{}: remediation cleared — the Lead will stop re-dispatching, and \
the next advance pass holds the PR for a human",
                            issue_ref.canonical()
                        );
                    } else {
                        println!(
                            "{}: nothing to clear, no remediation was armed",
                            issue_ref.canonical()
                        );
                    }
                    return Ok(());
                }

                // Both are reported, because they answer different questions.
                // `record` is what was armed; `active` is whether it is still
                // in force, which is derived from whether a newer success has
                // landed since. An armed record that is no longer active means
                // the IC pushed the fix and rung 10 will review the new head.
                let record = remediate::get_remediation(&database, &issue_ref)?;
                let active = remediate::active_remediation(&database, &issue_ref)?.is_some();

                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "issue": issue_ref.canonical(),
                            "armed": record.is_some(),
                            "in_force": active,
                            "pr_number": record.as_ref().map(|r| r.pr_number),
                            "head_sha": record.as_ref().map(|r| r.head_sha.clone()),
                            "armed_at": record.as_ref().map(|r| r.armed_at.clone()),
                            "armed_after_attempts": record
                                .as_ref()
                                .map(|r| r.armed_after_attempts),
                            "has_findings": record
                                .as_ref()
                                .is_some_and(|r| r.findings.is_some()),
                            "findings": record.as_ref().and_then(|r| r.findings.clone()),
                        }))?
                    );
                } else {
                    match record {
                        None => println!("{}: no remediation armed", issue_ref.canonical()),
                        Some(record) => {
                            println!(
                                "{issue}: remediation {state} — PR #{pr} at {sha}, armed {at} \
after {n} attempt(s)",
                                issue = issue_ref.canonical(),
                                state = if active {
                                    "IN FORCE"
                                } else {
                                    "superseded (a newer success has landed)"
                                },
                                pr = record.pr_number,
                                sha = record.head_sha,
                                at = record.armed_at,
                                n = record.armed_after_attempts,
                            );
                            match record.findings.as_deref() {
                                Some(findings) => println!("  the reviewer said: {findings}"),
                                None => println!("  the reviewer recorded no reason"),
                            }
                        }
                    }
                }
                Ok(())
            }
            AutopilotCmd::Timeout {
                db,
                repo,
                secs,
                clear,
                json,
            } => {
                let database = open_migrated_db(db)?;
                let current = if clear {
                    ironmem::autopilot::gate_config::set_wall_clock_timeout(&database, &repo, None)?
                        .wall_clock_timeout_secs
                } else if let Some(secs) = secs {
                    ironmem::autopilot::gate_config::set_wall_clock_timeout(
                        &database,
                        &repo,
                        Some(secs),
                    )?
                    .wall_clock_timeout_secs
                } else {
                    // Read-only: distinguish "onboarded, no bound" from "never
                    // onboarded". Both used to print UNBOUNDED and point the
                    // operator at a `timeout <repo> <secs>` that then failed
                    // with "no gate config has been proposed".
                    match ironmem::autopilot::gate_config::get_gate_config(&database, &repo)? {
                        Some(config) => config.wall_clock_timeout_secs,
                        None => {
                            return Err(MemoryError::NotFound(format!(
                                "no gate config for '{repo}' — run `ironmem autopilot onboard \
                                 {repo}` first; a wall-clock bound lives on the gate config"
                            )))
                        }
                    }
                };

                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "repo": repo,
                            "wall_clock_timeout_secs": current,
                        }))?
                    );
                } else {
                    match current {
                        Some(secs) => println!("{repo}: dispatches are bounded at {secs}s"),
                        None => println!(
                            "{repo}: dispatches are UNBOUNDED — a wedged dispatch here runs \
                             forever. Set one with `ironmem autopilot timeout {repo} <secs>`."
                        ),
                    }
                }
                Ok(())
            }
        },
        Commands::Harnesses { format } => {
            match format.as_str() {
                "json" => println!(
                    "{}",
                    ironmem::harness::registry_json(ironmem::harness::REGISTRY)?
                ),
                _ => print!(
                    "{}",
                    ironmem::harness::registry_text(ironmem::harness::REGISTRY)
                ),
            }
            Ok(())
        }
        Commands::WriteRules {
            target,
            harness,
            workspace,
        } => {
            use ironmem::write_rules::{
                apply_write_rules_plan, build_write_rules_plan, WriteOutcome,
            };
            let plan = build_write_rules_plan(
                std::path::Path::new(&workspace),
                target.as_deref(),
                harness.as_deref(),
                ironmem::harness::REGISTRY,
            )?;
            let outcomes = apply_write_rules_plan(&plan)?;
            for (path, outcome) in outcomes {
                let label = match outcome {
                    WriteOutcome::Created => "created",
                    WriteOutcome::Updated => "updated",
                    WriteOutcome::Unchanged => "unchanged",
                };
                eprintln!("ironmem write-rules: {label} {}", path.display());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse argv and return the `Serve` fields, panicking if the parsed
    /// command is not `Serve` (keeps each test focused on field assertions).
    fn parse_serve(args: &[&str]) -> (Option<String>, Option<String>, Option<String>, bool) {
        let cli = Cli::try_parse_from(args).expect("expected argv to parse");
        match cli.command {
            Commands::Serve {
                db,
                listen,
                connect,
                no_autospawn,
            } => (db, listen, connect, no_autospawn),
            _ => panic!("expected Commands::Serve, got a different variant"),
        }
    }

    #[test]
    fn serve_bare_uses_all_defaults() {
        let (db, listen, connect, no_autospawn) = parse_serve(&["ironmem", "serve"]);
        assert_eq!(db, None);
        assert_eq!(listen, None);
        assert_eq!(connect, None);
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_db_preserved_with_new_fields_defaulted() {
        let (db, listen, connect, no_autospawn) =
            parse_serve(&["ironmem", "serve", "--db", "/tmp/x.sqlite3"]);
        assert_eq!(db.as_deref(), Some("/tmp/x.sqlite3"));
        assert_eq!(listen, None);
        assert_eq!(connect, None);
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_listen_sets_listen_only() {
        let (db, listen, connect, no_autospawn) =
            parse_serve(&["ironmem", "serve", "--listen", "/tmp/d.sock"]);
        assert_eq!(db, None);
        assert_eq!(listen.as_deref(), Some("/tmp/d.sock"));
        assert_eq!(connect, None);
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_connect_sets_connect_only() {
        let (db, listen, connect, no_autospawn) =
            parse_serve(&["ironmem", "serve", "--connect", "/tmp/d.sock"]);
        assert_eq!(db, None);
        assert_eq!(listen, None);
        assert_eq!(connect.as_deref(), Some("/tmp/d.sock"));
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_listen_and_connect_are_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "ironmem",
            "serve",
            "--listen",
            "/a.sock",
            "--connect",
            "/b.sock",
        ]);
        assert!(result.is_err(), "expected --listen + --connect to conflict");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("connect") && msg.contains("listen"),
            "conflict error should mention both flags, got: {msg}"
        );
    }

    #[test]
    fn serve_no_autospawn_with_connect() {
        let (_db, listen, connect, no_autospawn) = parse_serve(&[
            "ironmem",
            "serve",
            "--no-autospawn",
            "--connect",
            "/tmp/d.sock",
        ]);
        assert_eq!(listen, None);
        assert_eq!(connect.as_deref(), Some("/tmp/d.sock"));
        assert!(no_autospawn);
    }

    #[test]
    fn autopilot_merge_defaults_are_the_conservative_ones() {
        // Every default here is the one that cannot merge by accident:
        // gate_green off, dry_run off but delete_branch off too, and squash.
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "merge",
            "owner/repo",
            "42",
            "--pr",
            "7",
        ])
        .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Merge {
                        repo,
                        issue,
                        pr,
                        path,
                        gate_green,
                        strategy,
                        delete_branch,
                        dry_run,
                        ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 42);
                assert_eq!(pr, 7);
                assert_eq!(path, ".");
                assert!(
                    !gate_green,
                    "gate_green must be opt-in: forgetting it must hold, never merge"
                );
                assert_eq!(strategy, "squash");
                assert!(!delete_branch);
                assert!(!dry_run);
            }
            _ => panic!("expected Commands::Autopilot(Merge), got a different variant"),
        }
    }

    #[test]
    fn autopilot_advance_does_not_merge_unless_asked() {
        // The one irreversible action in the subsystem. Forgetting the flag
        // must rehearse, never merge.
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "advance",
            "--repo",
            "owner/repo=/tmp/checkout",
        ])
        .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Advance {
                        repos,
                        merge,
                        strategy,
                        delete_branch,
                        dry_run,
                        max_advances,
                        ..
                    },
            } => {
                assert_eq!(repos, vec!["owner/repo=/tmp/checkout"]);
                assert!(!merge, "merging is opt-in");
                assert!(!delete_branch);
                assert!(!dry_run);
                assert_eq!(strategy, "squash");
                assert_eq!(max_advances, None, "the default lives in the module");
            }
            _ => panic!("expected Commands::Autopilot(Advance), got a different variant"),
        }
    }

    #[test]
    fn autopilot_advance_does_not_remediate_unless_asked() {
        // Rung 11 re-opens work a human may believe is finished, on a branch
        // a human may be reading. Forgetting the flag must leave rung 10's
        // behaviour exactly as it was: hold the PR for a human.
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "advance",
            "--repo",
            "owner/repo=/tmp/checkout",
        ])
        .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Advance {
                        remediate,
                        attempt_cap,
                        ..
                    },
            } => {
                assert!(!remediate, "re-dispatching is opt-in");
                assert_eq!(attempt_cap, None, "the default lives in the module");
            }
            _ => panic!("expected Commands::Autopilot(Advance), got a different variant"),
        }
    }

    #[test]
    fn autopilot_remediate_names_one_issue_and_defaults_to_reading() {
        // `--clear` is the human override; without it the command reads.
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "remediate", "owner/repo", "7"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Remediate {
                        repo, issue, clear, ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 7);
                assert!(!clear, "reading is the default; clearing is deliberate");
            }
            _ => panic!("expected Commands::Autopilot(Remediate), got a different variant"),
        }
        assert!(
            Cli::try_parse_from(["ironmem", "autopilot", "remediate", "owner/repo"]).is_err(),
            "a remediation belongs to one issue, and there is nothing to guess from"
        );
    }

    #[test]
    fn autopilot_advance_requires_a_repo_and_its_checkout() {
        // A review reads the issue's worktree, which is cut from a local
        // checkout. There is nothing to guess from.
        assert!(
            Cli::try_parse_from(["ironmem", "autopilot", "advance"]).is_err(),
            "--repo is required"
        );
        assert!(
            parse_repo_target("owner/repo", "HEAD").is_err(),
            "a --repo without a path is refused"
        );
    }

    #[test]
    fn autopilot_merge_requires_a_pr_number() {
        assert!(
            Cli::try_parse_from(["ironmem", "autopilot", "merge", "owner/repo", "42"]).is_err(),
            "a merge with no PR to merge must not parse"
        );
    }

    #[test]
    fn autopilot_merge_accepts_a_dry_run_and_a_strategy() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "merge",
            "owner/repo",
            "42",
            "--pr",
            "7",
            "--gate-green",
            "--strategy",
            "rebase",
            "--dry-run",
        ])
        .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Merge {
                        gate_green,
                        strategy,
                        dry_run,
                        ..
                    },
            } => {
                assert!(gate_green);
                assert_eq!(strategy, "rebase");
                assert!(dry_run);
            }
            _ => panic!("expected Commands::Autopilot(Merge), got a different variant"),
        }
    }

    #[test]
    fn autopilot_exhaust_parses_the_issue() {
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "exhaust", "owner/repo", "42"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Exhaust {
                        repo,
                        issue,
                        dry_run,
                        ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 42);
                assert!(!dry_run);
            }
            _ => panic!("expected Commands::Autopilot(Exhaust), got a different variant"),
        }
    }

    #[test]
    fn autopilot_labels_takes_only_a_repo() {
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "labels", "owner/repo"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Labels { repo, path, json },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(path, ".");
                assert!(!json);
            }
            _ => panic!("expected Commands::Autopilot(Labels), got a different variant"),
        }
    }

    #[test]
    fn autopilot_onboard_defaults_path_to_current_directory() {
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "onboard", "owner/repo"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Onboard {
                        repo,
                        path,
                        db,
                        json,
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(path, ".");
                assert_eq!(db, None);
                assert!(!json);
            }
            _ => panic!("expected Commands::Autopilot(Onboard), got a different variant"),
        }
    }

    #[test]
    fn autopilot_onboard_accepts_an_explicit_path() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "onboard",
            "owner/repo",
            "--path",
            "/tmp/some-checkout",
        ])
        .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Onboard { path, .. },
            } => assert_eq!(path, "/tmp/some-checkout"),
            _ => panic!("expected Commands::Autopilot(Onboard), got a different variant"),
        }
    }

    #[test]
    fn autopilot_run_parses_the_issue_and_brief() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "run",
            "owner/repo",
            "283",
            "--title",
            "Make the gate pass",
            "--body",
            "The suite is red.",
        ])
        .expect("parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Run {
                        repo,
                        issue,
                        title,
                        body,
                        base,
                        dispatch_class,
                        ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 283);
                assert_eq!(title, "Make the gate pass");
                assert_eq!(body.as_deref(), Some("The suite is red."));
                assert_eq!(base, "HEAD");
                assert_eq!(dispatch_class, "unclassified");
            }
            _ => panic!("expected Commands::Autopilot(Run), got a different variant"),
        }
    }

    #[test]
    fn autopilot_review_parses_the_pr_and_class() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--pr",
            "322",
            "--class",
            "documentation",
        ])
        .expect("review args should parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Review {
                        repo,
                        issue,
                        pr,
                        dispatch_class,
                        base,
                        head,
                        gate_green,
                        ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 283);
                assert_eq!(pr, 322);
                assert_eq!(dispatch_class, "documentation");
                assert_eq!(base, "main");
                assert_eq!(head, None, "head defaults to the issue's autopilot branch");
                assert!(
                    !gate_green,
                    "the gate must not be assumed green when the flag is absent"
                );
            }
            _ => panic!("expected Commands::Autopilot(Review), got a different variant"),
        }
    }

    #[test]
    fn autopilot_review_requires_a_pr_and_a_dispatch_class() {
        // Without `--pr` there is nothing to review; without `--class` the
        // double-classification check has nothing to compare against, and
        // defaulting it would make one half of the spec's fail-closed rule
        // silently vacuous.
        assert!(Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--class",
            "documentation",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--pr",
            "322",
        ])
        .is_err());
    }

    #[test]
    fn autopilot_review_gate_green_is_opt_in() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--pr",
            "322",
            "--class",
            "test_only",
            "--gate-green",
        ])
        .expect("review args should parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Review { gate_green, .. },
            } => assert!(gate_green),
            _ => panic!("expected Commands::Autopilot(Review), got a different variant"),
        }
    }

    #[test]
    fn autopilot_run_requires_a_title() {
        let result = Cli::try_parse_from(["ironmem", "autopilot", "run", "owner/repo", "283"]);
        assert!(result.is_err(), "--title is required for the turn prompt");
    }

    #[test]
    fn autopilot_run_rejects_both_body_and_body_file() {
        let result = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "run",
            "owner/repo",
            "283",
            "--title",
            "t",
            "--body",
            "inline",
            "--body-file",
            "issue.md",
        ]);
        assert!(
            result.is_err(),
            "--body and --body-file are mutually exclusive"
        );
    }

    #[test]
    fn autopilot_approve_requires_a_repo_argument() {
        let result = Cli::try_parse_from(["ironmem", "autopilot", "approve"]);
        assert!(
            result.is_err(),
            "expected missing repo argument to fail parsing"
        );
    }

    #[test]
    fn autopilot_approve_parses_repo_and_json_flag() {
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "approve", "owner/repo", "--json"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Approve { repo, json, .. },
            } => {
                assert_eq!(repo, "owner/repo");
                assert!(json);
            }
            _ => panic!("expected Commands::Autopilot(Approve), got a different variant"),
        }
    }
}
