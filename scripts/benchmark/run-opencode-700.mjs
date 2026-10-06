// SPDX-License-Identifier: Apache-2.0

// One explicit paid batch. Recovery uses retained receipts; startup does not
// retry a preflight request whose native outcome is unknown or unsuccessful.
import assert from 'node:assert/strict'
import { execFile, execFileSync } from 'node:child_process'
import { createHash, randomUUID } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { promisify } from 'node:util'
import { executeDeviceBenchmark } from '../lib/benchmark-device-adapter.mjs'
import { withDeviceTaskRuntime } from '../lib/device-task-runtime.mjs'
import { benchmarkConfiguration } from './run-real-task-benchmark.mjs'
import { deviceTaskProvider, loadDeviceProviderEnvironment } from '../acceptance/run-device-task-vertical.mjs'

const configPath = resolve(process.argv[2])
const config = JSON.parse(readFileSync(configPath))
assert.equal(config.concurrency, 12)
assert.equal(config.publishResults, false)
assert.deepEqual(Object.keys(config.providerOverrides).sort(), ['glm', 'qwen'])
const directory = resolve(config.evidenceRoot)
const environment = loadDeviceProviderEnvironment({ envFile: config.privateEnvironmentFile })
environment.WWC_DEVICE_TASK_OPENCODE_ROUTES = JSON.stringify(config.providerOverrides)
for (const prefix of ['ZHIPU_', 'OPENCODE_', 'FUSION_SEAT_GLM_', 'FUSION_SEAT_OPENCODE_']) {
  for (const key of Object.keys(environment)) if (key.startsWith(prefix)) delete environment[key]
}
const providers = ['glm', 'mimo', 'deepseek', 'qwen'].map(name => deviceTaskProvider(name, environment))
assert.deepEqual(providers.map(provider => provider.modelId),
  ['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'])
const retained = (name, value) => writeFileSync(join(directory, name), `${JSON.stringify(value, null, 2)}\n`,
  { flag: 'wx', mode: 0o600 })
const atomic = (name, value) => {
  const destination = join(directory, name), temporary = `${destination}.tmp-${process.pid}`
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 })
  renameSync(temporary, destination)
}
let phase = 'preflight', failure = null
function progress() {
  const database = join(directory, 'benchmark.sqlite3')
  let counts = { completed: 0, failed: 0, active: 0, pending: 0, unstarted: 700 }
  if (existsSync(database)) {
    const db = new DatabaseSync(database, { readOnly: true })
    try {
      const values = db.prepare(`SELECT count(*) AS total,
        sum(CASE WHEN record IS NOT NULL AND json_extract(record,'$.status')='completed' THEN 1 ELSE 0 END) AS completed,
        sum(CASE WHEN record IS NOT NULL AND json_extract(record,'$.status')='failed' THEN 1 ELSE 0 END) AS failed,
        sum(CASE WHEN record IS NULL AND token IS NOT NULL THEN 1 ELSE 0 END) AS active,
        sum(CASE WHEN record IS NULL AND token IS NULL THEN 1 ELSE 0 END) AS unstarted
        FROM benchmark_cell`).get()
      assert.equal(values.total, 700)
      counts = { ...counts, completed: values.completed, failed: values.failed,
        active: values.active, unstarted: values.unstarted }
      if (counts.active > 0 && phase === 'provisioning') phase = 'running'
    } finally { db.close() }
  }
  atomic('live-progress.json', { experimentId: config.experimentId, updatedAt: new Date().toISOString(),
    controllerPid: process.pid, phase, denominator: 700, terminal: counts.completed + counts.failed,
    ...counts, failure, taskConcurrency: 12, providerWorkflowConcurrency: 3, providerRequestConcurrency: 3 })
}

