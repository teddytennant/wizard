export const meta = {
  name: 'code-review',
  description: 'Review the current changes across several dimensions, then have independent skeptics try to refute each finding.',
  whenToUse: 'When the user wants a thorough review of a diff, a branch or a set of files, with every finding checked before it is reported. args is optional text naming the target (a branch, a PR number, a path); with none, the uncommitted changes plus commits not on the base branch are reviewed.',
  phases: [
    { title: 'Review', detail: 'one reviewer per dimension reads the change' },
    { title: 'Verify', detail: 'two skeptics per finding try to refute it' },
    { title: 'Report', detail: 'rank what survived' },
  ],
}

const TARGET = typeof args === 'string' && args.trim()
  ? args.trim()
  : 'the uncommitted changes plus the commits on this branch that are not on the base branch'
const VOTES = 2
const MAX_PER_DIMENSION = 8

const DIMENSIONS = [
  { key: 'correctness', brief: 'logic errors, wrong conditions, off-by-one, null or error handling that loses a failure, broken invariants, behavior that differs from what the code or its docs say' },
  { key: 'security', brief: 'injection, unchecked input at a trust boundary, path traversal, secrets in code or logs, auth or permission checks that can be skipped' },
  { key: 'concurrency', brief: 'data races, lock ordering, await points that break an invariant, resources leaked on early return or cancellation, blocking calls on an async path' },
  { key: 'tests', brief: 'changed behavior with no test, tests that cannot fail, tests that pin the bug, assertions that check nothing' },
  { key: 'simplicity', brief: 'dead code, needless indirection, a copy of logic that already exists in the repo, a change larger than the problem needs' },
]

const FINDINGS = { type: 'object', properties: { findings: { type: 'array', maxItems: MAX_PER_DIMENSION, items: {
  type: 'object', properties: {
    file: { type: 'string' }, line: { type: 'number' }, title: { type: 'string' },
    explanation: { type: 'string' }, severity: { enum: ['high', 'medium', 'low'] } },
  required: ['file', 'title', 'explanation', 'severity'] } } }, required: ['findings'] }

const VERDICT = { type: 'object', properties: { isReal: { type: 'boolean' }, evidence: { type: 'string' } }, required: ['isReal', 'evidence'] }

const rank = { high: 0, medium: 1, low: 2 }

phase('Review')
const reviewed = await pipeline(
  DIMENSIONS,
  d => agent(
    `Review ${TARGET}.\n\nYour dimension is ${d.key}: ${d.brief}.\n\nRead the change with git diff and git log, then read the surrounding code, not just the hunks. Report only problems you can point at: a file, a line and a reason a maintainer would accept. Do not report style preferences, and do not report anything outside your dimension. If you find nothing, return an empty list. At most ${MAX_PER_DIMENSION} findings, worst first.\n\nStructured output only.`,
    { label: `review:${d.key}`, phase: 'Review', schema: FINDINGS }),
  (review, d) => parallel((review ? review.findings : []).map(f => () => parallel(Array.from({ length: VOTES }, (_, v) => () => agent(
    `Skeptic ${v + 1} of ${VOTES}. A reviewer reports this problem in ${TARGET}. Try to refute it: read the code, trace the path, run a test or a command if that settles it. Say it is real only if you could not knock it down.\n\nFile: ${f.file}${f.line ? `:${f.line}` : ''}\nTitle: ${f.title}\nClaim: ${f.explanation}\n\nStructured output only.`,
    { label: `verify:${f.title.slice(0, 32)}`, phase: 'Verify', schema: VERDICT })))
    .then(votes => {
      const cast = votes.filter(Boolean)
      const real = cast.filter(x => x.isReal).length
      return { ...f, dimension: d.key, votes: cast.length, real, evidence: cast.filter(x => x.isReal).map(x => x.evidence) }
    }))))

phase('Report')
const all = reviewed.filter(Boolean).flat().filter(Boolean)
const confirmed = all.filter(f => f.votes > 0 && f.real * 2 > f.votes)
const unverified = all.filter(f => f.votes === 0)
confirmed.sort((a, b) => rank[a.severity] - rank[b.severity] || b.real - a.real)
log(`${all.length} findings reviewed, ${confirmed.length} held up, ${all.length - confirmed.length - unverified.length} refuted, ${unverified.length} could not be checked`)
return {
  target: TARGET,
  confirmed: confirmed.map(f => ({ dimension: f.dimension, severity: f.severity, file: f.file, line: f.line || null, title: f.title, explanation: f.explanation, votes: `${f.real}/${f.votes}`, evidence: f.evidence })),
  unverified: unverified.map(f => ({ dimension: f.dimension, file: f.file, title: f.title })),
  stats: { dimensions: DIMENSIONS.length, found: all.length, confirmed: confirmed.length, refuted: all.length - confirmed.length - unverified.length, unverified: unverified.length },
}
