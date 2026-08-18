# HumanLayer Pilot Threat Model

**Review date:** 2026-08-17
**Scope:** HumanLayer evaluation for issues [#304](https://github.com/ironrace/ironmem/issues/304) and [#311](https://github.com/ironrace/ironmem/issues/311). This is the authoritative security gate for this pilot.

**Evidence snapshot.** The facts below were checked against the linked primary documentation, the npm registry metadata, and this repository at this review date. “Fact” means the source says it; “inference” is a security conclusion from facts; “unknown” is deliberately not filled in by marketing, a generic privacy policy, or a source-code scan. A normal developer shell failed the secret-presence negative control: multiple secret-shaped variable *names* existed. Values were neither read nor printed. That shell and account are therefore not approved.

## Executive decision

**Broader rollout: BLOCKED. All HumanLayer pilot launches are currently BLOCKED**, including public or synthetic material. Public or synthetic material in one disposable selected GitHub repository on a dedicated approved **Linux** host/account is the only repository class eligible to become **PERMITTED** after every mandatory gate passes, including the currently unimplemented static preflight gate below. macOS/Darwin pilot execution is **PROHIBITED** and **BLOCKED**. Private, internal, confidential, regulated, customer-data, production-connected, secret-bearing, signing, deployment, and infrastructure-administration repositories are **PROHIBITED**.

This decision is an inference from a daemon whose agent has the authority of its OS user, cloud task/session/artifact flow, GitHub write permissions, non-subtractive default file copying, unresolved vendor/model evidence, and unresolved Darwin dispatcher/TOCTOU evidence. The current Mac developer environment and any macOS execution of these documentation checks are non-qualifying development evidence only, never pilot approval. A human confirmation cannot override this host classification; changing it requires a separately approved security issue/design plus regression and canary evidence.

## Compatibility invariant

All HumanLayer work is additive new code, configuration, or documentation. It must not overwrite or change `/collab`, `iron-build`, `iron-plan`, `iron-spec`, `iron-tdd`, or their support files or contracts. Existing workflows remain independently runnable. A shared-boundary change requires a separate approved issue and regression coverage; this pilot does not authorize one.

## Sources and evidence quality

### Primary HumanLayer evidence

| Source | Established fact / qualification |
|---|---|
| [Remote daemons](https://docs.humanlayer.com/explanation/remote-daemons) | The API sends work to the daemon and receives events; the daemon agent has the file/network authority of its OS user. Login refresh/session material is under `~/.humanlayer/riptide/`; a launch token exchanged for process credentials is a credential. |
| [Tasks](https://docs.humanlayer.com/explanation/tasks) | Task files are under `.humanlayer/tasks/<slug>/`; supported edits synchronize task artifacts between local and cloud, including text/object-storage behavior. |
| [Workspace config](https://docs.humanlayer.com/reference/workspace-config) | `copyGlobs` append and deduplicate; `disabled: true` disables automatic worktree setup; `setupCommand` executes as `sh -c` after copying. |
| [GitHub integration](https://docs.humanlayer.com/guide/github-integration) | The app requests Contents, Issues, Pull requests read/write and Metadata read; selected-repository installation is available; issue/comments may be imported and artifact links written back. |
| [Codex guide](https://docs.humanlayer.com/guide/codex) and [Bedrock guide](https://docs.humanlayer.com/guide/bedrock) | HumanLayer launches provider tools on the host and provider/environment setup is material to the boundary. These are not evidence of a HumanLayer sandbox. |
| [Current product](https://www.humanlayer.com/) | The service synchronizes through HumanLayer APIs and uses user-provided model subscriptions/credentials; the site says the current product is not fully open source. |
| [Privacy policy](https://www.humanlayer.dev/legal/privacy-policy.html) | Generic service-provider/cloud/analytics categories, product-improvement language, retention as needed, deletion/anonymization with isolated backups, reasonable security, limited employee access, and legally required breach notice are described. It does **not** establish product-specific no-training, retention period, named subprocessors, deletion SLA/verification, encryption, tenant isolation, audit logs, incident SLA, DPA, SOC 2, or penetration testing. |
| [npm package](https://www.npmjs.com/package/@humanlayer/cli) and [registry metadata](https://registry.npmjs.org/@humanlayer%2fcli) | Inspected release: `@humanlayer/cli` 0.31.59; wrapper integrity `sha512-uC6nCjYPOT55oHt9kOPhk3WCXbH2/bOPKz/6Xqg1EcM41x46UHAuUyzIxWbsQKfj43TBfqX6aKe6v2PPcvUnFw==`; wrapper shasum `81da4e2e57a68542463b682cbca69ce24af56d16`; darwin-arm64 shasum `33490919cf505fe08a955535dd710fcc4f4a1fd5`; downloaded Darwin platform binary SHA-256 `1ea566ece5d0b13f31514e99727fe2e97427c6a82b68ad709a8ed6ccc2d64791`. This Darwin ARM64 tuple is an evidence snapshot only, not an approved pilot artifact. Before any Linux pilot launch, capture and approve the actual Linux platform package/binary version, integrity, shasum, downloaded-binary SHA-256, and absolute executable path. |
| [Pinned public code snapshot](https://github.com/humanlayer/humanlayer/tree/99abe673498cf8bdcd5f989aebe9406a27185b3b) | Commit `99abe673498cf8bdcd5f989aebe9406a27185b3b`, dated 2026-06-18, is a reproducible public-code snapshot only; it is not assurance for the current hosted product. |

The current CLI help exposes provider/model/thinking choices but no sandbox/approval switches. A binary string scan similarly cannot prove effective subprocess sandbox or approvals. **Unknown:** the effective agent subprocess policy. Compensate with host isolation and canaries.

### Provider and IronMem evidence

[OpenAI’s agent approvals/security](https://learn.chatgpt.com/docs/agent-approvals-security) and [sandboxing](https://learn.chatgpt.com/docs/sandboxing) describe Codex’s own OS sandbox, approvals, and prompt-injection considerations. They are capabilities of Codex, not proof that HumanLayer launches Codex with those defaults. [OpenAI API data controls](https://developers.openai.com/api/docs/guides/your-data#storage-requirements-and-retention-controls-per-endpoint) establish API no-training-by-default unless opted in and conditional abuse-log/application-state retention; Zero Data Retention and Modified Abuse Monitoring require eligibility/configuration.

[Anthropic consumer retention](https://privacy.claude.com/en/articles/10023548-how-long-do-you-store-my-data), [training use](https://privacy.claude.com/en/articles/7996885-how-do-you-use-personal-data-in-model-training), and the [Claude Code FAQ](https://support.claude.com/en/articles/14554922-claude-code-user-faq) distinguish consumer settings/retention from commercial Team, Enterprise, and API no-training-by-default unless opted into a program.

IronMem evidence is local and repository-relative: [local SQLite and shared store](../README.md#shared-memory-across-harnesses), [daemon/socket configuration](../README.md#shared-daemon-mode), [isolated `IRONMEM_DB_PATH`](../README.md#shared-memory-across-harnesses), [access mode](../README.md#use-with-any-mcp-client), and [optional rerank/preferences](../README.md#llm-rerank-opt-in). Recalled memory included in an agent prompt crosses the selected provider boundary. Default shared storage is unsuitable for this pilot; optional LLM reranking/preference extraction must stay off.

## System and trust boundaries

```mermaid
flowchart LR
  subgraph H[Local pilot host]
    P[Pilot owner]
    CL[HumanLayer client / dedicated OS account]
    D[HumanLayer daemon + agent subprocess]
    W[Sanitized precreated worktree]
    P -->|out-of-band approval| CL
    D <-->|source, diffs, tool calls/results| W
  end
  subgraph C[HumanLayer cloud / authorization boundary]
    API[Authorized API, task/session service, artifact sync]
  end
  subgraph G[GitHub]
    R[One disposable repository]
  end
  subgraph I[IronMem local store]
    M[MCP + pilot SQLite + pilot socket]
  end
  subgraph MP[Model provider]
    X[Selected provider account]
  end
  subgraph AP[Anthropic optional LLM provider]
    A[Anthropic account / endpoint]
  end
  CL -->|task/session commands and authentication| API
  API -->|authorized task/session commands| D
  D -->|session events| API
  API -->|session status/events| CL
  D <-->|task artifact sync| API
  API <-->|issue/import/comment/PR traffic| R
  D <-->|agent prompts/source/tool results| X
  D <-->|MCP calls| M
  M <-->|local SQLite| M
  M -. disabled optional IronMem-to-Anthropic path .-> A
```

The host is the primary authority boundary. HumanLayer cloud, GitHub, IronMem’s local Unix-socket/SQLite boundary, and the selected model provider are distinct controllers/processors. Every arrow is a potential disclosure or command path; untrusted content does not become authority by crossing a boundary.

## Data inventory and flow

| Data class | Source → destination | Persistence | Pilot handling |
|---|---|---|---|
| Issue/comment/ticket data | GitHub → HumanLayer → daemon | GitHub, cloud/task files | Public/synthetic only; treat as untrusted. |
| Prompts/messages/session events | Pilot/daemon → HumanLayer and provider | Cloud/provider per account terms | No sensitive prompts; capture account evidence. |
| Source/diffs/tool calls/results | Worktree/agent → provider; selected artifacts → cloud | Worktree, provider, possible artifacts | Sanitized disposable repo; human review. |
| Task artifacts | `.humanlayer/tasks/` ↔ HumanLayer | Local and cloud/object storage | Stop on sensitive content; request deletion. |
| HumanLayer auth/launch tokens | Login/launch → daemon/cloud | `~/.humanlayer/riptide/`, cloud | Pilot-only, short-lived where possible; revoke at teardown. |
| GitHub App credentials | App/cloud → selected repository | GitHub/HumanLayer | One repo, exact manifest; never expose token. |
| Provider credentials | Pilot account → provider CLI/API | Account/credential store | Pilot-only, short-lived; no browser/personal auth. |
| Copied workspace files | Source repo → generated worktree | Local worktree | Automatic copying disabled; manual sanitized worktree only. |
| IronMem drawers/diary/code maps/metrics | Agent MCP ↔ local SQLite | Pilot SQLite/socket | Isolated paths; no cross-repo recall; metrics reviewed. |
| Telemetry/crash data | CLI/host → vendor/provider | Vendor/host logs | Minimize, classify as possible disclosure, retain incident evidence. |

## Pilot architecture and mandatory controls

1. Use a dedicated approved **Linux** non-admin OS account on an approved immutable/measured Linux host image (or verified-boot equivalent), with a recorded kernel, OS release, architecture, patch baseline, restricted filesystem, explicit network allowlist, and a trusted privileged Host-operator supervisor/control plane. The pilot account has no write or control access to that supervisor, the static helper, their parent chains, or approved executable locations. Any baseline drift is **BLOCKED**. The normal developer environment and every macOS/Darwin execution environment are **PROHIBITED** for the pilot.
2. Use one disposable selected GitHub repository containing only public/synthetic material. Protect its default branch/ruleset; create draft PRs only; no merge or deployment.
3. The **Pilot owner** may give out-of-band confirmation only for an explicitly allowed, bounded human-gated action within this public/synthetic, draft-only pilot (for example, beginning an otherwise compliant session or creating an allowed draft PR). Computer control for permission changes or destructive actions is not human-gated: it is **PROHIBITED**.
4. Pin package versions and integrity; disable automatic workspace setup; forbid a local override; manually precreate and sanitize the worktree.
5. The only qualifying preflight boundary is a future, statically linked, reviewed Linux executable at `/opt/ironmem-humanlayer/policy/ironmem-humanlayer-preflight`, invoked directly with an `execve`-equivalent by the trusted Host-operator supervisor. It must not use a shell, interpreter, `PATH` lookup, `uname`, `printf`, `mktemp`, `grep`, `head`, Python, Git, or any subprocess. It does not yet exist: its additive implementation, independent review, pinning, installation, and approval are `STATIC-HELPER-MISSING`, a **BLOCKED** launch gate even for public/synthetic material.
6. The trusted supervisor directly executes only approved canonical HumanLayer and, if separate, provider artifacts after the static helper returns a bounded structured pass record. No production secrets, `.env` defaults, personal GitHub/cloud auth, SSH identities, inherited credential helpers, shell launcher, or `PATH` lookup is permitted.
7. Set pilot-specific `IRONMEM_DB_PATH` and `IRONMEM_DAEMON_SOCKET`. Keep `IRONMEM_MCP_MODE` least-privilege for the task, with IronMem LLM rerank and LLM preference extraction off.
8. Back up the pilot SQLite/worktree only to approved pilot storage and review every diff/artifact before any external action.

## GitHub least-privilege permission manifest

**Installation target:** exactly one named disposable pilot repository, selected-repositories mode. **Owner:** **GitHub administrator**; **reviewer:** **Security reviewer**.

| Requested | Explicitly absent |
|---|---|
| Contents read/write; Issues read/write; Pull requests read/write; Metadata read | Actions, Administration, Checks, Deployments, Environments, Members, Organization administration, Packages, Pages, Secrets, Workflows |

Capture the installation repository list and permission screen before launch and each session. Permission/repository drift is **BLOCKED**: suspend/disconnect the app, revoke changed grants, preserve evidence, and require new approval. Branch protection/ruleset must prohibit direct default-branch writes and merging by the pilot automation.

## Credential and environment preflight

The normal review shell failed a negative control because secret-shaped names were present; values were never read and exact names are not published. It is **PROHIBITED** for the pilot.

The **Host operator** records only names/classes and presence, never values. The future static preflight executable, not a host command or script, must enumerate environment keys internally without serializing values; it must emit only a bounded structured pass/fail record. The operator separately records SSH-identity presence, GitHub/credential-helper status, and expected pilot credential-file presence/permissions without printing keys, environment values, tokens, `.env` contents, or credentials.

The following synthetic adversarial block is **non-qualifying CI/development evidence only**. It is retained as a test-vector specification for the future static helper; it must never authorize a pilot, be installed as a launch gate, or be treated as trusted host evidence. It contains no real secret, places distinct sentinels on both lines of a synthetic value and another in a synthetic `sitecustomize` startup hook reachable only through `PYTHONPATH`, and demonstrates that a candidate `/usr/bin/python3 -I -S -c` key enumerator avoids those sentinels while deliberately unsafe serializers and startup-hook execution are detected:

```sh
set -eu
if [ "$(/usr/bin/uname -s)" != Linux ]; then
  /usr/bin/printf '%s\n' 'BLOCKED: non-Linux enumerator result is non-qualifying pilot evidence' >&2
  exit 2
fi
test_dir="$(/usr/bin/mktemp -d)"
export test_dir
trap '/usr/bin/python3 -I -S -c '"'"'import os; p = os.environ["test_dir"]; os.unlink(p + "/sitecustomize.py"); os.rmdir(p)'"'"'' EXIT HUP INT TERM
/usr/bin/printf '%s\n' 'import sys; print("PYTHON_STARTUP_SENTINEL")' > "$test_dir/sitecustomize.py"
synthetic_value="$(/usr/bin/printf 'SYNTHETIC_FIRST_LINE_SENTINEL\nSYNTHETIC_SECOND_LINE_SENTINEL')"
require_absent() {
  output="$1"
  sentinel="$2"
  label="$3"
  if /usr/bin/printf '%s\n' "$output" | /usr/bin/grep -Fq "$sentinel"; then
    /usr/bin/printf '%s\n' "$label leaked a synthetic sentinel" >&2
    exit 1
  fi
}
require_detected() {
  output="$1"
  sentinel="$2"
  label="$3"
  if ! /usr/bin/printf '%s\n' "$output" | /usr/bin/grep -Fq "$sentinel"; then
    /usr/bin/printf '%s\n' "$label was not detected" >&2
    exit 1
  fi
  /usr/bin/printf '%s\n' "$label detected and fail-closed"
}
has_exact_line() {
  output="$1"
  expected="$2"
  found=1
  while IFS= read -r line || [ -n "$line" ]; do
    if [ "$line" = "$expected" ]; then
      found=0
      break
    fi
  done <<EOF
$output
EOF
  return "$found"
}
has_only_environment_key_names() {
  output="$1"
  saw_key=1
  while IFS= read -r line || [ -n "$line" ]; do
    [ -n "$line" ] || continue
    case "$line" in
      [!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_]* | *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_]*) return 1 ;;
    esac
    saw_key=0
  done <<EOF
$output
EOF
  return "$saw_key"
}
approved_output_is_valid() {
  output="$1"
  has_exact_line "$output" SYNTHETIC_MULTILINE_VALUE || return 1
  has_exact_line "$output" PYTHONPATH || return 1
  has_only_environment_key_names "$output" || return 1
  require_absent "$output" SYNTHETIC_FIRST_LINE_SENTINEL 'approved key enumerator'
  require_absent "$output" SYNTHETIC_SECOND_LINE_SENTINEL 'approved key enumerator'
  require_absent "$output" PYTHON_STARTUP_SENTINEL 'approved key enumerator'
}
approved_output="$(
  /usr/bin/env -i PATH=/usr/bin:/bin \
    SYNTHETIC_MULTILINE_VALUE="$synthetic_value" \
    PYTHONPATH="$test_dir" \
    /usr/bin/python3 -I -S -c 'import os; print("\n".join(sorted(os.environ.keys())))'
)"
if ! approved_output_is_valid "$approved_output"; then
  /usr/bin/printf '%s\n' 'approved key enumerator output failed validation' >&2
  exit 1
fi
/usr/bin/printf '%s\n' "$approved_output"
empty_output=''
if approved_output_is_valid "$empty_output"; then
  /usr/bin/printf '%s\n' 'empty approved output was accepted' >&2
  exit 1
fi
/usr/bin/printf '%s\n' 'empty approved output rejected and fail-closed'
filtered_output='PATH'
if approved_output_is_valid "$filtered_output"; then
  /usr/bin/printf '%s\n' 'filtered approved output was accepted' >&2
  exit 1
fi
/usr/bin/printf '%s\n' 'filtered approved output rejected and fail-closed'
full_value_output="$synthetic_value"
require_detected "$full_value_output" SYNTHETIC_FIRST_LINE_SENTINEL 'full-value serializer'
require_detected "$full_value_output" SYNTHETIC_SECOND_LINE_SENTINEL 'full-value serializer'
first_line_output="$(/usr/bin/printf '%s\n' "$synthetic_value" | /usr/bin/head -n 1)"
require_detected "$first_line_output" SYNTHETIC_FIRST_LINE_SENTINEL 'first-line-only serializer'
startup_hook_output="$(
  /usr/bin/env -i PATH=/usr/bin:/bin PYTHONPATH="$test_dir" \
    /usr/bin/python3 -c 'import os; print("\n".join(sorted(os.environ.keys())))'
)"
require_detected "$startup_hook_output" PYTHON_STARTUP_SENTINEL 'startup-hook execution'
```

Expected result on a Linux development host is exit status 0: the candidate output is key names only, contains exact lines `SYNTHETIC_MULTILINE_VALUE` and `PYTHONPATH`, and every nonempty emitted line has environment-key syntax (no `=`, whitespace, or non-name characters). A platform or interpreter may add key names, but it must never contain either value sentinel or the startup sentinel. The control reports that empty/filtered output and the three deliberately unsafe paths were detected and fail closed, without printing their unsafe outputs. On Darwin/macOS it exits 2 and is non-qualifying **BLOCKED** evidence. Any unexpected sentinel appearance, malformed key line, or undetected rejected path is a CI/development failure and a required test-vector fix before the static helper may be approved. `/usr/bin/python3` layout, including standard Ubuntu symlinks, is irrelevant to pilot qualification because Python is not in the qualifying control.

`scripts/test_humanlayer_workspace_policy.py`, its pytest bridge, explicit-root checks, and the shell block above are likewise **non-qualifying CI/development evidence only**. They must never be installed, directly executed, or relied on as the pilot launch security boundary. The future static helper must implement equivalent or stronger internal test vectors and checks.

The foundational trust assumption is an approved immutable/measured Linux image (or verified-boot equivalent), recorded kernel/OS/architecture/patch baseline, and a trusted privileged Host-operator supervisor. The supervisor—not the agent or pilot user—directly executes the one static helper with an `execve`-equivalent and receives its bounded structured result. The helper must internally: assert its Linux platform/architecture from its build/runtime ABI; enumerate environment keys without values; self-test full-value, first-line, empty/filtered, and startup/config-injection failure modes; perform exact workspace JSON equality; reject a local override; verify the tracked repository `.gitignore` has the exact local-override rule; and validate an exact canonical repository root without spawning Git. It must not invoke a shell, interpreter, `PATH` lookup, `uname`, `printf`, `mktemp`, `grep`, `head`, Python, Git, or another subprocess.

The helper is intentionally unimplemented. It must be delivered only as additive new code in a separate approved security issue, then independently reviewed, reproducibly built, statically linked for the approved Linux target, pinned, signed, installed, and approved by the **Security reviewer** before any pilot launch. Required review material is source, reproducible build recipe, compiler and dependency versions, SBOM, Linux target and static-link evidence, test vectors, artifact path, SHA-256, and signature. Until then, `STATIC-HELPER-MISSING` is a fail-closed **BLOCKED** condition.

When that future helper is approved, the **Host operator** performs its atomic privileged installation at `/opt/ironmem-humanlayer/policy/ironmem-humanlayer-preflight`: stage reviewed bytes in the same root-controlled policy directory, set numeric UID/GID 0 (`root:root`) and a non-writable mode, fsync the staged file where supported, verify its approved hash and signature, atomically rename it into place, then fsync the destination directory where supported. Manifest approval follows only that durable rename. The agent and pilot users must never write the staged file, leaf, or any parent.

Before every launch after implementation, trusted host-side controls inspect `/opt`, `/opt/ironmem-humanlayer`, `/opt/ironmem-humanlayer/policy`, and the static-helper leaf with `lstat` or equivalent—not an agent-provided path. Each must be canonical and non-symlink, numeric UID/GID 0 (`root:root`), and not writable by the pilot, group, or other. Record numeric UID/GID/mode and resolved owner/group names for every component, plus helper SHA-256 and signature status, and compare them to the approved manifest. The trusted supervisor then directly executes the static helper, with no shell or intermediary executable, against the canonical pilot worktree and accepts only its bounded structured pass record.

Before the supervisor starts HumanLayer or a separate provider executable, trusted host-side controls must verify each artifact and every parent: canonical non-symlink path; numeric UID/GID 0 (`root:root`); no pilot/group/other write bit; version, SHA-256, and signature against the approved manifest. The supervisor directly executes the verified artifact in a TOCTOU-resistant manner, never through a shell or `PATH` lookup. If the provider is embedded in HumanLayer, the **Model-provider administrator** explicitly records that fact and binds provider evidence to the verified HumanLayer artifact; there is no separate provider executable path.

Darwin/macOS remains a **PROHIBITED** rationale, not an implemented control: `/usr/bin/python3` and `/usr/bin/git` may dispatch through Xcode/CommandLineTools; the reviewed evidence does not pin dispatchers, selected Developer directory, resolved target paths/hashes, the full Developer/target parent-chain immutability, or TOCTOU-resistant execution end-to-end. `root:wheel` (UID 0, GID 0) and `/usr/bin/xcrun` are recorded only as facts explaining why macOS is blocked. A separately approved security issue/design, regression coverage, and canary evidence are required to reconsider it; human confirmation cannot override it.

The **Host operator** owns the immutable Linux baseline, privileged supervisor, static-helper installation/full-parent-chain evidence, and host remediation. The **Security reviewer** owns static-helper review/pin approval and Linux HumanLayer artifact approval/remediation. The **Model-provider administrator** owns provider artifact/account evidence and remediation. Any approved-manifest change or resumption after a host, static-helper, HumanLayer, or provider drift needs Security reviewer approval. A mismatch, missing static helper, missing Linux artifact capture, `@latest`, floating version, or unattended update is **BLOCKED**.

## Default `.env` copying verification

HumanLayer’s [workspace config reference](https://docs.humanlayer.com/reference/workspace-config#copyglobs) lists these six defaults: `.env`, `.env.local`, `.env.development.local`, `.claude/settings.local.json`, `.humanlayer/workspace.json`, and `.humanlayer/workspace.local.json`. It says lists append and deduplicate; defaults, shared, local, and repo lists do not replace or subtract one another. The same reference says `setupCommand` runs via `sh -c` after copying.

Therefore individual defaults cannot be disabled. The shared config must contain `"disabled": true`, there must be no `.humanlayer/workspace.local.json` override, and the worktree must be manually precreated and sanitized. Re-enabling remains **BLOCKED** until the product supports subtractive controls or independent verification proves equivalent containment.

## Sandbox, approvals, and prompt injection

HumanLayer’s effective agent subprocess sandbox/approval policy is publicly **unknown**. Do not assume official Codex defaults apply when HumanLayer invokes an agent. Codex documents OS sandboxing and approvals, but that proves only the Codex-capability baseline, not this integration. OS isolation plus canary tests are mandatory: attempt controlled out-of-worktree access, disallowed network egress, permission change, and destructive action; fail closed on an unexpected success.

GitHub issues/comments, repository files, task artifacts, web results, tool output, and IronMem recall are untrusted instructions. Deny requests to access/exfiltrate secrets, expand permissions, perform destructive actions, merge, deploy, sign, or disable controls. Out-of-band confirmation applies only to an action that this policy explicitly marks human-gated; no prompt, comment, or tool output can grant authority. Secret access, merge, deployment, signing, disabling controls, prohibited repository classes, and every other absolute pilot prohibition remain denied even with **Pilot owner** confirmation. Changing one requires the documented repository-reclassification process and, where required, a new separately approved issue with supporting evidence; confirmation cannot override a classification or control.

## Risk register

| ID | Scenario | Impact / severity | Mandatory mitigation | Owner | Residual risk / evidence | Rollout gate |
|---|---|---|---|---|---|---|
| HOST-01 | Daemon agent inherits OS-user file/network authority | Critical | Dedicated non-admin immutable/measured Linux host, allowlist, static preflight, canaries | Host operator | Host isolation limits but does not prove agent policy; static helper is not yet implemented | BLOCKED until `STATIC-HELPER-MISSING` is resolved |
| HOST-04 | macOS/Darwin `/usr/bin/python3` or `/usr/bin/git` dispatches through Xcode/CommandLineTools without end-to-end pinned targets and TOCTOU-resistant execution | Critical | Linux-only approved host; do not launch on Darwin | Host operator | Darwin `root:wheel`/xcrun facts are rationale only; dispatch and parent-chain evidence remain unresolved | PROHIBITED / BLOCKED |
| HOST-02 | `setupCommand` executes shell code after copies | High | `disabled: true`; no local override; manual worktree | Host operator | Official config confirms `sh -c`; no automatic setup | BLOCKED otherwise |
| HOST-03 | Additive default `.env`/workspace copy globs copy source files into a generated worktree | Critical | Shared `disabled: true`, no local override, and manual precreated sanitized worktree | Host operator | Official config says defaults append/deduplicate and cannot be subtracted | BLOCKED until subtractive control or independently verified equivalent containment |
| CRED-01 | Inherited environment, SSH, `gh`, cloud, or `.env` credentials leak | Critical | Clean account; static helper key-only preflight; no personal auth or inherited credential helper | Host operator | Normal shell failed negative control; helper is unimplemented | BLOCKED until static-helper evidence exists |
| CRED-02 | HumanLayer launch/refresh token is stolen | High | Treat as credential; pilot-only storage, revoke/rotate | Vendor owner | Token-path fact, storage controls unknown | BLOCKED for broad use |
| GH-01 | GitHub App content/issue/PR write scope is abused | High | One disposable repo; draft PR only; ruleset | GitHub administrator | Official requested scope is broad within repo | BLOCKED until all pilot gates pass |
| GH-02 | Repo/permission drift expands blast radius | Critical | Capture manifest; suspend/disconnect on drift | GitHub administrator | Requires operational review evidence | BLOCKED on drift |
| PI-01 | Prompt injection from issue, code, artifact, web/tool/memory | High | Treat all as untrusted; out-of-band gate; canaries | Pilot owner | No prompt policy is complete | BLOCKED until all pilot gates pass |
| PI-02 | Computer control changes permissions or destroys data | Critical | No computer control for those actions | Pilot owner | No authorized pilot path; residual bypass risk is limited by host enforcement and containment canaries | PROHIBITED |
| SC-01 | npm, platform binary, Homebrew, desktop, model CLI updates are compromised | Critical | Exact pins/integrity, reviewed updates, no unattended update | Security reviewer | Integrity only identifies reviewed artifact | BLOCKED on mismatch |
| SC-03 | Linux HumanLayer platform package/binary is not pinned and integrity-captured | Critical | Capture/approve actual Linux package version, integrity, shasum, binary SHA-256, and canonical executable path before launch | Security reviewer | Inspected Darwin ARM64 tuple is evidence-only, not a Linux pilot artifact | BLOCKED before launch |
| SC-04 | `STATIC-HELPER-MISSING`: no independently reviewed, static Linux preflight executable exists | Critical | Separate approved additive-code issue; reviewed source/reproducible static build/compiler/dependencies/SBOM/test vectors; pin path/hash/signature; immutable installation | Security reviewer | The required executable is deliberately not claimed to exist | BLOCKED: no HumanLayer pilot launch |
| SC-02 | Hosted workflows/skills or GitHub App changes alter behavior | High | Version/permission change review, evidence capture | Vendor owner | Hosted behavior not pinned by public repo | BLOCKED pending review |
| VENDOR-01 | Cloud retains sessions/artifacts or uses them beyond expectations | Critical | Public/synthetic only; deletion request procedure | Vendor owner | Product-specific retention/training unknown | BLOCKED broadly |
| VENDOR-02 | Tenant, employee, subprocessor, audit, incident controls are inadequate | Critical | Obtain contractual/security evidence | Vendor owner | Generic privacy policy insufficient | BLOCKED broadly |
| MODEL-01 | Provider receives prompt/source/tool/memory data | Critical | Account-class/data-control capture; public/synthetic only | Model-provider administrator | Provider path depends on selected auth/account | BLOCKED until all launch gates, including static helper, pass |
| MODEL-02 | Browser/subscription terms are mistaken for API commitments | High | Verify auth mechanism and account class | Model-provider administrator | API controls do not automatically apply | BLOCKED absent evidence |
| IM-01 | Default shared IronMem store contaminates cross-repo context | High | Pilot DB/socket; later cross-repo canary | Host operator | Shared-store default is documented | BLOCKED on failed canary |
| IM-02 | Optional IronMem LLM rerank/preferences add egress | High | Leave rerank/preference extraction off | Host operator | Optional paths documented; disabled state must be captured | PROHIBITED when enabled |

## Vendor evidence and unanswered questions

Generic privacy statements are insufficient. Every critical gap below is a wider-rollout blocker owned by the **Vendor owner** until written, product-specific evidence is captured and approved by the **Security reviewer**.

| Evidence request | Current status / gate |
|---|---|
| Data classes leaving host; storage locations/regions; retention | Unknown product-specific answer — BLOCKED. |
| Training/product improvement | Generic language only; no product-specific opt-out/no-training assurance — BLOCKED. |
| Named subprocessors | Unknown — BLOCKED. |
| Deletion SLA, verification, and backup lifecycle | Generic deletion/anonymization/backups only — BLOCKED. |
| Encryption in transit/at rest and key management | Unknown — BLOCKED. |
| Tenant isolation and employee access | Generic limitation only; isolation/control detail unknown — BLOCKED. |
| Audit logs/export/retention | Unknown — BLOCKED. |
| Incident notification SLA and process | Legally-required-notice wording only — BLOCKED. |
| DPA, SOC 2, and penetration-test evidence | Not established by reviewed public evidence — BLOCKED. |
| Effective sandbox/approval policy; credential storage/rotation | Unknown — BLOCKED. |

## Model-provider controls

HumanLayer cloud is not the selected model provider; both data paths must be reviewed. Subscription/browser authentication is not API use. Do not apply OpenAI API commitments to ChatGPT/Codex subscription or browser authentication without explicit verification. OpenAI API evidence is conditional: API inputs/outputs are not used for training by default unless opted in, while abuse logging, application state, ZDR, and MAM vary by endpoint, eligibility, and configuration.

Anthropic’s official material distinguishes consumer retention/training-setting consequences from commercial Claude for Work/API no-training-by-default unless opted into a program; Claude Code account treatment must be verified for the actual plan. Before each pilot, the **Model-provider administrator** captures provider, account class, authentication method, data controls, retention, training setting, region, deletion setting, credential lifetime, and approval. Missing evidence is **BLOCKED**.

## Supply-chain manifest

No `@latest`, floating model, unattended update, or unreviewed auto-update is permitted. Each update is reviewed before use and reopens the relevant risk gates.

| Component/channel | Required evidence owner | Required record |
|---|---|---|
| npm CLI/meta package | Security reviewer | Exact `@humanlayer/cli` pin and npm integrity/shasum. |
| Immutable/measured Linux host and privileged supervisor | Host operator | Approved image or verified-boot evidence; kernel/OS/architecture/patch baseline; supervisor identity/control boundary; pilot account cannot write or control it; baseline drift blocks. Darwin/Xcode toolchain is unsupported pilot evidence. |
| Static Linux preflight executable | Host operator (install/host evidence); Security reviewer (review/pin approval) | **Unimplemented `STATIC-HELPER-MISSING` blocker.** Future reviewed static artifact at `/opt/ironmem-humanlayer/policy/ironmem-humanlayer-preflight`: source, reproducible build recipe, compiler/dependency versions, SBOM, Linux target, static-link evidence, test vectors, version, SHA-256, signature, and bounded structured-output schema. The manifest records for `/opt`, `/opt/ironmem-humanlayer`, `/opt/ironmem-humanlayer/policy`, and leaf canonical `lstat` non-symlink state, numeric UID/GID 0 (`root:root`), numeric mode, and resolved owner/group names; no component may be pilot/group/other writable. Privileged installation is staged-file fsync, hash/signature verification, atomic rename, then destination-directory fsync; approval follows durable rename. |
| Linux HumanLayer platform executable | Security reviewer (artifact approval); Host operator (host-chain verification) | Before any pilot launch, capture/approve actual Linux package version, integrity, shasum, downloaded-binary SHA-256, canonical executable path, runtime SHA-256, version, signature, and canonical non-symlink leaf/full-parent-chain numeric UID/GID 0 (`root:root`)/non-writable evidence. The trusted supervisor directly executes the verified artifact. The inspected Darwin ARM64 tuple is evidence snapshot only and must not be used as a pilot artifact. |
| Homebrew desktop / Darwin channel | Host operator | Evidence-only and **PROHIBITED** for this initial Linux-only pilot; no macOS package, desktop, or dispatcher is an approved launch channel. |
| Codex/Claude CLI/provider | Model-provider administrator (artifact/account); Host operator (host-chain verification) | Exact version, channel, account/auth class, canonical executable path, SHA-256, signature, and canonical non-symlink leaf/full-parent-chain numeric UID/GID 0 (`root:root`)/non-writable evidence before every launch; supervisor direct execution. If embedded in HumanLayer, explicitly bind provider evidence to the verified HumanLayer artifact and record that no separate path exists. |
| HumanLayer workflows/skills | Vendor owner | Reviewed version/behavior and change record. |
| GitHub App permissions | GitHub administrator | One-repo manifest and before/after capture. |

## Failure and incident handling

| Condition | Fail-closed behavior |
|---|---|
| Secret-shaped name, `.env`, SSH identity, or cloud/deployment credential found | Stop before launch; remove access path; rotate if exposed; preserve name-only evidence. |
| Workspace setup enabled or local override exists | Stop; delete generated sanitized workspace if needed; restore `disabled: true`. |
| `.humanlayer/workspace.local.json` discovered | Stop; do not launch; preserve only path/name and policy-failure evidence without uploading or copying its content; quarantine/remove the override. The CI script may document remediation but is non-qualifying; after the future static helper exists, it must return a fresh pass record. Require Security reviewer approval before resumption. |
| GitHub repository/permission drift | Stop; revoke/disconnect installation; preserve capture; reauthorize only after approval. |
| Canary disproves containment or approval gate | Stop; revoke tokens; preserve evidence; no broader data. |
| Injection seeks secrets, permissions, destructive action, merge, deployment, or control disabling | Deny; stop if attempted; out-of-band escalation and review. |
| Sensitive artifact/session leakage | Stop/revoke/rotate; request cloud/provider deletion; preserve evidence; verify deletion including backups where available. |
| Package/version/integrity mismatch | Stop install/launch; quarantine artifact; review and update manifest only with approval. |
| `STATIC-HELPER-MISSING`, absent/invalid structured preflight pass record, or an attempted script/shell/interpreter launch gate | **Host operator** stops; do not launch; preserve safe status evidence; do not substitute CI/development scripts; implement and approve the separate additive static-helper issue before reconsideration. |
| Unsupported host or any Darwin/macOS execution attempt | **Host operator** stops; do not launch; preserve safe platform/`uname` evidence; record no pilot approval. |
| Linux host baseline/supervisor, static-helper parent chain/leaf path/hash/signature/UID/GID/mode, or symlink state drifts | **Host operator** stops; do not launch; preserve safe image/supervisor/path/hash/signature/numeric-owner/group/mode evidence; restore or review approved Linux host artifacts. **Security reviewer** approves any manifest change and resumption. |
| HumanLayer executable leaf/full-parent-chain path/hash/signature/version/integrity drifts or cannot be directly executed by the trusted supervisor | **Host operator** stops before launch; **Security reviewer** preserves safe executable/version/integrity evidence, restores or reviews the approved HumanLayer artifact, and approves any manifest change and resumption. |
| Provider executable leaf/full-parent-chain path/hash/signature/version or account evidence drifts, or it cannot be directly executed by the trusted supervisor | **Host operator** stops before launch; **Model-provider administrator** preserves safe executable/account evidence and remediates the provider artifact/account; **Security reviewer** approves any manifest change and resumption. |
| IronMem resolves shared store or optional LLM egress | Stop; isolate DB/socket; disable feature; run later canary. |
| Vendor/provider evidence stale, missing, or contradictory | Keep classification PROHIBITED and broader rollout BLOCKED. |

## Pilot checklist and evidence record

No box is pre-checked without captured evidence.

### Preflight

- [ ] Pilot owner: repository classification and disposable-repo URL; evidence: ______
- [ ] Host operator: approved immutable/measured Linux image (or verified-boot equivalent), kernel/OS/architecture/patch baseline, and privileged-supervisor control boundary recorded; pilot account cannot write/control them; evidence: ______
- [ ] Host operator: clean non-admin account/host and name-only environment review; evidence: ______
- [ ] Host operator: configured network allowlist applied and captured; evidence: ______
- [ ] Host operator: clean account, no `.env`, SSH identity, personal GitHub/cloud authentication, inherited credential helper, or production credential; name-only evidence only; evidence: ______
- [ ] GitHub administrator: one-repo permission capture and branch protection/ruleset; evidence: ______
- [ ] Security reviewer: package pins/integrities and no update drift; evidence: ______
- [ ] Model-provider administrator: account-class/data-control record and short-lived credential; evidence: ______
- [ ] Host operator: `IRONMEM_DB_PATH`/`IRONMEM_DAEMON_SOCKET` isolated; rerank/preferences off; evidence: ______
- [ ] Security reviewer: workspace shared disable/no local override/manual sanitized worktree; evidence: ______
- [ ] Security reviewer: containment canaries completed and passed (out-of-workspace read/write, network egress, merge/deploy, and permission-broadening/approval behavior as applicable); evidence: ______
- [ ] Security reviewer: separate approved additive-code issue has delivered the static preflight source/reproducible build/compiler/dependency/SBOM/static-link/test-vector review and artifact pin/signature; until captured this unchecked `STATIC-HELPER-MISSING` item blocks every launch; evidence: ______

### Per-session

- [ ] Pilot owner: public/synthetic task content verified; evidence: ______
- [ ] Host operator: immutable Linux image/supervisor baseline and clean account/credential presence rechecked; evidence: ______
- [ ] Host operator: before every daemon/agent launch, inspect `/opt`, `/opt/ironmem-humanlayer`, `/opt/ironmem-humanlayer/policy`, and `/opt/ironmem-humanlayer/policy/ironmem-humanlayer-preflight` with trusted host-side controls. Record canonical non-symlink state, numeric UID/GID 0 (`root:root`), numeric mode/resolved names, no pilot/group/other write bit, static-helper SHA-256/signature, and the full parent chain against the approved manifest; evidence: ______
- [ ] Host operator: trusted supervisor directly executes the one static helper against the canonical pilot worktree, with no shell, interpreter, `PATH` lookup, Git, or other subprocess in the qualifying boundary; record only its bounded structured pass/fail evidence. A missing helper or record blocks launch; evidence: ______
- [ ] Host operator: before every launch, inspect the HumanLayer and separate provider artifact leaves/full parent chains for canonical non-symlink, numeric UID/GID 0 (`root:root`), no pilot/group/other write bit; record host-chain evidence for the approved manifest; evidence: ______
- [ ] Security reviewer: before every Linux launch, record approved actual Linux HumanLayer package version, integrity, shasum, downloaded-binary SHA-256, canonical executable path, runtime SHA-256, version, and signature; compare to the approved manifest and confirm trusted-supervisor direct execution. Darwin ARM64 evidence is not acceptable; evidence: ______
- [ ] Model-provider administrator: before every launch, record approved provider executable path or embedded-HumanLayer binding, SHA-256, signature, version/channel, direct-execution evidence, and account evidence; compare to the approved manifest; evidence: ______
- [ ] GitHub administrator: repository/permission drift check; evidence: ______
- [ ] Pilot owner: draft PR/diff/artifact review, no merge/deploy; evidence: ______
- [ ] Security reviewer: injection/canary anomalies and logs reviewed; evidence: ______

### Teardown

- [ ] Host operator: stop daemon; revoke/delete local pilot credentials and riptide session material; evidence: ______
- [ ] GitHub administrator: disconnect/revoke app or document retained one-repo installation; evidence: ______
- [ ] Vendor owner: artifact/session deletion request and verification record; evidence: ______
- [ ] Model-provider administrator: revoke provider credentials and record retention/deletion action; evidence: ______
- [ ] Host operator: archive approved evidence, destroy pilot worktree/DB according to retention decision; evidence: ______

## Repository classification decision

| Classification | Decision | Conditions |
|---|---|---|
| Public or synthetic data in one disposable repo on a dedicated approved Linux host/account | BLOCKED (eligible to become PERMITTED) | `STATIC-HELPER-MISSING` blocks every current launch. It may become PERMITTED only after every checklist item, immutable Linux/supervisor/static-helper gate, direct-execution artifact gate, draft-only workflow, and canary passes. |
| macOS/Darwin host or execution | PROHIBITED / BLOCKED | Xcode/CommandLineTools dispatcher, Developer-directory/target, parent-chain, and TOCTOU-resistant execution evidence are unresolved; current Mac checks are non-qualifying development evidence only. |
| Private/internal/confidential/customer/regulated | PROHIBITED | May change only after all critical vendor/provider/sandbox/incident gates have evidence and accountable approval. |
| Production-connected, secret-bearing, signing, deployment, infrastructure administration | PROHIBITED | Requires separate security architecture and approved issue; this threat model cannot approve it. |

Changing a classification requires a new evidence review by the **Security reviewer**, written approval by the **Pilot owner**, permission review by the **GitHub administrator**, host review by the **Host operator**, vendor evidence by the **Vendor owner**, and provider evidence by the **Model-provider administrator**. Enabling a Darwin/macOS pilot additionally requires a separately approved security issue/design plus regression and canary evidence; human confirmation cannot override this prohibition.

## Critical blockers and exit criteria

Wider rollout remains **BLOCKED** until every item has current evidence and accountable approval: product-specific vendor retention/training/subprocessor/deletion/encryption/isolation/audit/incident/DPA-assurance evidence; demonstrated effective sandbox/approval containment; subtractive-copy support or equivalent independently verified containment; `STATIC-HELPER-MISSING` resolved through a separately approved additive-code issue with reviewed source/reproducible static build/compiler/dependencies/SBOM/test vectors, pinned signed artifact, durable atomic installation, full immutable parent-chain evidence, and trusted-supervisor direct execution; immutable/measured Linux image/supervisor baseline; approved actual Linux HumanLayer artifact and provider artifact/account direct-execution evidence; one-repo GitHub enforcement and drift response; IronMem isolation canary and disabled optional egress; and tested incident stop/revoke/rotate/delete-request/preserve-evidence handling. The Python policy script, pytest bridge, explicit-root checks, and shell block are CI/development-only and cannot satisfy this gate. Darwin/Xcode dispatcher resolution, target and Developer-directory parent-chain immutability, and TOCTOU-resistant execution remain unresolved, so wider host support is **BLOCKED**.

## Issue #311 acceptance mapping

| Acceptance criterion | Satisfying section |
|---|---|
| Scope, evidence, facts/unknowns, rollout decision | Introduction; Executive decision; Sources and evidence quality |
| Additive compatibility | Compatibility invariant |
| Trust-boundary diagram and data flow | System and trust boundaries; Data inventory and flow |
| Linux-only eligible host/workspace/GitHub/IronMem controls and static-helper launch blocker | Executive decision; Pilot architecture; GitHub manifest; credential preflight; copying verification; Supply-chain manifest; Critical blockers |
| Sandbox, injection, and all risk prefixes | Sandbox, approvals, and prompt injection; Risk register |
| Vendor/provider and supply-chain evidence | Vendor evidence; Model-provider controls; Supply-chain manifest |
| Fail-closed response, checklist, classification | Failure and incident handling; Pilot checklist; Repository classification decision |
| Rollout blockers, ownership, traceability | Critical blockers; Review cadence and ownership |

## Review cadence and ownership

The **Pilot owner** may authorize only an eventually compliant narrow Linux public/synthetic pilot and owns out-of-band confirmation for explicitly allowed actions; no current launch is authorized. The **Host operator** owns the immutable/measured Linux baseline, privileged supervisor, static-helper installation/full-parent-chain evidence, HumanLayer/provider host-chain evidence, Linux platform qualification, IronMem isolation, and host remediation. Darwin dispatcher/Xcode facts are blocker rationale, not a Host-operator implementation duty for this pilot. The **GitHub administrator** owns app install, permissions, and drift. The **Security reviewer** owns static-helper source/build/pin approval and Linux HumanLayer executable/version/integrity evidence, approves manifest changes and resumptions, and gates classification. The **Vendor owner** obtains HumanLayer controls/contract evidence. The **Model-provider administrator** owns provider artifact/account evidence and provider remediation; Security reviewer approval remains required for related manifest changes or resumption.

Evidence may be no older than one quarter. Review immediately after any permission, product, provider, authentication, package/version, workflow/skill, environment, or incident change; otherwise perform a quarterly review at maximum. A missed review makes broader rollout **BLOCKED** until renewed evidence is approved.
