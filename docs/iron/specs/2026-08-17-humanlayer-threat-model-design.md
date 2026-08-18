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

The pilot is fail-closed. Public or synthetic repositories are eligible only after all operational gates in the authoritative `docs/HUMANLAYER_THREAT_MODEL.md` pass; that document's stricter all-launch **BLOCKED** decision supersedes any earlier implication that a pilot may proceed. The daemon, broker, and provider run only as the dedicated non-admin pilot identity after an irreversible verified privilege drop, with a clean home and an exact approved environment and invocation contract. GitHub access is limited to one disposable pilot repository. HumanLayer automatic workspace setup remains disabled because documented `copyGlobs` defaults are additive and include `.env` files.

Rejected alternatives:

- **Normal developer account:** rejected because inherited environment and filesystem/network authority expose unrelated credentials and repositories.
- **Rely on `.humanlayer/workspace.local.json` to remove `.env` patterns:** rejected because HumanLayer documents `copyGlobs` as append-and-deduplicate, never replace or subtract.
- **Install the GitHub App on the canonical organization:** rejected because the app requires contents, issues, and pull-request write access; a selected disposable repository contains the blast radius.
- **Assume the agent provider's sandbox protects the host:** rejected because HumanLayer's effective subprocess sandbox and approval policy is not established by its public documentation.

## Data Flow

1. GitHub issue content and comments enter HumanLayer through its GitHub App and become a task ticket/artifacts.
2. HumanLayer's API sends work to a connected daemon; the daemon sends session events back through the API to web, desktop, and mobile interfaces.
3. The daemon normally starts a separate model-provider agent on the pilot host. That normal spawn is blocked unless a separately approved immutable broker is proven to be the sole HumanLayer provider-exec path; HumanLayer, broker, and provider must share the verified dedicated non-admin pilot identity and restrictions, and the agent can access the daemon user's worktree, environment, filesystem, and network only subject to controls actually enforced on that host.
4. Task files under `.humanlayer/tasks/<task-slug>/` synchronize with HumanLayer cloud artifact storage when supported agent file tools touch them.
5. Source snippets, prompts, retrieved memory, tool results, and conversation context needed by the agent travel to the selected model provider.
6. IronMem remains local by default: the agent calls the IronMem MCP process/Unix socket, which reads and writes a pilot-specific SQLite store. Any recalled content included in the agent context then crosses the model-provider boundary. Optional IronMem LLM reranking and LLM preference extraction remain disabled so IronMem does not create an additional Anthropic path.
7. HumanLayer's cloud-side GitHub integration reads and writes the selected repository's contents, issues, pull requests, and metadata according to the installed app permissions.

## Future launch contract

No current launch is authorized. A future static helper must validate a supervisor-built, versioned exact minimal `envp`, reject every unknown or secret-shaped key without exemption, and emit bounded value-free reason codes only; credentials may not use environment variables. The privileged supervisor first creates and locks the launch namespace, immutable/masked configuration and policy view, separately writable task mount, cgroup/network/LSM setup, and every verified executable/configuration/credential/audit descriptor. **Only then** does it run the helper inside that exact frozen view against the opened descriptors. The planned-policy record binds actual namespace/mount/configuration/file/descriptor identities, but cannot claim later bounding-set/securebits state, UID/GID drop, `no_new_privs`, or actual child state. It then drops every bounding capability while permitted, locks securebits, closes unexpected descriptors, clears groups, sets all real/effective/saved GID then UID to the dedicated pilot identity, clears all capability sets, and verifies the post-drop state. Only post-drop it applies `no_new_privs` and unprivileged-safe restrictions. The post-drop record binds actual child state to the planned-record hash; the supervisor rechecks and descriptor-execs HumanLayer in the same namespace with no agent window.

Implicit configuration, plugin, and tool discovery from HOME/XDG/default, repository/parent, environment, and current directories must be disabled or masked; the supervisor exposes only approved immutable configuration objects and narrow session/auth/task mounts. A separate provider requires both a broker and immutable kernel-enforced execution domains: HumanLayer may execute only a labeled broker entrypoint and automatically transition to broker domain; broker alone may execute the labeled provider and automatically transition to provider domain; provider cannot regain prior privileges or spawn unapproved processes. LSM policy source/version/hash/signature, domains/profiles, labels/inodes, transition matrix, loaded-policy identity, and parent chains are transaction evidence. A live supervisor monitor with TPM/root-trust-held signing key must validate planned/post-drop records, sign a descriptor-bound short-lived authorization, and atomically single-use consume its nonce/counter over narrow authenticated IPC before the broker fd-execs provider. Hash linkage alone is insufficient; no reusable bearer token is allowed. Broker/provider, domain, mapping, invocation, credential, namespace, descriptor, monitor, and authorization identities are pinned and bound to the transaction.

