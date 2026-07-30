# Tier Lineup

`iron-plan` assigns a tier. This file resolves it to a concrete model. The tier
names are fixed: `cheap`, `standard`, `deep`, `frontier`. An unrecognized tier
is a hard error at plan-parse time — never default to `standard`.

<!-- harness:claude -->
| Tier | Model alias | API id | Effort |
|---|---|---|---|
| `cheap` | `haiku` | `claude-haiku-4-5` | *(unsupported — do not pass)* |
| `standard` | `sonnet` | `claude-sonnet-5` | `medium` |
| `deep` | `opus` | `claude-opus-5` | `xhigh` |
| `frontier` | `fable` | `claude-fable-5` | `high` |

The **model alias** is the operative value. Subagent dispatch takes
`model: haiku|sonnet|opus|fable`, not a full API id; the API id column is for
traceability only.

## Effort is not settable on every dispatch path

The `Agent` tool accepts `model` but has **no `effort` parameter**. Only
`Workflow`'s `agent()` accepts `effort`. So:

- Dispatching through `Workflow` → the effort column is applied.
- Dispatching through the plain `Agent` tool → **only the model takes effect**.
  The effort column is documentation.

Record which path you used. Never report that an effort was applied when it
was not — the routing dataset is only worth keeping if it is honest.

**Haiku 4.5 rejects `effort` outright** (HTTP 400). The `cheap` row carries no
effort value on purpose. Do not synthesize one, on either path.

## When `frontier` returns 400

Claude Fable 5 requires 30-day data retention. An organization configured for
zero data retention gets `400 invalid_request_error` on *every* request,
regardless of the task. ironmem ships to other users, so treat that 400 as a
**configuration problem, not a task failure**:

1. Say plainly that `frontier` is unavailable under this org's data-retention
   setting.
2. Fall back to `deep` and state the substitution.
3. Continue. Do not report the task as failed, and do not retry `frontier`
   again in this run.
<!-- /harness -->

<!-- harness:codex -->
| Tier | Model | Reasoning effort |
|---|---|---|
| `cheap` | `gpt-5.3-spark` | `low` |
| `standard` | `gpt-5.6-luna` | `medium` |
| `deep` | `gpt-5.6-terra` | `high` |
| `frontier` | `gpt-5.6-sol` | `high` |

Both values are settable on every dispatch — `reasoning_effort` is a direct
parameter of `spawn_agent`, so unlike the Claude lineup there is no
best-effort caveat here. Record the values you passed.
<!-- /harness -->
