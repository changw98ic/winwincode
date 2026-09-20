#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

/**
 * Device-task vertical: Device pairing, Device-local Provider secrets, then
 * StrongFlow on the Device Worker path.
 *
 * Real model secrets are configured only on the Device process. The Server
 * never receives WWC_SERVER_MODEL_* or GLM API keys.
 */

import assert from 'node:assert/strict'
import { join, resolve } from 'node:path'
import { writeFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'

import {
  runApiProductionVertical,
  workItemCreatePayload,
  deviceOnlyServerEnvironment,
  deviceProviderSecretBundle,
  assertServerEnvironmentIsDeviceOnly,
} from './run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const directory = resolve(
  process.env.WWC_DEVICE_TASK_RESULT_DIRECTORY
    ?? join('test-results', 'device-task', new Date().toISOString().replaceAll(':', '-')),
)
const deliveryId = 'dlv_01J00000000000000000000001'
const report = { complete: false, directory, steps: [], execution: 'device-worker-only' }
const save = () => writeFileSync(join(directory, 'device-task-result.json'), `${JSON.stringify(report, null, 2)}\n`)

import { mkdirSync } from 'node:fs'
mkdirSync(directory, { recursive: true, mode: 0o700 })

const useLoopback = process.env.WWC_DEVICE_TASK_LOOPBACK === '1'
const useDeviceDeterministic = process.env.WWC_DEVICE_TASK_DETERMINISTIC === '1'
  || useLoopback
  || process.env.ZHIPU_API_KEY === undefined
for (const name of useDeviceDeterministic ? [] : ['ZHIPU_API_KEY', 'ZHIPU_BASE_URL', 'ZHIPU_MODEL']) {
  assert.ok(process.env[name], `${name} is required unless WWC_DEVICE_TASK_DETERMINISTIC=1`)
}

// Secrets stay Device-local. The Server environment must stay free of model keys.
const deviceSecrets = useDeviceDeterministic
  ? []
  : [process.env.ZHIPU_API_KEY]
const deviceProvider = useDeviceDeterministic
  ? {
    providerId: 'winwincode-device-deterministic',
    modelId: 'device-deterministic-model',
  }
  : {
    providerId: 'zhipu-glm',
    modelId: process.env.ZHIPU_MODEL,
  }
const deviceRoute = {
  providerId: deviceProvider.providerId,
  modelId: deviceProvider.modelId,
  // Filled after pairing by establishDeviceOnlyExecutionPath.
  clientNodeId: null,
}

const serverEnvironment = deviceOnlyServerEnvironment({
  WWC_SERVER_WORKER_MODE: 'remote',
  WWC_DEBUG_RUNTIME: '1',
  WWC_DEBUG_RUNTIME_LOG: join(directory, 'server-runtime.log'),
  WWC_SERVER_EXECUTION_LEASE_SECONDS: '600',
})
assertServerEnvironmentIsDeviceOnly(serverEnvironment)

