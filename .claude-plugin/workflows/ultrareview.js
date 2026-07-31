export const meta = {
  name: 'ultrareview',
  description: 'Pipelined multi-lens code review: find, adversarially verify, then auto-fix confirmed findings',
  phases: [
    { title: 'Find', detail: 'one agent per review lens, schema-forced findings' },
    { title: 'Verify', detail: 'adversarial refute pass on every surviving CRITICAL/HIGH' },
    { title: 'Fix', detail: 'one agent per file, CONFIRMED non-invasive findings only' },
    { title: 'Audit', detail: 'scope-creep audit on the fix diff' },
  ],
}

// ---------------------------------------------------------------- args

const A = args || {}
const FABLE = A.fable === true
const REPORT_ONLY = A.reportOnly === true
const FILES = Array.isArray(A.files) ? A.files : []
const CHANGED_LINES = typeof A.changedLines === 'number' ? A.changedLines : 0
const REQUESTED = Array.isArray(A.lenses) && A.lenses.length ? A.lenses : ['A', 'B', 'C', 'D']
const VERIFY_CAP = 8
const SEV = ['LOW', 'MEDIUM', 'HIGH', 'CRITICAL']
const CORE = ['A', 'B', 'C', 'D']

// ------------------------------------------------------------- roster

// Falls back to a general-purpose agent when the pr-review-toolkit plugin is
// absent, so the review degrades instead of erroring.
function toolkit(name) {
  return A.toolkitAvailable ? `pr-review-toolkit:${name}` : 'general-purpose'
}

// `model`/`effort` are the standard tier from the issue's Phase 3 table.
// `fable: true` marks a lens eligible for the --fable swap. B is false on
// purpose and permanently: Fable's bug-finding gains exclude security analysis,
// and its classifiers decline security-shaped briefs.
const ROSTER = {
  A: { key: 'code-reviewer (correctness)', agentType: 'code-reviewer', model: 'opus', effort: 'xhigh', fable: true, blastRadius: true },
  B: { key: 'security-reviewer', agentType: 'security-reviewer', model: 'opus', effort: 'xhigh', fable: false },
  C: { key: 'architect', agentType: 'architect', model: 'opus', effort: 'xhigh', fable: true, blastRadius: true },
  D: { key: 'doc-reviewer', agentType: 'doc-reviewer', model: 'sonnet', effort: 'medium', fable: false },
  E: { key: 'marketing-claims auditor', agentType: A.marketingAgentType || 'general-purpose', model: 'sonnet', effort: 'medium', fable: false },
  F: { key: 'comment-analyzer', agentType: toolkit('comment-analyzer'), model: 'sonnet', effort: 'low', fable: false },
  G: { key: 'pr-test-analyzer', agentType: toolkit('pr-test-analyzer'), model: 'sonnet', effort: 'high', fable: false },
  H: { key: 'silent-failure-hunter', agentType: toolkit('silent-failure-hunter'), model: 'opus', effort: 'high', fable: false },
  I: { key: 'type-design-analyzer', agentType: toolkit('type-design-analyzer'), model: 'sonnet', effort: 'high', fable: false },
  J: { key: 'concurrency-reviewer', agentType: 'general-purpose', model: 'opus', effort: 'xhigh', fable: true },
  K: { key: 'performance-reviewer', agentType: A.perfAgentAvailable ? 'performance-optimizer' : 'general-purpose', model: 'opus', effort: 'high', fable: false },
}

function tierFor(id) {
  const lens = ROSTER[id]
  // Fable at high, not xhigh: lower Fable effort often exceeds prior models at
  // xhigh, and xhigh across a fan-out means minutes-long turns per lens.
  if (FABLE && lens.fable) return { model: 'fable', effort: 'high' }
  return { model: lens.model, effort: lens.effort }
}

// --------------------------------------------------------------- band

function bandFor() {
  if (CHANGED_LINES > 800 || FILES.length > 20) return 'large'
  if (CHANGED_LINES >= 200) return 'medium'
  return 'small'
}

