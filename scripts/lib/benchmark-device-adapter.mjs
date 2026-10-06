import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { existsSync, globSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs'
import { basename, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { pathToFileURL } from 'node:url'
import { benchmarkAggregationDigest, buildBenchmarkPlan, executeBenchmarkCell, executeFormalBenchmark,
  recoverBenchmarkCell, validateBenchmarkConfiguration, validateFrozenTaskSource,
  validateBenchmarkDispatchPolicy, validateFormalBenchmarkOptions, verifyBenchmarkLedgerIdentity } from '../benchmark/run-real-task-benchmark.mjs'
import { assertBenchmarkExecutionReceipts } from './benchmark-execution-receipts.mjs'
import { benchmarkDeviceEnvironment, expiredCrashedDeviceWorkRun, failedDeviceDispatch,
  loadDeviceProviderEnvironment, deviceTaskProvider,
  runDeviceTaskVertical } from '../acceptance/run-device-task-vertical.mjs'
import { loadDeviceAgentTask } from './device-agent-task.mjs'
import { apiProductionSourceDigest, assertCompletedDelivery, inspectRegisteredDeviceTask, terminalDeviceFailure,
  serverTargetDirectory, verifyApiProductionSourceSeal } from '../acceptance/run-api-production-vertical.mjs'
import { exportDeviceCandidate, exportDeviceExecutionReceipts } from '../acceptance/export-device-candidate.mjs'
import { deviceFailureWithModelCauses } from './device-model-failures.mjs'
import { assertDeviceBenchmarkRunning, removeDevicePublicSmoke } from './device-production-fixture.mjs'
import { publishBenchmarkLedger } from '../benchmark/publish-benchmark-submission.mjs'
import { deviceTaskIdentities, withDeviceTaskRuntime } from './device-task-runtime.mjs'
import { providerWorkflowLimiter } from '../benchmark/benchmark-opencode-routes.mjs'

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
const seats = { 'glm-5.3-flash': 'glm', 'mimo-v2.6-pro': 'mimo', 'deepseek-flash': 'deepseek', 'qwen3.8-flash': 'qwen' }
const aggregationProvider = 'glm-5.3-flash'
const aggregationInputPath = 'benchmark-inputs/aggregation.json'
const shellWord = value => `'${value.replaceAll("'", "'\\''")}'`
const evidenceFailure = () => Object.assign(new Error('Frozen benchmark evidence is unavailable or changed'), {
  code: 'BENCHMARK_EVIDENCE_FAILED',
})

export { terminalDeviceFailure }

export function terminalBenchmarkDeviceFailure(observation) {
  const productFailure = terminalDeviceFailure(observation)
  if (productFailure !== null) return productFailure
  const { delivery, workRunAggregate } = observation
  const runs = workRunAggregate?.runs ?? []
  const items = workRunAggregate?.items ?? []
  if (delivery?.status !== 'waiting_human'
      || !delivery.attention?.some(item => item.status === 'open' && item.blocking === true)
      || runs.length === 0 || items.length === 0
      || runs.some(run => !['settled', 'candidate_ready', 'failed', 'cancelled'].includes(run.state))
      || items.some(item => !['done', 'candidate_ready', 'failed', 'cancelled'].includes(item.state))) return null
  return { code: 'DEVICE_TASK_ATTENTION', status: 'waiting_human' }
}

function benchmarkSourceIdentity(agentSettings) {
  const hash = createHash('sha256')
  for (const path of globSync('**/*.{mjs,py}', { cwd: resolve(import.meta.dirname, '..') }).sort()) {
    hash.update(path).update('\0').update(readFileSync(resolve(import.meta.dirname, '..', path))).update('\0')
  }
  return { productSourceDigest: apiProductionSourceDigest(), runnerSourceDigest: hash.digest('hex'),
    privateSettingsDigest: agentSettings?.jevSettingsFile ? sha256(readFileSync(agentSettings.jevSettingsFile)) : null }
}

function publicSmokeExecutionLock(value) {
  if (value === undefined) return null
  assert.ok(typeof value === 'string' && value.length > 0, 'public smoke execution lock must be a host path')
  return resolve(value)
}

export function deviceBenchmarkExperimentBinding(options, source, catalog) {
  const automaticTaskActions = options.automaticTaskActions === undefined ? false : options.automaticTaskActions
  assert.equal(typeof automaticTaskActions, 'boolean', 'task action authorization must be explicit')
  const executionLock = publicSmokeExecutionLock(options.publicSmokeExecutionLock)
  return { experimentId: options.experimentId, source, aggregationProvider,
    automaticTaskActions,
    ...(executionLock === null ? {} : { publicSmokeExecutionLock: executionLock }),
    preparedCatalogSha256: sha256(JSON.stringify(catalog)),
    agentSettingsSha256: sha256(JSON.stringify(options.agentSettings)),
    providerEvidenceSha256: sha256(JSON.stringify(options.providerEvidence)),
    ...(options.providerOverrides === undefined ? {} : {
      providerOverridesSha256: sha256(JSON.stringify(options.providerOverrides)) }),
    ...options.frozenSourceIdentity, productSourceSealSha256: options.productSourceSealSha256 }
}

export function benchmarkDeviceProfiles(plan, options = {}) {
  validateBenchmarkDispatchPolicy(plan, options)
  const selected = options.selectedConfigurationIds === undefined ? null : new Set(options.selectedConfigurationIds)
  const profiles = [...new Map(plan.cells.filter(cell => selected === null || selected.has(cell.configurationId))
    .map(cell => [cell.configurationId, cell])).values()]
  // Validate the profiles being admitted before claiming work. The full frozen
  // plan stays in the ledger, including profiles awaiting real JEV settings.
  for (const profile of profiles) benchmarkDeviceEnvironment(profile,
    options.agentSettings, options.providerEnvironment ?? process.env)
  return profiles
}

export async function executeDeviceBenchmark(options) {
  const automaticTaskActions = options.automaticTaskActions === undefined ? false : options.automaticTaskActions
  assert.equal(typeof automaticTaskActions, 'boolean', 'task action authorization must be explicit')
  publicSmokeExecutionLock(options.publicSmokeExecutionLock)
  options = { ...options, automaticTaskActions }
  const catalog = JSON.parse(readFileSync(resolve(options.preparedInputsDirectory, 'prepared-inputs.json'), 'utf8'))
  const plan = buildBenchmarkPlan({ taskIds: catalog.tasks.map(task => task.taskId) })
  const providerEnvironment = { ...(options.providerEnvironment ?? process.env),
    ...(options.providerOverrides === undefined ? {} : {
      WWC_DEVICE_TASK_OPENCODE_ROUTES: JSON.stringify(options.providerOverrides) }) }
  const profiles = benchmarkDeviceProfiles(plan, { ...options, providerEnvironment })
  const ledgerPath = resolve(options.evidenceRoot, 'benchmark.sqlite3')
  // Invalid first-run configuration must not freeze an unused experiment.
  validateFormalBenchmarkOptions({ providerEvidence: options.providerEvidence, ledgerPath,
    experimentBinding: { experimentId: options.experimentId } })
  const source = await validateFrozenTaskSource({ repositoryRoot: options.sourceRoot,
    repositoryUrl: catalog.source.repositoryUrl, revision: catalog.source.revision })
  assert.equal(source.sourceDigest, catalog.source.sourceDigest)
  assert.deepEqual(plan.cells.filter(cell => cell.configurationId === 'main-A' && cell.comparison === 'glm-5.3-flash')
    .map(cell => cell.taskId), source.taskIds)
  assert.equal(process.env.WWC_API_SKIP_BUILD, '1', 'formal execution uses one prebuilt sealed product')
  const binaryDirectory = resolve(serverTargetDirectory(resolve(import.meta.dirname, '../..')), 'debug')
  const cliBinary = process.env.WWC_CLI_BINARY ?? resolve(binaryDirectory, 'wwc')
  assert.ok(existsSync(cliBinary), `formal Device CLI is missing: ${cliBinary}`)
  assert.ok(['https://github.com/changw98ic/agent-benchmark-submissions.git',
    'git@github.com:changw98ic/agent-benchmark-submissions.git'].includes(options.publicationRepositoryUrl),
  'formal benchmark requires the frozen public submission repository')
  const sourceSeal = verifyApiProductionSourceSeal({ serverBinary: resolve(binaryDirectory, 'winwincode-server'),
    helperExecutable: resolve(binaryDirectory, 'winwincode-kernel-helper') })
  options = { ...options, providerEnvironment, frozenSourceIdentity: benchmarkSourceIdentity(options.agentSettings),
    productSourceSealSha256: sha256(`${JSON.stringify(sourceSeal.seal, null, 2)}\n`) }
  const evidenceRoot = resolve(options.evidenceRoot)
  mkdirSync(evidenceRoot, { recursive: true, mode: 0o700 })
  const experimentBinding = deviceBenchmarkExperimentBinding(options, source, catalog)
  // Reject changed experiment policy before opening Device/Worker services.
  // The execution driver rechecks the same identity before claiming work.
  verifyBenchmarkLedgerIdentity(plan, { providerEvidence: options.providerEvidence, ledgerPath, experimentBinding,
    concurrency: options.concurrency, selectedConfigurationIds: options.selectedConfigurationIds })
  const shared = await withDeviceTaskRuntime({ directory: resolve(evidenceRoot, 'runtime'),
    profiles,
    providers: Object.keys(seats).map(name => deviceTaskProvider(seats[name], providerEnvironment)),
    agentSettings: options.agentSettings, providerEnvironment,
  }, async runtime => {
    const sharedOptions = { ...options, runtimeResolver: runtime.forTask }
    const limit = providerWorkflowLimiter(3)
    const adapter = {
      runModel: (request, runner) => limit(request.provider,
        () => runBenchmarkDeviceModel(request, runner, sharedOptions)),
      aggregate: (request, runner) => limit(aggregationProvider,
        () => runBenchmarkDeviceAggregation(request, runner, sharedOptions)),
    }
    return executeFormalBenchmark(plan, {
      providerEvidence: options.providerEvidence, ledgerPath,
      experimentBinding,
      concurrency: options.concurrency, selectedConfigurationIds: options.selectedConfigurationIds,
      executeCell: (cell, runner) => executeBenchmarkCell(cell, adapter, runner),
      recoverCell: (cell, observed) => recoverBenchmarkDeviceCell(cell, observed, sharedOptions),
      onRecord: record => {
        const path = resolve(evidenceRoot, `${sha256(record.runId)}.result.json`)
        const bytes = `${JSON.stringify(record, null, 2)}\n`
        try { writeFileSync(path, bytes, { flag: 'wx', mode: 0o600 }) } catch (error) {
          if (error.code !== 'EEXIST') throw error
          assert.equal(readFileSync(path, 'utf8'), bytes)
        }
      },
    })
  })
  const result = shared.flow.scenario
  // A selected cohort leaves real unclaimed rows in the full experiment. The
  // public publisher requires a completely finalized ledger, so defer it while
  // another profile is still planned instead of fabricating failure records.
  if (options.publishResults !== false && result.records.every(record => ['completed', 'failed'].includes(record.status))) {
    publishBenchmarkLedger({ ledgerPath: resolve(evidenceRoot, 'benchmark.sqlite3'),
      preparedInputsDirectory: options.preparedInputsDirectory,
      repositoryUrl: options.publicationRepositoryUrl,
      receiptDirectory: resolve(evidenceRoot, 'publication-receipts') })
  }
  return result
}

export async function prepareBenchmarkDeviceTask(request, { preparedInputsDirectory, sourceRoot, evidenceRoot,
  aggregation, productSessionId, publicSmokeExecutionLock: executionLockPath }) {
  assert.ok(typeof request.callId === 'string' && request.callId.startsWith(`${request.runId}:`))
  const catalog = JSON.parse(readFileSync(resolve(preparedInputsDirectory, 'prepared-inputs.json'), 'utf8'))
  const taskRecord = catalog.tasks.find(task => task.taskId === request.taskId)
  assert.ok(taskRecord, 'task must belong to the frozen input catalog')
  const source = await validateFrozenTaskSource({ repositoryRoot: sourceRoot,
    repositoryUrl: catalog.source.repositoryUrl, revision: catalog.source.revision })
  assert.equal(source.sourceDigest, catalog.source.sourceDigest)
  const bytes = readFileSync(resolve(preparedInputsDirectory, `${request.taskId}.json`))
  assert.equal(sha256(bytes), taskRecord.sha256, 'prepared task changed')
  const artifact = JSON.parse(bytes)
  assert.equal(artifact.taskId, request.taskId)
  assert.equal(artifact.spec.id, request.taskId)
  assert.equal(artifact.sourceRevision, source.revision)
  assert.equal(artifact.sourceDigest, source.sourceDigest)
  assert.equal(artifact.imageId, taskRecord.imageId)
  assert.match(artifact.imageId, /^sha256:[0-9a-f]{64}$/u)
  const directory = resolve(evidenceRoot, sha256(request.callId))
  const args = ['-I', resolve(import.meta.dirname, '../benchmark/benchmark-public-smoke.py'), '--task-root', resolve(sourceRoot),
    '--task-id', request.taskId, '--image-id', artifact.imageId,
    '--evidence-directory', resolve(directory, 'public-attempts')]
  const executionLock = publicSmokeExecutionLock(executionLockPath)
  if (executionLock !== null) args.push('--execution-lock', executionLock)
  const mcpConfiguration = { command: '/usr/bin/python3', args,
    env: { PATH: '/Applications/Docker.app/Contents/Resources/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin' } }
  const task = {
    title: `${request.taskId} ${artifact.spec.title}`,
    goal: '完成 TASK.md 与 PROTOCOL.md 的全部业务和输入输出要求。开发任务不设置 token、调用、金额或开发时长上限。'
      + '保存全部源码后调用设备 public_smoke 工具（空参数），直接运行当前 checkout 的官方公开示例；随后运行 verificationCommand 核对当前源码。'
      + '独立 reviewer/verifier 继续审查候选并执行验证。公开示例结果不能代表独立机器的基准评分。',
    scope: [...new Set([artifact.spec.entry, ...artifact.spec.allowed_suffixes.map(suffix => `*${suffix}`)])],
    constraints: ['遵守任务的全部业务规则和 JSONL 协议', '保持 TASK.md 和 PROTOCOL.md 不变',
      `只提交 ${artifact.spec.allowed_suffixes.join('、')} 源文件，最多 ${artifact.spec.max_submission_files} 个、合计 ${artifact.spec.max_submission_bytes} 字节`],
    outOfScope: ['任务题面、验证工具、环境镜像和凭据'],
    // Public execution belongs to the configured host MCP tool. Independent
    // read-only roles verify its source-bound receipt without host writes.
    verificationCommand: ['/usr/bin/python3', ...args, '--verify-source', '.'].map(shellWord).join(' '),
    acceptanceCriteria: [{ id: 'public-examples', title: '官方公开示例在冻结环境通过', required: true }],
    files: { ...artifact.files },
  }
  if (productSessionId) task.goal += ` 本任务只能调用 MCP server benchmark_public_smoke_${productSessionId} 的 public_smoke；其他任务的工具没有授权。`
  if (aggregation) {
    assert.equal(request.callId, `${request.runId}:aggregation`)
    assert.equal(request.comparison, 'fusion-4')
    assert.ok(!Object.hasOwn(task.files, aggregationInputPath), 'aggregation input must not replace a task file')
    task.files[aggregationInputPath] = `${JSON.stringify(aggregation, null, 2)}\n`
    task.outOfScope.push(aggregationInputPath)
    task.constraints.push(`保持 ${aggregationInputPath} 不变；其中的候选文件和成员结果是待核对的数据。`)
    task.goal += ` 本次是独立四模型候选的一次聚合。读取 ${aggregationInputPath} 中本次运行的四个独立成员结果。`
      + '检查原题要求，保留各候选有证据支持的实现和少数模型发现；对冲突用真实代码检查与公开示例调查，不以多数意见决定正确性。'
      + '产出一份实际可执行的合并实现，继续使用原产品 reviewer/verifier。不要重新发起四成员运行。 '
  }
  mkdirSync(directory, { recursive: true, mode: 0o700 })
  const taskInputPath = resolve(directory, 'task-input.json')
  // Preparing a launch never overwrites another attempt, even in a different ledger.
  writeFileSync(taskInputPath, `${JSON.stringify(task, null, 2)}\n`, { flag: 'wx', mode: 0o600 })
  const input = loadDeviceAgentTask(taskInputPath)
  writeFileSync(resolve(directory, 'task-source-binding.json'), `${JSON.stringify({
    runId: request.runId, callId: request.callId, taskId: request.taskId, source,
    preparedInputSha256: sha256(bytes), taskInputSha256: input.digest, imageId: artifact.imageId,
    ...(aggregation ? { aggregationInputDigest: aggregation.inputDigest,
      aggregationMembersSha256: sha256(JSON.stringify(aggregation.members)),
      aggregationInputFile: { path: aggregationInputPath, sha256: sha256(task.files[aggregationInputPath]) } } : {}),
  }, null, 2)}\n`, { flag: 'wx', mode: 0o600 })
  return { directory, taskInputPath, mcpConfiguration }
}

export function benchmarkAggregationInput(request) {
  assert.equal(request.engine, 'fusion-engine')
  assert.equal(request.comparison, 'fusion-4')
  assert.equal(request.callId, `${request.runId}:aggregation`)
  assert.equal(request.algorithmVersion, 'fusion-4-v1')
  assert.match(request.inputDigest, /^[0-9a-f]{64}$/u)
  assert.equal(request.members.length, 4)
  assert.equal(request.inputDigest, benchmarkAggregationDigest(request.runId, request.taskId, request.members))
  const providers = Object.keys(seats)
  let source
  let productSourceSealSha256
  const members = request.members.map((member, index) => {
    const provider = providers[index]
    const callId = `${request.runId}:member:${provider}`
    if (member.status === 'failed') {
      assert.equal(member.provider, provider)
      assert.equal(member.callId, callId)
      return { provider, callId, status: 'failed', failure: member.failure }
    }
    const manifestBytes = readFileSync(member.submissionManifest.path)
    assert.equal(sha256(manifestBytes), member.submissionManifest.sha256)
    const manifest = JSON.parse(manifestBytes)
    assert.equal(manifest.productComplete, true)
    assert.equal(manifest.modelRoute.modelId, provider)
    assert.deepEqual(manifest.configuration, validateBenchmarkConfiguration(request))
    assert.deepEqual(manifest.candidate, member.candidate)
    const bindingBytes = readFileSync(resolve(member.directory, 'task-source-binding.json'))
    assert.equal(sha256(bindingBytes), member.taskSourceBindingSha256)
    const binding = JSON.parse(bindingBytes)
    assert.match(binding.source.sourceDigest, /^[0-9a-f]{64}$/u)
    assert.match(manifest.productSourceSealSha256, /^[0-9a-f]{64}$/u)
    assert.equal(binding.runId, request.runId, 'aggregation cannot reuse a standalone candidate')
    assert.equal(binding.callId, callId)
    assert.equal(binding.taskId, request.taskId)
    assert.equal(binding.taskInputSha256, manifest.taskInputSha256)
    source ??= binding.source
    productSourceSealSha256 ??= manifest.productSourceSealSha256
    assert.deepEqual(binding.source, source, 'members must use the same frozen task source')
    assert.equal(manifest.productSourceSealSha256, productSourceSealSha256)
    const filesBytes = readFileSync(resolve(member.submissionManifest.path, '..', 'candidate-files.json'))
    assert.equal(sha256(filesBytes), manifest.candidateFilesSha256)
    const candidate = JSON.parse(filesBytes)
    assert.equal(candidate.commit, manifest.candidate.candidateCommitId)
    assert.equal(candidate.tree, manifest.candidate.candidateTreeId)
    return { provider, callId, status: member.status, candidate }
  })
  if (!members.some(member => member.candidate)) {
    throw Object.assign(new Error('All four independent members failed'), { code: 'FUSION_NO_SUCCESSFUL_MEMBERS' })
  }
  return { inputDigest: request.inputDigest, members }
}

export async function runBenchmarkDeviceAggregation(request, runner, options) {
  let aggregation
  try { aggregation = benchmarkAggregationInput(request) } catch (error) {
    if (error.code === 'FUSION_NO_SUCCESSFUL_MEMBERS') throw error
    throw evidenceFailure()
  }
  // The aggregation is one product execution using an existing model seat,
  // with separate receipts; it never calls executeBenchmarkCell recursively.
  const result = await runBenchmarkDeviceModel({ ...request, provider: aggregationProvider,
    reasoningEffort: 'max', budgetLimits: null }, runner, { ...options, aggregation })
  return { ...result, aggregationInputDigest: request.inputDigest, aggregationProvider }
}

export async function runBenchmarkDeviceModel(request, runner, options) {
  assert.ok(Object.hasOwn(seats, request.provider), 'benchmark requires one of the four exact models')
  assert.equal(request.reasoningEffort, 'max')
  assert.equal(request.budgetLimits, null)
  if (options.frozenSourceIdentity) {
    try { assert.deepEqual(benchmarkSourceIdentity(options.agentSettings), options.frozenSourceIdentity) } catch {
      throw evidenceFailure()
    }
  }
  benchmarkDeviceEnvironment(request, options.agentSettings, options.providerEnvironment ?? process.env)
  const identities = options.runtimeResolver
    ? deviceTaskIdentities(options.experimentId, request.callId) : {}
  let prepared
  try { prepared = await prepareBenchmarkDeviceTask(request, { ...options, ...identities }) } catch { throw evidenceFailure() }
  const runtime = options.runtimeResolver ? await options.runtimeResolver(request, prepared) : null
  const finalizeFailure = async result => {
    if (runtime && result.status === 'failed') {
      try {
        const removal = await removeTerminalBenchmarkPublicSmoke(registeredLaunch, runtime)
        if (removal) result.publicSmokeRemoval = removal
      } catch { result.publicSmokeRemoval = { outcome: 'unknown' } }
    }
    return result
  }
  let registeredLaunch = null
  let report
  try {
    report = await runDeviceTaskVertical({ ...request, ...prepared, ...identities, runtime,
      providerName: seats[request.provider], requestedModel: request.provider,
      agentSettings: options.agentSettings,
      automaticTaskActions: options.automaticTaskActions ?? false,
      registerLaunch: async target => {
        await runner.registerLaunch(target)
        registeredLaunch = target
      },
      providerEnvironment: options.providerEnvironment ?? process.env })
  } catch (error) {
    if (!registeredLaunch) throw error
    if (error?.code === 'STUCK_TOOL_REPEAT_LIMIT') {
      try { return await finalizeFailure(stoppedDeviceResult(request, registeredLaunch, error.report)) }
      catch { throw evidenceFailure() }
    }
    if (error?.code === 'DEVICE_WORKER_CRASHED') {
      try { return await finalizeFailure(crashedDeviceResult(request, registeredLaunch, options, error.report)) }
      catch { throw evidenceFailure() }
    }
    if (['DEVICE_DISPATCH_FAILED', 'DEVICE_EXECUTION_FAILED'].includes(error?.code)) {
      try { return await finalizeFailure(failedDispatchDeviceResult(request, registeredLaunch, options, error.report)) }
      catch { throw evidenceFailure() }
    }
    try {
      const reportBytes = readFileSync(resolve(registeredLaunch.directory, 'device-task-result.json'))
      const report = JSON.parse(reportBytes)
      assert.deepEqual(error.report, report, 'thrown product result must match its persisted projection')
      assert.equal(report.productSessionId, registeredLaunch.productSessionId)
      assert.equal(report.deliveryId, registeredLaunch.deliveryId)
      if (!report.delivery?.detail || !report.delivery?.workRunAggregate) {
        throw Object.assign(new Error('Device execution has no authoritative terminal outcome'), {
          code: /^DEVICE_[A-Z0-9_]{1,100}$/.test(report.errorCode ?? '')
            ? report.errorCode : 'DEVICE_EXECUTION_UNRESOLVED',
          unresolvedDeviceExecution: true,
        })
      }
      const observation = persistedDeviceObservation(report, registeredLaunch)
      assert.ok(terminalBenchmarkDeviceFailure(observation), 'only a persisted terminal product result can be finalized')
      return await finalizeFailure(await resolveRegisteredDeviceTask(request, registeredLaunch, options, {
        report, reportBytes, observation,
      }))
    } catch (unresolved) {
      if (unresolved.unresolvedDeviceExecution === true) throw unresolved
      // The product call has been launched, so uncertain state must stay
      // unresolved in the durable ledger rather than be retried as a task.
      throw evidenceFailure()
    }
  }
  try {
    const commit = report.delivery.detail.currentCandidate.candidateCommitId
    const result = deviceResult(prepared.directory, prepared.directory, commit)
    if (options.productSourceSealSha256) assert.equal(result.productSourceSealSha256, options.productSourceSealSha256)
    return result
  } catch { throw evidenceFailure() }
}

// A failed call can still have a running role or an unresolved projection. Only
// current, matching terminal product facts allow removing its own MCP server.
export async function removeTerminalBenchmarkPublicSmoke(launch, runtime) {
  const report = JSON.parse(readFileSync(resolve(launch.directory, 'device-task-result.json'), 'utf8'))
  assert.equal(report.productSessionId, launch.productSessionId)
  assert.equal(report.deliveryId, launch.deliveryId)
  if (!report.publicSmoke || report.publicSmokeRemoval?.outcome === 'deleted') return null
  const id = `benchmark_public_smoke_${launch.productSessionId}`
  assert.equal(report.publicSmoke.id, id)
  assert.equal(report.publicClientId, runtime.devicePath.publicClientId)
  const delivery = (await runtime.api.query('delivery.get', { deliveryId: launch.deliveryId })).result
  assert.equal(delivery.deliveryId, launch.deliveryId)
  assert.equal(delivery.readCursor.deliveryId, launch.deliveryId)
  const workRunAggregate = (await runtime.api.query('workrun.get', {
    deliveryId: launch.deliveryId, workItemId: null, atCursor: delivery.readCursor,
  })).result
  assert.deepEqual(workRunAggregate.readCursor, delivery.readCursor)
  if (!terminalBenchmarkDeviceFailure({ delivery, workRunAggregate })) return null
  const path = resolve(launch.directory, 'public-smoke-removal.json')
  if (existsSync(path)) {
    const retained = JSON.parse(readFileSync(path, 'utf8'))
    assert.equal(retained.id, id)
    assert.equal(retained.publicClientId, report.publicClientId)
    assert.equal(retained.receipt.outcome, 'deleted')
    return retained.receipt
  }
  const receipt = await removeDevicePublicSmoke({ api: runtime.api,
    publicClientId: report.publicClientId, id })
  writeFileSync(path, `${JSON.stringify({ id, publicClientId: report.publicClientId,
    readCursor: delivery.readCursor, receipt }, null, 2)}\n`, { flag: 'wx', mode: 0o600 })
  return receipt
}

function deviceResult(directory, evidenceDirectory, commit) {
  const manifestPath = resolve(evidenceDirectory, 'submission-evidence', commit, 'manifest.json')
  const manifestBytes = readFileSync(manifestPath)
  const manifest = JSON.parse(manifestBytes)
  const executionReceipts = assertBenchmarkExecutionReceipts(
    JSON.parse(readFileSync(resolve(evidenceDirectory, 'execution-receipts.json'), 'utf8')))
  return { status: 'completed', directory, candidate: manifest.candidate,
    productSourceSealSha256: manifest.productSourceSealSha256,
    submissionManifest: { path: manifestPath, sha256: sha256(manifestBytes) },
    executionReceipts,
    taskSourceBindingSha256: sha256(readFileSync(resolve(directory, 'task-source-binding.json'))),
    externalVerdict: null, externalScore: null }
}

// Shared recovery reads all physical directories but checks only this launch's
// source Session and its Controller-created role Sessions. A neighbor's stop
// cannot become this task's outcome when its projection has not arrived yet.
export function assertRetainedDeviceTaskRunning(launch, runs = []) {
  const deviceData = realpathSync(resolve(launch.directory, 'device-data'))
  const workers = globSync('worker-sessions/*', { cwd: deviceData }).map(path => basename(path))
  if (!existsSync(resolve(launch.directory, 'runtime-binding.json'))) {
    assertDeviceBenchmarkRunning(deviceData, workers, runs)
    return workers
  }
  const sessions = new Set([launch.productSessionId])
  const database = new DatabaseSync(resolve(launch.directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
  try {
    database.exec('PRAGMA busy_timeout = 5000')
    for (const row of database.prepare(`SELECT work_run_id, dispatch_payload FROM scheduler_execution_jobs
      WHERE delivery_id = ?`).all(launch.deliveryId)) {
      const job = JSON.parse(Buffer.from(row.dispatch_payload).toString('utf8'))
      assert.equal(job.scope.kind, 'work-run')
      assert.equal(job.scope.workRunId, row.work_run_id)
      assert.match(job.scope.productSessionId, /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
      sessions.add(job.scope.productSessionId)
    }
  } finally { database.close() }
  for (const session of sessions) assertDeviceBenchmarkRunning(deviceData, workers, runs, session)
  return workers
}

export function stoppedDeviceResult(request, launch, expectedReport, { recovering = false } = {}) {
  const reportBytes = readFileSync(resolve(launch.directory, 'device-task-result.json'))
  const report = JSON.parse(reportBytes)
  if (expectedReport !== null) assert.deepEqual(report, expectedReport, 'Core stop must match the persisted product report')
  assert.ok(recovering ? report.errorCode === undefined || report.errorCode === 'STUCK_TOOL_REPEAT_LIMIT'
    : report.errorCode === 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(report.productSessionId, launch.productSessionId)
  assert.equal(report.deliveryId, launch.deliveryId)
  assert.equal(report.complete, false)
  const runs = report.delivery?.workRunAggregate?.runs ?? []
  const workerSessionIds = recovering
    ? globSync('worker-sessions/*', { cwd: realpathSync(resolve(launch.directory, 'device-data')) })
      .map(path => basename(path))
    : runs.map(run => run.workerSessionId).filter(Boolean)
  let stop = null
  try {
    if (recovering) assertRetainedDeviceTaskRunning(launch, runs)
    else assertDeviceBenchmarkRunning(resolve(launch.directory, 'device-data'), workerSessionIds, runs)
  }
  catch (error) { stop = error }
  assert.equal(stop?.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.ok(workerSessionIds.includes(stop.logicalWorkerSessionId ?? stop.workerSessionId)
    || runs.some(run => run.workerSessionId === stop.logicalWorkerSessionId))
  const evidenceDirectory = recovering
    ? resolve(launch.directory, 'recovery', `core-stop-${sha256(reportBytes)}`) : launch.directory
  if (recovering) mkdirSync(evidenceDirectory, { recursive: true, mode: 0o700 })
  const executionReceipts = recovering
    ? exportDeviceExecutionReceipts(launch.directory, evidenceDirectory).evidence
    : JSON.parse(readFileSync(resolve(launch.directory, 'execution-receipts.json')))
  assert.ok(Array.isArray(executionReceipts.calls), 'Core stop has no model receipt ledger')
  const binding = JSON.parse(readFileSync(resolve(launch.directory, 'task-source-binding.json')))
  assert.equal(binding.runId, request.runId)
  assert.equal(binding.callId, request.callId)
  assert.equal(binding.taskId, request.taskId)
  return { status: 'failed', failure: { code: 'STUCK_TOOL_REPEAT_LIMIT' },
    termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' }, productComplete: false,
    provider: request.provider, callId: request.callId, directory: launch.directory,
    executionReceipts, delivery: report.delivery,
    stopProof: { workerSessionId: stop.workerSessionId, runKey: stop.runKey,
      ...(stop.logicalWorkerSessionId ? { logicalWorkerSessionId: stop.logicalWorkerSessionId } : {}),
      productReportSha256: sha256(reportBytes) },
    ...(recovering ? { recovery: { kind: 'retained-core-stop' } } : {}),
    externalVerdict: null, externalScore: null }
}

function persistedDeviceObservation(report, launch) {
  const delivery = report.delivery?.detail
  const workRunAggregate = report.delivery?.workRunAggregate
  assert.equal(report.productSessionId, launch.productSessionId)
  assert.equal(report.deliveryId, launch.deliveryId)
  assert.equal(delivery?.deliveryId, launch.deliveryId)
  assert.equal(delivery?.readCursor?.deliveryId, launch.deliveryId)
  assert.deepEqual(workRunAggregate?.readCursor, delivery.readCursor,
    'persisted Delivery and WorkRun states must describe the same read cursor')
  return { registeredLaunch: launch, delivery, workRunAggregate,
    productSession: { id: report.productSessionId } }
}

function crashedDeviceResult(request, launch, options, expectedReport = null) {
  const directory = launch.directory
  const reportBytes = readFileSync(resolve(directory, 'device-task-result.json'))
  const report = JSON.parse(reportBytes)
  if (expectedReport !== null) assert.deepEqual(report, expectedReport)
  assert.equal(report.complete, false)
  assert.equal(report.productSessionId, launch.productSessionId)
  assert.equal(report.deliveryId, launch.deliveryId)
  assert.equal(report.errorCode, 'DEVICE_WORKER_CRASHED')
  const crash = report.workerCrash
  assert.ok(crash && report.workRuns?.some(run => run.id === crash.workRunId
    && ['queued', 'leased', 'running'].includes(run.state)))
  assert.deepEqual(expiredCrashedDeviceWorkRun(directory, crash.workRunId, crash.workerSessionId), crash)
  const binding = JSON.parse(readFileSync(resolve(directory, 'task-source-binding.json'), 'utf8'))
  assert.equal(binding.runId, request.runId)
  assert.equal(binding.callId, request.callId)
  const sealSha256 = sha256(readFileSync(resolve(directory, 'product-source-seal.json')))
  if (options.productSourceSealSha256) assert.equal(sealSha256, options.productSourceSealSha256)
  const receipts = exportDeviceExecutionReceipts(directory)
  return { status: 'failed', failure: { code: 'DEVICE_WORKER_CRASHED', status: 'failed' },
    provider: request.provider ?? aggregationProvider, callId: request.callId, directory,
    productComplete: false, candidate: null, submissionManifest: null,
    executionReceipts: receipts.evidence, productSourceSealSha256: sealSha256,
    taskSourceBindingSha256: sha256(readFileSync(resolve(directory, 'task-source-binding.json'))),
    workerCrash: crash, productReportSha256: sha256(reportBytes),
    externalVerdict: null, externalScore: null }
}

export function failedDispatchDeviceResult(request, launch, options, expectedReport = null,
  { recovering = false } = {}) {
  const directory = launch.directory
  const reportBytes = readFileSync(resolve(directory, 'device-task-result.json'))
  const report = JSON.parse(reportBytes)
  if (expectedReport !== null) assert.deepEqual(report, expectedReport)
  assert.equal(report.complete, false)
  assert.equal(report.productSessionId, launch.productSessionId)
  assert.equal(report.deliveryId, launch.deliveryId)
  const workRunIds = [...new Set([report.workRunId,
    ...(report.workRuns ?? []).map(run => run.id)].filter(Boolean))]
  const dispatchFailure = workRunIds.map(id => failedDeviceDispatch(directory, id, launch.deliveryId))
    .find(Boolean)
  assert.ok(dispatchFailure, 'no retained terminal dispatch for the registered task')
  if (!recovering) assert.deepEqual(dispatchFailure, report.dispatchFailure)
  else if (report.dispatchFailure) assert.deepEqual(dispatchFailure, report.dispatchFailure)
  const bindingBytes = readFileSync(resolve(directory, 'task-source-binding.json'))
  const binding = JSON.parse(bindingBytes)
  assert.equal(binding.runId, request.runId)
  assert.equal(binding.callId, request.callId)
  assert.equal(binding.taskId, request.taskId)
  const sealSha256 = sha256(readFileSync(resolve(directory, 'product-source-seal.json')))
  if (options.productSourceSealSha256) assert.equal(sealSha256, options.productSourceSealSha256)
  const receipts = exportDeviceExecutionReceipts(directory)
  const failure = deviceFailureWithModelCauses({ code: dispatchFailure.failureOrigin === 'execution'
    ? 'DEVICE_EXECUTION_FAILED' : 'DEVICE_DISPATCH_FAILED', status: 'failed' },
    receipts.evidence, [dispatchFailure.jobId])
  assert.ok(report.errorCode === failure.code
    || (recovering && (report.errorCode === undefined || report.errorCode === null)))
  if (report.failure) assert.deepEqual(report.failure, failure)
  return { status: 'failed', failure,
    provider: request.provider ?? aggregationProvider, callId: request.callId, directory,
    productComplete: false, candidate: null, submissionManifest: null,
    executionReceipts: receipts.evidence, productSourceSealSha256: sealSha256,
    taskSourceBindingSha256: sha256(bindingBytes), dispatchFailure,
    productReportSha256: sha256(reportBytes), delivery: report.delivery ?? null,
    ...(recovering ? { recovery: { kind: 'retained-dispatch-failure' } } : {}),
    externalVerdict: null, externalScore: null }
}

function loopbackUnavailable(error) {
  const transientCodes = new Set(['ECONNREFUSED', 'ECONNRESET', 'ETIMEDOUT', 'EHOSTUNREACH'])
  for (let cause = error; cause; cause = cause.cause) if (transientCodes.has(cause.code)) return true
  return false
}

// Reconcile a launched product task after its transport/driver has been
// interrupted. The current terminal product authorities decide the outcome.
export async function resolveRegisteredDeviceTask(request, launch, options = {}, persisted = null) {
    const originalBytes = readFileSync(resolve(launch.directory, 'device-task-result.json'))
    const original = JSON.parse(originalBytes)
    const binding = JSON.parse(readFileSync(resolve(launch.directory, 'task-source-binding.json'), 'utf8'))
    assert.equal(binding.runId, request.runId)
    assert.equal(binding.callId, request.callId)
    assert.equal(binding.taskId, request.taskId)
    assert.equal(binding.taskInputSha256, original.taskInputDigest)
    assert.equal(original.productSessionId, launch.productSessionId)
    assert.equal(original.deliveryId, launch.deliveryId)
    assert.equal(original.modelRoute.modelId, request.provider ?? aggregationProvider)
    const productSourceSealSha256 = sha256(readFileSync(resolve(launch.directory, 'product-source-seal.json')))
    if (options.productSourceSealSha256) assert.equal(productSourceSealSha256, options.productSourceSealSha256)
    if (request.engine === 'fusion-engine') assert.equal(binding.aggregationInputDigest, request.inputDigest)
    assert.deepEqual(original.benchmarkConfiguration, Object.fromEntries(
      ['configurationId', 'track', 'fusion', 'jev', 'jevContext', 'jevJudge'].map(key => [key, request[key]])))
    if (original.errorCode === 'DEVICE_WORKER_CRASHED') {
      return crashedDeviceResult(request, launch, options)
    }
    if (['DEVICE_DISPATCH_FAILED', 'DEVICE_EXECUTION_FAILED'].includes(original.errorCode)) {
      return failedDispatchDeviceResult(request, launch, options)
    }
    if (!original.complete && (original.errorCode === undefined || original.errorCode === null)
      && [original.workRunId, ...(original.workRuns ?? []).map(run => run.id)]
        .some(id => id && failedDeviceDispatch(launch.directory, id, launch.deliveryId))) {
      return failedDispatchDeviceResult(request, launch, options, null, { recovering: true })
    }
    try {
      assertRetainedDeviceTaskRunning(launch, original.delivery?.workRunAggregate?.runs ?? [])
    } catch (error) {
      if (error.code !== 'STUCK_TOOL_REPEAT_LIMIT' || original.complete !== false) throw error
      return stoppedDeviceResult(request, launch, null, { recovering: true })
    }
    if (persisted) {
      assert.deepEqual(originalBytes, persisted.reportBytes)
      assert.deepEqual(original, persisted.report)
    }
    let observation = persisted?.observation
    if (!observation) {
      try {
        observation = await inspectRegisteredDeviceTask(launch)
      } catch (error) {
        if (!loopbackUnavailable(error)) throw error
        observation = persistedDeviceObservation(original, launch)
        assert.ok(terminalBenchmarkDeviceFailure(observation) || observation.delivery.status === 'done',
          'an unavailable product can only recover from a durable terminal projection')
      }
    }
    assert.ok(observation.workRunAggregate.runs.every(run => !['queued', 'leased', 'running'].includes(run.state)),
      'active product execution cannot be finalized by recovery')
    const terminalFailure = terminalBenchmarkDeviceFailure(observation)
    if (terminalFailure !== null) {
      const candidate = observation.delivery.currentCandidate
      const evidenceDirectory = resolve(launch.directory, 'recovery', sha256(JSON.stringify(observation)))
      mkdirSync(evidenceDirectory, { recursive: true, mode: 0o700 })
      const report = { ...original, complete: false,
        candidateRef: candidate?.candidateRef ?? null,
        delivery: { detail: observation.delivery, workRunAggregate: observation.workRunAggregate },
        productFailure: terminalFailure,
        recovery: { originalReportSha256: sha256(originalBytes), observation } }
      const path = resolve(evidenceDirectory, 'device-task-result.json')
      const bytes = `${JSON.stringify(report, null, 2)}\n`
      try { writeFileSync(path, bytes, { flag: 'wx', mode: 0o600 }) } catch (error) {
        if (error.code !== 'EEXIST') throw error
        assert.equal(readFileSync(path, 'utf8'), bytes)
      }
      const receipts = exportDeviceExecutionReceipts(launch.directory, evidenceDirectory)
      let exported = null
      if (candidate !== null) {
        exported = exportDeviceCandidate(launch.directory, resolve(launch.directory, 'task-input.json'), evidenceDirectory)
      }
      const manifestPath = exported
        ? resolve(evidenceDirectory, 'submission-evidence', candidate.candidateCommitId, 'manifest.json') : null
      const failure = deviceFailureWithModelCauses(terminalFailure, receipts.evidence,
        observation.workRunAggregate.runs.filter(run => run.state === 'failed')
          .map(run => run.executionJobId))
      return { status: 'failed', failure, provider: request.provider ?? aggregationProvider,
        callId: request.callId, directory: launch.directory,
        productComplete: false, candidate: exported?.candidate ?? null,
        submissionManifest: manifestPath ? { path: manifestPath, sha256: sha256(readFileSync(manifestPath)) } : null,
        executionReceipts: receipts.evidence,
        productSourceSealSha256,
        taskSourceBindingSha256: sha256(readFileSync(resolve(launch.directory, 'task-source-binding.json'))),
        delivery: report.delivery, recovery: report.recovery,
        externalVerdict: null, externalScore: null }
    }
    assertCompletedDelivery(observation.delivery, observation.workRunAggregate)
    const evidenceDirectory = resolve(launch.directory, 'recovery', sha256(JSON.stringify(observation)))
    mkdirSync(evidenceDirectory, { recursive: true, mode: 0o700 })
    const report = { ...original, complete: true,
      candidateRef: observation.delivery.currentCandidate.candidateRef,
      delivery: { detail: observation.delivery, workRunAggregate: observation.workRunAggregate },
      recovery: { originalReportSha256: sha256(originalBytes), observation } }
    const path = resolve(evidenceDirectory, 'device-task-result.json')
    const bytes = `${JSON.stringify(report, null, 2)}\n`
    try { writeFileSync(path, bytes, { flag: 'wx', mode: 0o600 }) } catch (error) {
      if (error.code !== 'EEXIST') throw error
      assert.equal(readFileSync(path, 'utf8'), bytes)
    }
    const manifest = exportDeviceCandidate(launch.directory, resolve(launch.directory, 'task-input.json'), evidenceDirectory)
    return { ...deviceResult(launch.directory, evidenceDirectory, manifest.candidate.candidateCommitId),
      recovery: report.recovery,
      ...(request.engine === 'fusion-engine' ? { aggregationInputDigest: request.inputDigest, aggregationProvider } : {}) }
}

export async function recoverBenchmarkDeviceCell(cell, observed, options) {
  return recoverBenchmarkCell(cell, observed,
    (request, launch) => resolveRegisteredDeviceTask(request, launch, options))
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    assert.equal(process.argv.length, 3, 'Usage: benchmark-device-adapter.mjs config.json')
    const config = JSON.parse(readFileSync(resolve(process.argv[2]), 'utf8'))
    const result = await executeDeviceBenchmark({ ...config, providerEnvironment: loadDeviceProviderEnvironment() })
    console.log(JSON.stringify({ denominator: result.denominator,
      completed: result.records.filter(record => record.status === 'completed').length,
      externalVerdict: null, externalScore: null }))
  } catch (error) {
    console.error(JSON.stringify({ code: typeof error.code === 'string' && /^[A-Z][A-Z0-9_]{0,127}$/u.test(error.code)
      ? error.code : 'BENCHMARK_RUN_FAILED' }))
    process.exitCode = 1
  }
}
