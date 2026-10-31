export const meta = {
  name: 'batch',
  description: 'Apply one change across many places: plan independent units, edit each in its own git worktree, review every result.',
  whenToUse: 'When the user wants a refactor, migration or mechanical change spread across many files or modules and done in parallel. args is the instruction. Each unit is edited in its own worktree and left there for the user to merge; nothing is merged or pushed.',
  phases: [
    { title: 'Plan', detail: 'find the places to change and split them into independent units' },
    { title: 'Edit', detail: 'one worker per unit, each in its own worktree' },
    { title: 'Review', detail: 'a reviewer checks each unit against the instruction' },
  ],
}

const INSTRUCTION = typeof args === 'string' ? args.trim() : String((args && (args.instruction || args.task)) || '').trim()
if (!INSTRUCTION) {
  return { error: "No instruction given. Pass it as args: Workflow({name: 'batch', args: '<what to change everywhere>'})." }
}
const MAX_UNITS = 24

const PLAN = { type: 'object', properties: {
  units: { type: 'array', maxItems: MAX_UNITS, items: { type: 'object', properties: {
    title: { type: 'string' },
    files: { type: 'array', items: { type: 'string' } },
    change: { type: 'string' } }, required: ['title', 'files', 'change'] } },
  verify: { type: 'string' },
  notes: { type: 'string' } }, required: ['units'] }

const EDIT = { type: 'object', properties: {
  changed: { type: 'array', items: { type: 'string' } },
  summary: { type: 'string' },
  checked: { type: 'boolean' },
  problems: { type: 'string' } }, required: ['changed', 'summary', 'checked'] }

const REVIEW = { type: 'object', properties: {
  acceptable: { type: 'boolean' }, issues: { type: 'array', items: { type: 'string' } } }, required: ['acceptable', 'issues'] }

phase('Plan')
const plan = await agent(
  `Instruction: ${INSTRUCTION}\n\nExplore the repository (search it, read the code) and find every place this applies. Split the work into units that can be edited at the same time without touching the same files: group by module or directory, not one unit per line. Each unit gets a title, the files it owns and a precise description of the change to make there. At most ${MAX_UNITS} units; if there are more places than that, make the units bigger.\n\nAlso say in "verify" which command checks the result (a build, a test, a linter) so each worker can run it. Do not edit anything.\n\nStructured output only.`,
  { label: 'plan', phase: 'Plan', schema: PLAN, disallowedTools: ['Write', 'Edit'] })
if (!plan || !plan.units || plan.units.length === 0) {
  return { instruction: INSTRUCTION, error: 'The planner found nothing to change.' }
}
const claimed = new Set()
const units = []
for (const u of plan.units) {
  const files = u.files.filter(f => !claimed.has(f))
  if (files.length === 0) continue
  files.forEach(f => claimed.add(f))
  units.push({ ...u, files })
}
log(`${units.length} units: ${units.map(u => u.title).join('; ')}`)

const results = await pipeline(
  units,
  u => agent(
    `You are one of ${units.length} workers applying the same instruction in parallel, each in your own git worktree.\n\nOverall instruction: ${INSTRUCTION}\n\nYour unit: ${u.title}\nFiles you own: ${u.files.join(', ')}\nThe change: ${u.change}\n\nMake the change in your files only. Other workers own the rest, so leave them alone even if you see the same issue there. ${plan.verify ? `Then run: ${plan.verify}. ` : ''}If it fails because of your change, fix it; if it fails for reasons outside your files, say so in problems. Commit your work in the worktree with a short message. Report the files you changed, whether the check passed, and anything the reviewer should know.\n\nStructured output only.`,
    { label: `edit:${u.title.slice(0, 40)}`, phase: 'Edit', schema: EDIT, isolation: 'worktree' }),
  (edit, u) => edit && agent(
    `A worker applied "${INSTRUCTION}" to this unit and says it is done. Check its work against the instruction; do not take its summary on trust.\n\nUnit: ${u.title}\nFiles it owned: ${u.files.join(', ')}\nIntended change: ${u.change}\nWorker summary: ${edit.summary}\nWorker ran the check: ${edit.checked ? 'yes' : 'no'}${edit.problems ? `\nWorker reported: ${edit.problems}` : ''}\n\n${edit.worktree ? `Its worktree is ${edit.worktree}; look at what it committed (git -C that path show HEAD) and at anything uncommitted (git -C that path diff).` : 'Its worktree is gone, so it changed nothing.'} Say it is acceptable only if the diff does what the instruction asks in those files, changes nothing else, and does not break the build. List concrete issues otherwise.\n\nStructured output only.`,
    { label: `review:${u.title.slice(0, 40)}`, phase: 'Review', schema: REVIEW, disallowedTools: ['Write', 'Edit'] })
    .then(review => ({ unit: u.title, files: u.files, edit, review })))

const done = results.filter(Boolean)
const accepted = done.filter(r => r.review && r.review.acceptable)
const rejected = done.filter(r => r.review && !r.review.acceptable)
const unreviewed = done.filter(r => !r.review)
const failed = units.length - done.length
log(`${accepted.length} accepted, ${rejected.length} need work, ${unreviewed.length} unreviewed, ${failed} failed`)
return {
  instruction: INSTRUCTION,
  accepted: accepted.map(r => ({ unit: r.unit, worktree: r.edit.worktree || null, changed: r.edit.changed, summary: r.edit.summary })),
  needsWork: rejected.map(r => ({ unit: r.unit, worktree: r.edit.worktree || null, issues: r.review.issues, summary: r.edit.summary })),
  unreviewed: unreviewed.map(r => ({ unit: r.unit, worktree: r.edit.worktree || null, summary: r.edit.summary })),
  failed,
  note: 'Each unit was edited in its own worktree, named in its worktree field. Nothing was merged or pushed.',
  stats: { planned: units.length, accepted: accepted.length, needsWork: rejected.length, unreviewed: unreviewed.length, failed },
}