const BAND = bandFor()
const SELECTED = (BAND === 'small' ? REQUESTED.filter((id) => CORE.includes(id)) : REQUESTED.slice())
  .filter((id) => ROSTER[id])
const DROPPED_BY_BAND = REQUESTED.filter((id) => !SELECTED.includes(id))
const FABLE_SUGGESTED = BAND === 'large' && !FABLE

if (DROPPED_BY_BAND.length) {
  log(`band=${BAND} (${CHANGED_LINES} changed lines, ${FILES.length} files) — dropped conditional lenses: ${DROPPED_BY_BAND.join(', ')}`)
}
if (FABLE_SUGGESTED) log('this diff qualifies for --fable')

// ------------------------------------------------------------ schemas

const FINDINGS_SCHEMA = {
  type: 'object',
  required: ['findings'],
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        required: ['file', 'line', 'severity', 'confidence', 'issue', 'failure_scenario', 'suggested_fix'],
        properties: {
          file: { type: 'string', description: 'repo-relative path' },
          line: { type: 'integer', description: '1-based line in the changed file; 0 for a file-level finding' },
          severity: { type: 'string', enum: ['CRITICAL', 'HIGH', 'MEDIUM', 'LOW'] },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
          issue: { type: 'string' },
          failure_scenario: { type: 'string', description: 'concrete inputs or state reaching the code and the wrong behaviour that results; mandatory for CRITICAL/HIGH' },
          suggested_fix: { type: 'string' },
        },
      },
    },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['verdict', 'evidence', 'fix_complexity', 'fix_class'],
  properties: {
    verdict: { type: 'string', enum: ['CONFIRMED', 'REFUTED', 'PLAUSIBLE'] },
    evidence: { type: 'string', description: 'the quoted code path, guard, or reason it could not be proved either way' },
    fix_complexity: { type: 'string', enum: ['mechanical', 'local', 'invasive'] },
    fix_class: { type: 'string', enum: ['security', 'concurrency', 'correctness', 'error-handling', 'docs', 'comments', 'magic-numbers', 'other'] },
  },
}

const FIX_SCHEMA = {
  type: 'object',
  required: ['results'],
  properties: {
    results: {
      type: 'array',
      items: {
        type: 'object',
        required: ['file', 'line', 'outcome', 'note'],
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          outcome: { type: 'string', enum: ['fixed', 'skipped', 'no_change_needed'] },
          note: { type: 'string' },
        },
      },
    },
  },
}

const AUDIT_SCHEMA = {
  type: 'object',
  required: ['in_scope', 'out_of_scope_changes', 'summary'],
  properties: {
    in_scope: { type: 'boolean' },
    out_of_scope_changes: { type: 'array', items: { type: 'string' } },
    summary: { type: 'string' },
  },
}

// ------------------------------------------------------------- briefs

// Coverage-first. The confidence floor is deliberately absent: Opus follows a
// stated confidence bar literally and drops real findings below it, and the
// harness already filters twice downstream (Phase 5 demote/dedup, Phase 5.5
// adversarial verify). Finder-stage self-filtering is redundant and lossy.
const OUTPUT_CONTRACT = [
  'Report every issue you find, including ones you are uncertain about or consider low-severity.',
  'Do not filter for importance or confidence — a separate verification stage does that.',
  'For each finding include a confidence level and severity so the downstream filter can rank them.',
  'The failure scenario is mandatory for CRITICAL/HIGH: state the concrete inputs or state that reach the code and the wrong behaviour that results ("X called with empty list -> index panic at line N"). A finding you cannot express as a failure scenario is at most MEDIUM.',
  'Findings only. No praise, no summary, no plan comparison, no word budget.',
].join(' ')

const BLAST_RADIUS = 'For each changed public/exported symbol (function signature, return semantics, enum variants, API shape), locate its callers — `grep` is fine; if the `mcp__ironmem__symbol_neighbors` / `symbol_lookup` tools are available and the repo is indexed, use them — and verify each caller still behaves correctly under the new semantics. Bugs at the boundary between changed and unchanged code count double.'

