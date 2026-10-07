import assert from 'node:assert/strict'
import test from 'node:test'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'

import { driveDelivery, runApiProductionVertical, waitForDeviceWorkerRegistered,
  workItemCreatePayload } from '../scripts/run-api-production-vertical.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { runDeviceTaskVertical } from '../scripts/run-device-task-vertical.mjs'
import { deviceTaskIdentities, withDeviceTaskRuntime } from '../scripts/device-task-runtime.mjs'
import { benchmarkConfiguration } from '../scripts/run-real-task-benchmark.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'

/**
 * Production vertical acceptance coverage is retained: Chat, StrongFlow,
 * cancel, restart. The path is Device Worker only.
 *
 * Live execution requires:
 * - built winwincode-server / winwincode-kernel-helper / wwc CLI
 * - Device enrollment + occupancy + repository binding
 * - Device-local Provider (deterministic test Provider or real Device GLM config)
 *
 * Server-local model execution is not a fallback. When the Device CLI or
 * Device Provider runtime is unavailable, this test fails with that exact
 * runtime gap instead of fabricating success.
 */
test('a scenario propagates its Device cleanup failure', async () => {
  await assert.rejects(runApiProductionVertical({
    devicePrerequisites: true,
    restart: false,
    repeat: false,
    scenario: { async run({ devicePath }) {
      const stop = devicePath.stop
      devicePath.stop = async () => {
        await stop()
        throw new Error('scenario cleanup probe')
      }
      return { completed: true }
    } },
  }), /scenario cleanup probe/u)
})

test('standalone Server API drives Chat and StrongFlow through Device Worker production vertical', async () => {
  const report = await runApiProductionVertical({
    devicePrerequisites: true,
  })
  assert.equal(report.schemaVersion, 'winwincode.api-production-vertical.v1')
  assert.equal(report.execution, 'device-worker-only')
  assert.ok(Array.isArray(report.devicePrerequisites))
  assert.ok(report.devicePrerequisites.includes('device-enroll-pair'))
  assert.ok(report.devicePrerequisites.includes('chat-strongflow-cancel-restart'))
  assert.equal(report.deviceProvider?.secretPlacement, 'device-local-only')
  assert.ok(report.devicePath, 'production vertical must expose Device path evidence')
  assert.ok(report.devicePath.publicClientId)
  assert.equal(report.flow.chat.status, 'Completed')
  assert.equal(report.flow.chat.assistant.role, 'assistant')
  assert.equal(report.flow.chat.assistant.state, 'completed')
  assert.ok(report.flow.chat.assistant.content.trim().length > 0)
  assert.equal(report.flow.strongflow.status, 'done')
  assert.equal(report.flow.strongflow.verdictStatus, 'pass')
  assert.ok(report.flow.strongflow.workItemStates.length > 0)
  assert.equal(report.flow.strongflow.workItemStates.every(state => state === 'done'), true)
  assert.ok(report.flow.strongflow.workRunStates.length > 0)
  assert.equal(report.flow.strongflow.workRunStates.every(state => state === 'settled'), true)
  const calls = report.flow.strongflow.workRunRuntime.flatMap(run => run.coreToolCalls)
  assert.ok(calls.some(call => call.toolName.endsWith('exec_command')
    && call.cellId !== null && call.parentRequestSequence !== null
    && call.execution === 'completed' && call.disposition === 'accepted'
    && call.delivery === 'offered'), 'public API must preserve the original nested Core command receipt')

  assert.deepEqual(report.deterministic, {
    contentEqual: true,
    firstSessionId: 'psn_01J00000000000000000000001',
    repeatSessionId: 'psn_01J00000000000000000000002',
  })
  assert.deepEqual(report.restart, {
    deliveryBytesStable: true,
    messageBytesStable: true,
    status: 'done',
  })
})


