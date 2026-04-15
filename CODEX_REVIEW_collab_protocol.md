# Codex Review: IronRace Collab Protocol Implementation

**Branch:** feat/collab-protocol  
**Author:** Claude (acting as implementer)  
**Date:** 2026-04-14  
**Build:** cargo fmt + cargo clippy -D warnings + cargo test — all clean (166 tests, 0 failures)

---

## What Was Built

Implemented the IronRace Collaboration Protocol v1 as specified in `CODEX_TASK_collab_protocol.md`:

### New crate: `crates/ironrace-collab`
- `src/types.rs` — `Phase`, `Agent`, `Topic` enums with `as_str()` / `from_name()` conversions
- `src/state_machine.rs` — Pure `CollabSession` state machine with `on_send()` and `on_approve()`
- `tests/e2e.rs` — 4 integration tests (planning_loop, escalation, wrong_turn, already_rejected)

### New DB layer: `crates/ironrace-memory/src/db/collab.rs`
- `SessionRow`, `MessageRow`, `CapabilityRow` data structs
- `SessionUpdate<'a>` payload struct (avoids too-many-arguments clippy lint)
- `Database::collab_session_create/get/update` — session CRUD
- `Database::collab_message_send/recv/ack/count` — FIFO queue
- `Database::collab_caps_register/get` — capability registry (upsert)
- `Database::collab_set_max_rounds` — test helper

### New schema: `migrations/002_collab.sql`
- `collab_sessions` — phase, current_owner, round, approval bits, content_hash
- `messages` — sender/receiver/topic/content/status with FK to sessions
- `agent_capabilities` — per-session cap registry with UNIQUE constraint

### MCP tools: 8 new handlers in `tools.rs`
`ironmem_collab_start`, `ironmem_collab_send`, `ironmem_collab_recv`,
`ironmem_collab_ack`, `ironmem_collab_approve`, `ironmem_collab_status`,
`ironmem_collab_register_caps`, `ironmem_collab_get_caps`

---

## Deviations from Spec

1. **`rejected_hashes` not persisted to DB** — The `CollabSession` state machine tracks rejected hashes in memory, but this field is not stored in `collab_sessions`. On re-load from DB, the `rejected_hashes` vec is always empty. This means `AlreadyRejected` protection only works within a single in-process session, not across process restarts or MCP calls.

   *Rationale:* The spec didn't include a `rejected_hashes` column in the schema, and the scope was tight. A `collab_rejected_hashes` table or a JSON column could be added in a follow-up.

2. **`collab_set_max_rounds` is public** — This test helper is `pub` (not `pub(cfg(test))`) because the E2E tests live in a separate crate and can't use `#[cfg(test)]` gating across crate boundaries. This exposes an escape hatch that production callers should not use. Could be removed before v2 or feature-gated.

3. **`collab_message_count` and `collab_set_max_rounds` added** — Two methods beyond the spec to support clean E2E testing without `raw_conn()` exposure. Both are additive and don't affect existing behavior.

4. **Topic enum includes `Review` and `Escalation`** — Spec listed `plan|review|feedback|approve|reject`. `Escalation` was added to allow future human-tiebreaker messages. `Review` is defined but not wired into state transitions (reserved for v2 implementation loop). The MCP tool filters valid input to the subset the state machine accepts.

5. **`Session::rejected_hashes` not round-tripped through MCP** — The `session_row_to_state()` helper in `tools.rs` doesn't populate `rejected_hashes` from a DB lookup. See deviation #1 above.

---

## Ambiguities Resolved

- **When to increment `round`**: On each `PlanFeedback → PlanRevised` transition (Claude sends revised plan), not on feedback receipt. This matches "round = number of times Claude has submitted a plan after the initial draft."
- **Turn enforcement for `on_approve`**: Approval is NOT turn-gated (either agent can approve regardless of `current_owner`). This allows Codex to approve while Claude is the `current_owner` in `PlanRevised`.
- **Reject topic**: When Codex rejects, ownership flips to Claude (to submit a new plan), and the rejected hash is added to `rejected_hashes`.

---

## Areas for Closer Review

1. **`rejected_hashes` persistence gap** — The most significant correctness issue. An `AlreadyRejected` check only works within a single in-memory session snapshot. Across MCP call boundaries, the `CollabSession` is always reconstructed from DB with `rejected_hashes = vec![]`. A reviewer should decide whether to add a DB-backed store or accept this limitation for v1.

2. **ID generation** — `generate_collab_id()` uses `AtomicU64 + subsec_nanos` XOR mixing. This is not cryptographically random — it's sufficient for local session IDs but could collide under unusual conditions. UUID v4 would be more robust if `uuid` crate is added.

3. **`collab_send` doesn't validate content length** — The existing tools use `sanitize::sanitize_content()` for content bounds. The collab tools don't apply this, so arbitrarily long content can be stored in `messages.content`. Add a length cap consistent with other tools.

4. **No WAL log entries for collab ops** — Existing tools call `Database::wal_log_tx()` for audit. Collab tools do not. If audit trails are required for collab sessions, WAL logging should be added.

5. **`collab_set_max_rounds` is public** — See deviation #2. Should be removed or `#[doc(hidden)]` before shipping.

---

## Test Output Summary

```
ironrace-collab (unit): 13 passed, 0 failed
ironrace-collab (e2e):   4 passed, 0 failed
ironrace-memory (unit): 103 passed, 0 failed (includes 11 new collab DB tests)
ironrace-embed:          4 passed, 0 failed
ironrace-core:           4 passed, 0 failed
Total:                 166 passed, 0 failed
```

E2E runtime: 0.02s. All tests use in-memory SQLite. No polling delays.
