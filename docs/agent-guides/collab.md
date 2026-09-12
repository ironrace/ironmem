# Collab and Superpowers routing

For `/collab` and bundled Superpowers workflows, use explicit phase-based routing rather than a personal default:

- Implementation controller and workers: `gpt-5.6-luna` at `max`.
- Exploration, documentation, and mechanical work: `gpt-5.6-luna` at `medium`.
- Planning and normal review: `gpt-5.6-terra` at `high`.
- Architecture or security escalation: `gpt-5.6-sol` at `high`.

Sol is an escalation tier, not the routine default. Dispatches should set both the model and reasoning effort explicitly.