// `fable` variants are deliberately de-prescribed: enumerated hunt-lists
// measurably reduce Fable's output quality, so those briefs collapse to goal +
// constraints. The output contract stays — it is a contract, not scaffolding.
const BRIEFS = {
  A: {
    standard: 'You are in diff-review mode, not plan-alignment mode: skip plan comparison, skip praise. Hunt for bugs: trace data flow through every changed function; simulate execution on edge inputs (empty, None/null, zero, negative, max, unicode, concurrent); off-by-one, inverted conditions, wrong operator, missed early return; error paths that corrupt state; resource leaks. Also: type safety, dead code, magic numbers. Do not review security, architecture, or docs — other lenses own those.',
    fable: 'Find the correctness bugs in this diff. Security, architecture, and documentation belong to other reviewers — skip them. How you look is up to you.',
  },
  B: {
    standard: 'OWASP Top 10, injection, auth/authz, secret exposure, SSRF, path traversal, unsafe crypto, input validation at boundaries, rate limiting, error-message leakage, deserialization. Use ecosystem-appropriate scanners when present (`cargo audit`, `pip-audit`, `bandit`, `npm audit`, `gitleaks`). Read-only: findings only, never edit files. Do not review general code quality — Agent A owns it.',
  },
  C: {
    standard: 'You are reviewing a diff, not designing a system: no ADRs, no scalability roadmaps. Focus on defects with architectural cause: state-machine correctness (unreachable/missing transitions), migration safety (data loss, non-reversible steps), API contract stability (breaking change without versioning), coupling that will force shotgun surgery, abstraction placed in the wrong layer, invariants held in one module silently assumed by another.',
    fable: 'Find the defects in this diff whose cause is architectural — wrong layer, broken invariant, unsafe migration, contract change without versioning. You are reviewing a diff, not designing a system: no ADRs, no roadmaps. How you look is up to you.',
  },
  D: {
    standard: 'Documentation completeness for this diff: missing public-API docstrings, breaking changes without CHANGELOG/migration notes, new env vars or config flags absent from `.env.example` or README, stale comments referring to removed/renamed code, README examples that drift from new behaviour, codemap entries missing for new modules. Findings only — never edit files.',
  },
  E: {
    standard: 'Cross-check every user-visible claim in this diff against its ground-truth source in code/config. Each finding: claim -> ground-truth source -> verdict -> suggested fix. Read-only.',
  },
  F: {
    standard: 'Comment accuracy vs the code it describes, comment rot (comment says X, code does Y), missing context for non-obvious logic. Findings only. Ignore project-specific conventions baked into your agent definition that this repo does not use.',
  },
  G: {
    standard: 'Behavioural test coverage gaps, missing edge cases (happy path only, no negative cases), tests that assert on mocks instead of real behaviour, tests that would still pass if the new logic were deleted. Findings only.',
  },
  H: {
    standard: 'Swallowed exceptions, bare `except:` / empty catch blocks / discarded fallible results (`let _ =` on a `Result`), fallback behaviour that masks real failures instead of surfacing them, missing error logging, retries that exhaust silently. Findings only. Ignore project-specific logging functions or error-ID registries named in your agent definition unless this repo actually has them.',
  },
  I: {
    standard: 'Encapsulation, invariant expression (can the type be constructed in an invalid state?), whether the type earns its complexity. Findings only.',
  },
  J: {
    standard: 'Data races and TOCTOU; read-modify-write on shared state without a lock or an atomic `UPDATE ... WHERE ... RETURNING`; missing or wrongly-scoped transactions; lock-ordering deadlocks; non-idempotent operations that get retried (webhooks, queue consumers); await points while holding locks; channel/queue operations that can drop or duplicate messages. Every CRITICAL/HIGH must spell out the interleaving ("A reads balance, B commits, A writes stale"). Findings only.',
    fable: 'Find the concurrency defects in this diff. Every CRITICAL/HIGH must spell out the interleaving that produces the failure ("A reads balance, B commits, A writes stale"). Findings only. How you look is up to you.',
  },
  K: {
    standard: 'N+1 queries, unbounded queries/collections, missing pagination, O(n^2) on user-scaled data, allocation or I/O in hot loops, missing or wrong indexes for new query shapes. Findings only — read-only, never edit files. Skip micro-optimisations; flag only what degrades at realistic scale.',
  },
}