test('the actual task entry completes parallel Sessions in one Server and Device runtime', async t => {
  const directory = process.env.WWC_PARALLEL_FIXTURE_DIRECTORY ?? mkdtempSync(join(tmpdir(), 'wwc-parallel-tasks-'))
  mkdirSync(directory, { recursive: true })
  if (process.env.WWC_PARALLEL_FIXTURE_DIRECTORY === undefined) t.after(() => rmSync(directory, { recursive: true, force: true }))
  const oldDeterministic = process.env.WWC_DEVICE_TASK_DETERMINISTIC
  process.env.WWC_DEVICE_TASK_DETERMINISTIC = '1'
  t.after(() => {
    if (oldDeterministic === undefined) delete process.env.WWC_DEVICE_TASK_DETERMINISTIC
    else process.env.WWC_DEVICE_TASK_DETERMINISTIC = oldDeterministic
  })
  const task = { title: 'Independent task', goal: 'Write status: complete to TASK.md.',
    scope: ['TASK.md'], constraints: ['Only change TASK.md'], outOfScope: [],
    verificationCommand: 'npm run verify',
    acceptanceCriteria: [{ id: 'done', required: true, title: 'TASK.md is complete' }],
    files: { 'TASK.md': 'status: pending\n', 'package.json': JSON.stringify({
      private: true, scripts: { verify: "node -e \"if(require('fs').readFileSync('TASK.md','utf8')!=='status: complete\\n')process.exit(1)\"" },
    }) + '\n' } }
  const inputPath = join(directory, 'native-task.json')
  writeFileSync(inputPath, JSON.stringify(task))
  const profile = benchmarkConfiguration('main-A')
  const options = { directory, profiles: [profile],
    providers: [deterministicDeviceProvider()], build: process.env.WWC_API_SKIP_BUILD !== '1',
    timeoutMillis: 1_200_000,
  }
  let retained
  const result = await withDeviceTaskRuntime(options, async runtime => {
      const contexts = new Map()
      for (const id of ['first', 'second']) contexts.set(id, await runtime.forTask({
        ...profile, provider: 'device-deterministic-model',
      }, { taskInputPath: inputPath, directory: join(directory, `${id}-repository`) }))
      const context = contexts.get('first'), api = context.api
      const launch = async id => {
        const prior = join(directory, id, 'device-task-result.json')
        if (existsSync(prior)) {
          const retained = JSON.parse(readFileSync(prior, 'utf8'))
          assert.equal(retained.complete, true, 'an interrupted fixture task must not be replayed')
          const projection = (await api.query('delivery.get', { deliveryId: retained.deliveryId })).result
          assert.equal(projection.status, 'done')
          assert.equal(projection.verdict.status, 'pass')
          return retained
        }
        return runDeviceTaskVertical({ directory: join(directory, id),
          ...deviceTaskIdentities('native-parallel-entry', id), runtime: contexts.get(id),
          timeoutMillis: 1_200_000 })
      }
      const results = await Promise.allSettled([launch('first'), launch('second')])
      for (const result of results) if (result.status === 'rejected') throw result.reason
      const [first, second] = results.map(result => result.value)
      assert.equal(first.complete, true)
      assert.equal(second.complete, true)
      assert.equal(first.delivery.detail.verdict.status, 'pass')
      assert.equal(second.delivery.detail.verdict.status, 'pass')
      assert.notEqual(first.productSessionId, second.productSessionId)
      assert.notEqual(first.deliveryId, second.deliveryId)
      assert.equal(first.publicClientId, second.publicClientId)
      const receipts = value => readDeviceExecutionReceipts(value.directory, {
        deliveryId: value.deliveryId, productSessionId: value.productSessionId,
      })
      const a = receipts(first), b = receipts(second)
      assert.ok(a.calls.length > 0 && b.calls.length > 0)
      assert.equal(a.calls.some(call => b.calls.some(other => call.exchangeId === other.exchangeId)), false)
      assert.equal(a.performance.some(row => b.performance.some(other => row.runKey === other.runKey)), false)
      for (const task of [first, second]) {
        assert.equal(task.sourceAnchor.state, 'drained')
        const core = new DatabaseSync(join(contexts.get('first').devicePath.deviceData,
          'worker-sessions', task.sourceAnchor.workerSessionId, 'data/codex-runtime/worker-codex.sqlite3'),
        { readOnly: true })
        try { assert.equal(core.prepare('SELECT count(*) AS n FROM model_call_ledger').get().n, 0) }
        finally { core.close() }
        assert.equal(task.delivery.workRunAggregate.runs.length, 3)
        assert.equal(task.delivery.workRunAggregate.runs.every(run => run.state === 'settled'), true)
      }
      // Seed retained, drained history from a Worker which completed the real
      // task entry. On reopen, the new anchor sorts beyond the first 200 rows.
      const registry = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
      // The live Server continues heartbeats while this fixture adds history.
      registry.exec('PRAGMA busy_timeout = 5000')
      try {
        const template = registry.prepare('SELECT * FROM execution_workers WHERE worker_id = ?').get(first.sourceAnchor.workerId)
        const scope = registry.prepare('SELECT * FROM execution_worker_scopes WHERE worker_id = ?').get(first.sourceAnchor.workerId)
        const columns = Object.keys(template)
        const insert = registry.prepare(`INSERT INTO execution_workers (${columns.join(',')}) VALUES (${columns.map(() => '?').join(',')})`)
        for (let i = 1; i <= 201; i++) {
          const workerId = `wrk_${String(i).padStart(26, '0')}`
          const instance = `wki_${String(i).padStart(26, '0')}`
          if (registry.prepare('SELECT 1 FROM execution_workers WHERE worker_id = ?').get(workerId)) continue
          insert.run(...columns.map(key => key === 'worker_id' ? workerId : key === 'worker_instance_id' ? instance : template[key]))
          registry.prepare('INSERT INTO execution_worker_instances VALUES (?,?,?)').run(workerId, instance, template.started_at)
          registry.prepare('INSERT INTO execution_worker_scopes VALUES (?,?,?)').run(workerId, scope.scope_key, scope.scope_json)
          assert.equal((await api.command('worker.drain', 0, { workerId, reason: 'Historical drained fixture' })).outcome, 'completed')
        }
      } finally { registry.close() }
      const firstPage = await api.query('worker.list', { states: [] })
      assert.equal(firstPage.result.items.length, 200)
      assert.equal(firstPage.page.hasMore, true)
      assert.equal(firstPage.result.items.every(worker => worker.state === 'draining'), true)
      retained = { first, second, receipts: [a, b],
        baseline: context.baseline, binding: context.repositoryBindingId,
        client: context.devicePath.publicClientId,
        endpoint: readFileSync(join(directory, 'server-endpoint.json'), 'utf8'),
        certificate: readFileSync(join(directory, 'fixture-cert.pem'), 'utf8') }
      return { completed: 2, calls: [a.calls.length, b.calls.length] }
  })
  assert.equal(result.flow.scenario.completed, 2)
  const reopened = await withDeviceTaskRuntime({ ...options, build: false }, async runtime => {
    const context = await runtime.forTask({ ...profile, provider: 'device-deterministic-model' },
      { taskInputPath: inputPath, directory: join(directory, 'first-repository') })
    assert.equal(context.baseline, retained.baseline)
    assert.equal(context.repositoryBindingId, retained.binding)
    assert.equal(context.devicePath.publicClientId, retained.client)
    assert.equal(readFileSync(join(directory, 'server-endpoint.json'), 'utf8'), retained.endpoint)
    assert.equal(readFileSync(join(directory, 'fixture-cert.pem'), 'utf8'), retained.certificate)
    for (const [index, prior] of [retained.first, retained.second].entries()) {
      const projection = (await context.api.query('delivery.get', { deliveryId: prior.deliveryId })).result
      assert.equal(projection.status, 'done')
      assert.equal(projection.verdict.status, 'pass')
      assert.deepEqual(readDeviceExecutionReceipts(prior.directory, {
        deliveryId: prior.deliveryId, productSessionId: prior.productSessionId,
      }).calls, retained.receipts[index].calls)
    }
    assert.equal(context.devicePath.modelServer.requests.length, 0, 'reopening completed calls must not invoke a model')
    const next = await runDeviceTaskVertical({ directory: join(directory, 'third'),
      ...deviceTaskIdentities('native-parallel-entry', 'third'), runtime: context,
      timeoutMillis: 1_200_000 })
    assert.equal(next.complete, true)
    assert.equal(next.delivery.detail.verdict.status, 'pass')
    const foreign = await context.api.query('worker.list', { states: [] })
    assert.equal(foreign.result.items.every(worker => worker.state === 'draining' && worker.revision === 1), true)
    return { retainedCompleted: 2, continued: 1 }
  })
  assert.deepEqual(reopened.flow.scenario, { retainedCompleted: 2, continued: 1 })
})