try {
  const vertical = await runApiProductionVertical({
    directory,
    restart: false,
    repeat: false,
    devicePrerequisites: true,
    deviceRoute,
    deviceProviderSecrets: deviceSecrets,
    wwcBinary: process.env.WWC_CLI_BINARY ?? resolve(root, 'target/debug/wwc'),
    serverEnvironment,
    timeoutMillis: 600_000,
    scenario: {
      files: {
        'TASK.md': 'status: pending\n',
        'package.json': `${JSON.stringify({
          name: 'device-agent-task',
          private: true,
          type: 'module',
          scripts: { verify: 'node verify.mjs' },
        }, null, 2)}\n`,
        'verify.mjs': "import assert from 'node:assert/strict'\nimport { readFileSync } from 'node:fs'\nassert.equal(readFileSync('TASK.md', 'utf8'), 'status: complete\\n')\n",
      },
      async run({ api, repository, baseline, modelRoute, devicePath }) {
        assert.ok(devicePath, 'Device-only vertical must expose devicePath')
        report.publicClientId = devicePath.publicClientId
        report.repositoryBindingId = devicePath.repositoryBindingId
        report.steps.push(...devicePath.steps)
        report.modelRoute = modelRoute
        report.providerSecretBundle = deviceProviderSecretBundle({
          providerId: deviceProvider.providerId,
          modelId: deviceProvider.modelId,
          apiKey: null,
        })
        save()

        const created = await api.command('delivery.create', 0, {
          deliveryId,
          spec: {
            title: 'Web 到 Device Client 的真实 Agent 任务',
            goal: '只编辑 TASK.md，把唯一一行 status: pending 改为 status: complete，然后运行 npm run verify；不要修改其他文件。',
            scope: ['TASK.md'],
            constraints: ['只修改 TASK.md', '验证必须通过'],
            outOfScope: ['依赖、配置、其他文件'],
            baseRevision: baseline,
            repositoryId: 'rep_01J00000000000000000000000',
            publicationTarget: null,
            sourceProductSessionId: null,
            acceptanceCriteria: [{
              id: 'task-complete',
              required: true,
              title: 'TASK.md 精确等于 status: complete，且 npm run verify 成功',
            }],
          },
        })
        assert.equal(created.outcome, 'completed')
        const aggregate = async () => (await api.query('workrun.get', {
          deliveryId,
          workItemId: null,
          atCursor: null,
        })).result
        const itemPayload = workItemCreatePayload(await aggregate(), created.currentRevision)
        itemPayload.items[0].title = '完成 Device Client 本地任务'
        itemPayload.items[0].goal = '只编辑 TASK.md，把 status: pending 改为 status: complete，并运行 npm run verify。'
        const items = await api.command('workitems.create', created.currentRevision, itemPayload)
        assert.equal(items.outcome, 'completed')
        const started = await api.command('workrun.start', items.currentRevision, {
          deliveryId,
          dispatchProfile: 'executor',
        })
        assert.equal(started.outcome, 'completed')
        const workRunId = started.result.activeWorkRunId
        assert.ok(workRunId, 'workrun.start must return the active WorkRun')
        report.workRunId = workRunId
        report.steps.push('web.workrun.started')
        save()

        const launched = await devicePath.launchAnchor({ workRunId })
        report.workerSessionId = launched.workerSessionId
        report.steps.push('client.worker.launched')
        save()

        const deadline = Date.now() + 600_000
        let terminal = null
        while (Date.now() < deadline) {
          const current = (await aggregate()).runs.find(item => item.id === workRunId)
          report.workRunState = current?.state ?? null
          save()
          if (['candidate_ready', 'settled', 'failed', 'cancelled'].includes(current?.state)) {
            terminal = current
            break
          }
          await new Promise(resolvePromise => setTimeout(resolvePromise, 500))
        }
        assert.ok(terminal, 'Agent WorkRun did not reach a terminal state')
        assert.ok(
          ['candidate_ready', 'settled'].includes(terminal.state),
          `Agent failed in state ${terminal.state}`,
        )
        const detail = (await api.query('delivery.get', { deliveryId })).result
        assert.ok(detail.currentCandidate, 'completed Agent task must freeze a candidate')
        report.candidateRef = detail.currentCandidate.candidateRef
        report.complete = true
        report.steps.push('agent.task.completed')
        save()
        return { ...report, providerRoute: modelRoute }
      },
    },
  })
  report.providerRoute = vertical.flow.scenario.providerRoute
  report.devicePath = vertical.devicePath
  save()
  console.log(JSON.stringify(report, null, 2))
} catch (error) {
  report.error = String(error instanceof Error ? error.message : error)
  for (const secret of deviceSecrets) {
    if (secret) report.error = report.error.replaceAll(secret, '<redacted>')
  }
  save()
  console.error(JSON.stringify(report, null, 2))
  process.exitCode = 1
}