function sharedInputs() {
  return [
    `Repo: ${A.repoPath}`,
    `Mode: ${A.mode}`,
    `Diff range: ${A.diffRange}`,
    `Changed files (${FILES.length}):\n${FILES.map((f) => `  - ${f}`).join('\n')}`,
    A.context ? `Context:\n${A.context}` : '',
    `Review input:\n${A.reviewInput}`,
    A.expandCmd
      ? `To expand an indexed file/hunk to exact source, run:\n  ${A.expandCmd}\n(substitute the real <path> and <ordinal>). A targeted \`git diff ${A.diffRange} -- <path>\` also works.`
      : '',
    'Inspect changed source and its callers independently before reading whole files. Review what changed, not the whole codebase.',
  ]
    .filter(Boolean)
    .join('\n\n')
}

function briefFor(id, model) {
  const b = BRIEFS[id]
  const body = model === 'fable' && b.fable ? b.fable : b.standard
  const blast = ROSTER[id].blastRadius ? `\n\nBLAST RADIUS:\n${BLAST_RADIUS}` : ''
  return `${body}\n\n---\n\n${sharedInputs()}\n\n---\n\nOUTPUT CONTRACT:\n${OUTPUT_CONTRACT}${blast}`
}

// ---------------------------------------------------------------- find

async function runLens(id) {
  const lens = ROSTER[id]
  const tier = tierFor(id)

  const dispatch = (model, effort) =>
    agent(briefFor(id, model), {
      label: `find:${id} ${lens.key}`,
      phase: 'Find',
      agentType: lens.agentType,
      model,
      effort,
      schema: FINDINGS_SCHEMA,
    }).catch(() => null)

  let res = await dispatch(tier.model, tier.effort)
  let answeredBy = `${tier.model}/${tier.effort}`
  let retried = false

  // A Fable refusal is HTTP 200 with empty content, so an empty Fable lens is
  // indistinguishable from a clean pass — printing it as "0 findings" would
  // count a refusal toward APPROVE. Re-dispatch once on Opus. Empty returns
  // from non-Fable models are NOT retried.
  if (isEmpty(res) && tier.model === 'fable') {
    retried = true
    res = await dispatch(lens.model, lens.effort)
    answeredBy = `${lens.model}/${lens.effort} (retry after empty fable/${tier.effort} return)`
  }

  return { id, key: lens.key, findings: (res && Array.isArray(res.findings) && res.findings) || [], answeredBy, retried }
}

function isEmpty(res) {
  return !res || !Array.isArray(res.findings) || res.findings.length === 0
}

// ----------------------------------------------------- phase 5 as code

function normalize(f) {
  if (!f || !f.file) return null
  return {
    file: String(f.file).trim(),
    line: Number.isInteger(f.line) ? f.line : 0,
    severity: SEV.includes(f.severity) ? f.severity : 'LOW',
    confidence: f.confidence || 'medium',
    issue: String(f.issue || '').trim(),
    failure_scenario: String(f.failure_scenario || '').trim(),
    suggested_fix: String(f.suggested_fix || '').trim(),
  }
}

// Phase 5 rule: a CRITICAL/HIGH with no concrete failure scenario drops to
// MEDIUM. Runs before verification so the cap is spent on falsifiable claims.
function demote(f) {
  if ((f.severity === 'CRITICAL' || f.severity === 'HIGH') && f.failure_scenario.length < 20) {
    return { ...f, severity: 'MEDIUM', demoted: true }
  }
  return f
}

