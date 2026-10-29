# PR #367 ultrareview resolution

Decision: proceed with the localized fixes and the explicit limitations below;
this is not a claim that all security/performance risks are eliminated. No merge.
The adversarial HIGH verification remains **13 confirmed / 2 refuted out of 15**.
Refuted: I1 (trusted in-process artifact construction plus layout-bound drawer
reads) and J2 (documented lockless suspension limit; production holds flock).
Rollback anchor: `60779640f210b04591996ff2745e4613c0e7767e` (`60779640`).

Finding IDs below are the original lens order, including LOW findings.
“Documented” means the code limitation is retained, not repaired. Operator
contracts and risk notes are in [AUTOPILOT.md](../AUTOPILOT.md).

| Findings | Resolution and evidence |
|---|---|
| A1 | Five-second drain grace prevents a descendant retaining a pipe from hanging normal exit indefinitely. Regression holds a pipe beyond the grace. |
| A2, A4 | Validate daily budget and loaded ledger; reserve retries deduplicate IDs; zero-dollar reservations release by record match. Tests pin NaN/negative/infinite budget and duplicate reserve. |
| A3, B5, H1, C7 | Absolute fail-closed launcher sentinel, successful-cache executable recheck; removed binaries re-resolve. No bare git/ps fallback. |
| A5 | Quarantine checks for existing destinations and adds a UUID suffix; a real-git collision regression preserves old evidence. |
| A6 | Future lock timestamps use unknown-age wording; reproduced “held for 0s” before fix. |
| A7 | Over-cap stderr retains bounded head/tail for refusal parsing while still reporting the cap error. Regression reproduced missing session-in-use refusal. |
| A8 | Cleanup lock errors report Skipped rather than Absent. |
| B1 | **Mitigated, confinement unpatched.** Private prompt/scratch and GH_TOKEN/GITHUB_TOKEN/SSH_AUTH_SOCK removal. Explicit operator-equivalent-risk note; tracked-tree/HEAD check cannot protect ignored/outside files or operator credentials. Cross-platform OS confinement changes the reviewer execution boundary and needs a separate design. Do not call this finding closed. |
| B2 | Both --implementer help surfaces state unsandboxed operator authority. Environment needed for signing/push intentionally retained; external sandbox is the isolation boundary. |
| B3 | Muse prompt uses write_private; Unix scratch directory is 0700. Fixed inherited Unix unreachable-code warning. |
| B4 | **Documented, unpatched:** ps markers lack UID/executable authentication and can be spoofed to hold an issue. They never authorize a kill. Adding UID to process snapshots/fixtures is a separate evidence-format change; false negatives for setuid descendants must be considered. |
| B6, J8 | Existing non-Linux birth-check-to-signal residual window retained and documented. Linux uses pidfds; no portable equivalent was introduced. |
| B7 | Case-insensitive Basic-auth pattern covers short base64 credentials; regression failed before fix. |
| B8 | **Documented, raw evidence retained:** journals/prompts/stderr can contain secrets despite owner-only modes. Scrubbing on settle could destroy parser/recovery evidence and is not a safe textual rewrite. Retention is opportunistic; named invocation remains. |
| C1, G8 | **Documented intentional exception:** liveness can finalize awaiting-quiet evidence even in queue/status/dry-run. Real reaped-child Lead dry-run regression asserts completion promotion and byte-identical database records. Splitting settlement from every reader is cross-cutting and deferred. CLI dry-run help states the exception. |
| C2, I2 | Unknown phases defer with DispatchPhaseUnknown. Legacy labels remain readable/writable because existing compatibility fixtures write them; no write-side unknown-phase ban. Free String remains an explicit compatibility boundary. |
| C3, J1 | Supervised launch rechecks ownership/renews its lease after signing setup immediately before dispatch. run_issue library callers must hold the issue flock through provisioning and dispatch. Lockless suspension between final check and spawn remains documented. |
| C4 | **Documented downgrade hold:** strict additive exit fields are intentionally unreadable to older binaries. Complete/clear with the writing version before downgrade; no wire version change in this fix pass. |
| C5 | CHANGELOG breaking Rust API note covers Cleared, renamed fields, new outcome field and constructor restrictions. Existing serialized codex_artifacts spelling preserved. |
| C6 | **Documented, unpatched:** recovers_invocations also selects today's unpriced accounting. Adding a priced recovering harness requires independent recovery/pricing/native-budget/env capabilities. Separating the run loop’s reservation and recovery branches now would be invasive; no new harness is added here. |
| C8 | Shared git environment helpers move to crate::git_env; fixture git moves to crate::test_git. Consumers use the leaf, with old import paths re-exported for compatibility. Source scan still requires exactly one canonical scrub definition. |
| D1, D2 | Guide + CHANGELOG explain flock path/scope/Held surfaces/kernel release, never-unlink operation, and breaking local-storage startup refusal/remedy/platform limits. |
| D3 | Verified current tree already says Muse default and “Muse reviewer” table header. Preserve the correct current text; no Claude reviewer added. |
| D4 | Qualify retained artifacts, document >14-day opportunistic pruning and named/current exemption in guide + CHANGELOG. |
| F1–F3 | Correct dollar-preauthorization wording in CLI/guide. Retain rejection when priced-spend-plus-ceiling can never pass, and name --daily-budget-usd. Supplied edit removing this rejection caused both Codex/Muse tests to fail; restore behavior with accurate wording. |
| F4, F5 | Module attributes shared supervision to invocation; is_met docs include started/non-timeout guards. |
| F6 | Drop opaque review-round codes from comments while preserving the requirements they describe. |
| F7 | Harness headers require re-measuring argv/options, refusals and journal/usage/terminal shapes on upgrades. Run real argv marker cross-test and parser fixtures; paid probes need approval. |
| F8 | Deprecated shims now name removal version 0.7.0. |
| G1 | Feed real CodexCli/MuseCli argv into ProcessRow::marker; measured fixtures remain additional cases. |
| G2 | Scan cached_launcher("git") constructors as well as literal git; regression reproduced missed unsafe constructor. |
| G3 | Exact 119/120/121-second lease boundaries and malformed stamp; 89/90/91-second refresh margin checked against persisted claims with injected time. |
| G4 | Real-filesystem tests cover symlink refusal/target preservation, parent-is-file failure, free replacement retry, repeated-replacement exhaustion and 0700 lock directory. Non-EWOULDBLOCK flock errors require an unsupported/erroring filesystem; not synthesized. Churn fixture retains replaced inodes under unique names to avoid rapid inode reuse/parallel fork FD interference. |
| G5 | Relative gitdir link, surviving commondir, absent HEAD, missing prefix, wrong admin parent, missing repo spawn failure. Existing deleted-admin test retained. |
| G6 | All 13 network FUSE subtype spellings and nested longest-prefix behavior tested. Other-platform remote-FS detection remains explicitly unsupported; not claimed tested. |
| G7 | Absolute Muse root skips cwd lookup; whitespace error remains specific. Regression reproduced spurious NotFound. |
| G9 | Duplicate reserve, unknown settle, double settle and invalid daily budgets pinned. Settling an absent reservation intentionally banks reported cost (late outcome after release); callers must settle each outcome once. Cap tests use distinct IDs for distinct dispatches, preserving the original cap assertion. |
| G10 | Exact past/future 120-second launch equality and retention equality/future mtime tested with explicit clocks. |
| G11 | Per-name git/ps independence, unknown-name passthrough, poisoned mutex recovery and vanished executable checks. |
| G12 | Negative/fractional/string usage counts preserve last valid integer; existing u32 overflow clamp test retained. |
| H2 | git_common_dir errors warn and refuse claim publication, rather than silently publishing no durable lock pointer. |
| H3 | Settled-state release warns for lock-check errors; Held remains a normal quiet deferral. |
| H4 | Unreadable Linux FUSE mountinfo refuses startup instead of assuming local. Linux CI caught an inherited reference to nonexistent libc::FUSE_MAGIC; classification now uses the module constant in the cross-platform helper, with an empty-mountinfo regression tested on every platform. |
| H5, I4 | Present malformed claimed_at holds its lease; write-side common_dir must be absolute. Legacy missing claim fields remain tolerated. |
| H6 | Reservation lookup explicitly documented as a lower bound at API and recovery call site; corrupt drawers warn and do not hide another day’s valid reservation. |
| H7 | Ambiguous orphan marker rows now warn with pid and scrubbed reason, instead of disappearing silently. They are not promoted into authenticated markers. |
| H8 | Unreadable/future invocation age emits warning and retains evidence. |
| H9 | Owner-token unset failure warns and returns qualified cleanup wording. |
| I1 | **REFUTED** by adversarial verification; no artifact-layout redesign. |
| I3 | Refuse claimed-but-empty session ID. Remaining harness-specific session/model/refusal fields keep legacy compatibility: canonical UUID/native-ID checks occur in harness/recovery adapters. Banning legacy synthetic IDs at drawer load would be a compatibility change. |
| I5 | Validate loaded ledger date/key, finite non-negative totals/reservations, unique reservation and unpriced IDs. |
| I6, I7 | Available variants non_exhaustive; HarnessSnapshots and SharedDaemon fields crate-private. Process-source traits remain trusted integration seams. |
| I8 | Validate unique spawn files, confinement inside invocation directory and disjointness from evidence/identity before creation. Duplicate fixture now requires refusal before any prompt exists; old comment that duplicates simulate a real creation race was inaccurate under this stronger contract. Actual AlreadyExists race remains fail-closed. |
| I9 | is_impossible has the same started/success/non-timeout/non-error trust guards as is_met. |
| I10 | ParsedRun::with_verdict scrubs/bounds reason to 500 chars for direct and parsed construction. Empty reason remains schema-valid (schema has no minLength); no invented wire requirement. |
| I11 | Shared DispatchSpec validation checks model token, finite non-negative budget and nonzero turn cap before supervised/Claude dispatch. Native session semantics remain in adapters. |
| I12 | **Documented trusted-construction contract:** public Worktree values must come from ensure_worktree. Privatizing all fields/public consumers is an invasive library API change; no false claim of a validated constructor. |
| J2 | **REFUTED** by adversarial verification; documented lockless suspension limit unchanged. |
| J3 | advance_issue holds flock from before review through merge, dropping before cleanup reacquires. Real-lock regressions verify held refusal and ownership during review; dry-run does not create lock files. |
| J4 | **Documented, unpatched:** concurrency cap is plan-time, not transactional global admission. Use one Lead for strict global cap. Per-issue locks cannot serialize different issues. New global admission state is invasive. |
| J5 | Supervision read/update in one immediate transaction, including proposal/question/notified/reset paths. Real multi-connection race failed with BUSY before fix and passes afterward. External notifications/advice remain outside transaction. |
| J6, J7 | **Documented, unpatched:** review/advice check-then-act caps can overshoot concurrently. New reservation/refund/liveness lifecycle is a separate accounting change; dispatch reservations are already atomic. |
| J9 | Document quiet-scan-to-record TOCTOU; no atomic containment claim. |
| J10 | Never unlink lock files, even apparently idle: replacement after acquisition voids exclusivity. |
| J11 | Retry COMMIT on BUSY with the same transaction; FnOnce mutation never replayed. Real rollback-journal reader contention regression failed before fix and now commits exactly once. |
| K1 | Parent adjacency index and BFS remove depth-times-full-row rescans; gone-parent children use same index. Keep before/after kernel authentication captures rather than memoizing them away. No performance benchmark claim. |
| K2, K12 | Artifact limits move off 25ms exit poll onto observation cadence. Table scans slow from 250ms to 1s after a minute and 2s after five minutes; exit detection unchanged. |
| K3 | Quiet polling slows from 250ms to 1s (roughly fourfold fewer repeated scans). **Owner/birth caching unpatched:** PID reuse and credential changes make cached ownership unsafe; fresh checks preserved. No Linux benchmark run locally. |
| K4, K9, K11 | **Documented invasive remainder:** no shared per-tick artifact/state/settle cache. A valid design needs state/mtime invalidation and explicit settle side effects across registry/queue/reconcile/release; stale answers must never authorize takeover or clear. |
| K5, K6 | HashSet descendant membership replaces nested scans for recorded-tree evidence and live identity comparison. Enrollment evidence still persists immediately; no unsafe write throttling. |
| K7 | Teardown full-table polls exponentially back off to 100ms within the existing one-second grace. Regression measured 22 scans/250ms before fix and now permits at most 8. Direct kernel-only replacement deferred because multithreaded zombies require active-task semantics. |
| K8 | **Documented, unpatched:** historical reservation scan remains unbounded. A cutoff would lose long-lived holds; indexing/dedicated reservation schema needs migration design. |
| K10 | Quiet candidates use pid index plus per-pass daemon-executable memo; unsignalable traversal uses parent index and seen set. |

