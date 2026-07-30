---
name: iron-tdd
description: Use when implementing any feature or bugfix, before writing implementation code. Red-green-refactor discipline with a catalogue of testing anti-patterns.
---

# Iron TDD

Write the test first. Watch it fail. Write the minimal code that passes it.

**Core principle:** if you never watched the test fail, you don't know it
tests the right thing — it's an assertion you hope is wired up, not proof.

## When to Use

Always: new features, bug fixes, refactors, behavior changes. A bug fixed
without a regression test is a bug that comes back.

The only candidate exceptions are throwaway prototypes, generated code, and
configuration files — and even those need your human partner's permission, so
ask before skipping rather than granting yourself the exemption. "Skip it just
this once" on anything else is the rationalization, not the exception — treat
the thought itself as a red flag.

## The Iron Law

```
NO PRODUCTION CODE WITHOUT A FAILING TEST FIRST
```

Wrote code before the test? Delete it. Don't keep it as "reference," don't
adapt it while writing the test, don't look at it. Implement fresh from the
test. Discarding an hour of work is cheaper than trusting code you never
watched fail.

## The Cycle

1. Write one failing test that states the behavior you want.
2. **Run it and read the failure.** A test you have not watched fail is not a
   test — it is an assertion you hope is wired up. Record the exact failure
   message.
3. Write the minimal implementation that makes it pass. Not the general
   solution; the minimal one.
4. Run the test again and confirm it passes.
5. Refactor with the test green. Re-run after every refactor.
6. Commit.

Never write implementation code before step 2 has produced an observed failure.
If you cannot make the test fail, the test is wrong.

## What Counts as a Real Test

A test is a claim about behavior, checked against the real system. A test
that asserts on a mock — that a mocked function was called, that a mocked
element rendered — is not a test of your code; it is a test that the mock
exists, and it will pass whether or not the feature works.

Mock the slow or external edge of a dependency chain, never the behavior
under test. If you can't say whether a passing test is exercising real code
or a mock's canned return value, stop and find out before trusting the
result.

A bug fix follows the same cycle: write a failing test that reproduces the
bug, then apply the cycle above. Never fix a bug without a test proving it
existed and now doesn't.

## Red Flags — Stop and Start Over

- Writing implementation before the test, "just this once."
- A test that passes the first time you run it.
- Not being able to state the exact failure message you observed.
- Asserting on a mock instead of on behavior.
- "Tests after achieve the same goal" — they don't. Tests-after verify what
  you built; tests-first discover what was actually required.

## Reference

Read `./references/testing-anti-patterns.md` before adding mocks or test
utilities. It catalogues five recurring failure modes — testing mock
behavior, test-only methods on production classes, under-informed mocking,
incomplete mocks, and tests as an afterthought — with the fix for each.