function keyOf(f) {
  const sig = f.issue
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, ' ')
    .trim()
    .split(' ')
    .slice(0, 8)
    .join(' ')
  return `${f.file}:${f.line}:${sig}`
}

// -------------------------------------------------------------- verify

// Opus at medium is safe here only because of how the brief fails: the
// verifier returns PLAUSIBLE when it cannot prove either way, and PLAUSIBLE is
// kept. The failure mode is "keeps a false positive", not "deletes a real
// CRITICAL". That holds only while REFUTED requires quoting the specific
// guard/invariant — do not soften that clause.
function verifierBrief(f) {
  return [
    'Adversarially verify this review finding — your job is to REFUTE it.',
    '',
    `Finding: \`${f.file}:${f.line} — ${f.issue} — ${f.failure_scenario}\``,
    '',
    `Repo: ${A.repoPath}. Diff range: ${A.diffRange}.`,
    '',
    'Read the actual code, trace the claimed path, check the claimed inputs can actually reach it. Verdict: `CONFIRMED` (quote the code path that proves it), `REFUTED` (quote the guard/invariant that prevents it), or `PLAUSIBLE` (could not prove either way). One paragraph max.',
    '',
    'Then classify the fix this finding would need, honestly, even if you refuted it:',
    '- `fix_complexity`: `mechanical` (a constant, a rename, a doc or comment edit), `local` (contained to one function or file), or `invasive` (crosses module boundaries, changes a public contract, or needs a design decision).',
    '- `fix_class`: security | concurrency | correctness | error-handling | docs | comments | magic-numbers | other.',
  ].join('\n')
}

const UNVERIFIED = (why) => ({ verdict: 'UNVERIFIED', evidence: why, fix_complexity: 'invasive', fix_class: 'other' })

const verifyCache = new Map()
let verifyBudget = VERIFY_CAP
let pastCap = 0

// Memoised by finding key, so two lenses reporting the same defect share one
// verifier instead of racing two. JS is single-threaded up to the first await,
// so the cache write always beats a concurrent second caller.
function verifyOnce(f) {
  const k = keyOf(f)
  if (verifyCache.has(k)) return verifyCache.get(k)

  // CRITICALs are always verified; HIGHs consume the remaining budget. Anything
  // past it stays UNVERIFIED — and UNVERIFIED is never fixed.
  if (f.severity !== 'CRITICAL' && verifyBudget <= 0) {
    pastCap += 1
    const capped = Promise.resolve(UNVERIFIED(`past the verification cap of ${VERIFY_CAP}`))
    verifyCache.set(k, capped)
    return capped
  }
  verifyBudget -= 1

  const p = agent(verifierBrief(f), {
    label: `verify:${f.file}:${f.line}`,
    phase: 'Verify',
    agentType: 'general-purpose',
    model: 'opus',
    effort: 'medium',
    schema: VERDICT_SCHEMA,
  })
    .then((v) => v || UNVERIFIED('verifier returned no result'))
    .catch(() => UNVERIFIED('verifier errored'))

  verifyCache.set(k, p)
  return p
}

// ----------------------------------------------------------------- fix

// Routed by the verifier's declared fix_class / fix_complexity. Floor is
// sonnet/medium on code: Edit requires a prior Read, and at low effort an agent
// consolidates tool calls and may skip it.
function fixTier(f) {
  const cls = f.verification.fix_class
  if (cls === 'security' || cls === 'concurrency' || f.severity === 'CRITICAL') {
    return { model: 'opus', effort: 'xhigh', rank: 3 }
  }
  if (cls === 'correctness' || cls === 'error-handling' || f.verification.fix_complexity === 'local') {
    return { model: 'opus', effort: 'high', rank: 2 }
  }
  return { model: 'sonnet', effort: 'medium', rank: 1 }
}

function groupTier(list) {
  return list.map(fixTier).reduce((a, b) => (b.rank > a.rank ? b : a))
}

