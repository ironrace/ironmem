<!-- GENERATED from skills/ — do not edit -->
# Tier Lineup

`iron-plan` assigns a tier. This file resolves it to a concrete model. The tier
names are fixed: `cheap`, `standard`, `deep`, `frontier`. An unrecognized tier
is a hard error at plan-parse time — never default to `standard`.


| Tier | Model | Reasoning effort |
|---|---|---|
| `cheap` | `gpt-5.3-spark` | `low` |
| `standard` | `gpt-5.6-luna` | `medium` |
| `deep` | `gpt-5.6-terra` | `high` |
| `frontier` | `gpt-5.6-sol` | `high` |

Both values are settable on every dispatch — `reasoning_effort` is a direct
parameter of `spawn_agent`, so unlike the Claude lineup there is no
best-effort caveat here. Record the values you passed.
