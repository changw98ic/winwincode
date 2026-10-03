import assert from 'node:assert/strict'
import test from 'node:test'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { deviceTaskIdentities, prepareDeviceTaskBaseline } from '../scripts/device-task-runtime.mjs'
import { pendingDeviceTaskWorkRuns, cancelStoppedDeviceTask } from '../scripts/run-device-task-vertical.mjs'

test('task baselines preserve a dirty shared checkout and isolate frozen task trees', t => {
  const directory = mkdtempSync(join(tmpdir(), 'device-task-baselines-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const repository = join(directory, 'source')
  mkdirSync(repository)
  const git = (...args) => execFileSync('git', ['-C', repository, ...args], { encoding: 'utf8' }).trim()
  git('init', '--quiet')
  git('config', 'user.name', 'fixture')
  git('config', 'user.email', 'fixture@example.invalid')
  writeFileSync(join(repository, 'existing.txt'), 'committed\n')
  git('add', '.')
  git('commit', '--quiet', '-m', 'fixture')
  writeFileSync(join(repository, 'existing.txt'), 'user change\n')
  const before = git('status', '--porcelain')
  const head = git('rev-parse', 'HEAD')
  const input = content => ({ title: 'Frozen task', goal: 'Implement task', scope: ['answer.txt'],
    constraints: [], outOfScope: [], verificationCommand: 'test -s answer.txt',
    acceptanceCriteria: [{ id: 'answer', title: 'Answer exists', required: true }],
    files: { 'answer.txt': content } })
  const firstPath = join(directory, 'first.json'), secondPath = join(directory, 'second.json')
  writeFileSync(firstPath, JSON.stringify(input('first\n')))
  writeFileSync(secondPath, JSON.stringify(input('second\n')))
  const a = prepareDeviceTaskBaseline(repository, firstPath, join(directory, 'a'))
  const b = prepareDeviceTaskBaseline(repository, secondPath, join(directory, 'b'))
  const replay = prepareDeviceTaskBaseline(repository, firstPath, join(directory, 'replay'))
  assert.deepEqual(replay, a)
  assert.notEqual(a.baseline, b.baseline)
  assert.equal(git('show', `${a.baseline}:answer.txt`), 'first')
  assert.equal(git('show', `${b.baseline}:answer.txt`), 'second')
  assert.equal(git('rev-parse', 'HEAD'), head)
  assert.equal(git('status', '--porcelain'), before)
  assert.equal(readFileSync(join(repository, 'existing.txt'), 'utf8'), 'user change\n')
})

test('frozen launches retain identities across restart and distinct calls own distinct Sessions', () => {
  const first = deviceTaskIdentities('experiment', 'task:provider')
  assert.deepEqual(deviceTaskIdentities('experiment', 'task:provider'), first)
  for (const second of [deviceTaskIdentities('experiment', 'task:member:provider'),
    deviceTaskIdentities('other-experiment', 'task:provider')]) {
    assert.notEqual(first.productSessionId, second.productSessionId)
    assert.notEqual(first.deliveryId, second.deliveryId)
  }
  assert.match(first.productSessionId, /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
})

test('queued Controller roles are discovered only for their own Delivery before lease projection', t => {
  const directory = mkdtempSync(join(tmpdir(), 'device-pending-roles-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  mkdirSync(join(directory, 'server-data'))
  const db = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT, work_run_id TEXT,
    dispatch_payload BLOB, state TEXT, submitted_at TEXT)`)
  const run = 'wrn_01J00000000000000000000001'
  const insert = db.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?,?,?,?)')
  for (const [job, delivery, id, state] of [['reviewer', 'task-a', run, 'queued'],
    ['foreign-corrupt', 'task-b', 'invalid', 'queued'], ['done', 'task-a', 'invalid', 'completed']]) {
    insert.run(job, delivery, id, Buffer.from(JSON.stringify({ scope: { kind: 'work-run', workRunId: id } })), state, 'now')
  }
  assert.deepEqual(pendingDeviceTaskWorkRuns(directory, 'task-a'), [run])
  db.prepare("UPDATE scheduler_execution_jobs SET state = 'running' WHERE job_id = 'reviewer'").run()
  assert.deepEqual(pendingDeviceTaskWorkRuns(directory, 'task-a'), [])
  db.prepare("UPDATE scheduler_execution_jobs SET state = 'queued', dispatch_payload = '{}' WHERE job_id = 'reviewer'").run()
  assert.throws(() => pendingDeviceTaskWorkRuns(directory, 'task-a'))
})


test('a stopped task cancels only its active and queued roles and source Session', async () => {
  const commands = []
  const api = {
    query: async (name, payload) => {
      assert.equal(payload.deliveryId ?? 'task-a', 'task-a')
      return { result: name === 'workrun.get'
        ? { readCursor: { deliveryId: 'task-a' }, runs: [
          { id: 'active-a', state: 'running' }, { id: 'done-a', state: 'settled' }] }
        : name === 'delivery.get' ? { deliveryId: 'task-a', deliveryRevision: 3 }
          : { id: 'source-a', revision: 1, state: 'active' } }
    },
    command: async (name, revision, payload) => {
      commands.push({ name, revision, payload })
      return { outcome: 'completed' }
    },
  }
  const result = await cancelStoppedDeviceTask(api, '/missing-task-directory', 'task-a', 'source-a')
  assert.deepEqual(result.workRunIds, ['active-a'])
  assert.deepEqual(commands.map(command => [command.name, command.payload.workRunId ?? command.payload.productSessionId]),
    [['workrun.cancel', 'active-a'], ['session.cancel', 'source-a']])
})