function fixBrief(file, list) {
  return [
    `Apply the minimal correct fix for each verified finding in \`${file}\`. Every finding below was independently confirmed by an adversarial verifier whose job was to refute it — treat them as real defects.`,
    '',
    `Repo: ${A.repoPath}`,
    `File: ${file}`,
    '',
    'Findings:',
    list
      .map((f, i) =>
        [
          `${i + 1}. [${f.severity}] line ${f.line} — ${f.issue}`,
          `   failure scenario: ${f.failure_scenario}`,
          `   suggested fix: ${f.suggested_fix}`,
          `   verifier evidence: ${f.verification.evidence}`,
        ].join('\n'),
      )
      .join('\n\n'),
    '',
    'Rules:',
    '- Read the file before editing it. Never edit blind.',
    '- Fix only what is listed. No refactors, no drive-by cleanups, no reformatting of untouched lines, no new dependencies. A scope audit runs on your diff.',
    '- Match the surrounding code style, naming, and error-handling idiom.',
    '- If a finding is not a real defect in the code as it stands, return `no_change_needed` and say why. Do not invent a change.',
    '- If the fix would require changing a public contract, editing another file, or making a design decision, return `skipped` with the reason. Do not attempt it.',
    '- Do not touch tests unless a finding is about a test. Do not run the test suite — validation runs after every fix agent has finished.',
    '',
    'Return exactly one result per finding above.',
  ].join('\n')
}

function auditBrief(files) {
  return [
    'Audit an auto-fix patch set for scope creep. Read only the patch.',
    '',
    `Run: \`git -C ${A.repoPath} diff ${A.rollbackSha} -- .\``,
    `That range is exactly the fixes applied by this review — ${A.rollbackSha} is a snapshot taken before the first edit.`,
    '',
    `Files the fix agents were authorised to touch:\n${files.map((f) => `  - ${f}`).join('\n')}`,
    '',
    'Answer one question: does this patch set do what the findings asked and nothing else? Flag any hunk that is a refactor, a rename, a reformat of untouched lines, a new dependency, a test change unrelated to a finding, or an edit to a file not listed above. Do not edit anything.',
  ].join('\n')
}

// ------------------------------------------------------------- execute

phase('Find')
log(`roster: ${SELECTED.join(', ')} · band=${BAND} · ${FABLE ? 'fable' : 'standard'} · ${REPORT_ONLY ? 'report-only' : 'auto-fix'}`)

// pipeline, not parallel: a lens's CRITICAL/HIGH findings start verifying the
// moment that lens returns, while slower lenses are still reading.
const lensResults = (
  await pipeline(
    SELECTED,
    (id) => runLens(id),
    async (res) => {
      if (!res) return null
      const graded = res.findings.map(normalize).filter(Boolean).map(demote)
      const hot = graded.filter((f) => f.severity === 'CRITICAL' || f.severity === 'HIGH')
      await parallel(hot.map((f) => () => verifyOnce(f)))
      return { ...res, findings: graded }
    },
  )
).filter(Boolean)

// Phase 5 synthesis, as code rather than as an agent.
phase('Verify')
const merged = new Map()
for (const res of lensResults) {
  for (const f of res.findings) {
    const k = keyOf(f)
    const prev = merged.get(k)
    if (!prev) {
      merged.set(k, { ...f, lenses: [res.id] })
      continue
    }
    if (!prev.lenses.includes(res.id)) prev.lenses.push(res.id)
    // Escalate: two lenses at different severities -> take the higher, and
    // adopt the better-evidenced wording along with it.
    if (SEV.indexOf(f.severity) > SEV.indexOf(prev.severity)) {
      prev.severity = f.severity
      prev.demoted = f.demoted
      if (f.failure_scenario.length > prev.failure_scenario.length) {
        prev.issue = f.issue
        prev.failure_scenario = f.failure_scenario
        prev.suggested_fix = f.suggested_fix
      }
    }
  }
}

