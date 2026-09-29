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
import { existsSync, lstatSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { pathToFileURL } from 'node:url'
import { DatabaseSync } from 'node:sqlite'
import { parseEnv } from 'node:util'
import { loadDeviceAgentTask } from './device-agent-task.mjs'
import { exportDeviceCandidate, exportDeviceExecutionReceipts } from './export-device-candidate.mjs'
import { installDevicePublicSmoke, resolveDeviceTaskApprovals, seedDeviceLocalProvider } from './device-production-fixture.mjs'
import { BenchmarkError, validateBenchmarkConfiguration } from './run-real-task-benchmark.mjs'

import {
  runApiProductionVertical,
  inspectRegisteredDeviceTask,
  workItemCreatePayload,
  deviceOnlyServerEnvironment,
  deviceProviderSecretBundle,
  driveDelivery,
  assertServerEnvironmentIsDeviceOnly,
  serverTargetDirectory,
} from './run-api-production-vertical.mjs'

const providerSeats = {
  glm: { prefix: 'ZHIPU', providerId: 'zhipu-glm', protocol: 'anthropic_messages' },
  mimo: { prefix: 'XIAOMI', providerId: 'xiaomi-mimo', protocol: 'anthropic_messages' },
  deepseek: { prefix: 'DEEPSEEK', providerId: 'deepseek', protocol: 'anthropic_messages' },
  qwen: { prefix: 'OPENCODE', providerId: 'opencode', protocol: 'openai_chat_completions' },
}
const fusionSeats = [
  ['glm', 'fusion-glm', 'glm-5.3-flash'],
  ['mimo', 'fusion-mimo', 'mimo-v2.6-pro'],
  ['deepseek', 'fusion-deepseek', 'deepseek-flash'],
  ['qwen', 'fusion-qwen', 'qwen3.8-flash'],
]

export function loadDeviceProviderEnvironment({ envFile = resolve(import.meta.dirname, '..', '.env'),
  environment = process.env } = {}) {
  let fileEnvironment = {}
  if (envFile !== null) {
    try {
      const file = lstatSync(envFile)
      if (!file.isFile() || file.isSymbolicLink() || (file.mode & 0o077) !== 0) {
        throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE',
          'Device provider environment file must be a regular owner-only file')
      }
      fileEnvironment = parseEnv(readFileSync(envFile, 'utf8'))
    } catch (error) {
      if (error.code === 'ENOENT') fileEnvironment = {}
      else if (error instanceof BenchmarkError) throw error
      else throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE',
        'Private Device provider environment file is unavailable')
    }
  }
  const resolvedEnvironment = { ...fileEnvironment, ...environment }
  for (const [provider, , model] of fusionSeats) {
    const modelVariable = `${providerSeats[provider].prefix}_MODEL`
    if (fileEnvironment[modelVariable] === model) resolvedEnvironment[modelVariable] = model
  }
  return resolvedEnvironment
}

function fusionConfigurationIssues(environment) {
  const missing = []
  const mismatched = []
  for (const [provider, , model] of fusionSeats) {
    const prefix = providerSeats[provider].prefix
    for (const suffix of ['API_KEY', 'BASE_URL', 'MODEL']) {
      if (!environment[`${prefix}_${suffix}`]) missing.push(`${prefix}_${suffix}`)
    }
    if (environment[`${prefix}_MODEL`] && environment[`${prefix}_MODEL`] !== model) {
      mismatched.push(`${prefix}_MODEL must equal ${model}`)
    }
  }
  return { missing, mismatched }
}

