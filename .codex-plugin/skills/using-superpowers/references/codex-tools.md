# Codex Tool Mapping

Skills use Claude Code tool names. When you encounter these in a skill, use your platform equivalent:

| Skill references | Codex equivalent |
|-----------------|------------------|
| `Task` tool (dispatch subagent) | `spawn_agent` (see [Named agent dispatch](#named-agent-dispatch)) |
| Multiple `Task` calls (parallel) | Multiple `spawn_agent` calls |
| Task returns result | `wait_agent` |
| Task completes automatically | `close_agent` to free slot |
| `TodoWrite` (task tracking) | `update_plan` |
| `Skill` tool (invoke a skill) | Skills load natively — just follow the instructions |
| `Read`, `Write`, `Edit` (files) | Use your native file tools |
| `Bash` (run commands) | Use your native shell tools |

## Subagent dispatch requires multi-agent support

Add to your Codex config (`~/.codex/config.toml`):

```toml
[features]
multi_agent = true
```

This enables `spawn_agent`, `wait_agent`, and `close_agent` for skills like `dispatching-parallel-agents` and `subagent-driven-development`.

## Named agent dispatch

Claude Code skills reference named agent types like `superpowers:code-reviewer`.
Codex can expose named project/user roles through `[agents.<name>]` config
entries. Prefer a configured native role when one matches the work (for
example, `reviewer`, `explorer`, or `docs_researcher`). The plugin itself does
not own those config entries, so this repository's defaults must remain
correct even when a role is not configured.

When a skill says to dispatch a named agent type:

1. Find the agent's prompt file (e.g., `agents/code-reviewer.md` or the skill's
   local prompt template like `code-quality-reviewer-prompt.md`)
2. Read the prompt content
3. Fill any template placeholders (`{BASE_SHA}`, `{WHAT_WAS_IMPLEMENTED}`, etc.)
4. Spawn the configured native role with the filled content as the `message`
   and the explicit model/reasoning override from the routing table below. If
   the role or override is unavailable, spawn a `worker` agent with that same
   explicit override.

| Skill instruction | Codex equivalent |
|-------------------|------------------|
| `Task tool (superpowers:code-reviewer)` | `spawn_agent(agent_type="reviewer", model="gpt-5.6-terra", reasoning_effort="high", message=...)` when configured; otherwise `spawn_agent(agent_type="worker", model="gpt-5.6-terra", reasoning_effort="high", message=...)` with `code-reviewer.md` content |
| `Task tool (general-purpose)` with inline prompt | `spawn_agent(agent_type="worker", model="gpt-5.6-luna", reasoning_effort="medium", message=...)` for exploration/mechanical work, or the matching explicit row below |

### Message framing

The `message` parameter is user-level input, not a system prompt. Structure it
for maximum instruction adherence:

```
Your task is to perform the following. Follow the instructions below exactly.

<agent-instructions>
[filled prompt content from the agent's .md file]
</agent-instructions>

Execute this now. Output ONLY the structured response following the format
specified in the instructions above.
```

- Use task-delegation framing ("Your task is...") rather than persona framing ("You are...")
- Wrap instructions in XML tags — the model treats tagged blocks as authoritative
- End with an explicit execution directive to prevent summarization of the instructions

### Model routing defaults

Use explicit overrides for generic workers so behavior does not depend on the
user's current Codex default:

| Work | Model | Reasoning effort |
|---|---|---|
| Implementation controller/workers | `gpt-5.6-luna` | `max` |
| Exploration, docs, and mechanical work | `gpt-5.6-luna` | `medium` |
| Planning and normal review | `gpt-5.6-terra` | `high` |
| Architecture/security escalation | `gpt-5.6-sol` | `high` |

Sol is an explicit escalation tier, not the routine default. A native role's
config may provide its own baseline, but protocol-critical dispatches should
still pass the phase-appropriate override when the API allows it.

## Environment Detection

Skills that create worktrees or finish branches should detect their
environment with read-only git commands before proceeding:

```bash
GIT_DIR=$(cd "$(git rev-parse --git-dir)" 2>/dev/null && pwd -P)
GIT_COMMON=$(cd "$(git rev-parse --git-common-dir)" 2>/dev/null && pwd -P)
BRANCH=$(git branch --show-current)
```

- `GIT_DIR != GIT_COMMON` → already in a linked worktree (skip creation)
- `BRANCH` empty → detached HEAD (cannot branch/push/PR from sandbox)

See `using-git-worktrees` Step 0 and `finishing-a-development-branch`
Step 1 for how each skill uses these signals.

## Codex App Finishing

When the sandbox blocks branch/push operations (detached HEAD in an
externally managed worktree), the agent commits all work and informs
the user to use the App's native controls:

- **"Create branch"** — names the branch, then commit/push/PR via App UI
- **"Hand off to local"** — transfers work to the user's local checkout

The agent can still run tests, stage files, and output suggested branch
names, commit messages, and PR descriptions for the user to copy.