const all = [...merged.values()]
for (const f of all) {
  // Already resolved for anything verified during the pipeline; only a finding
  // escalated by the cross-lens merge costs a fresh verifier here.
  f.verification =
    f.severity === 'CRITICAL' || f.severity === 'HIGH'
      ? await verifyOnce(f)
      : { verdict: 'N/A', evidence: '', fix_complexity: 'mechanical', fix_class: 'other' }
}

const refuted = all.filter((f) => f.verification.verdict === 'REFUTED')
const surviving = all.filter((f) => f.verification.verdict !== 'REFUTED')

const verifyStats = {
  confirmed: surviving.filter((f) => f.verification.verdict === 'CONFIRMED').length,
  plausible: surviving.filter((f) => f.verification.verdict === 'PLAUSIBLE').length,
  refuted: refuted.length,
  unverified: surviving.filter((f) => f.verification.verdict === 'UNVERIFIED').length,
}
if (pastCap > 0) log(`${pastCap} HIGH finding(s) past the verification cap of ${VERIFY_CAP} — tagged UNVERIFIED, not fixed`)

// CONFIRMED only. Never PLAUSIBLE, never UNVERIFIED, never before this point.
const confirmed = surviving.filter((f) => f.verification.verdict === 'CONFIRMED')
const invasive = confirmed.filter((f) => f.verification.fix_complexity === 'invasive')
const fixable = REPORT_ONLY ? [] : confirmed.filter((f) => f.verification.fix_complexity !== 'invasive')

if (REPORT_ONLY && confirmed.length) log(`--report-only: ${confirmed.length} confirmed finding(s) left unpatched`)
if (invasive.length) log(`${invasive.length} confirmed finding(s) classified invasive — reported, never patched`)

// One agent per file, all files in parallel. No worktree isolation: the fixes
// must land in the user's working tree.
const byFile = new Map()
for (const f of fixable) {
  if (!byFile.has(f.file)) byFile.set(f.file, [])
  byFile.get(f.file).push(f)
}

phase('Fix')
let groups = []
if (byFile.size > 0) {
  groups = (
    await parallel(
      [...byFile.entries()].map(([file, list]) => () => {
        const tier = groupTier(list)
        return agent(fixBrief(file, list), {
          label: `fix:${file}`,
          phase: 'Fix',
          model: tier.model,
          effort: tier.effort,
          schema: FIX_SCHEMA,
        }).then((r) => ({ file, tier: `${tier.model}/${tier.effort}`, results: (r && r.results) || [] }))
      }),
    )
  ).filter(Boolean)
}

// Fold each agent's outcome back onto its finding so the report can separate
// fixed from remaining.
const outcomeByFileLine = new Map()
for (const g of groups) {
  for (const r of g.results) outcomeByFileLine.set(`${r.file}:${r.line}`, r)
}
for (const f of fixable) {
  const r = outcomeByFileLine.get(`${f.file}:${f.line}`)
  f.outcome = r ? r.outcome : 'skipped'
  f.outcome_note = r ? r.note : 'fix agent returned no result for this finding'
}

const applied = fixable.filter((f) => f.outcome === 'fixed').length
log(`fixes applied: ${applied} across ${byFile.size} file(s)`)

phase('Audit')
let scopeAudit = null
if (applied > 0) {
  scopeAudit = await agent(auditBrief([...byFile.keys()]), {
    label: 'scope-audit',
    phase: 'Audit',
    agentType: 'general-purpose',
    model: 'opus',
    effort: 'high',
    schema: AUDIT_SCHEMA,
  }).catch(() => null)
}

return {
  band: BAND,
  changedLines: CHANGED_LINES,
  fileCount: FILES.length,
  droppedByBand: DROPPED_BY_BAND,
  fableSuggested: FABLE_SUGGESTED,
  coverage: lensResults.map((r) => ({ id: r.id, key: r.key, count: r.findings.length, answeredBy: r.answeredBy, retried: r.retried })),
  findings: surviving,
  refuted,
  invasive,
  fixes: { applied, files: byFile.size, groups },
  scopeAudit,
  verifyStats,
  reportOnly: REPORT_ONLY,
}