export function deviceTaskProvider(providerName, environment = process.env) {
  const seat = providerSeats[providerName]
  assert.ok(seat, 'Provider must be glm, mimo, deepseek, or qwen')
  for (const suffix of ['API_KEY', 'BASE_URL', 'MODEL']) {
    assert.ok(environment[`${seat.prefix}_${suffix}`], `${seat.prefix}_${suffix} is required for a real Provider`)
  }
  const customHeaders = providerName === 'qwen' && environment.OPENCODE_SESSION_VALUE
    ? { [environment.OPENCODE_SESSION_HEADER ?? 'x-opencode-session']: environment.OPENCODE_SESSION_VALUE }
    : undefined
  return {
    apiKey: environment[`${seat.prefix}_API_KEY`],
    providerId: seat.providerId,
    modelId: environment[`${seat.prefix}_MODEL`],
    protocol: seat.protocol,
    endpoint: seat.protocol === 'openai_chat_completions'
      ? environment[`${seat.prefix}_BASE_URL`]
      : `${environment[`${seat.prefix}_BASE_URL`].replace(/\/$/u, '')}/v1/messages`,
    displayName: `${providerName} live agent-loop smoke`,
    ...(customHeaders === undefined ? {} : { customHeaders }),
  }
}

export function fusionDeviceProviders(profile, environment = process.env) {
  assert.ok(Array.isArray(profile.members) && profile.members.length === 4, 'Fusion trial requires four independent members')
  const providers = profile.members.map(member => {
    const name = Object.keys(providerSeats).find(name => providerSeats[name].providerId === member.provider)
    const provider = deviceTaskProvider(name, environment)
    assert.equal(provider.modelId, member.model, 'Fusion member must match the configured Device model')
    assert.equal(member.reasoning, 'max', 'Fusion trial requires max reasoning')
    return provider
  })
  assert.equal(new Set(providers.map(provider => provider.providerId)).size, 4, 'Fusion trial requires four distinct providers')
  return providers
}

export function benchmarkDeviceEnvironment(configuration, settings = {}, environment = process.env) {
  validateBenchmarkConfiguration(configuration)
  const result = { PYTHONDONTWRITEBYTECODE: '1',
    WWC_WORKER_MODEL_REASONING_EFFORT: 'max', WWC_BENCHMARK_TOOL_REPEAT_GUARD: '1',
    WWC_WORKER_FUSION: undefined, WWC_WORKER_JEV_CONTEXT: undefined,
    WWC_WORKER_JEV_JUDGE: undefined, WWC_DEVICE_JEV_SETTINGS_FILE: undefined }
  if (configuration.fusion) {
    const issues = fusionConfigurationIssues(environment)
    const details = [
      ...(issues.missing.length ? [`missing routes: ${issues.missing.join(', ')}`] : []),
      ...(issues.mismatched.length ? [`frozen model mismatch: ${issues.mismatched.join('; ')}`] : []),
    ]
    if (details.length) {
      throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE', `Fusion ${details.join('; ')}`)
    }
    const profile = { members: fusionSeats.map(([name, id, model]) => ({
      id, provider: providerSeats[name].providerId, model, reasoning: 'max',
    })) }
    try { fusionDeviceProviders(profile, environment) } catch {
      throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE', 'Fusion requires the four frozen Device provider routes')
    }
    result.WWC_WORKER_FUSION = JSON.stringify(profile)
  }
  if (configuration.jev && (typeof settings.jevSettingsFile !== 'string' || !settings.jevSettingsFile)) {
    throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE', 'Enabled JEV requires a private Device settings file')
  }
  if (configuration.jevContext) {
    const profile = settings.jevContext
    if (!profile || Object.keys(profile).sort().join(',') !== 'policy,provider'
      || typeof profile.provider !== 'string' || !profile.provider || !profile.policy) {
      throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE', 'JEV Context requires its provider and retention policy')
    }
    result.WWC_WORKER_JEV_CONTEXT = JSON.stringify(profile)
  }
  if (configuration.jevJudge) {
    if (typeof settings.jevJudge !== 'string' || !settings.jevJudge) {
      throw new BenchmarkError('BENCHMARK_CONFIGURATION_UNAVAILABLE', 'JEV Judge requires its Device provider')
    }
    result.WWC_WORKER_JEV_JUDGE = settings.jevJudge
  }
  if (configuration.jev) result.WWC_DEVICE_JEV_SETTINGS_FILE = resolve(settings.jevSettingsFile)
  return result
}

