import test, { after } from 'node:test'
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const validator = join(root, 'scripts/validate-engineering-runtime-backlog.mjs')
const raw = readFileSync(join(root, 'docs/engineering-runtime/backlog-migration.json'), 'utf8')
const snapshot = JSON.parse(raw)
const temporaryDirectories = []
after(() => { for (const dir of temporaryDirectories) rmSync(dir, { recursive: true, force: true }) })
const keys = ['id', 'title', 'description', 'acceptance_criteria', 'status', 'issue_type', 'assignee', 'labels', 'dependencies']
const digest = value => createHash('sha256').update(JSON.stringify(value, (_, item) => item && typeof item === 'object' && !Array.isArray(item) ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) : item)).digest('hex')
const run = (...args) => spawnSync(process.execPath, [validator, ...args], { cwd: root, encoding: 'utf8', env: { ...process.env, PATH: '' } })
function expect(result, success, message) {
  assert.equal(result.error, undefined)
  assert.equal(result.status === 0, success, result.stdout + result.stderr)
  if (message) assert.match(result.stdout + result.stderr, message)
}
// Synthetic records exercise comparison logic, never substitute for live Beads.
function fixture() {
  const data = structuredClone(snapshot)
  const records = data.mapping.map(row => {
    const record = { id: row.old_id, title: 'fixture', description: 'fixture', acceptance_criteria: null, status: row.status_at_audit, issue_type: 'task', assignee: null, labels: [], dependencies: [], metadata: { engineering_runtime_review: { ...row, audit_bead: data.audit_bead } } }
    row.source_record_sha256 = digest(Object.fromEntries(keys.map(key => [key, record[key]])))
    return record
  })
  for (const [stable, id] of Object.entries(data.new_beads)) {
    const parent = data.new_bead_prerequisites[stable]
    records.push({ id, title: `[${stable}] fixture`, description: stable, acceptance_criteria: 'fixture', status: 'open', metadata: { engineering_runtime_plan_ids: [stable] }, dependencies: parent ? [{ depends_on_id: data.new_beads[parent], type: 'blocks' }] : [] })
  }
  for (const entry of data.task_plan.entries) {
    let record = records.find(item => item.id === entry.bead_id)
    if (!record) { record = { id: entry.bead_id, title: entry.title, description: entry.stable_id, acceptance_criteria: 'fixture', status: 'open', metadata: { engineering_runtime_plan_ids: [] }, dependencies: [] }; records.push(record) }
    record.title = `${record.title} [${entry.stable_id}]`; record.description = `${record.description ?? ''} ${entry.stable_id}`; record.acceptance_criteria = 'fixture'
    record.metadata ??= {}; record.metadata.engineering_runtime_plan_ids = [...new Set([...(record.metadata.engineering_runtime_plan_ids ?? []), entry.stable_id])]
  }
  for (const entry of data.task_plan.entries) for (const dependency of entry.depends_on ?? []) {
    const owner = data.task_plan.entries.find(item => item.stable_id === dependency)?.bead_id
    if (owner && owner !== entry.bead_id) records.find(item => item.id === entry.bead_id).dependencies.push({ depends_on_id: owner, type: 'blocks' })
  }
  for (const entry of data.task_plan.entries) {
    const record = records.find(item => item.id === entry.bead_id)
    entry.record_sha256 = digest(Object.fromEntries(keys.map(key => [key, record?.[key] ?? null])))
  }
  for (const row of data.mapping) {
    const record = records.find(item => item.id === row.old_id)
    row.source_record_sha256 = digest(Object.fromEntries(keys.map(key => [key, record?.[key] ?? null])))
  }
  return { data, records }
}
function files(data, records = []) {
  const dir = mkdtempSync(join(tmpdir(), 'wwc-er-review-'))
  temporaryDirectories.push(dir)
  const snapshotPath = join(dir, 'snapshot.json'), recordsPath = join(dir, 'records.json')
  writeFileSync(snapshotPath, JSON.stringify(data)); writeFileSync(recordsPath, JSON.stringify(records))
  return ['--snapshot', snapshotPath, '--records', recordsPath]
}