## Post-push review follow-up

Greptile P1 `discussion_r4407090159` identified that a bounded Claude drain
error could retry beside the still-running pipe holder. A real stub spawning
a 60-second tool reproduced two dispatches before the infrastructure cap.
`DispatchUnsettled` now writes `paused-completion-unproven` before accounting,
returns implementation-in-flight, charges one unpriced unit and no attempt,
and refuses subsequent run/provisioning. Queue, liveness, advance review/merge
and automatic release keep the hold until explicit operator clearance.
Tests cover the real pipe holder, subsequent run, release after an old success,
queue deferral and both advance review/merge paths. No bare PID/group is killed
after the root has been reaped.

## Validation and work not run

Initial supplied tree: fmt exit 1; clippy exit 101 (Unix unreachable code);
workspace tests exit 101 (3001 passed, 4 failed, 1 ignored). Fixes above retain
cap assertions; no tests were skipped or loosened. Existing ignored tests
remain as inherited.

Final local gates (sequential, unpiped; read each process exit):

- `cargo fmt --all -- --check`: exit 0, no output.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: exit 0,
  `Finished dev profile [unoptimized + debuginfo] target(s) in 13.08s`.
- `cargo test --workspace`: exit 0; 3513 passed across 52 test
  summaries, 0 failed, 1 ignored. Library output:
  `test result: ok. 3030 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 79.50s`.
  Full local output: `/tmp/ironmem-pr367-workspace.log`.
- `cargo test -p ironmem --lib ultrareview -- --nocapture`: exit 0,
  `23 passed; 0 failed; 0 ignored`.

The inherited skip is `search::rerank::tests::rerank_latency_bench` (timing
benchmark). Post-push CI results are recorded in the completion message.
No paid live Claude/Codex/Muse dispatch, no OS confinement implementation, no
merge, no new dependency, and no repeat of the prior completed Linux Docker
suite. CI on the pushed head supplies Linux validation. Real Claude ps argv
sampling is still an approval-required open item for Jeff (#368).
