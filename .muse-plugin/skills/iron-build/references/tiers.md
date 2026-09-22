<!-- GENERATED from skills/ — do not edit -->
# Tier Lineup

`iron-plan` assigns a tier. This file resolves it to a concrete model. The tier
names are fixed: `cheap`, `standard`, `deep`, `frontier`. An unrecognized tier
is a hard error at plan-parse time — never default to `standard`.



| Tier | Model | Effort |
|---|---|---|
| `cheap` | `muse-spark-1.3` | `low` |
| `standard` | `muse-spark-1.3` | `medium` |
| `deep` | `muse-spark-1.3` | `high` |
| `frontier` | `muse-spark-1.3` | `max` |

All four rows share one model family: on Muse, effort is the only routing
dial. The model column names the installed `muse-spark` id; if the session
runs the `-contributor` variant, pass that id instead — same model, same
effort table. `minimal` and `xhigh` exist as reasoning efforts but sit
outside this table: below `low` a worker is underpowered for plan tasks, and
nothing here needs a stop between `high` and `max`.

## Effort is not settable on every dispatch path

Only `Workflow`'s `agent()` accepts per-call `model`/`effort`.
`subagent_spawn` inherits the parent route and takes no routing parameters,
so:

- Dispatching through `Workflow` → the model and effort columns are applied.
- Dispatching through `subagent_spawn` → **the tier does not take effect**.
  The child runs at whatever route the parent session uses.

Record which path you used. Never report that a tier was applied when it
was not — the routing dataset is only worth keeping if it is honest.