test('snapshot is portable and explicitly not a live check', () => {
  assert.doesNotMatch(raw, /\/(?:Users|Volumes|private|tmp)\//)
  expect(run('--mode=snapshot'), true, /live Beads status=NOT_RUN/)
})
test('offline records can pass without Beads but never claim live verification', () => {
  const { data, records } = fixture()
  expect(run('--mode=records', ...files(data, records)), true, /mode=records;.*live Beads status=NOT_RUN/)
  expect(run('--mode=live'), false, /live records query failed/)
})
for (const [name, mutate, message] of [
  ['cycle', d => { const [a, b] = d.mapping.filter(r => r.classification === 'MERGE'); a.canonical_owner = b.old_id; b.canonical_owner = a.old_id; d.duplicate_scope_groups = [a, b].map(r => ({ source: r.old_id, target: r.canonical_owner })) }, /merge cycle/],
  ['empty prerequisites', d => { d.new_bead_prerequisites = {} }, /prerequisites/],
  ['wrong prerequisite', d => { d.new_bead_prerequisites['WWC-ER-0002'] = 'WWC-ER-0002' }, /prerequisites/],
  ['invalid plan hash', d => { d.source_plan_sha256 = 'not-a-hash' }, /plan hash/],
  ['duplicate mapping', d => { d.mapping[1].old_id = d.mapping[0].old_id }, /duplicate old_id/],
  ['missing mapping', d => { d.mapping.pop() }, /mapping count/],
  ['repeated merge group', d => { d.duplicate_scope_groups[1] = d.duplicate_scope_groups[0] }, /source repeated/],
  ['duplicate new ID', d => { d.new_beads['WWC-ER-0003'] = d.new_beads['WWC-ER-0002'] }, /IDs must be distinct/],
]) {
  test(`rejects snapshot ${name}`, () => {
    const data = structuredClone(snapshot); mutate(data)
    const args = files(data)
    expect(run('--mode=snapshot', ...args.slice(0, 2)), false, message)
  })
}
for (const field of ['description', 'status', 'assignee', 'dependencies', 'metadata']) {
  test(`rejects actual ${field} changes against the recorded fingerprint`, () => {
    const { data, records } = fixture()
    if (field === 'dependencies') records[0][field] = [{ depends_on_id: records[1].id, type: 'blocks' }]
    else if (field === 'metadata') records[0].metadata.engineering_runtime_review.canonical_owner = records[1].id
    else records[0][field] = field === 'status' ? 'blocked' : 'changed'
    expect(run('--mode=records', ...files(data, records)), false, /source (task|metadata) changed/)
  })
}
test('rejects missing live prerequisite edges', () => {
  const { data, records } = fixture()
  records.find(r => r.id === data.new_beads['WWC-ER-0003']).dependencies = []
  expect(run('--mode=records', ...files(data, records)), false, /missing prerequisite/)
})
test('rejects task-plan omissions and owner metadata', () => {
  const { data, records } = fixture()
  data.task_plan.entries.pop()
  expect(run('--mode=records', ...files(data, records)), false, /task plan identity\/count drift/)
  const valid = fixture()
  valid.records.find(r => r.id === valid.data.task_plan.entries[0].bead_id).metadata.engineering_runtime_plan_ids = []
  expect(run('--mode=records', ...files(valid.data, valid.records)), false, /task plan metadata missing/)
})
test('rejects changed task-plan owner acceptance criteria against its fingerprint', () => {
  const { data, records } = fixture()
  const entry = data.task_plan.entries[0]
  records.find(record => record.id === entry.bead_id).acceptance_criteria = 'changed fixture acceptance'
  expect(run('--mode=records', ...files(data, records)), false, /task plan record changed/)
})
test('rejects task-plan dependency and cycle fixtures', () => {
  const missing = fixture()
  const first = missing.data.task_plan.entries.find(entry => entry.depends_on.length)
  first.depends_on = ['WWC-ER-9999']
  expect(run('--mode=snapshot', ...files(missing.data).slice(0, 2)), false, /task plan dependency missing/)
  const cycle = fixture()
  const [a, b] = cycle.data.task_plan.entries.slice(0, 2)
  a.depends_on = [b.stable_id]; b.depends_on = [a.stable_id]
  a.internal_dependencies = [b.stable_id]; b.internal_dependencies = [a.stable_id]
  const ar = cycle.records.find(r => r.id === a.bead_id), br = cycle.records.find(r => r.id === b.bead_id)
  ar.dependencies.push({ depends_on_id: b.bead_id, type: 'blocks' }); br.dependencies.push({ depends_on_id: a.bead_id, type: 'blocks' })
  expect(run('--mode=records', ...files(cycle.data, cycle.records)), false, /dependency cycle/)
})
test('rejects missing cross-owner block edge', () => {
  const { data, records } = fixture()
  const entry = data.task_plan.entries.find(item => item.depends_on.some(dep => data.task_plan.entries.find(candidate => candidate.stable_id === dep)?.bead_id !== item.bead_id))
  const dependency = entry.depends_on.find(dep => data.task_plan.entries.find(candidate => candidate.stable_id === dep)?.bead_id !== entry.bead_id)
  const owner = data.task_plan.entries.find(item => item.stable_id === dependency).bead_id
  const record = records.find(item => item.id === entry.bead_id)
  record.dependencies = record.dependencies.filter(item => item.depends_on_id !== owner)
  expect(run('--mode=records', ...files(data, records)), false, /missing cross-owner block/)
})
test('rejects duplicate task-plan owner claims', () => {
  const { data, records } = fixture()
  const entry = data.task_plan.entries[0]
  records.push({ id: 'fixture-second-owner', title: entry.stable_id, description: entry.stable_id, acceptance_criteria: 'fixture', status: 'open', metadata: { engineering_runtime_plan_ids: [entry.stable_id] }, dependencies: [] })
  expect(run('--mode=records', ...files(data, records)), false, /owner claim conflict/)
})
test('rejects typos, missing values, duplicate options and offline input in live mode', () => {
  for (const args of [['--mdoe=live'], ['--mode=wat'], ['--mode'], ['--mode=snapshot', '--mode=live'], ['--mode=records'], ['--mode=live', '--records=fixture.json'], ['--snapshot'], ['--records=x']]) expect(run(...args), false)
})
