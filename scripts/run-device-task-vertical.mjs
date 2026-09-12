#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { randomBytes, X509Certificate } from 'node:crypto'
import { chmodSync, mkdirSync, openSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

import { runApiProductionVertical, workItemCreatePayload } from './run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const directory = resolve(
  process.env.WWC_DEVICE_TASK_RESULT_DIRECTORY
    ?? join('test-results', 'device-task', new Date().toISOString().replaceAll(':', '-')),
)
const deviceData = join(directory, 'device-data')
const target = join(root, 'target', 'debug')
const wwc = join(target, 'wwc')
const deliveryId = 'dlv_01J00000000000000000000001'
const schemaVersion = 'winwincode/v1'
const ownerUserId = 'usr_01J00000000000000000000000'
const report = { complete: false, directory, steps: [] }
const save = () => writeFileSync(join(directory, 'device-task-result.json'), `${JSON.stringify(report, null, 2)}\n`)

mkdirSync(directory, { recursive: true, mode: 0o700 })
const fleetCredential = join(directory, 'unused-fleet-worker.credential')
writeFileSync(fleetCredential, randomBytes(32).toString('base64url'), { mode: 0o600 })
chmodSync(fleetCredential, 0o600)

function checkedWwc(args, environment = process.env) {
  const result = spawnSync(wwc, args, {
    cwd: root,
    encoding: 'utf8',
    env: environment,
    stdio: 'pipe',
  })
  assert.equal(result.status, 0, `${args.join(' ')} failed: ${result.stderr || result.stdout}`)
  return result.stdout.trim().length === 0 ? null : JSON.parse(result.stdout)
}