// A WorkRun cannot submit new evidence once its execution lease expires,
// whether the Worker has exited or is still retrying a rejected frame.
export function expiredDeviceWorkRunLease(directory, workRunId, workerSessionId, now = Date.now(), jobId = null) {
  const devicePath = join(directory, 'device-data', 'device-client.sqlite3')
  const serverPath = join(directory, 'server-data', 'control-plane.sqlite3')
  if (!existsSync(devicePath) || !existsSync(serverPath)) return null
  const device = new DatabaseSync(devicePath, { readOnly: true })
  let worker
  try {
    worker = device.prepare(`SELECT worker_id, worker_instance_id, state, exit_code, last_observed_at
      FROM worker_process_registry WHERE worker_session_id = ?`).get(workerSessionId)
  } finally { device.close() }
  if (!worker) return null
  const server = new DatabaseSync(serverPath, { readOnly: true })
  let lease
  let terminal
  try {
    lease = jobId === null
      ? server.prepare(`SELECT job_id, lease_id, expires_at FROM execution_leases
        WHERE worker_id = ? AND worker_instance_id = ? ORDER BY issued_at DESC LIMIT 1`)
        .get(worker.worker_id, worker.worker_instance_id)
      : server.prepare(`SELECT job_id, lease_id, expires_at FROM execution_leases
        WHERE worker_id = ? AND worker_instance_id = ? AND job_id = ? LIMIT 1`)
        .get(worker.worker_id, worker.worker_instance_id, jobId)
    terminal = lease && server.prepare('SELECT 1 FROM execution_lease_terminals WHERE lease_id = ?')
      .get(lease.lease_id)
  } finally { server.close() }
  const expiry = Date.parse(lease?.expires_at)
  if (!lease || terminal || !Number.isFinite(expiry) || expiry >= now) return null
  return { workRunId, workerSessionId, workerState: worker.state, workerId: worker.worker_id,
    workerInstanceId: worker.worker_instance_id, exitCode: worker.exit_code,
    lastObservedAt: worker.last_observed_at, jobId: lease.job_id,
    leaseId: lease.lease_id, leaseExpiresAt: lease.expires_at }
}

export function expiredCrashedDeviceWorkRun(directory, workRunId, workerSessionId, now = Date.now()) {
  const expired = expiredDeviceWorkRunLease(directory, workRunId, workerSessionId, now)
  return expired?.workerState === 'crashed' ? expired : null
}

