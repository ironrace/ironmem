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
| `frontier` | `opus` | `claude-opus-5` | `xhigh` |

The **model alias** is the operative value. Subagent dispatch takes
`model: haiku|sonnet|opus`, not a full API id; the API id column is for
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

## `deep` and `frontier` resolve to the same model

Both rows are Opus 5 at `xhigh`. The tiers stay distinct as *plan vocabulary* —
`iron-plan` keeps assigning them, and the reviewer floor keeps reading them —
but on the Claude lineup they dispatch identically.

Two consequences, both already implied by `../SKILL.md`:

- A `deep` task's `frontier` reviewer is a fresh-context reviewer, not a
  stronger model. That is the point: independent eyes on the diff. Do not
  claim a capability step-up in the run record.
- Escalation out of `deep` has no higher model to reach for, so treat a failing
  `deep` task the way the skill already treats a failing `frontier` one — stop
  and ask the human rather than re-running the same model against itself.

Record `tier_used` as the tier the plan named. The model column is what makes
the routing dataset honest.
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