`SUPERVISOR-ARTIFACT-MISSING`, `STATIC-HELPER-MISSING`, `INVOCATION-POLICY-MISSING`, `IMPLICIT-CONFIG-MEDIATION-MISSING`, `LAUNCH-TRANSACTION-MISSING`, `CREDENTIAL-MECHANISM-MISSING`, `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, and `BROKER-AUTHORIZATION-MISSING` are independent launch blockers until implemented as separately approved additive code, reviewed, pinned, and approved. An embedded provider still requires verified component/no-spawn proof; it does not waive any host, transaction, configuration, or credential gate.

The **Host operator** owns supervisor installation, frozen-view ordering, irreversible drop, post-drop/transaction evidence, loaded LSM/domain transitions, live-monitor operation, and operational remediation of `BROKER-SOLE-PATH-MISSING` and `BROKER-AUTHORIZATION-MISSING`. The **Security reviewer** approves supervisor, invocation-policy, configuration-mediation, LSM/monitor trust, transaction schema/manifest, and credential-contract changes and resumption. The **Model-provider administrator** owns broker mapping, provider artifact/account evidence, and non-environment authentication evidence for `CREDENTIAL-MECHANISM-MISSING`. These roles cannot waive a blocker.

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
| `SUPERVISOR-ARTIFACT-MISSING`, `STATIC-HELPER-MISSING`, `INVOCATION-POLICY-MISSING`, `IMPLICIT-CONFIG-MEDIATION-MISSING`, `LAUNCH-TRANSACTION-MISSING`, `CREDENTIAL-MECHANISM-MISSING`, `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, `BROKER-AUTHORIZATION-MISSING`, or embedded/no-spawn proof is missing | Abort; public/synthetic remains eligible only, not authorized, until the authoritative threat-model gate is satisfied. |
| Helper runs before the frozen view/open descriptors exist; planned/post-drop record mismatch; LSM label/domain/transition/policy drift; or monitor authorization signature/key/staleness/replay/IPC/FD/binding/race failure | Abort before exec; preserve only bounded value-free evidence; restore/review the affected transaction, LSM, monitor, or broker contract before Security reviewer-approved resumption. |
| Root HumanLayer/broker/provider execution, retained group/capability, `no_new_privs` off, unexpected FD, or invocation/profile/namespace drift | Abort before exec; preserve only bounded value-free evidence; do not resume without Security reviewer approval. |
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
| `SUPERVISOR-ARTIFACT-MISSING` provenance future-contract test | A separately implemented supervisor/config is pinned from reviewed source/build/SBOM/schema/version/hashes/signatures and immutable parent chains; binary/config drift blocks before execution. |
| Invocation/post-drop future-contract test | Root execution, retained group/capability, `no_new_privs` off, extra/reordered/disallowed argv, cwd/FD/credential-channel drift, and namespace/profile weakening are rejected with bounded value-free codes. |
| Ordered launch-transaction future-contract test | A separately implemented supervisor proves frozen namespace/mount/config/task view and opened descriptors precede helper; planned-policy then post-drop records bind the required actual identities; helper-before-view, order/view/descriptor mismatch, and same-namespace descriptor-exec vectors fail closed. |
| Configuration-discovery future-contract test | HOME/XDG/repository/parent/environment/current-directory/plugin/tool injection, default fallback, symlink/hardlink, and writable configuration mutation are masked or rejected. |
| Kernel-domain and provider-spawn future-contract test | Missing/wrong/mutable labels, no/wrong/replayed transition, direct/alternate exec, and policy reload/drift fail; a separate provider is reachable only through the approved broker, or embedded/no-spawn proof prevents a separate execution path. |
| Broker-authorization future-contract test | Forged/wrong-key/stale/replayed/counter-rollback/unavailable-monitor/IPC/sealed-FD/wrong-binding/consumption-race authorization fails; monitor permits exactly one descriptor-bound broker use. |
| Workspace policy check | Automatic HumanLayer workspace setup is disabled and no local override re-enables it. |
| GitHub permission capture | The installation lists exactly one disposable repository and exactly the documented permission set. |
| Agent canary tests | Out-of-workspace read/write, network egress, merge/deploy, and permission-broadening attempts are denied or require the named human gate. |
| IronMem isolation test | The HumanLayer task uses the pilot database/socket and cannot recall another repository's seeded canary. |
| Existing-workflow regression check | The diff does not modify collab/iron workflow implementation or support files; their existing tests remain unchanged and runnable. |

## Consequences

Public or synthetic code on a disposable repository and dedicated host identity is eligible only after every operational gate in the authoritative threat model passes; no current pilot can proceed. This includes independent resolution of `SUPERVISOR-ARTIFACT-MISSING`, `STATIC-HELPER-MISSING`, `INVOCATION-POLICY-MISSING`, `IMPLICIT-CONFIG-MEDIATION-MISSING`, `LAUNCH-TRANSACTION-MISSING`, `CREDENTIAL-MECHANISM-MISSING`, `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, `BROKER-AUTHORIZATION-MISSING`, and provider mediation or embedded/no-spawn proof. This preserves the intended credential, confidentiality, and GitHub blast-radius limits while leaving future orchestration evaluation conditional on independently implemented controls.

The cost is operational friction: a separate account/host, manual preflight evidence, pinned packages, a disposable repository, and no automatic HumanLayer workspace provisioning. Private/internal code cannot enter the pilot until HumanLayer supplies adequate product-specific retention, training, subprocessor, deletion, encryption, tenant-isolation, employee-access, audit, incident-response, and assurance evidence. These are deliberate rollout blockers, not follow-up suggestions.
