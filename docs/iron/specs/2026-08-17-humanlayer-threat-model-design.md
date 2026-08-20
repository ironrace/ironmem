# HumanLayer Threat Model Design

**Date:** 2026-08-17 (revised 2026-08-19)

**Scope:** Define the security boundary, pilot controls, and rollout decision for evaluating HumanLayer without changing IronMem's existing collaboration or iron workflows.

**Status:** Approved design

## Problem

Issue [#311](https://github.com/ironrace/ironmem/issues/311) requires a threat model before HumanLayer receives broad repository, host, or credential access. HumanLayer's documented architecture crosses local-host, HumanLayer-cloud, GitHub, IronMem, and model-provider boundaries. Its daemon and agent inherit the authority of their OS user; its task artifacts and session events traverse the HumanLayer API; its GitHub App requests repository write permissions; and its default workspace copy list includes `.env` files that cannot be removed with an override — only disabled wholesale.

## Goals

- Map every local, HumanLayer, GitHub, IronMem, and model-provider boundary and the data crossing it.
- Record host, GitHub, credential, prompt-injection, supply-chain, IronMem, model-provider, and vendor-data risks with a mitigation and accountable owner.
- Define a proportionate pilot posture that is achievable on the existing workstation.
- Make unresolved critical vendor risks block wider rollout without blocking the pilot itself.

## Non-goals

- Do not approve broad production rollout, autonomous merge, deployment, or access to production credentials.
- Do not treat marketing claims or a generic privacy policy as proof of product-specific security controls.
- Do not overwrite, replace, repurpose, or change `/collab`, `iron-build`, `iron-plan`, `iron-spec`, `iron-tdd`, or their contracts. HumanLayer integration is additive new code, configuration, and documentation; existing workflows remain independently runnable.
- Do not solve the memory-isolation, orchestration, review-routing, intake, concurrency, or comparative-evaluation work owned by issues #305–#310 and #312.
- Do not specify host-hardening infrastructure disproportionate to the pilot's exposure. See the threat model's appendix.

## Proportionality principle

Controls are justified against the authority this workstation already grants to Claude Code and Codex, not against an absolute standard. Any control that would equally prohibit the agentic tools already in daily use is not a HumanLayer finding. The design addresses only HumanLayer's marginal additions: cloud artifact sync, a GitHub App with write scope, additive workspace copy globs, a post-copy `sh -c` setup command, and one more vendor in the data path.

## Architecture

The deliverable is the tracked operational threat model at `docs/HUMANLAYER_THREAT_MODEL.md`. It is evidence-led: verified facts point to primary sources and unknowns remain unknown. A data-flow diagram defines boundaries, and the risk register ties each threat to a control and a status.

The enforced technical control is a fail-closed repository workspace policy: `.humanlayer/workspace.json` is exactly `{"disabled": true}`, the machine-local override is absent and gitignored, and a focused regression test prevents drift. Disabling automatic workspace setup is what prevents both `.env` copying and `setupCommand` execution. Remaining controls are operational: one disposable repository, scoped GitHub App, no production secrets in the pilot shell, pilot-specific IronMem paths, and a human merge gate.

## Data flow

1. GitHub issue content and comments enter HumanLayer through its GitHub App and become task artifacts.
2. HumanLayer's API sends work to a connected daemon; the daemon sends session events back.
3. Task files under `.humanlayer/tasks/<task-slug>/` may synchronize with HumanLayer cloud artifact storage.
4. Prompts, source, retrieved memory, tool results, and conversation context travel to the selected model provider.
5. IronMem remains local. Recalled content included in context crosses the provider boundary; optional reranking and preference extraction remain disabled.
6. HumanLayer's cloud-side GitHub integration reads and writes only the selected repository under the captured permission manifest.

## Risk and ownership

The Pilot owner owns repository classification, the decision to launch, and the human merge gate. Operational controls — repository selection, GitHub App scope and drift review, pilot credential hygiene, IronMem path isolation, and version pinning — are the Pilot owner's responsibility in this single-operator setup; the threat model records them as checklist items rather than delegating to distinct roles.

Vendor data handling (retention, training, subprocessors, deletion, encryption, incident response) is unresolved and is the sole condition blocking rollout beyond one disposable repository.

## Error handling

| Condition | Required behavior |
|---|---|
| Workspace policy test fails, or `.humanlayer/workspace.local.json` exists | Do not launch. Restore the fail-closed configuration and rerun the test. |
| A `.env` or credential file is found in a pilot worktree | Abort the session, destroy the worktree, rotate anything exposed. |
| GitHub App repository or permission drift is detected | Suspend the app, revoke changed grants, preserve evidence, re-approve before continuing. |
| A pilot session attempts to merge, deploy, change permissions, or disable a control | Stop the session; treat as a prompt-injection indicator and preserve the transcript. |
| `@humanlayer/cli` version or binary hash differs from the pinned tuple | Do not launch until the new version is reviewed and the pin updated. |
| Vendor evidence is missing or contradictory | Keep private and production-connected repositories PROHIBITED; the disposable-repo pilot may continue. |

## Testing

| Test | What it proves |
|---|---|
| `test_shared_workspace_setup_is_disabled` | `.humanlayer/workspace.json` is exactly `{"disabled": true}`, so additive `copyGlobs` and `setupCommand` never run. |
| `test_local_override_is_absent_and_ignored` | The machine-local override does not exist and is ignored by the tracked `.gitignore`, so the fail-closed policy cannot be silently re-enabled. |
| `--repo-root` validation tests | The policy check validates an absolute, existing Git worktree root and rejects relative, missing, subdirectory, and unsafe roots. |

Documentation is reviewed by humans. No test asserts on the prose of a Markdown file.

## Documentation structure

The operational document states the executive decision, the proportionality anchor, evidence with primary sources, a data-flow diagram, mandatory controls, the GitHub permission manifest, a risk register, open vendor questions, a pilot checklist, the repository classification, the compatibility invariant, and an appendix recording deliberately out-of-scope hardening.

## Consequences

A narrow pilot on one disposable public or synthetic repository is permitted on the existing workstation once the five mandatory controls and the preflight checklist pass, producing draft pull requests reviewed by a human. Private, customer-data, production-connected, and secret-bearing repositories — including `tenfourpro` — remain prohibited on the concrete grounds that their checkouts hold live credentials. Broader rollout stays blocked on vendor data-handling evidence. Host-hardening infrastructure (dedicated Linux host, privileged supervisor, static preflight helper, kernel-enforced process domains, TPM-backed authorization broker) is recorded as future hardening, not as a launch gate, because it addresses a privilege-escalation adversary rather than this pilot's actual exposure and does not mitigate prompt injection. The additive compatibility invariant is preserved.
