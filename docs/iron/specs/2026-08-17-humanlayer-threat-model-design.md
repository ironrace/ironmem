# HumanLayer Threat Model Design

**Date:** 2026-08-17

**Scope:** Define the security boundary, pilot controls, and rollout decision for evaluating HumanLayer without changing IronMem's existing collaboration or iron workflows.

**Status:** Approved design

## Problem

Issue [#311](https://github.com/ironrace/ironmem/issues/311) requires a threat model before HumanLayer receives broad repository, host, or credential access. HumanLayer's documented architecture crosses local-host, HumanLayer-cloud, GitHub, IronMem, and model-provider boundaries. Its daemon and agent inherit the authority of their OS user; its task artifacts and session events traverse the HumanLayer API; its GitHub App requests repository write permissions; and its default workspace copy list includes `.env` files that cannot be removed with an override.

The review shell also contains secret-shaped environment variable names. Their values were not read, but their presence proves that the normal developer account is not an acceptable pilot execution environment. A same-identity process boundary is also insufficient: ptrace-equivalent inspection, mutation, descriptor duplication, and process control could bypass credential and provider mediation.

## Goals

- Map every local, HumanLayer, GitHub, IronMem, and model-provider boundary and the data crossing it.
- Record host, GitHub, credential, prompt-injection, supply-chain, process-isolation, IronMem, model-provider, and vendor-data risks with a mitigation and accountable owner.
- Define an enforceable, least-privilege Linux pilot posture and bounded value-free preflight evidence.
- Pin the monitor signing key and monotonic counter as distinct, independently attested identities.
- Define provider execution without contradictory broker requirements.
- Make unresolved critical risks block wider rollout.

## Non-goals

- Do not approve broad production rollout, autonomous merge, deployment, or access to production credentials.
- Do not treat marketing claims or a generic privacy policy as proof of product-specific security controls.
- Do not contact the vendor, accept legal terms, install a GitHub App, or launch a HumanLayer daemon as part of this documentation task.
- Do not overwrite, replace, repurpose, or change `/collab`, `iron-build`, `iron-plan`, `iron-spec`, `iron-tdd`, or their contracts. HumanLayer integration is additive new code, configuration, and documentation; existing workflows remain independently runnable.
- Do not solve the memory-isolation, orchestration, review-routing, intake, concurrency, or comparative-evaluation work owned by issues #305–#310 and #312.

## Architecture

The deliverable is the tracked operational threat model at `docs/HUMANLAYER_THREAT_MODEL.md`. It is evidence-led: verified facts point to primary sources and unknowns remain unknown. A data-flow diagram and data-class table define boundaries, while the risk register ties each threat to controls, owners, evidence, residual risk, and a rollout gate.

The authoritative threat model's stricter all-launch **BLOCKED** decision supersedes any earlier implication that a pilot may proceed. Public or synthetic repositories are only eligible to become permitted on a dedicated approved Linux host after every applicable gate passes. The current Python policy script, pytest bridge, shell checks, and any Darwin/macOS result are non-qualifying development evidence. The normal developer account, Darwin/macOS execution, private/confidential repositories, and production-connected or secret-bearing work remain prohibited.

A privileged supervisor builds one frozen launch transaction, executes the reviewed static helper, irreversibly drops to a dedicated non-admin identity, records actual post-drop state, and descriptor-executes HumanLayer without an agent window. Automatic HumanLayer workspace setup remains disabled because `copyGlobs` are additive and include `.env`. GitHub access is restricted to one disposable repository. IronMem uses a pilot-specific database/socket and disables optional LLM egress.

## Data flow

1. GitHub issue content and comments enter HumanLayer through its GitHub App and become task artifacts.
2. HumanLayer's API sends work to a connected daemon; the daemon sends session events back to HumanLayer interfaces.
3. The supervisor constructs the frozen view and transaction, performs planned and post-drop attestation, and executes HumanLayer in its kernel-enforced process domain.
4. Provider execution follows exactly one of the mutually exclusive paths below. A separate provider is reached only through the approved broker and authorization monitor; an embedded provider has no separate process or spawn path.
5. Task files under `.humanlayer/tasks/<task-slug>/` may synchronize with HumanLayer cloud artifact storage.
6. Prompts, source, retrieved memory, tool results, and conversation context travel to the selected model provider.
7. IronMem remains local by default. Recalled content included in context crosses the provider boundary; optional reranking and preference extraction remain disabled.
8. HumanLayer's cloud-side GitHub integration reads and writes only the selected repository under the captured permission manifest.

## Future launch contract

No current launch is authorized. The all-launch baseline blockers are:

- `SUPERVISOR-ARTIFACT-MISSING`
- `STATIC-HELPER-MISSING`
- `INVOCATION-POLICY-MISSING`
- `IMPLICIT-CONFIG-MEDIATION-MISSING`
- `LAUNCH-TRANSACTION-MISSING`
- `CREDENTIAL-MECHANISM-MISSING`
- `PROCESS-ISOLATION-MISSING`

Each is independent. All must be resolved through separately approved additive implementation, reviewed source/build/SBOM/schema/manifest evidence, immutable installation, per-launch measurement, and bounded value-free records. Current Python and shell checks cannot resolve them.

### Ordered supervisor transaction

The privileged supervisor first creates and locks namespace, mount, configuration, policy, cgroup, network, LSM, executable, credential-object, and audit-sink state. Credential-content and readable-audit descriptors stay outside the helper boundary. Only inside that frozen view does a static helper validate the exact versioned minimal `envp`, immutable inputs, and signed non-secret credential metadata, emitting a one-way bounded value-free planned-policy record. The helper has no credential-content/readable-audit FD and cannot reopen descriptors or `/proc`.

The supervisor then checks every capability and identity transition: ambient/inheritable remain empty; `CAP_SETPCAP` removes the entire bounding set; securebits lock; exactly `{CAP_SETGID, CAP_SETUID}` remain temporarily in effective/permitted only; groups and all GIDs then UIDs change; all capability sets clear; no exec occurs with transient capabilities. Post-drop it sets `no_new_privs`, applies unprivileged-safe confinement, introduces the credential FD only to its approved consumer, and writes a one-way post-drop record. The record binds actual identities, capability/securebits state, FD map, invocation, environment digest, profiles, namespaces, credential identity/digest/channel but never content, and planned-record hash. Exact or explicitly allowed transition comparison and immediate same-namespace fd-exec are mandatory.

Implicit HOME/XDG/default/repository/parent/environment/current-directory plugin, tool, and configuration discovery is disabled or masked. Configuration, artifact, invocation, credential, namespace, descriptor, domain, and policy identities remain bound to the transaction.

### Cross-process isolation

`PROCESS-ISOLATION-MISSING` independently blocks every launch until kernel-enforced process isolation is transaction-bound and tested. HumanLayer/daemon/agent, broker, and provider domains must not have ptrace-equivalent authority over one another. Use distinct identities or an explicit reviewed combination of LSM, seccomp, PID-namespace, and `/proc` isolation. Every protected process sets and attests `PR_SET_DUMPABLE=0`.

Cross-domain `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, unauthorized signals/process control, and `/proc/<pid>/{mem,fd,environ}` are denied. HumanLayer inherits no credential FD. For every launch, the signed manifest plus planned-policy and post-drop records pin exact policy source/version/hash/signature, profile IDs, loaded-policy identity, numeric domain identities, signal/control matrix, credential-FD map, and applicable process/domain state to the transaction. The embedded/no-spawn path binds that baseline evidence to its HumanLayer component and no-spawn proof. The separate-provider path additionally binds it to broker, provider, and monitor records. Negative vectors attempt attach, inspect, mutate, signal/control, and descriptor duplication across every applicable domain pair. Failures use only bounded value-free codes such as `PROCESS_ATTACH_FORBIDDEN`, `PROCESS_INSPECT_FORBIDDEN`, `PROCESS_MUTATE_FORBIDDEN`, `PROCESS_CONTROL_FORBIDDEN`, and `PROCESS_DESCRIPTOR_DUP_FORBIDDEN`.

### Provider paths

Provider execution has exactly two mutually exclusive qualifying paths:

- The **embedded/no-spawn path** requires verified HumanLayer artifact version/hash/signature, component manifest and SBOM, exact invocation binding, inherited post-drop/process-isolation evidence, and proof that no separate provider executable or spawn path exists. No separate broker or authorization monitor is required.
- The **separate-provider path** requires every all-launch baseline gate plus resolution of `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, and `BROKER-AUTHORIZATION-MISSING`. An immutable broker is HumanLayer's sole permitted provider-exec entrypoint; automatic kernel domain transitions enforce HumanLayer→broker→provider; the broker validates and fd-execs the pinned provider using a sealed single-use monitor authorization.

The paths cannot be combined or partially substituted. Evidence for embedded/no-spawn does not waive a baseline blocker. Broker, sole-path, and monitor evidence is neither required nor meaningful for the embedded/no-spawn path. A separate provider cannot use embedded evidence to waive any member of the broker trio.

### Signing key, counter, and durable authorization

This subsection applies only to the separate-provider path. The live monitor is the exact `authorization-monitor` mode of the pinned supervisor. Its signed configuration pins protocol, argv, verification key, socket, ledger, UID/GID, profiles, signing-key ID, and signing-key handle `0x81010001`. That persistent signing-key handle is not the rollback counter.

The signed configuration contains a separate mandatory field named `counterNvIndex`. It is an exact reviewed configured 32-bit NV-index value. Omitted, automatic, first-free, or signing-key-derived selection is prohibited; this design deliberately does not invent an arbitrary fixed NV handle.

TCG TPM 2.0 defines an NV Index TPM Name as a digest over its public area and a `TPM_NT_COUNTER` index as an eight-octet increment-only counter. Provisioning and every privileged startup therefore attest:

- the configured counter handle and TPM Name;
- complete `TPMS_NV_PUBLIC`: `nvIndex`, `nameAlg`, exact attributes including `TPM_NT_COUNTER`, `authPolicy`, and eight-byte `dataSize`;
- TPM/EK/AK identity and hierarchy/provisioning identity;
- creation evidence and authorization/reset/undefine ownership; and
- binding to the signing-key Name, key ID, attested epoch, ledger identity, and launch transaction.

The Host operator owns provisioning and lifecycle authority under Security reviewer approval. The post-drop runtime monitor may read and increment the counter under approved policy but cannot define, undefine, clear, reset, or reprovision the NV index or TPM. Counter substitution, recreation, public-area/attribute/policy/Name drift, or unauthorized reset/undefine fails closed with bounded value-free `MONITOR_COUNTER_IDENTITY_MISMATCH`.

The monitor uses only preopened verified FDs after its checked irreversible capability/identity drop. It advances the pinned hardware counter, appends and fsyncs the integrity-protected ledger and directory, then acknowledges one descriptor-bound use. Crash before acknowledgement may consume and requires reissue; restart/reboot invalidates outstanding tokens. Recovery requires a fresh privileged supervisor, new attested epoch, reattested counter identity, and counter/ledger reconciliation. Ambiguity, rollback, TPM replacement, identity drift, epoch/PCR mismatch, or clock anomaly blocks Security-reviewed recovery.

## Risk and ownership

The Host operator owns the Linux baseline, supervisor/helper installation and measurement, ordered launch transaction, process-domain policies and negative tests, and all-launch blocker remediation. For the separate-provider path it additionally owns remediation of `BROKER-SOLE-PATH-MISSING` and `BROKER-AUTHORIZATION-MISSING`, broker-domain operation, privileged monitor startup, TPM NV provisioning/lifecycle evidence, ledger operation, and recovery. The runtime monitor owns no provisioning or reset authority.

The Security reviewer approves supervisor, helper, invocation, configuration, credential, transaction, process-isolation identity/profile, and classification changes. On a separate-provider path it also approves broker, LSM, monitor, signing-key, counter public identity, TPM hierarchy/provisioning, ledger, and recovery evidence. The Model-provider administrator owns account/auth evidence and exactly one provider evidence set: HumanLayer component/SBOM/invocation/no-spawn evidence, or broker mapping/provider artifact evidence. Other established Pilot owner, GitHub administrator, and Vendor owner responsibilities remain unchanged. No role may waive a blocker.

The operational risk register must classify same-identity cross-process inspection as Critical and `PROCESS-ISOLATION-MISSING` as an all-launch gate. It must classify counter-identity substitution/recreation/reset as Critical and `MONITOR_COUNTER_IDENTITY_MISMATCH` as a separate-provider failure. Provider risks and ownership must identify which path they apply to.

## Error handling

| Condition | Required behavior |
|---|---|
| Any all-launch baseline blocker is unresolved | Abort before HumanLayer exec; preserve bounded value-free status only. |
| Process profile/identity evidence drifts; dumpability is enabled; attach/read/write/FD-dup/proc/signal/control negative succeeds; or HumanLayer inherits a credential FD | Abort every launch with bounded process-domain status; never preserve inspected process/environment/credential content; restore policy and rerun all-pairs negatives before Security-reviewed resumption. |
| Embedded/no-spawn evidence is missing or a separate spawn appears | Abort the embedded path; do not fall through to a partially configured separate path. |
| On the separate-provider path, `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, or `BROKER-AUTHORIZATION-MISSING` is unresolved | Abort that path; preserve bounded broker/domain status; do not require or claim these gates for a verified embedded/no-spawn path. |
| On the separate-provider path, `counterNvIndex` is omitted/automatic, or counter handle/TPM Name/`TPMS_NV_PUBLIC`/TPM/provisioning/creation/lifecycle/binding evidence drifts | Abort with `MONITOR_COUNTER_IDENTITY_MISMATCH`; invalidate outstanding authorizations; prohibit monitor use and require Security-reviewed privileged recovery. |
| On the separate-provider path, monitor privilege, FD, IPC, ledger, counter, epoch, replay, crash, or recovery invariant fails | Abort before provider exec; acknowledge no authorization; invalidate outstanding tokens and begin only fresh privileged recovery. |
| Any helper, transaction, credential, invocation, configuration, artifact, host, GitHub, vendor, provider, or IronMem gate fails | Apply the authoritative threat model's fail-closed response; never log credential/audit content. |

## Testing

| Test | What it proves |
|---|---|
| Markdown acceptance audit | Both documents contain the all-launch isolation, distinct counter identity, and exact two-path contracts. |
| Baseline blocker matrix | Every launch resolves the seven baseline blockers; current checks remain non-qualifying and authorize nothing. |
| Cross-process isolation vectors | Every domain pair rejects attach, `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, proc inspection, mutation, signal/control, and descriptor duplication; `PR_SET_DUMPABLE=0`, identities/profiles, and HumanLayer's no-credential-FD map are transaction evidence. |
| Provider-path classification vectors | Exactly one path is selected; embedded/no-spawn succeeds only with artifact/component/SBOM/invocation/no-spawn proof and no broker/monitor dependency; a separate provider requires `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, and `BROKER-AUTHORIZATION-MISSING` resolved. |
| TPM counter identity vectors | Signing-key handle `0x81010001` and distinct signed `counterNvIndex` are required and exact; omitted/auto/first-free selection, signing-key reuse, substitution, recreation, TPM Name or `TPMS_NV_PUBLIC` drift, TPM/provisioning drift, unauthorized reset/undefine, and wrong signing-key/epoch/ledger/transaction binding fail as `MONITOR_COUNTER_IDENTITY_MISMATCH`. |
| Monitor authorization and recovery vectors | The monitor can read/increment only; durable counter/ledger ordering, one use, crash/restart invalidation, and fresh privileged recovery are enforced. |
| Existing environment/workspace, supply-chain, transaction, invocation, configuration, credential, sandbox, GitHub, vendor, provider, and IronMem vectors | All prior authoritative controls continue to fail closed with bounded value-free evidence. |

## Documentation structure

The operational document must synchronize the three contracts through executive decision, boundaries, architecture/startup/operation, risk register, supply-chain/config manifest, failure handling, test vectors, checklist, classification, critical exit criteria, acceptance mapping, consequences, and ownership. Blocker lists state the seven all-launch baseline blockers once and apply the broker trio only to the separate-provider path. They never list all blockers unconditionally and then append “or embedded/no-spawn proof.”

## Consequences

Public or synthetic code on a disposable repository and dedicated approved Linux host is eligible only after every all-launch baseline gate and exactly one provider path passes; no current pilot can proceed. `PROCESS-ISOLATION-MISSING` therefore blocks embedded and separate providers alike. Embedded/no-spawn proof can remove the need for a broker and monitor, but cannot weaken host, transaction, configuration, credential, or process-isolation controls. A separate provider remains prohibited until `PROVIDER-SPAWN-MEDIATION-MISSING`, `BROKER-SOLE-PATH-MISSING`, and `BROKER-AUTHORIZATION-MISSING`, the pinned signing key, independently pinned counter identity, durable authorization, and recovery contract pass. This preserves the additive compatibility invariant, Linux-only classification, credential/confidentiality/GitHub limits, bounded value-free evidence, existing owner roles, and the fact that current Python/shell checks are non-qualifying.