async function preflight() {
  const evidenceFile = join(directory, 'provider-evidence.json')
  if (existsSync(evidenceFile)) return JSON.parse(readFileSync(evidenceFile))
  const preflightDirectory = join(directory, 'preflight')
  assert.ok(!existsSync(preflightDirectory), 'preflight receipts must be inspected before recovery')
  mkdirSync(preflightDirectory, { mode: 0o700 })
  const root = resolve(import.meta.dirname, '../..')
  const probe = resolve(process.env.CARGO_TARGET_DIR, 'debug/examples/opencode_go_probe')
  const runFile = promisify(execFile)
  const observed = []
  const result = await withDeviceTaskRuntime({ directory: preflightDirectory,
    profiles: [benchmarkConfiguration('main-A')], providers,
    agentSettings: config.agentSettings, providerEnvironment: environment }, async runtime => {
    // A fixture ModelOpen tests native routing and max on a small input. It is
    // labelled separately from the 700 complete product task lifecycles.
    const store = join(preflightDirectory, 'device-data/providers')
    const results = await Promise.allSettled(providers.map(async provider => {
      const id = createHash('sha256').update(`${directory}:${provider.providerId}:${randomUUID()}`)
        .digest('hex').toUpperCase().slice(0, 26)
      const fixture = JSON.parse(readFileSync(join(root, 'tests/fixtures/contracts/execution-port.valid.json')))
      const open = structuredClone(fixture.messages.find(message => message.kind === 'model.open'))
      open.messageId = `xmsg_${id}`; open.requestId = `req_${id}`; open.modelExchangeId = `mdl_${id}`
      open.sessionIdentity.productSessionId = `psn_${id}`
      open.sentAt = new Date().toISOString()
      const request = { model: provider.modelId, instructions: 'Reply OK.',
        input: [{ type: 'message', role: 'user', content: [{ type: 'input_text', text: 'Reply OK.' }] }],
        tools: [], tool_choice: 'auto', parallel_tool_calls: true,
        reasoning: { effort: 'max' }, stream: true, store: false }
      const bytes = Buffer.from(JSON.stringify({ requestId: open.requestId, provider: provider.providerId,
        sessionId: open.sessionIdentity.productSessionId, threadId: open.sessionIdentity.codexThreadId, request }))
      open.request = { contentType: 'application/json', dataBase64: bytes.toString('base64'),
        payloadDigest: `sha256:${createHash('sha256').update(bytes).digest('hex')}` }
      const input = join(preflightDirectory, `${provider.providerId}.open.json`)
      const output = join(preflightDirectory, `${provider.providerId}.chunks.json`)
      writeFileSync(input, `${JSON.stringify(open)}\n`, { flag: 'wx', mode: 0o600 })
      await runFile(probe, ['execute-benchmark', store, input, output], { encoding: 'utf8' })
      const chunks = JSON.parse(readFileSync(output))
      const frames = chunks.filter(chunk => chunk.payload).map(chunk => JSON.parse(Buffer.from(chunk.payload.dataBase64, 'base64')))
      const models = [...new Set(frames.filter(frame => frame.type === 'server_model').map(frame => frame.model))]
      assert.deepEqual(models, [provider.modelId], 'preflight must observe the exact requested upstream model')
      assert.ok(frames.some(frame => frame.type === 'completed'))
      observed.push({ requestedModelId: provider.modelId, observedModelId: models[0], providerId: provider.providerId,
        endpoint: provider.endpoint, credentialPresent: true, supportsReasoningEffort: 'max',
        reasoningEvidence: 'native max request accepted', modelExchangeId: open.modelExchangeId,
        evidenceClass: 'native-provider-preflight-not-formal-task' })
    }))
    const failed = results.find(result => result.status === 'rejected')
    if (failed) throw failed.reason
    return { nativePreflightCalls: observed.length, formalTaskCalls: 0 }
  })
  retained('preflight-result.json', result)
  observed.sort((a, b) => providers.findIndex(provider => provider.modelId === a.requestedModelId)
    - providers.findIndex(provider => provider.modelId === b.requestedModelId))
  retained('provider-evidence.json', observed)
  return observed
}

const launch = { experimentId: config.experimentId, pid: process.pid,
  processStartedAt: execFileSync('ps', ['-p', String(process.pid), '-o', 'lstart='], { encoding: 'utf8' }).trim(),
  controller: resolve(process.argv[1]), controllerSha256: createHash('sha256').update(readFileSync(process.argv[1])).digest('hex'),
  configurationSha256: createHash('sha256').update(readFileSync(configPath)).digest('hex'),
  progressPath: join(directory, 'live-progress.json'), ledgerPath: join(directory, 'benchmark.sqlite3'),
  frozenSource: resolve(import.meta.dirname, '../..'), launchTime: new Date().toISOString() }
retained('active-controller.json', launch)
progress()
const timer = setInterval(progress, 10_000)
try {
  // Public smoke uses the compiled Web encryption module before native launch.
  // Check it before Device provisioning or claiming any formal task.
  const { encryptDeviceExtension } = await import('../../apps/client/dist/module/device-provider-encryption.js')
  assert.equal(typeof encryptDeviceExtension, 'function')
  const providerEvidence = await preflight()
  phase = 'provisioning'; progress()
  const result = await executeDeviceBenchmark({ ...config, providerEvidence, providerEnvironment: environment })
  assert.equal(result.denominator, 700)
  assert.ok(result.records.every(record => ['completed', 'failed'].includes(record.status)))
  retained('batch-result.json', result)
  phase = 'finished'; progress()
  retained('exit.json', { exitCode: 0, finishedAt: new Date().toISOString(), terminal: 700 })
} catch (error) {
  // Exception details remain private. The public progress uses bounded codes.
  writeFileSync(join(directory, `controller-error-${randomUUID()}.log`), String(error?.stack ?? error), { mode: 0o600 })
  phase = 'blocked'; failure = typeof error?.code === 'string' ? error.code : 'CONTROLLER_FAILURE'
  progress(); retained('exit.json', { exitCode: 1, finishedAt: new Date().toISOString(), failure })
  process.exitCode = 1
} finally { clearInterval(timer) }
