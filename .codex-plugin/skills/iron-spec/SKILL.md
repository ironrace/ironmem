---
name: iron-spec
description: Use before any creative work - creating features, building components, adding functionality, or modifying behavior. Explores intent, requirements, and design, and writes an approved design doc before implementation.
---
<!-- GENERATED from skills/ — do not edit -->

# Iron Spec

Turn an idea into an approved design document before any code is written.

## When to Use

Triggered by "let's build X", "add feature Y", "change how Z behaves" — anything that creates or changes behavior rather than restoring behavior that used to work.

Not for a bug fix with a known cause. A reproduction plus a diagnosed root cause is already a specification; go straight to `iron-tdd`. Come back here if the fix turns out to need a design decision — a new interface, a changed contract, a second way to do something the codebase already does one way.

Skip it for typo fixes, dependency bumps, and mechanical renames. A design document for a one-line change is ceremony.

## The Loop

You are interviewing, not drafting. Stay in the loop until the only open questions left are implementation details.

1. **Restate the problem in your own words and get agreement** before proposing any solution. If the restatement is wrong, everything downstream is wrong, and this is the cheapest place in the project to find that out.

2. **Ask one question at a time. Never batch.** A batch of five questions gets two answered, and the three that got skipped are the hard ones. Asking singly also lets the next question depend on the last answer, which is the whole value of interviewing rather than sending a form.

3. **Prefer concrete alternatives to open-ended prompts.** "A: nullable column on the existing table — simpler, but widens the hot row. B: side table — one more join, but the hot path stays narrow. Which?" beats "how should we store this?". People choose accurately long before they can specify.

4. **Track unresolved decisions in update_plan**, one item each, so the state of the interview lives somewhere durable instead of in a conversation you are going to lose to compaction.

5. **Surface conflicting requirements instead of averaging them.** Two goals that cannot both hold is the most valuable thing an interview can find. Name the conflict and make the human choose.

Write the document only after the loop converges. Drafting early turns the interview into a review of your draft, and people critique drafts far less freely than they answer questions.

## What to Nail Down

Each row is a heading in the output document. A section you cannot fill is a question you have not asked yet.

| Section | Must answer |
|---|---|
| **Problem** | What is broken or missing today, with evidence — file paths, line counts, an actual error. Not "we need X", but why the absence of X hurts. |
| **Goals** | What "done" looks like, stated so you could tell whether it holds. |
| **Non-goals** | What this deliberately does not do, and why. A spec with no non-goals has been described, not scoped. |
| **Architecture** | The shape of the solution: components, what owns what, the decisions taken and the alternatives rejected. |
| **Data flow** | How a request or a piece of data moves end to end, where it is persisted, and what those files or records are named. |
| **Error handling** | A table of condition → behavior, one row per failure mode you can name. This is where silent-failure bugs get designed out. |
| **Testing** | A table of test → what it covers. What proves each goal actually holds. |
| **Consequences** | What gets better, what gets worse, what the project is now committed to. Include the costs; a consequences section with no costs is a pitch. |

Add a **Migration** section whenever existing data, installs, or on-disk state has to survive the change.

## Output

Write to `docs/iron/specs/YYYY-MM-DD-<topic>-design.md`, opening with the date, a one-line scope statement, and a status line:

- `**Status:** Draft` — while the interview is open, and while the human is reading.
- `**Status:** Approved design, pending implementation plan` — only after the human says so in words.

Nothing you conclude on your own promotes a spec. "This looks complete to me" is not approval.

## Handoff

Once the status reads approved, the next step is `iron-plan`, which turns the spec into tier-tagged, bite-sized tasks.

Do not start implementing from a spec. A spec settles *what* and *why*; it does not settle file paths, task ordering, or test-by-test steps, and an implementer working straight from one invents all three silently.

## Red Flags

Each of these means stop and back up:

- Proposing a solution before the problem statement has been agreed.
- Asking several questions in one message.
- Writing code — even "just a sketch" — during specification.
- Marking a spec approved because it looks finished. Only the human approves.
- A spec with no non-goals section, or one whose non-goals say "none".
- A design with no rejected alternative. If nothing was considered and dropped, no design decision was actually made.
- Answering your own open question with an assumption and moving on. Ask it.
