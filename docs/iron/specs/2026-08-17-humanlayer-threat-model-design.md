# HumanLayer Threat Model Design

**Date:** 2026-08-17

**Scope:** Define the security boundary, pilot controls, and rollout decision for evaluating HumanLayer without changing IronMem's existing collaboration or iron workflows.

**Status:** Approved design

## Problem

Issue [#311](https://github.com/ironrace/ironmem/issues/311) requires a threat model before HumanLayer receives broad repository, host, or credential access. HumanLayer's documented architecture crosses local-host, HumanLayer-cloud, GitHub, IronMem, and model-provider boundaries. Its daemon and agent inherit the authority of their OS user; its task artifacts and session events traverse the HumanLayer API; its GitHub App requests repository write permissions; and its default workspace copy list includes `.env` files that cannot be removed with an override.

The review shell also contains secret-shaped environment variable names. Their values were not read, but their presence proves that the normal developer account is not an acceptable pilot execution environment.

## Goals

- Map every local, HumanLayer, GitHub, IronMem, and model-provider boundary and the data crossing it.
- Record host, GitHub, credential, prompt-injection, supply-chain, IronMem, model-provider, and vendor-data risks with a mitigation and accountable owner.
- Define an enforceable, least-privilege pilot posture and preflight evidence.
- Record what public evidence does and does not establish about retention, training, subprocessors, deletion, encryption, access control, and incident response.
- Make unresolved critical risks block wider rollout.
- Decide which repository classifications may enter the pilot.

## Non-goals

- Do not approve broad production rollout, autonomous merge, deployment, or access to production credentials.
- Do not treat marketing claims or a generic privacy policy as proof of product-specific security controls.
- Do not contact the vendor, accept legal terms, install a GitHub App, or launch a HumanLayer daemon as part of this documentation task.
- Do not overwrite, replace, repurpose, or change `/collab`, `iron-build`, `iron-plan`, `iron-spec`, `iron-tdd`, or their contracts. HumanLayer integration is additive new code, configuration, and documentation; existing workflows remain independently runnable.
- Do not solve the memory-isolation, orchestration, review-routing, intake, concurrency, or comparative-evaluation work owned by issues #305–#310 and #312.

## Architecture

The deliverable is a tracked operational threat model at `docs/HUMANLAYER_THREAT_MODEL.md`. It is evidence-led: verified facts are linked to primary vendor, provider, registry, or repository sources; unknowns are marked as unknown rather than inferred. A data-flow diagram and data-class table define the boundaries, while a risk register ties each threat to controls, owners, evidence, residual risk, and a rollout gate.

The pilot is fail-closed. Public or synthetic repositories are eligible only after all operational gates in the authoritative `docs/HUMANLAYER_THREAT_MODEL.md` pass; that document's stricter all-launch **BLOCKED** decision supersedes any earlier implication that a pilot may proceed. The daemon runs as a dedicated least-privilege OS user with a clean home and an exact approved environment contract. GitHub access is limited to one disposable pilot repository. HumanLayer automatic workspace setup remains disabled because documented `copyGlobs` defaults are additive and include `.env` files.

Rejected alternatives:

- **Normal developer account:** rejected because inherited environment and filesystem/network authority expose unrelated credentials and repositories.
- **Rely on `.humanlayer/workspace.local.json` to remove `.env` patterns:** rejected because HumanLayer documents `copyGlobs` as append-and-deduplicate, never replace or subtract.
- **Install the GitHub App on the canonical organization:** rejected because the app requires contents, issues, and pull-request write access; a selected disposable repository contains the blast radius.
- **Assume the agent provider's sandbox protects the host:** rejected because HumanLayer's effective subprocess sandbox and approval policy is not established by its public documentation.

## Data Flow

1. GitHub issue content and comments enter HumanLayer through its GitHub App and become a task ticket/artifacts.
2. HumanLayer's API sends work to a connected daemon; the daemon sends session events back through the API to web, desktop, and mobile interfaces.
3. The daemon normally starts a separate model-provider agent on the pilot host. That normal spawn is blocked unless a separately approved immutable broker is proven to be the sole HumanLayer provider-exec path; the agent can access the daemon user's worktree, environment, filesystem, and network only subject to controls actually enforced on that host.
4. Task files under `.humanlayer/tasks/<task-slug>/` synchronize with HumanLayer cloud artifact storage when supported agent file tools touch them.
5. Source snippets, prompts, retrieved memory, tool results, and conversation context needed by the agent travel to the selected model provider.
6. IronMem remains local by default: the agent calls the IronMem MCP process/Unix socket, which reads and writes a pilot-specific SQLite store. Any recalled content included in the agent context then crosses the model-provider boundary. Optional IronMem LLM reranking and LLM preference extraction remain disabled so IronMem does not create an additional Anthropic path.
7. HumanLayer's cloud-side GitHub integration reads and writes the selected repository's contents, issues, pull requests, and metadata according to the installed app permissions.

## Future launch contract

No current launch is authorized. A future static helper must validate a supervisor-built, versioned exact minimal `envp`, reject every unknown or secret-shaped key without exemption, and emit bounded value-free reason codes only; credentials may not use environment variables. A future single supervisor-controlled transaction must bind helper validation to the exact immutable `envp`, nonce, Linux mount namespace, read-only workspace policy files, canonical repository/policy descriptors, and same-namespace HumanLayer descriptor execution. A separate provider requires a broker that HumanLayer alone can invoke and that open-fd executes the approved artifact in that same transaction; an embedded provider requires proof of no separate spawn path. `STATIC-HELPER-MISSING`, `LAUNCH-TRANSACTION-MISSING`, `PROVIDER-SPAWN-MEDIATION-MISSING`, and any missing dedicated non-environment credential mechanism are launch blockers until independently implemented, reviewed, pinned, and approved in separate additive-code issues.

## Error Handling

| Condition | Required behavior |
|---|---|
| Secret-shaped environment variable, `.env` file, personal SSH identity, or cloud/deployment credential is present | Abort before daemon launch; do not redact and continue. |
| Unknown/secret-shaped environment key, allowlist-version mismatch, credential supplied through environment, or value-bearing helper record | Abort; preserve only a bounded value-free reason code, never a key name or value. |
| Workspace setup is enabled or a local override exists | Abort; automatic setup remains disabled until HumanLayer supports subtracting default copy globs or an equivalent independently verified control. |
| GitHub App is installed for more than the enumerated pilot repository or its permissions drift | Suspend/disconnect the installation and block the pilot. |
| Sandbox or approval behavior cannot be demonstrated with canary tests | Treat the agent as having full daemon-user authority; retain public/synthetic eligibility only after every authoritative launch gate is satisfied. |
| A prompt requests permission expansion, secret access, merge, deployment, or destructive computer control | Deny and escalate to the human pilot owner outside the agent conversation. |
| A task artifact unexpectedly contains sensitive data | Stop the session and integration; record the incident; request cloud deletion; do not resume until deletion is verified. |
| HumanLayer version or package integrity differs from the approved manifest | Abort and review the new release before updating the manifest. |
| Static helper, nonce-bound launch transaction, non-environment credential mechanism, or provider-spawn broker/no-spawn proof is missing | Abort; public/synthetic remains eligible only, not authorized, until the authoritative threat-model gate is satisfied. |
| Vendor evidence is missing, stale, or contradictory | Keep private/confidential repository classes prohibited and keep broader rollout blocked. |
| IronMem resolves to the shared default database, enables LLM reranking/extraction, or crosses repository scope | Abort and reconfigure a pilot-specific database/socket with remote LLM features off. |

## Testing

| Test | What it proves |
|---|---|
| Markdown acceptance audit | Every #311 acceptance criterion has a named section and explicit decision. |
| Link and source audit | Each verified product/provider claim points to a primary source and unknowns are labeled. |
| Environment negative control | The normal developer shell is rejected when secret-shaped variable names are present; values are never printed. |
| Clean-account preflight | The dedicated pilot account exposes only the exact approved environment allowlist, no `.env` defaults, no personal SSH/cloud auth, and pilot-only credentials supplied through an approved non-environment mechanism. |
| Static helper future-contract vectors | A separately implemented helper accepts only the approved exact set and rejects unknown and secret-shaped keys, empty/multiline/full-value/first-line/filtered input, startup/config injection, and allowlist-version mismatch without emitting values. |
| Launch-transaction future-contract test | A separately implemented supervisor proves nonce/envp/namespace/file/artifact binding and same-namespace descriptor execution with no intervening agent window. |
| Provider-spawn future-contract test | A separate provider is reachable only through the approved broker, or embedded/no-spawn proof prevents a separate execution path. |
| Workspace policy check | Automatic HumanLayer workspace setup is disabled and no local override re-enables it. |
| GitHub permission capture | The installation lists exactly one disposable repository and exactly the documented permission set. |
| Agent canary tests | Out-of-workspace read/write, network egress, merge/deploy, and permission-broadening attempts are denied or require the named human gate. |
| IronMem isolation test | The HumanLayer task uses the pilot database/socket and cannot recall another repository's seeded canary. |
| Existing-workflow regression check | The diff does not modify collab/iron workflow implementation or support files; their existing tests remain unchanged and runnable. |

## Consequences

Public or synthetic code on a disposable repository and dedicated host identity is eligible only after every operational gate in the authoritative threat model passes; no current pilot can proceed. This preserves the intended credential, confidentiality, and GitHub blast-radius limits while leaving future orchestration evaluation conditional on independently implemented controls.

The cost is operational friction: a separate account/host, manual preflight evidence, pinned packages, a disposable repository, and no automatic HumanLayer workspace provisioning. Private/internal code cannot enter the pilot until HumanLayer supplies adequate product-specific retention, training, subprocessor, deletion, encryption, tenant-isolation, employee-access, audit, incident-response, and assurance evidence. These are deliberate rollout blockers, not follow-up suggestions.