export async function runDeviceTaskVertical({
  directory: requestedDirectory,
  providerName: requestedProvider,
  taskInputPath,
  registerLaunch,
  callId,
  requestedModel,
  configurationId,
  track,
  fusion,
  jev,
  jevContext,
  jevJudge,
  agentSettings,
  mcpConfiguration,
  providerEnvironment = process.env,
} = {}) {
  const switches = { configurationId, track, fusion, jev, jevContext, jevJudge }
  const configuration = registerLaunch !== undefined || Object.values(switches).some(value => value !== undefined)
    ? validateBenchmarkConfiguration(switches) : null
  const deviceAgentEnvironment = configuration
    ? benchmarkDeviceEnvironment(configuration, agentSettings, providerEnvironment) : providerEnvironment
  if (configuration && agentSettings === undefined && ['WWC_WORKER_FUSION', 'WWC_WORKER_JEV_CONTEXT',
    'WWC_WORKER_JEV_JUDGE', 'WWC_DEVICE_JEV_SETTINGS_FILE'].some(key => process.env[key] !== undefined)) {
    throw new BenchmarkError('BENCHMARK_CONFIGURATION_INVALID', 'Benchmark profiles must be supplied explicitly')
  }
  const root = resolve(import.meta.dirname, '..')
  const wwcBinary = process.env.WWC_CLI_BINARY ?? resolve(serverTargetDirectory(root), 'debug/wwc')
  const directory = resolve(
    requestedDirectory ?? process.env.WWC_DEVICE_TASK_RESULT_DIRECTORY
      ?? join('test-results', 'device-task', new Date().toISOString().replaceAll(':', '-')),
  )
  const deliveryId = 'dlv_01J00000000000000000000001'
  const productSessionId = 'psn_01J00000000000000000000001'
  const report = { complete: false, directory, steps: [], execution: 'device-worker-only' }
  report.benchmarkConfiguration = configuration
  const save = () => writeFileSync(join(directory, 'device-task-result.json'), `${JSON.stringify(report, null, 2)}\n`)

  mkdirSync(directory, { recursive: true, mode: 0o700 })

  const useLoopback = process.env.WWC_DEVICE_TASK_LOOPBACK === '1'
  const useDeviceDeterministic = process.env.WWC_DEVICE_TASK_DETERMINISTIC === '1'
    || useLoopback
  const providerName = requestedProvider ?? process.env.WWC_DEVICE_TASK_PROVIDER ?? 'glm'
  const { apiKey, ...deviceProvider } = useDeviceDeterministic
    ? { providerId: 'winwincode-device-deterministic', modelId: 'device-deterministic-model' }
    : deviceTaskProvider(providerName, providerEnvironment)
  const customHeaders = deviceProvider.customHeaders
  const fusionProviders = deviceAgentEnvironment.WWC_WORKER_FUSION === undefined ? []
    : fusionDeviceProviders(JSON.parse(deviceAgentEnvironment.WWC_WORKER_FUSION), providerEnvironment)
  assert.ok(!useDeviceDeterministic || fusionProviders.length === 0, 'Fusion trial requires real providers')
  const additionalProviders = fusionProviders.filter(provider => provider.providerId !== deviceProvider.providerId)
  const deviceSecrets = useDeviceDeterministic ? [] : [apiKey,
    ...additionalProviders.flatMap(provider => [provider.apiKey, ...Object.values(provider.customHeaders ?? {})])]
  const deviceRoute = {
    providerId: deviceProvider.providerId,
    modelId: deviceProvider.modelId,
    // Filled after pairing by establishDeviceOnlyExecutionPath.
    clientNodeId: null,
  }

  const inputPath = taskInputPath ?? process.env.WWC_DEVICE_TASK_INPUT
  const input = inputPath ? loadDeviceAgentTask(inputPath) : null
  const serverEnvironment = deviceOnlyServerEnvironment({
    WWC_SERVER_WORKER_MODE: 'remote',
    WWC_DEBUG_RUNTIME: '1',
    WWC_DEBUG_RUNTIME_LOG: join(directory, 'server-runtime.log'),
    WWC_SERVER_EXECUTION_LEASE_SECONDS: '600',
    ...(input ? { WWC_SERVER_MAX_RUNTIME_SECONDS: 'unlimited' } : {}),
  })
  assertServerEnvironmentIsDeviceOnly(serverEnvironment)

  const task = input?.task ?? {
    title: 'Web 到 Device Client 的真实 Agent 任务',
    goal: '只编辑 TASK.md，把唯一一行 status: pending 改为 status: complete，然后运行 npm run verify；不要修改其他文件。',
    scope: ['TASK.md'],
    constraints: ['只修改 TASK.md', '验证必须通过'],
    outOfScope: ['依赖、配置、其他文件'],
    verificationCommand: 'npm run verify',
    acceptanceCriteria: [{ id: 'task-complete', required: true, title: 'TASK.md 精确等于 status: complete，且 npm run verify 成功' }],
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
  }
  report.taskInputDigest = input?.digest ?? null
  report.taskTitle = task.title
  report.formalBenchmark = false
  if (input) {
    assert.equal(useDeviceDeterministic, false, 'explicit task input requires an external Provider')
    assert.equal(deviceAgentEnvironment.WWC_WORKER_MODEL_REASONING_EFFORT, 'max', 'explicit tasks require max')
    assert.equal(deviceAgentEnvironment.WWC_BENCHMARK_TOOL_REPEAT_GUARD, '1', 'explicit tasks require the repeated-tool guard')
  }

  if (registerLaunch !== undefined) {
    assert.equal(typeof registerLaunch, 'function', 'registerLaunch must be a function')
    assert.ok(input, 'registered product launches require frozen task input')
    assert.equal(requestedModel, deviceProvider.modelId, 'configured model must match the benchmark request')
    assert.ok(typeof callId === 'string' && callId.length > 0, 'registered launch requires callId')
    if (!existsSync(wwcBinary)) {
      throw new BenchmarkError('DEVICE_CLI_MISSING', `Device CLI is missing: ${wwcBinary}`)
    }
    // The ledger commits this address before the first product command or model call.
    await registerLaunch({ callId, directory, productSessionId, deliveryId })
  }
  report.productSessionId = productSessionId
  report.deliveryId = deliveryId
  save()

  try {
    const vertical = await runApiProductionVertical({
      directory,
      restart: false,
      repeat: false,
      devicePrerequisites: true,
      deviceRoute,
      deviceProviderSecrets: deviceSecrets,
      deviceProvider,
      deviceAgentEnvironment,
      wwcBinary,
      serverEnvironment,
      timeoutMillis: 600_000,
      scenario: {
        files: task.files,
        async run({ api, repository, baseline, modelRoute, devicePath }) {
          assert.ok(devicePath, 'Device-only vertical must expose devicePath')
          report.publicClientId = devicePath.publicClientId
          report.repositoryBindingId = devicePath.repositoryBindingId
          report.steps.push(...devicePath.steps)
          report.modelRoute = modelRoute
          report.modelSource = useDeviceDeterministic ? 'deterministic-fixture' : 'external-provider'
          report.formalBenchmark = false
          if (mcpConfiguration || process.env.WWC_DEVICE_TASK_MCP_CONFIG) {
            assert.ok(input, 'public smoke MCP requires an explicit task input')
            report.publicSmoke = await installDevicePublicSmoke({
              api,
              publicClientId: devicePath.publicClientId,
              configuration: mcpConfiguration ?? JSON.parse(readFileSync(process.env.WWC_DEVICE_TASK_MCP_CONFIG, 'utf8')),
            })
            save()
          }
          assert.equal(devicePath.modelServer === null, !useDeviceDeterministic)
          report.providerSecretBundle = deviceProviderSecretBundle({
            providerId: deviceProvider.providerId,
            modelId: deviceProvider.modelId,
            apiKey: null,
          })
          save()

          report.fusionProviders = []
          for (const provider of additionalProviders) {
            report.fusionProviders.push(await seedDeviceLocalProvider({
              api, publicClientId: devicePath.publicClientId, ...provider,
            }))
            save()
          }
          const session = await api.command('session.create', 0, {
            productSessionId,
            projectId: 'prj_01J00000000000000000000000',
            repositoryId: 'rep_01J00000000000000000000000',
            title: task.title,
            modelRoute,
          })
          assert.equal(session.outcome, 'completed')
          await devicePath.launchAnchor({ productSessionId })
          const created = await api.command('delivery.create', 0, {
            deliveryId,
            spec: {
              title: task.title,
              goal: task.goal,
              scope: task.scope,
              constraints: task.constraints,
              outOfScope: task.outOfScope,
              baseRevision: baseline,
              repositoryId: 'rep_01J00000000000000000000000',
              publicationTarget: null,
              sourceProductSessionId: productSessionId,
              verificationCommand: task.verificationCommand,
              acceptanceCriteria: task.acceptanceCriteria,
            },
          })
          assert.equal(created.outcome, 'completed')
          const aggregate = async () => (await api.query('workrun.get', {
            deliveryId,
            workItemId: null,
            atCursor: null,
          })).result
          const itemPayload = workItemCreatePayload(await aggregate(), created.currentRevision)
          itemPayload.items[0].title = task.title
          itemPayload.items[0].goal = task.goal
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

          const anchored = new Map([[workRunId, launched.workerSessionId]])
          const delivery = await driveDelivery(api, input ? null : 600_000, modelRoute, Date.now, {
            resolveAttention: input === null ? true : 'verified-candidate',
            expectDeviceWorkRun: true,
            strictLaunchAnchor: true,
            assertRunning: devicePath.assertBenchmarkRunning,
            onProjection: ({ detail, workRunAggregate }) => {
              if (report.delivery?.detail?.readCursor?.token === detail.readCursor?.token) return
              report.delivery = { detail, workRunAggregate }
              report.candidateRef = detail.currentCandidate?.candidateRef ?? null
              save()
            },
            onActiveWorkRuns: async runs => {
              report.workRuns = runs
              save()
              for (const run of runs) {
                if (anchored.has(run.id)) continue
                const launched = await devicePath.launchAnchor({ workRunId: run.id })
                anchored.set(run.id, launched.workerSessionId)
              }
              if (input) {
                for (const run of runs) {
                  const expired = expiredDeviceWorkRunLease(directory, run.id,
                    anchored.get(run.id), Date.now(), run.executionJobId)
                  if (!expired) continue
                  report.workerLeaseExpired = expired
                  if (expired.workerState === 'crashed') report.workerCrash = expired
                  save()
                  throw new BenchmarkError(expired.workerState === 'crashed'
                    ? 'DEVICE_WORKER_CRASHED' : 'DEVICE_EXECUTION_LEASE_EXPIRED',
                  'Device WorkRun cannot finish after its execution lease expired')
                }
              }
              if (input) {
                await resolveDeviceTaskApprovals({
                  api, runs, publicSmokeId: report.publicSmoke?.id,
                  onDecision: decision => {
                    report.approvalDecisions ??= []
                    report.approvalDecisions.push(decision)
                    save()
                  },
                })
              }
            },
          })
          const detail = delivery.detail
          assert.ok(detail.currentCandidate, 'completed Agent task must freeze a candidate')
          report.delivery = delivery
          report.runtime = (await api.query('runtime.projection.get', {
            kind: 'product-session', productSessionId,
          })).result
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
    return report
  } catch (error) {
    report.error = String(error instanceof Error ? error.message : error)
    report.errorCode = error?.code ?? null
    for (const secret of [...deviceSecrets, ...Object.values(customHeaders ?? {})]) {
      if (secret) report.error = report.error.replaceAll(secret, '<redacted>')
    }
    save()
    throw Object.assign(new Error(report.error), { code: report.errorCode, report })
  } finally {
    if (input) {
      try {
        if (report.candidateRef) exportDeviceCandidate(directory, inputPath)
        else exportDeviceExecutionReceipts(directory)
      } catch {
        throw Object.assign(new Error('Candidate evidence export failed'), {
          code: 'BENCHMARK_EVIDENCE_FAILED', report,
        })
      }
    }
  }
}

export async function inspectUnresolvedDeviceTasks(ledgerPath) {
  const database = new DatabaseSync(ledgerPath, { readOnly: true })
  let launches
  try {
    launches = database.prepare(`
      SELECT c.run_id, l.target FROM benchmark_cell c
      LEFT JOIN benchmark_launch l ON l.ordinal = c.ordinal
      WHERE c.token IS NOT NULL AND c.record IS NULL
      ORDER BY c.ordinal, l.call_id
    `).all()
  } finally {
    database.close()
  }
  const observations = []
  for (const row of launches) {
    const registeredLaunch = row.target === null ? null : JSON.parse(row.target)
    try {
      observations.push({ runId: row.run_id, ...(registeredLaunch === null
        ? { registeredLaunch, observation: 'launch_not_registered' }
        : { observation: 'queried', ...await inspectRegisteredDeviceTask(registeredLaunch) }) })
    } catch (error) {
      // An unavailable original Server is unresolved, never permission to rerun.
      observations.push({ runId: row.run_id, registeredLaunch, observation: 'unavailable',
        code: typeof error.code === 'string' && /^[A-Z][A-Z0-9_]{0,127}$/u.test(error.code)
          ? error.code : 'PRODUCT_QUERY_FAILED' })
    }
  }
  return observations
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    assert.ok(process.argv.length === 2 || (process.argv.length === 4 && process.argv[2] === '--inspect-ledger'),
      'Usage: run-device-task-vertical.mjs [--inspect-ledger ledger.sqlite3]')
    console.log(JSON.stringify(process.argv[2] === '--inspect-ledger'
      ? await inspectUnresolvedDeviceTasks(resolve(process.argv[3]))
      : await runDeviceTaskVertical({ providerEnvironment: loadDeviceProviderEnvironment() }), null, 2))
  } catch (error) {
    console.error(JSON.stringify(error.report ?? { error: String(error), errorCode: error.code ?? null }, null, 2))
    process.exitCode = 1
  }
}