async function waitFor(check, label, timeoutMillis = 30_000) {
  const deadline = Date.now() + timeoutMillis
  for (;;) {
    const value = await check()
    if (value) return value
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`)
    await new Promise(resolvePromise => setTimeout(resolvePromise, 200))
  }
}

function stopProcessGroup(child) {
  if (child.exitCode !== null || child.signalCode !== null) return
  try {
    process.kill(-child.pid, 'SIGTERM')
  } catch {
    child.kill('SIGTERM')
  }
}

const useLoopback = process.env.WWC_DEVICE_TASK_LOOPBACK === '1'
for (const name of useLoopback ? [] : ['ZHIPU_API_KEY', 'ZHIPU_BASE_URL', 'ZHIPU_MODEL']) {
  assert.ok(process.env[name], `${name} is required`)
}
const providerEnvironment = useLoopback
  ? {}
  : {
      WWC_SERVER_MODEL_PROVIDER_ID: 'zhipu-glm',
      WWC_SERVER_MODEL_ID: process.env.ZHIPU_MODEL,
      WWC_SERVER_MODEL_ANTHROPIC_ENDPOINT: `${process.env.ZHIPU_BASE_URL.replace(/\/$/u, '')}/v1/messages`,
      WWC_SERVER_MODEL_API_KEY: process.env.ZHIPU_API_KEY,
    }

try {
  const vertical = await runApiProductionVertical({
    directory,
    restart: false,
    repeat: false,
    serverEnvironment: {
      ...providerEnvironment,
      WWC_SERVER_WORKER_MODE: 'remote',
      WWC_SERVER_WORKER_ID: 'wrk_00000000000000000000000001',
      WWC_SERVER_WORKER_INSTANCE_ID: 'wki_00000000000000000000000001',
      WWC_SERVER_WORKER_POOL_ID: 'wpl_00000000000000000000000001',
      WWC_SERVER_REMOTE_WORKER_CREDENTIAL_FILE: fleetCredential,
      WWC_SERVER_REMOTE_WORKER_EXPIRES_AT: '2099-01-01T00:00:00Z',
      WWC_SERVER_EXECUTION_LEASE_SECONDS: '600',
      WWC_DEBUG_RUNTIME: '1',
      WWC_DEBUG_RUNTIME_LOG: join(directory, 'server-runtime.log'),
    },
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
      async run({ api, repository, baseline, modelRoute }) {
        const tlsRoot = join(directory, 'device-server-root.der')
        writeFileSync(tlsRoot, new X509Certificate(readFileSync(join(directory, 'fixture-cert.pem'))).raw)
        const deviceOutput = []
        const deviceLog = openSync(join(directory, 'device-process.log'), 'a', 0o600)
        const deviceEnvironment = {
          ...process.env,
          WWC_DEVICE_TLS_ROOT_DER_FILE: tlsRoot,
          WWC_WORKER_TLS_ROOT_DER_FILE: tlsRoot,
          WWC_WORKER_HELPER_RELEASE_MANIFEST: join(target, 'winwincode-kernel-helper.release.json'),
          WWC_WORKER_HELPER_EXECUTABLE: join(target, 'winwincode-kernel-helper'),
          WWC_WORKER_MODEL_PROVIDER_ID: modelRoute.providerId,
          WWC_WORKER_MODEL_ID: modelRoute.modelId,
          WWC_WORKER_ACTION_SIGNING_KEY_HEX: '1f'.repeat(32),
          WWC_WORKER_EXECUTION_ENVELOPE_DIGEST: `sha256:${'a'.repeat(64)}`,
          WWC_DEBUG_REMOTE_WORKER: '1',
          GIT_CONFIG_NOSYSTEM: '1',
        }
        const device = spawn(wwc, [
          'device', 'serve',
          '--data-dir', deviceData,
          '--server-url', api.baseUrl,
          '--server-name', 'Device task Server',
          '--device-name', 'Device task Client',
        ], {
          cwd: root,
          detached: true,
          env: deviceEnvironment,
          stdio: ['ignore', deviceLog, deviceLog],
        })
        device.on('exit', (code, signal) => deviceOutput.push(`exit=${code} signal=${signal}`))
        try {
          const status = await waitFor(() => {
            if (device.exitCode !== null) throw new Error(`Device Client exited: ${deviceOutput.join('; ')}`)
            try {
              const value = checkedWwc(['device', 'status', '--data-dir', deviceData, '--json'])
              return value?.device?.enrolled ? value.device : false
            } catch {
              return false
            }
          }, 'Device Client enrollment')
          report.clientId = status.publicClientId
          report.steps.push('device.enrolled')
          save()

          const refreshed = checkedWwc([
            'device', 'refresh-code', '--data-dir', deviceData, '--json',
          ])
          await new Promise(resolvePromise => setTimeout(resolvePromise, 1_000))
          const connected = await api.request('/api/v1/clients/connections', {
            method: 'POST',
            body: {
              schemaVersion,
              clientId: status.publicClientId,
              connectionCode: refreshed.connectCode,
            },
          })
          assert.equal(connected.status, 201, `Client connect failed: ${connected.text}`)
          report.steps.push('web.client.connected')

          const occupied = await api.request('/api/v1/clients/occupancy', {
            method: 'POST',
            body: { schemaVersion, clientId: status.publicClientId },
          })
          assert.equal(occupied.status, 201, `Client occupancy failed: ${occupied.text}`)
          report.steps.push('web.client.occupied')

          const registered = checkedWwc([
            'repo', 'add', repository, '--data-dir', deviceData, '--json',
          ])
          const repositoryBindingId = registered.repository.repositoryBindingId
          await waitFor(async () => {
            const response = await api.request(`/api/v1/repositories?clientId=${status.publicClientId}`)
            return response.status === 200
              && response.json?.repositories?.some(item => item.repositoryBindingId === repositoryBindingId)
          }, 'Repository grant and projection')
          report.repositoryBindingId = repositoryBindingId
          report.steps.push('client.repository.available')

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

          const launched = await api.request('/api/v1/sessions', {
            method: 'POST',
            timeoutMillis: 30_000,
            body: {
              schemaVersion,
              clientId: status.publicClientId,
              repositoryBindingId,
              workRunId,
            },
          })
          assert.equal(launched.status, 201, `Worker launch failed: ${launched.text}`)
          report.workerSessionId = launched.json.workerSessionId
          report.workerId = launched.json.workerId
          report.workerInstanceId = launched.json.workerInstanceId
          report.steps.push('client.worker.launched')
          save()

          const terminal = await waitFor(async () => {
            const current = (await aggregate()).runs.find(item => item.id === workRunId)
            report.workRunState = current?.state ?? null
            save()
            return ['candidate_ready', 'settled', 'failed', 'cancelled'].includes(current?.state)
              ? current
              : false
          }, 'Agent WorkRun terminal state', 600_000)
          assert.ok(['candidate_ready', 'settled'].includes(terminal.state), `Agent failed in state ${terminal.state}`)
          const detail = (await api.query('delivery.get', { deliveryId })).result
          assert.ok(detail.currentCandidate, 'completed Agent task must freeze a candidate')
          report.candidateRef = detail.currentCandidate.candidateRef
          report.complete = true
          report.steps.push('agent.task.completed')
          save()

          const released = await api.request('/api/v1/clients/occupancy', {
            method: 'DELETE',
            body: {
              schemaVersion,
              clientId: status.publicClientId,
              mode: 'cancel_and_release',
              confirm: true,
            },
          })
          assert.equal(released.status, 200, `Client release failed: ${released.text}`)
          return { ...report, providerRoute: modelRoute }
        } finally {
          stopProcessGroup(device)
        }
      },
    },
  })
  report.providerRoute = vertical.flow.scenario.providerRoute
  save()
  console.log(JSON.stringify(report, null, 2))
} catch (error) {
  report.error = String(error instanceof Error ? error.message : error)
  if (process.env.ZHIPU_API_KEY) {
    report.error = report.error.replaceAll(process.env.ZHIPU_API_KEY, '<redacted>')
  }
  save()
  console.error(JSON.stringify(report, null, 2))
  process.exitCode = 1
}
