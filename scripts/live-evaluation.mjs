import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import {
  mkdir,
  open,
  readFile,
  rename,
  rm,
  writeFile,
} from 'node:fs/promises'
import { arch, platform } from 'node:process'
import { join, resolve } from 'node:path'

import { Context } from '@deepseek-ai/cordis'
import AgentRegistry, { installModelSelection } from '@deepseek-ai/dsh-agent'
import LlmRuntime, { createUserMessage } from '@deepseek-ai/dsh-llm'
import * as PiAiProvider from '@deepseek-ai/dsh-llm-pi-ai'
import { createLaunchEnvironmentSnapshot } from '@deepseek-ai/dsh-launch-environment'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ApprovalService from '@deepseek-ai/dsh-user-approval'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDeliverySpec,
  parseStrongFlowPlanReviewContextText,
  parseStrongFlowPlanReviewSolution,
} from '../packages/contracts/dist/index.js'
import {
  RuntimeSessionLedger,
  WinWinCodeAgentFactory,
} from '../packages/dsh-profile/dist/index.js'
import { WinWinCodeKernel } from '../packages/native/dist/index.js'
import {
  DeliveryRuntimeProjection,
  IndependentVerificationError,
  StrongFlowService,
  createIndependentVerificationAssignment,
  createStrongFlowDeliveryLocalProofAuthenticator,
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  freezeAcceptanceVerificationInput,
  freezeDeliveryCandidate,
  projectIndependentVerification,
  serializeIndependentVerificationSessionInput,
} from '../packages/strongflow/dist/index.js'
import {
  LIVE_EVALUATION_VERIFICATION_ROLES,
  measureLiveEvaluationResult,
} from './evaluation-measures.mjs'
import {
  fileDescriptor,
  readCanonicalJson,
  releaseSourceSha256,
} from './release-source-contract.mjs'

export const LIVE_EVALUATION_SCHEMA_VERSION = 1
export const LIVE_EVALUATION_PROJECTION_VERSION = 2

const repositoryRoot = resolve(import.meta.dirname, '..')
const portableIdPattern = /^[A-Za-z0-9][A-Za-z0-9._:@/-]{0,159}$/u
const runIdPattern = /^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$/u
const gitObjectPattern = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u
const credentialReferencePattern = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/u
const secretNamePattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu
const shellExcludedCredentialPattern = /(?:KEY|SECRET|TOKEN)/iu
const verificationRoles = LIVE_EVALUATION_VERIFICATION_ROLES
const verificationResultAttemptLimit = 2
const requiredTurns = 2 + verificationRoles.length * (1 + verificationResultAttemptLimit)

export class LiveEvaluationError extends Error {
  constructor(code, message, options) {
    super(message, options)
    this.name = 'LiveEvaluationError'
    this.code = code
  }
}

export class LiveEvaluationBudgetError extends LiveEvaluationError {
  constructor(code, message) {
    super(code, message)
    this.name = 'LiveEvaluationBudgetError'
  }
}

function fail(code, message, cause) {
  throw new LiveEvaluationError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function isRecord(value) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(value, keys, label) {
  if (!isRecord(value)) fail('INVALID_CONFIG', `${label} must be an object`)
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    fail('INVALID_CONFIG', `${label} has an unexpected shape`)
  }
}

function nonEmptyText(value, label, maximum = 65_536) {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > maximum
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    fail('INVALID_CONFIG', `${label} must be bounded non-empty text`)
  }
  return value
}

function portableId(value, label) {
  if (typeof value !== 'string' || !portableIdPattern.test(value)) {
    fail('INVALID_CONFIG', `${label} must be a portable identifier`)
  }
  return value
}

function runId(value) {
  if (typeof value !== 'string' || !runIdPattern.test(value)) {
    fail('INVALID_CONFIG', 'runId must be a path-safe identifier of at most 80 characters')
  }
  return value
}

function positiveInteger(value, label, maximum = Number.MAX_SAFE_INTEGER) {
  if (!Number.isSafeInteger(value) || value < 1 || value > maximum) {
    fail('INVALID_CONFIG', `${label} must be a positive safe integer`)
  }
  return value
}

function nonNegativeInteger(value, label, maximum = Number.MAX_SAFE_INTEGER) {
  if (!Number.isSafeInteger(value) || value < 0 || value > maximum) {
    fail('INVALID_CONFIG', `${label} must be a non-negative safe integer`)
  }
  return value
}

function nullableText(value, label, maximum = 65_536) {
  return value === null ? null : nonEmptyText(value, label, maximum)
}

function stringList(value, label) {
  if (!Array.isArray(value) || value.length > 200) {
    fail('INVALID_CONFIG', `${label} must be a bounded string array`)
  }
  const result = value.map((entry, index) => (
    nonEmptyText(entry, `${label}[${String(index)}]`)
  ))
  if (new Set(result).size !== result.length) {
    fail('INVALID_CONFIG', `${label} must not contain duplicates`)
  }
  return Object.freeze(result)
}

function nullableReasoningEffort(value, label) {
  if (value === null) return null
  return portableId(value, label)
}

function providerEndpoint(value) {
  const endpoint = nonEmptyText(value, 'provider.baseURL', 4_096)
  let parsed
  try {
    parsed = new URL(endpoint)
  } catch (error) {
    return fail('INVALID_CONFIG', 'provider.baseURL must be an absolute URL', error)
  }
  if (!['http:', 'https:'].includes(parsed.protocol)
    || parsed.username.length > 0
    || parsed.password.length > 0
    || parsed.search.length > 0
    || parsed.hash.length > 0) {
    fail(
      'INVALID_CONFIG',
      'provider.baseURL must be an HTTP URL without credentials, query, or fragment',
    )
  }
  return parsed.toString().replace(/\/$/u, '')
}

function nullableProviderEndpoint(value) {
  return value === null ? null : providerEndpoint(value)
}

function parsePricing(value) {
  exactKeys(value, [
    'source',
    'inputUsdMicrosPerMillionTokens',
    'outputUsdMicrosPerMillionTokens',
    'cacheReadUsdMicrosPerMillionTokens',
    'cacheWriteUsdMicrosPerMillionTokens',
  ], 'budgets.pricing')
  return Object.freeze({
    source: nonEmptyText(value.source, 'budgets.pricing.source', 2_048),
    inputUsdMicrosPerMillionTokens: nonNegativeInteger(
      value.inputUsdMicrosPerMillionTokens,
      'budgets.pricing.inputUsdMicrosPerMillionTokens',
      1_000_000_000,
    ),
    outputUsdMicrosPerMillionTokens: nonNegativeInteger(
      value.outputUsdMicrosPerMillionTokens,
      'budgets.pricing.outputUsdMicrosPerMillionTokens',
      1_000_000_000,
    ),
    cacheReadUsdMicrosPerMillionTokens: nonNegativeInteger(
      value.cacheReadUsdMicrosPerMillionTokens,
      'budgets.pricing.cacheReadUsdMicrosPerMillionTokens',
      1_000_000_000,
    ),
    cacheWriteUsdMicrosPerMillionTokens: nonNegativeInteger(
      value.cacheWriteUsdMicrosPerMillionTokens,
      'budgets.pricing.cacheWriteUsdMicrosPerMillionTokens',
      1_000_000_000,
    ),
  })
}

function parseBudgets(value) {
  exactKeys(value, [
    'maxWallTimeMillis',
    'maxModelCalls',
    'maxTurns',
    'maxTokensPerCall',
    'maxTotalTokens',
    'maxCostUsdMicros',
    'pricing',
  ], 'budgets')
  const budgets = Object.freeze({
    maxWallTimeMillis: positiveInteger(
      value.maxWallTimeMillis,
      'budgets.maxWallTimeMillis',
      86_400_000,
    ),
    maxModelCalls: positiveInteger(value.maxModelCalls, 'budgets.maxModelCalls', 10_000),
    maxTurns: positiveInteger(value.maxTurns, 'budgets.maxTurns', 1_000),
    maxTokensPerCall: positiveInteger(
      value.maxTokensPerCall,
      'budgets.maxTokensPerCall',
      1_000_000,
    ),
    maxTotalTokens: positiveInteger(
      value.maxTotalTokens,
      'budgets.maxTotalTokens',
      1_000_000_000,
    ),
    maxCostUsdMicros: positiveInteger(
      value.maxCostUsdMicros,
      'budgets.maxCostUsdMicros',
      1_000_000_000_000,
    ),
    pricing: parsePricing(value.pricing),
  })
  if (budgets.maxTurns < requiredTurns) {
    fail(
      'INVALID_CONFIG',
      `budgets.maxTurns must allow ${String(requiredTurns)} base and bounded correction turns`,
    )
  }
  if (budgets.maxModelCalls < budgets.maxTurns) {
    fail('INVALID_CONFIG', 'budgets.maxModelCalls must be at least budgets.maxTurns')
  }
  if (budgets.maxTotalTokens < budgets.maxTokensPerCall) {
    fail('INVALID_CONFIG', 'budgets.maxTotalTokens must be at least maxTokensPerCall')
  }
  return budgets
}

function parseProvider(value, budgets) {
  exactKeys(value, [
    'route',
    'model',
    'apiKeyEnv',
    'baseURL',
    'reasoningEffort',
    'timeoutMillis',
  ], 'provider')
  const route = portableId(value.route, 'provider.route')
  const model = portableId(value.model, 'provider.model')
  const apiKeyEnv = nonEmptyText(value.apiKeyEnv, 'provider.apiKeyEnv', 128)
  if (!credentialReferencePattern.test(apiKeyEnv)) {
    fail('INVALID_CONFIG', 'provider.apiKeyEnv must name one environment variable')
  }
  if (!shellExcludedCredentialPattern.test(apiKeyEnv)) {
    fail(
      'INVALID_CONFIG',
      'provider.apiKeyEnv must use a KEY, SECRET, or TOKEN name excluded from Codex shells',
    )
  }
  return Object.freeze({
    route,
    model,
    apiKeyEnv,
    baseURL: nullableProviderEndpoint(value.baseURL),
    reasoningEffort: nullableReasoningEffort(
      value.reasoningEffort,
      'provider.reasoningEffort',
    ),
    timeoutMillis: positiveInteger(
      value.timeoutMillis,
      'provider.timeoutMillis',
      budgets.maxWallTimeMillis,
    ),
  })
}

function parseHumanDecisions(value) {
  exactKeys(value, ['planReview', 'deliveryReview'], 'humanDecisions')
  exactKeys(
    value.planReview,
    ['action', 'comments', 'requestedChanges'],
    'humanDecisions.planReview',
  )
  exactKeys(value.deliveryReview, ['action', 'resolution'], 'humanDecisions.deliveryReview')
  if (value.planReview.action !== 'approve') {
    fail('INVALID_CONFIG', 'the first live evaluator requires an approved plan review')
  }
  if (value.deliveryReview.action !== 'approve') {
    fail('INVALID_CONFIG', 'the first live evaluator requires approved final delivery')
  }
  return Object.freeze({
    planReview: Object.freeze({
      action: 'approve',
      comments: nonEmptyText(value.planReview.comments, 'humanDecisions.planReview.comments'),
      requestedChanges: stringList(
        value.planReview.requestedChanges,
        'humanDecisions.planReview.requestedChanges',
      ),
    }),
    deliveryReview: Object.freeze({
      action: 'approve',
      resolution: nonEmptyText(
        value.deliveryReview.resolution,
        'humanDecisions.deliveryReview.resolution',
      ),
    }),
  })
}

function parseExecution(value) {
  exactKeys(value, ['commitMessage'], 'execution')
  const commitMessage = nonEmptyText(value.commitMessage, 'execution.commitMessage', 512)
  if (/\r|\n/u.test(commitMessage) || commitMessage !== commitMessage.trim()) {
    fail(
      'INVALID_CONFIG',
      'execution.commitMessage must be one trimmed Git subject line',
    )
  }
  return Object.freeze({
    commitMessage,
  })
}

function parseRepository(value) {
  exactKeys(value, ['sourcePath', 'expectedCommit'], 'repository')
  const sourcePath = resolve(nonEmptyText(value.sourcePath, 'repository.sourcePath', 4_096))
  const expectedCommit = nonEmptyText(
    value.expectedCommit,
    'repository.expectedCommit',
    64,
  )
  if (!gitObjectPattern.test(expectedCommit)) {
    fail('INVALID_CONFIG', 'repository.expectedCommit must be a full lowercase Git object id')
  }
  return Object.freeze({ sourcePath, expectedCommit })
}

/** Validate the exact, secret-free configuration accepted by the live runner. */
export function parseLiveEvaluationConfig(value) {
  exactKeys(value, [
    'schemaVersion',
    'runId',
    'projectionVersion',
    'repository',
    'provider',
    'budgets',
    'deliverySpec',
    'solution',
    'humanDecisions',
    'execution',
  ], 'live evaluation config')
  if (value.schemaVersion !== LIVE_EVALUATION_SCHEMA_VERSION) {
    fail('INVALID_CONFIG', 'live evaluation schemaVersion is unsupported')
  }
  if (value.projectionVersion !== LIVE_EVALUATION_PROJECTION_VERSION) {
    fail('INVALID_CONFIG', 'live evaluation projectionVersion is unsupported')
  }
  const parsedRunId = runId(value.runId)
  const repository = parseRepository(value.repository)
  const budgets = parseBudgets(value.budgets)
  const provider = parseProvider(value.provider, budgets)
  let deliverySpec
  let solution
  try {
    deliverySpec = parseDeliverySpec(value.deliverySpec, 'deliverySpec')
    solution = parseStrongFlowPlanReviewSolution(value.solution, 'solution')
  } catch (error) {
    return fail('INVALID_CONFIG', 'DeliverySpec or solution is invalid', error)
  }
  if (deliverySpec.schemaVersion !== DELIVERY_SCHEMA_VERSION
    || deliverySpec.revision < 2
    || deliverySpec.repository.kind !== 'local-git'
    || resolve(deliverySpec.repository.locator) !== repository.sourcePath
    || deliverySpec.baseRevision !== repository.expectedCommit
    || deliverySpec.sourceRef !== null
    || deliverySpec.publicationTarget !== null) {
    fail(
      'INVALID_CONFIG',
      'DeliverySpec must be revision 2 or later and identify the exact local repository base',
    )
  }
  return Object.freeze({
    schemaVersion: LIVE_EVALUATION_SCHEMA_VERSION,
    runId: parsedRunId,
    projectionVersion: LIVE_EVALUATION_PROJECTION_VERSION,
    repository,
    provider,
    budgets,
    deliverySpec,
    solution,
    humanDecisions: parseHumanDecisions(value.humanDecisions),
    execution: parseExecution(value.execution),
  })
}

function sha256(value) {
  return createHash('sha256').update(value).digest('hex')
}

async function sha256File(path) {
  return sha256(await readFile(path))
}

function stableJson(value) {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`
  if (isRecord(value)) {
    return `{${Object.keys(value).sort().map(key => (
      `${JSON.stringify(key)}:${stableJson(value[key])}`
    )).join(',')}}`
  }
  return JSON.stringify(value)
}

function configDigest(config) {
  return sha256(stableJson(config))
}

function selectedSecrets(config, environment) {
  const selected = environment[config.provider.apiKeyEnv]
  if (typeof selected !== 'string' || selected.trim().length < 8) {
    fail(
      'CREDENTIAL_MISSING',
      `provider credential reference ${config.provider.apiKeyEnv} is not a usable secret`,
    )
  }
  const candidates = new Set([selected])
  for (const [name, value] of Object.entries(environment)) {
    if (secretNamePattern.test(name) && typeof value === 'string' && value.length >= 8) {
      candidates.add(value)
    }
  }
  return Object.freeze([...candidates].filter(value => value.length > 0))
}

function redactSecrets(text, secrets) {
  let output = text
  for (const secret of secrets.toSorted((left, right) => right.length - left.length)) {
    output = output.split(secret).join('***')
  }
  return output
}

function mapStrings(value, transform) {
  if (typeof value === 'string') return transform(value)
  if (Array.isArray(value)) return value.map(entry => mapStrings(entry, transform))
  if (isRecord(value)) {
    return Object.fromEntries(
      Object.entries(value).map(([key, entry]) => [
        transform(key),
        mapStrings(entry, transform),
      ]),
    )
  }
  return value
}

function containsSecret(value, secrets) {
  if (typeof value === 'string') return secrets.some(secret => value.includes(secret))
  if (Array.isArray(value)) return value.some(entry => containsSecret(entry, secrets))
  if (isRecord(value)) {
    return Object.entries(value).some(([key, entry]) => (
      containsSecret(key, secrets) || containsSecret(entry, secrets)
    ))
  }
  return false
}

function redactSecretValues(value, secrets) {
  return mapStrings(value, text => redactSecrets(text, secrets))
}

function assertSecretsAbsent(value, secrets) {
  if (containsSecret(value, secrets)) {
    fail('SECRET_LEAK_BLOCKED', 'evaluation result still contains credential material')
  }
}

class LiveEvaluationJournal {
  #path
  #secrets
  #state

  constructor(path, state, secrets) {
    this.#path = path
    this.#secrets = secrets
    this.#state = redactSecretValues(structuredClone(state), secrets)
    assertSecretsAbsent(this.#state, secrets)
  }

  static async create(runRoot, state, secrets) {
    const journal = new LiveEvaluationJournal(join(runRoot, 'result.json'), state, secrets)
    await journal.write()
    return journal
  }

  get path() {
    return this.#path
  }

  get state() {
    return structuredClone(this.#state)
  }

  async patch(patch) {
    this.#state = redactSecretValues({
      ...this.#state,
      ...structuredClone(patch),
      updatedAtMillis: Date.now(),
    }, this.#secrets)
    assertSecretsAbsent(this.#state, this.#secrets)
    await this.write()
  }

  async phase(phase, patch = {}) {
    const atMillis = Date.now()
    this.#state = redactSecretValues({
      ...this.#state,
      ...structuredClone(patch),
      phase,
      phaseHistory: [...this.#state.phaseHistory, { phase, atMillis }],
      updatedAtMillis: atMillis,
    }, this.#secrets)
    assertSecretsAbsent(this.#state, this.#secrets)
    await this.write()
  }

  async write() {
    assertSecretsAbsent(this.#state, this.#secrets)
    const text = `${JSON.stringify(this.#state, null, 2)}\n`
    const temporary = `${this.#path}.tmp-${process.pid}-${randomBytes(6).toString('hex')}`
    await writeFile(temporary, text, { mode: 0o600 })
    await rename(temporary, this.#path)
  }
}

async function runProcess(command, arguments_, options = {}) {
  return new Promise((resolveProcess, rejectProcess) => {
    if (options.signal?.aborted) {
      rejectProcess(options.signal.reason ?? new Error('process operation was aborted'))
      return
    }
    const child = spawn(command, arguments_, {
      cwd: options.cwd,
      env: options.env ?? process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    const stdout = []
    const stderr = []
    child.stdout.on('data', chunk => stdout.push(chunk))
    child.stderr.on('data', chunk => stderr.push(chunk))
    const abort = () => child.kill('SIGTERM')
    options.signal?.addEventListener('abort', abort, { once: true })
    child.once('error', rejectProcess)
    child.once('close', (code, signal) => {
      options.signal?.removeEventListener('abort', abort)
      resolveProcess(Object.freeze({
        code,
        signal,
        stdout: Buffer.concat(stdout),
        stderr: Buffer.concat(stderr),
      }))
    })
  })
}

async function checkedProcess(command, arguments_, options = {}) {
  const result = await runProcess(command, arguments_, options)
  if (result.code !== 0 || result.signal !== null) {
    fail(
      options.errorCode ?? 'PROCESS_FAILED',
      `${options.label ?? command} did not complete successfully`,
    )
  }
  return result
}

/** Mandatory deterministic gate used by the production CLI before a paid call. */
export async function runLiveEvaluationPreflight(signal) {
  const startedAtMillis = Date.now()
  await checkedProcess(
    process.execPath,
    ['--test', 'tests/delivery-full-keyless.test.mjs'],
    {
      cwd: repositoryRoot,
      env: Object.fromEntries(
        Object.entries(process.env).filter(([name]) => (
          name !== 'NODE_TEST_CONTEXT' && !secretNamePattern.test(name)
        )),
      ),
      signal,
      errorCode: 'PREFLIGHT_FAILED',
      label: 'deterministic full Delivery preflight',
    },
  )
  return Object.freeze({
    command: 'node --test tests/delivery-full-keyless.test.mjs',
    commandSha256: sha256('node\u0000--test\u0000tests/delivery-full-keyless.test.mjs'),
    startedAtMillis,
    finishedAtMillis: Date.now(),
    status: 'passed',
  })
}

async function git(repository, arguments_, options = {}) {
  const result = await checkedProcess('git', ['-C', repository, ...arguments_], {
    ...options,
    errorCode: options.errorCode ?? 'GIT_FAILED',
    label: options.label ?? `git ${arguments_.join(' ')}`,
  })
  return result.stdout.toString('utf8').trim()
}

async function verifyAndCloneRepository(config, runRoot, signal) {
  const actual = await git(
    config.repository.sourcePath,
    ['rev-parse', '--verify', `${config.repository.expectedCommit}^{commit}`],
    { signal, label: 'repository commit verification' },
  )
  if (actual !== config.repository.expectedCommit) {
    fail('REPOSITORY_MISMATCH', 'repository expectedCommit does not resolve exactly')
  }
  const workspace = join(runRoot, 'workspace')
  await checkedProcess('git', [
    'clone',
    '--no-hardlinks',
    '--no-checkout',
    config.repository.sourcePath,
    workspace,
  ], { signal, errorCode: 'REPOSITORY_CLONE_FAILED', label: 'isolated repository clone' })
  await git(workspace, ['checkout', '--detach', config.repository.expectedCommit], { signal })
  await git(workspace, ['config', 'user.name', 'WinWinCode Evaluation'], { signal })
  await git(workspace, ['config', 'user.email', 'evaluation@winwincode.invalid'], { signal })
  const head = await git(workspace, ['rev-parse', 'HEAD'], { signal })
  const tree = await git(workspace, ['rev-parse', 'HEAD^{tree}'], { signal })
  if (head !== config.repository.expectedCommit) {
    fail('REPOSITORY_MISMATCH', 'isolated clone did not check out the pinned commit')
  }
  return Object.freeze({
    workspace,
    reviewWorkspace: join(runRoot, 'review-workspace'),
    baseCommitId: head,
    baseTreeId: tree,
  })
}

async function prepareReviewWorkspace(repository, reviewWorkspace, candidateCommitId, signal) {
  await checkedProcess('git', [
    'clone',
    '--no-hardlinks',
    '--no-checkout',
    repository,
    reviewWorkspace,
  ], { signal, errorCode: 'REVIEW_CLONE_FAILED', label: 'isolated review clone' })
  await git(reviewWorkspace, ['checkout', '--detach', candidateCommitId], { signal })
  await assertReviewWorkspace(reviewWorkspace, candidateCommitId, signal)
  return Object.freeze({ workspace: reviewWorkspace, candidateCommitId })
}

async function assertReviewWorkspace(reviewWorkspace, candidateCommitId, signal) {
  const head = await git(reviewWorkspace, ['rev-parse', 'HEAD'], { signal })
  const status = await git(
    reviewWorkspace,
    ['status', '--porcelain=v1', '-z', '--untracked-files=all', '--ignored=matching'],
    { signal },
  )
  if (head !== candidateCommitId || status.length > 0) {
    fail('REVIEW_CLONE_MISMATCH', 'review clone does not match the frozen candidate exactly')
  }
}

function nativePlatformName() {
  const platformName = platform === 'darwin' ? 'darwin' : platform === 'linux' ? 'linux' : null
  const architecture = arch === 'arm64' ? 'arm64' : arch === 'x64' ? 'x64' : null
  if (platformName === null || architecture === null) {
    fail('PLATFORM_UNSUPPORTED', `live evaluation does not support ${platform}/${arch}`)
  }
  return `${platformName}-${architecture}`
}

async function sourceIdentities() {
  const sourcesLockPath = join(repositoryRoot, 'upstream', 'sources.lock.json')
  const evaluatorPath = join(repositoryRoot, 'scripts', 'live-evaluation.mjs')
  const cliPath = join(repositoryRoot, 'scripts', 'run-live-evaluation.mjs')
  const measuresAdapterPath = join(repositoryRoot, 'scripts', 'evaluation-measures.mjs')
  const measuresCliPath = join(repositoryRoot, 'scripts', 'run-evaluation-measures.mjs')
  const measuresProjectionPath = join(
    repositoryRoot,
    'packages',
    'strongflow',
    'src',
    'evaluation-measures.ts',
  )
  const measuresRuntimePath = join(
    repositoryRoot,
    'packages',
    'strongflow',
    'dist',
    'evaluation-measures.js',
  )
  const preflightTestPath = join(repositoryRoot, 'tests', 'delivery-full-keyless.test.mjs')
  const sourcesLock = JSON.parse(await readFile(sourcesLockPath, 'utf8'))
  const workspace = readCanonicalJson(join(repositoryRoot, 'package.json'))
  const nativePackage = `native-${nativePlatformName()}`
  const buildInfoPath = join(
    repositoryRoot,
    'packages',
    nativePackage,
    'prebuild',
    'build-info.json',
  )
  const buildInfo = JSON.parse(await readFile(buildInfoPath, 'utf8'))
  const artifacts = {}
  for (const [name, artifact] of Object.entries(buildInfo.artifacts)) {
    const path = join(repositoryRoot, 'packages', nativePackage, 'prebuild', artifact.path)
    const actualSha256 = await sha256File(path)
    if (actualSha256 !== artifact.sha256) {
      fail('NATIVE_IDENTITY_MISMATCH', `native artifact ${name} differs from build-info.json`)
    }
    artifacts[name] = Object.freeze({
      path: artifact.path,
      sha256: actualSha256,
      bytes: artifact.bytes,
    })
  }
  return Object.freeze({
    project: Object.freeze({
      repository: workspace.repository,
      version: workspace.version,
      releaseSourceSha256: releaseSourceSha256(repositoryRoot),
      rootPackage: fileDescriptor(join(repositoryRoot, 'package.json')),
      pnpmLock: fileDescriptor(join(repositoryRoot, 'pnpm-lock.yaml')),
      upstreamSourcesLock: fileDescriptor(sourcesLockPath),
    }),
    evaluator: Object.freeze({
      schemaVersion: LIVE_EVALUATION_SCHEMA_VERSION,
      projectionVersion: LIVE_EVALUATION_PROJECTION_VERSION,
      runnerSha256: await sha256File(evaluatorPath),
      cliSha256: await sha256File(cliPath),
      measuresAdapterSha256: await sha256File(measuresAdapterPath),
      measuresCliSha256: await sha256File(measuresCliPath),
      measuresProjectionSha256: await sha256File(measuresProjectionPath),
      measuresRuntimeSha256: await sha256File(measuresRuntimePath),
      preflightTestSha256: await sha256File(preflightTestPath),
    }),
    codex: Object.freeze({
      repository: sourcesLock.codex.repository,
      tag: sourcesLock.codex.tag,
      commit: sourcesLock.codex.commit,
      archiveSha256: sourcesLock.codex.archiveSha256,
    }),
    dsh: Object.freeze({
      repository: sourcesLock.dsh.repository,
      tag: sourcesLock.dsh.tag,
      commit: sourcesLock.dsh.commit,
      archiveSha256: sourcesLock.dsh.archiveSha256,
    }),
    native: Object.freeze({
      package: buildInfo.package,
      target: buildInfo.target,
      profile: buildInfo.profile,
      nativeInterfaceVersion: buildInfo.nativeInterfaceVersion,
      buildInfoSha256: await sha256File(buildInfoPath),
      artifacts: Object.freeze(artifacts),
    }),
  })
}

function usageCost(tokens, pricing) {
  const parts = [
    [tokens.inputTokens, pricing.inputUsdMicrosPerMillionTokens],
    [tokens.outputTokens, pricing.outputUsdMicrosPerMillionTokens],
    [tokens.cacheReadTokens, pricing.cacheReadUsdMicrosPerMillionTokens],
    [tokens.cacheWriteTokens, pricing.cacheWriteUsdMicrosPerMillionTokens],
  ]
  let numerator = 0n
  for (const [count, rate] of parts) numerator += BigInt(count) * BigInt(rate)
  return Number((numerator + 999_999n) / 1_000_000n)
}

class LiveEvaluationBudget {
  #calls = []
  #limits
  #startedAtMillis
  #turns = 0
  #usage = {
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
    totalTokens: 0,
    costUsdMicros: 0,
  }
  #violation = null

  constructor(limits, startedAtMillis) {
    this.#limits = limits
    this.#startedAtMillis = startedAtMillis
  }

  get snapshot() {
    return Object.freeze({
      limits: this.#limits,
      turns: this.#turns,
      modelCalls: this.#calls.length,
      usage: Object.freeze({ ...this.#usage }),
      calls: Object.freeze(this.#calls.map(call => Object.freeze({ ...call }))),
      violation: this.#violation,
    })
  }

  reserveTurn(role) {
    this.#assertWallTime()
    if (this.#turns >= this.#limits.maxTurns) {
      throw new LiveEvaluationBudgetError('TURN_BUDGET_EXCEEDED', 'turn budget is exhausted')
    }
    this.#turns += 1
    return Object.freeze({ turn: this.#turns, role })
  }

  #assertWallTime() {
    if (Date.now() - this.#startedAtMillis > this.#limits.maxWallTimeMillis) {
      this.#violation ??= 'wall-time'
      throw new LiveEvaluationBudgetError(
        'WALL_TIME_BUDGET_EXCEEDED',
        'evaluation wall-time budget is exhausted',
      )
    }
  }

  assertContinue() {
    this.#assertWallTime()
    if (this.#violation === 'tokens') {
      throw new LiveEvaluationBudgetError(
        'TOKEN_BUDGET_EXCEEDED',
        'total token budget was exceeded by the last bounded model call',
      )
    }
    if (this.#violation === 'cost') {
      throw new LiveEvaluationBudgetError(
        'COST_BUDGET_EXCEEDED',
        'cost budget was exceeded by the last bounded model call',
      )
    }
    if (this.#violation === 'per-call-output') {
      throw new LiveEvaluationBudgetError(
        'PROVIDER_OUTPUT_LIMIT_EXCEEDED',
        'provider usage exceeds the configured per-call output cap',
      )
    }
  }

  wrap(options, stream) {
    this.assertContinue()
    if (this.#calls.length >= this.#limits.maxModelCalls) {
      throw new LiveEvaluationBudgetError(
        'MODEL_CALL_BUDGET_EXCEEDED',
        'model-call budget is exhausted',
      )
    }
    const call = {
      index: this.#calls.length + 1,
      provider: options.provider,
      model: options.model,
      sessionId: options.sessionId ?? null,
      purpose: options.purpose ?? null,
      maxTokens: options.maxTokens ?? null,
      startedAtMillis: Date.now(),
      finishedAtMillis: null,
      status: 'running',
      usage: null,
      costUsdMicros: 0,
    }
    if (call.maxTokens === null || call.maxTokens > this.#limits.maxTokensPerCall) {
      throw new LiveEvaluationBudgetError(
        'PER_CALL_TOKEN_BUDGET_EXCEEDED',
        'model request does not carry the configured per-call output cap',
      )
    }
    this.#calls.push(call)
    const budget = this
    return (async function* monitoredStream() {
      try {
        for await (const chunk of stream) {
          if (chunk.type === 'usage') budget.#recordUsage(call, chunk.usage)
          yield chunk
        }
        if (call.usage === null) {
          fail('MODEL_USAGE_MISSING', 'provider completed a model call without token usage')
        }
        call.status = 'completed'
      } catch (error) {
        call.status = 'failed'
        throw error
      } finally {
        call.finishedAtMillis = Date.now()
      }
    })()
  }

  #recordUsage(call, usage) {
    if (call.usage !== null) {
      fail('DUPLICATE_MODEL_USAGE', 'provider emitted token usage more than once for one call')
    }
    const tokens = Object.freeze({
      inputTokens: nonNegativeInteger(
        usage.inputTokens ?? 0,
        'usage.inputTokens',
        1_000_000_000,
      ),
      outputTokens: nonNegativeInteger(
        usage.outputTokens ?? 0,
        'usage.outputTokens',
        1_000_000_000,
      ),
      cacheReadTokens: nonNegativeInteger(
        usage.cacheReadTokens ?? 0,
        'usage.cacheReadTokens',
        1_000_000_000,
      ),
      cacheWriteTokens: nonNegativeInteger(
        usage.cacheWriteTokens ?? 0,
        'usage.cacheWriteTokens',
        1_000_000_000,
      ),
    })
    const providerExceededOutputCap = tokens.outputTokens > call.maxTokens
    const totalTokens = tokens.inputTokens
      + tokens.outputTokens
      + tokens.cacheReadTokens
      + tokens.cacheWriteTokens
    const costUsdMicros = usageCost(tokens, this.#limits.pricing)
    call.usage = Object.freeze({ ...tokens, totalTokens })
    call.costUsdMicros = costUsdMicros
    this.#usage.inputTokens += tokens.inputTokens
    this.#usage.outputTokens += tokens.outputTokens
    this.#usage.cacheReadTokens += tokens.cacheReadTokens
    this.#usage.cacheWriteTokens += tokens.cacheWriteTokens
    this.#usage.totalTokens += totalTokens
    this.#usage.costUsdMicros += costUsdMicros
    if (providerExceededOutputCap) {
      this.#violation ??= 'per-call-output'
      throw new LiveEvaluationBudgetError(
        'PROVIDER_OUTPUT_LIMIT_EXCEEDED',
        'provider usage exceeds the configured per-call output cap',
      )
    }
    if (this.#usage.totalTokens > this.#limits.maxTotalTokens) this.#violation ??= 'tokens'
    if (this.#usage.costUsdMicros > this.#limits.maxCostUsdMicros) {
      this.#violation ??= 'cost'
    }
  }
}

function terminalFailure(events) {
  const terminal = events.findLast(event => (
    event.kind === 'turn.completed' || event.kind === 'turn.aborted'
  ))
  if (terminal === undefined) return 'missing-terminal-event'
  if (terminal.kind === 'turn.aborted' || terminal.terminalReason !== 'completed') {
    return terminal.terminalReason ?? terminal.kind
  }
  const laterFailure = events.find(event => (
    BigInt(event.cursor.sequence) >= BigInt(terminal.cursor.sequence)
    && event.kind === 'failure'
  ))
  return laterFailure === undefined ? null : laterFailure.terminalReason ?? 'failure'
}

class LiveRoleSession {
  #budget
  #handle
  #home
  #role

  constructor(options) {
    this.#budget = options.budget
    this.#handle = options.handle
    this.#home = options.home
    this.#role = options.role
    this.stageRunId = options.stageRunId
    this.bindingId = options.bindingId
    this.dshSessionId = this.#handle.agent.id
    this.codexSessionId = options.codexSessionId
    this.rolloutPath = options.rolloutPath
  }

  async turn(prompt) {
    this.#budget.reserveTurn(this.#role)
    const before = await this.events()
    this.#handle.agent.followup(createUserMessage({
      content: [{ type: 'text', text: prompt }],
      source: { kind: 'user' },
    }))
    await this.#handle.agent.whenIdle()
    const after = await this.events()
    const turnEvents = after.slice(before.length)
    const failure = terminalFailure(turnEvents)
    if (failure !== null) {
      fail('CODEX_TURN_FAILED', `${this.#role} Codex turn ended as ${failure}`)
    }
    this.#budget.assertContinue()
    return Object.freeze(turnEvents)
  }

  async events() {
    return RuntimeSessionLedger.open(this.#home, this.dshSessionId)
      .then(ledger => ledger.read())
      .then(snapshot => snapshot.events)
  }

  cancel(reason) {
    this.#handle.agent.cancel({ kind: 'hook', reason })
  }

  async dispose() {
    await this.#handle.dispose()
  }
}

/** DSH may remove an idle role before the evaluator releases its local handle. */
export async function disposeCompletedRoleSession(session) {
  try {
    await session.dispose()
  } catch (error) {
    if (error?.code === 'SESSION_NOT_FOUND') return
    throw error
  }
}

class LiveDshRuntime {
  #budget
  #context
  #handles = new Set()
  #home
  #kernel
  #modelInfo
  #environment
  #provider
  #workspace

  constructor(options) {
    this.#budget = options.budget
    this.#context = new Context()
    this.#environment = options.environment
    this.#home = options.home
    this.#provider = options.provider
    this.#workspace = options.workspace
  }

  static async create(options) {
    const runtime = new LiveDshRuntime(options)
    await runtime.#start()
    return runtime
  }

  async #start() {
    this.#context.provide('launchEnvironment', createLaunchEnvironmentSnapshot([{
      source: 'process',
      values: this.#environment,
    }]))
    await this.#context.plugin(LlmRuntime)
    await this.#context.plugin(SessionStore)
    await this.#context.plugin(SystemPrompt)
    await this.#context.plugin(AgentRegistry)
    await this.#context.plugin(ApprovalService, { policy: 'never' })
    const budget = this.#budget
    const monitor = context => {
      context.on('llm/stream', (options, next) => budget.wrap(options, next()), {
        global: true,
        prepend: true,
      })
    }
    monitor.inject = ['llm']
    await this.#context.plugin(monitor)
    const provider = this.#provider
    await this.#context.plugin(PiAiProvider, {
      providers: {
        [provider.route]: {
          apiKeyEnv: provider.apiKeyEnv,
          ...(provider.baseURL === null ? {} : { baseURL: provider.baseURL }),
          timeoutMs: provider.timeoutMillis,
          streamIdleTimeoutMs: provider.timeoutMillis,
          retryPolicy: { mode: 'normal', maxRetries: 0 },
          models: [{
            id: provider.model,
            maxTokens: budget.snapshot.limits.maxTokensPerCall,
          }],
        },
      },
    })
    let modelInfo
    try {
      modelInfo = await this.#context.llm.resolveModelInfo(provider.route, provider.model)
    } catch (error) {
      return fail(
        'INVALID_CONFIG',
        'provider.route and provider.model must identify one model in the installed DSH catalog',
        error,
      )
    }
    const contextWindow = modelInfo.context?.contextWindow
    if (!Number.isSafeInteger(contextWindow)
      || contextWindow < budget.snapshot.limits.maxTokensPerCall) {
      fail(
        'INVALID_CONFIG',
        'the DSH catalog model context must be at least maxTokensPerCall',
      )
    }
    const inputModalities = [...modelInfo.inputModalities ?? []]
    if (!inputModalities.includes('text')) {
      fail('INVALID_CONFIG', 'the DSH catalog model must accept text input')
    }
    const reasoningEfforts = modelInfo.reasoning?.efforts.map(entry => entry.id) ?? []
    if (provider.reasoningEffort !== null
      && !reasoningEfforts.includes(provider.reasoningEffort)) {
      fail(
        'INVALID_CONFIG',
        `the DSH catalog model does not support reasoning effort ${provider.reasoningEffort}`,
      )
    }
    this.#modelInfo = Object.freeze({
      name: modelInfo.name,
      contextWindow,
      inputModalities: Object.freeze(inputModalities),
      reasoningEfforts: Object.freeze(reasoningEfforts),
    })
    const runtime = this
    const factoryPlugin = context => {
      new WinWinCodeAgentFactory(
        context,
        { home: runtime.#home, roleId: 'chat' },
        options => {
          if (runtime.#kernel !== undefined) {
            fail('SECOND_KERNEL_REJECTED', 'evaluation attempted to create another Codex kernel')
          }
          runtime.#kernel = new WinWinCodeKernel(options)
          return runtime.#kernel
        },
      )
    }
    factoryPlugin.inject = ['agents', 'sessions', 'llm', 'systemPrompt', 'approval']
    await this.#context.plugin(factoryPlugin)
  }

  get modelInfo() {
    return this.#modelInfo
  }

  async createRole(options) {
    const provider = this.#provider
    const handle = await this.#context.agents.create({
      sessionId: SessionId(options.sessionId),
      meta: { cwd: options.workspace ?? this.#workspace, agentPreset: options.role },
      agentOptions: {
        provider: provider.route,
        model: provider.model,
        maxTokens: this.#budget.snapshot.limits.maxTokensPerCall,
      },
      setup(agentContext) {
        installModelSelection(agentContext, {
          current: {
            provider: provider.route,
            model: provider.model,
            ...(provider.reasoningEffort === null
              ? {}
              : { reasoningEffort: provider.reasoningEffort }),
          },
          assembled: undefined,
        })
      },
    })
    this.#handles.add(handle)
    const ledger = await RuntimeSessionLedger.open(this.#home, handle.agent.id)
      .then(value => value.read())
    return new LiveRoleSession({
      budget: this.#budget,
      handle,
      home: this.#home,
      role: options.role,
      stageRunId: options.stageRunId,
      bindingId: options.bindingId,
      codexSessionId: ledger.manifest.kernelSessionId,
      rolloutPath: ledger.manifest.rolloutPath,
    })
  }

  cancelAll(reason) {
    for (const handle of this.#handles) handle.agent.cancel({ kind: 'hook', reason })
  }

  async close() {
    for (const handle of this.#handles) await handle.dispose().catch(() => undefined)
    this.#handles.clear()
    await this.#context.fiber.dispose().catch(() => undefined)
  }
}

function requestId(config, suffix) {
  return `eval:${config.runId}:${suffix}`
}

function identity(config, suffix) {
  return `${config.runId}:${suffix}`
}

function draftSpec(spec) {
  return parseDeliverySpec({
    ...spec,
    id: `draft-spec:${sha256(stableJson(spec)).slice(0, 32)}`,
    revision: spec.revision - 1,
    createdAtMillis: Math.max(0, spec.createdAtMillis - 1),
  }, 'derivedDraftSpec')
}

async function serviceCall(service, operation, input) {
  try {
    return await service[operation](input)
  } catch (error) {
    return fail('DELIVERY_MUTATION_FAILED', `StrongFlow ${operation} failed`, error)
  }
}

async function startStage(service, config, delivery, options) {
  return serviceCall(service, 'startStage', {
    requestId: requestId(config, `start:${options.stageRunId}`),
    deliveryId: delivery.id,
    expectedRevision: delivery.revision,
    stageRunId: options.stageRunId,
    deliveryTaskId: null,
    stage: options.stage,
    actorType: options.actorType,
    role: options.role,
    attention: options.attention ?? null,
  })
}

async function bindRole(service, config, delivery, roleSession) {
  return serviceCall(service, 'bindSession', {
    requestId: requestId(config, `bind:${roleSession.bindingId}`),
    deliveryId: delivery.id,
    expectedRevision: delivery.revision,
    bindingId: roleSession.bindingId,
    stageRunId: roleSession.stageRunId,
    dshSessionId: roleSession.dshSessionId,
    codexSessionId: roleSession.codexSessionId,
  })
}

async function bindHuman(service, config, delivery, stageRunId, suffix) {
  return serviceCall(service, 'bindSession', {
    requestId: requestId(config, `bind:human:${suffix}`),
    deliveryId: delivery.id,
    expectedRevision: delivery.revision,
    bindingId: identity(config, `binding:human:${suffix}`),
    stageRunId,
    dshSessionId: identity(config, `human-session:${suffix}`),
    codexSessionId: null,
  })
}

function plannerPrompt(config) {
  return JSON.stringify({
    protocol: 'winwincode.live-evaluation-planning.v1',
    deliverySpec: config.deliverySpec,
    approvedSolution: config.solution,
    instruction: [
      'Use Codex update_plan and Agent collaboration only when useful.',
      'Prepare an execution plan for this exact DeliverySpec and solution.',
      'Do not modify the repository and do not create another task authority.',
    ],
  })
}

function executorPrompt(config) {
  return JSON.stringify({
    protocol: 'winwincode.live-evaluation-execution.v1',
    deliverySpec: config.deliverySpec,
    approvedSolution: config.solution,
    candidateCommitMessage: config.execution.commitMessage,
    instruction: [
      'Implement the exact approved delivery in this isolated candidate workspace.',
      'Run the checks needed to support the acceptance criteria.',
      'Leave every intended source change in the Git worktree; do not create a commit.',
      'The stage controller will freeze those exact changes into the named candidate commit.',
      'Do not approve or verify your own work.',
    ],
  })
}

function firstVerificationPrompt(assignment) {
  return [
    serializeIndependentVerificationSessionInput(assignment),
    '',
    'Evidence collection turn:',
    '- Inspect the exact candidate and run one or more relevant checks through Codex tools.',
    '- Do not modify the candidate.',
    '- End this turn with a short plain-text observation.',
    '- Do not emit the final winwincode.independent-verification-result.v1 JSON yet.',
    'A second turn will supply the exact normalized RuntimeEvent IDs you may cite.',
  ].join('\n')
}

function finalVerificationPrompt(assignment, evidence) {
  return [
    'Return the final verification result now as one plain JSON object and no markdown.',
    `Role: ${assignment.role}.`,
    `Result contract: ${JSON.stringify(assignment.sessionInput.resultContract)}.`,
    'The following exact normalized evidence sources were observed in your earlier turn:',
    JSON.stringify(evidence),
    'Every evidence_sources object must be a byte-for-byte copy of one citation object from this list; do not copy outcome.',
    'Treat citation.type as an opaque system label. Never infer or reclassify it from what a command does.',
    `Example: for allowlist entry ${JSON.stringify(evidence[0])}, copy only ${JSON.stringify(evidence[0].citation)} into evidence_sources.`,
    'Evaluate every required criterion. Keep delivery_spec_id, revision, and candidate_ref exact.',
  ].join('\n')
}

function correctedVerificationPrompt(assignment, evidence, reasonCode, citationDetails) {
  return [
    'Correct the rejected verification result now as one plain JSON object and no markdown.',
    `The previous result was rejected with ${reasonCode}; do not repeat or quote it.`,
    ...(citationDetails.length === 0
      ? []
      : [
          `Rejected citation details: ${JSON.stringify(citationDetails)}.`,
          'Remove every rejected pair. For the same event_id, use only one of its allowed_for_event citation objects.',
        ]),
    'Serialize a new strict JSON object. Ensure every string is valid JSON and contains no literal double-quote characters.',
    `Role: ${assignment.role}.`,
    `Result contract: ${JSON.stringify(assignment.sessionInput.resultContract)}.`,
    'The following exact normalized evidence sources were observed in your earlier turn:',
    JSON.stringify(evidence),
    'Every evidence_sources object must be a byte-for-byte copy of one citation object from this list; do not copy outcome.',
    'Treat citation.type as an opaque system label. Never infer or reclassify it from what a command does.',
    `Example: for allowlist entry ${JSON.stringify(evidence[0])}, copy only ${JSON.stringify(evidence[0].citation)} into evidence_sources.`,
    'Evaluate every required criterion. Keep delivery_spec_id, revision, and candidate_ref exact.',
  ].join('\n')
}

async function collectAllEvents(home, delivery) {
  const events = []
  for (const binding of delivery.sessionBindings) {
    if (binding.dshSessionId === null || binding.codexSessionId === null) continue
    try {
      const stored = await RuntimeSessionLedger.open(home, binding.dshSessionId)
        .then(ledger => ledger.read())
      events.push(...stored.events)
    } catch {
      continue
    }
  }
  return Object.freeze(events)
}

function evidenceForBinding(delivery, runtimeEvents, bindingId) {
  const projection = new DeliveryRuntimeProjection({ delivery }).replay(runtimeEvents)
  const session = projection.stages.flatMap(stage => stage.sessions)
    .find(entry => entry.binding.id === bindingId)
  if (session === undefined) fail('EVIDENCE_MISSING', `runtime projection lacks ${bindingId}`)
  const evidence = session.evidenceLinks
    .filter(link => link.type !== 'review_finding')
    .map(link => Object.freeze({
      citation: Object.freeze({
        type: link.type,
        event_id: link.eventId,
      }),
      outcome: link.outcome,
    }))
  if (evidence.length === 0) {
    fail('EVIDENCE_MISSING', `verification session ${bindingId} produced no citable evidence`)
  }
  return Object.freeze(evidence)
}

function citationKey(citation) {
  return `${citation.type}\u0000${citation.event_id}`
}

function rejectedCitationDetails(options) {
  const allowedByKey = new Map(options.evidence.map(entry => (
    [citationKey(entry.citation), entry.citation]
  )))
  const allowedByEvent = new Map()
  for (const citation of allowedByKey.values()) {
    const matching = allowedByEvent.get(citation.event_id) ?? []
    matching.push(citation)
    allowedByEvent.set(citation.event_id, matching)
  }
  const sessionEvents = options.runtimeEvents.filter(event => (
      event.source.sessionId === options.session.dshSessionId
      && event.source.kernelSessionId === options.session.codexSessionId
  ))
  const latestTurnStart = sessionEvents
    .filter(event => event.kind === 'turn.started')
    .reduce((latest, event) => {
      const sequence = BigInt(event.cursor.sequence)
      return sequence > latest ? sequence : latest
    }, 0n)
  const latest = sessionEvents
    .filter(event => (
      BigInt(event.cursor.sequence) >= latestTurnStart
      && event.semantic?.kind === 'verification-result'
    ))
    .toSorted((left, right) => (
      BigInt(left.cursor.sequence) < BigInt(right.cursor.sequence) ? 1 : -1
    ))[0]
  if (latest?.semantic?.kind !== 'verification-result') return Object.freeze([])
  const rejected = new Map()
  for (const source of latest.semantic.findings.flatMap(finding => finding.evidenceSources)) {
    const citation = Object.freeze({ type: source.type, event_id: source.eventId })
    const key = citationKey(citation)
    if (!allowedByKey.has(key)) rejected.set(key, citation)
  }
  return Object.freeze([...rejected.values()].map(citation => Object.freeze({
    rejected: citation,
    allowed_for_event: Object.freeze(allowedByEvent.get(citation.event_id) ?? []),
  })))
}

function verificationResultStatus(options) {
  const citationDetails = rejectedCitationDetails(options)
  if (citationDetails.length > 0) {
    return Object.freeze({
      accepted: false,
      reasonCode: 'RESULT_EVIDENCE_MISMATCH',
      citationDetails,
    })
  }
  try {
    const projection = projectIndependentVerification({
      delivery: options.delivery,
      acceptance: options.acceptance,
      candidate: options.candidate,
      runtimeEvents: options.runtimeEvents,
      requiredRoles: verificationRoles,
    })
    const settlement = projection.sessions.find(session => (
      session.assignment?.sessionBindingId === options.sessionBindingId
    ))
    if (settlement?.state === 'settled' && settlement.findings.length > 0) {
      return Object.freeze({ accepted: true, reasonCode: null, citationDetails: [] })
    }
    return Object.freeze({
      accepted: false,
      reasonCode: `RESULT_${(settlement?.state ?? 'missing').toUpperCase()}`,
      citationDetails: [],
    })
  } catch (error) {
    if (error instanceof IndependentVerificationError) {
      return Object.freeze({
        accepted: false,
        reasonCode: error.code,
        citationDetails: [],
      })
    }
    throw error
  }
}

async function collectStructuredVerificationResult(options) {
  let reasonCode = 'RESULT_MISSING'
  let citationDetails = []
  for (let attempt = 1; attempt <= verificationResultAttemptLimit; attempt += 1) {
    await options.session.turn(attempt === 1
      ? finalVerificationPrompt(options.assignment, options.evidence)
      : correctedVerificationPrompt(
          options.assignment,
          options.evidence,
          reasonCode,
          citationDetails,
        ))
    await options.assertCandidate()
    const runtimeEvents = await options.readRuntimeEvents()
    const status = verificationResultStatus({
      delivery: options.delivery,
      acceptance: options.acceptance,
      candidate: options.candidate,
      runtimeEvents,
      sessionBindingId: options.session.bindingId,
      session: options.session,
      evidence: options.evidence,
    })
    if (status.accepted) return runtimeEvents
    reasonCode = status.reasonCode
    citationDetails = status.citationDetails
  }
  fail(
    'VERIFICATION_RESULT_INVALID',
    `${options.assignment.role} exhausted the structured verification-result correction limit`,
  )
}

async function freezeCandidateFacts(repository, baseCommitId, commitMessage, signal) {
  const headBeforeFreeze = await git(repository, ['rev-parse', 'HEAD'], { signal })
  if (headBeforeFreeze !== baseCommitId) {
    fail(
      'CANDIDATE_HEAD_CHANGED',
      'executor changed Git history instead of leaving source changes for candidate freeze',
    )
  }
  const statusBeforeFreeze = await git(
    repository,
    ['status', '--porcelain=v1', '-z', '--untracked-files=all'],
    { signal },
  )
  if (statusBeforeFreeze.length === 0) {
    fail('CANDIDATE_MISSING', 'executor produced no source change to freeze')
  }
  await checkedProcess('git', [
    '-C',
    repository,
    'add',
    '--all',
  ], { signal, errorCode: 'CANDIDATE_FREEZE_FAILED', label: 'candidate source freeze' })
  const stagedPaths = await checkedProcess('git', [
    '-C',
    repository,
    'diff',
    '--cached',
    '--name-only',
    '-z',
  ], { signal, errorCode: 'CANDIDATE_FREEZE_FAILED', label: 'candidate staged paths' })
  if (stagedPaths.stdout.length === 0) {
    fail('CANDIDATE_MISSING', 'executor changes do not produce a Git candidate')
  }
  await checkedProcess('git', [
    '-C',
    repository,
    '-c',
    'commit.gpgSign=false',
    '-c',
    'core.hooksPath=/dev/null',
    'commit',
    '--no-verify',
    '-m',
    commitMessage,
  ], { signal, errorCode: 'CANDIDATE_FREEZE_FAILED', label: 'candidate commit freeze' })
  const statusAfterFreeze = await git(
    repository,
    ['status', '--porcelain=v1', '-z', '--untracked-files=all'],
    { signal },
  )
  if (statusAfterFreeze.length > 0) {
    fail('CANDIDATE_DIRTY', 'candidate freeze did not produce a clean Git worktree')
  }
  const candidateCommitId = await git(repository, ['rev-parse', 'HEAD'], { signal })
  await checkedProcess('git', [
    '-C', repository, 'merge-base', '--is-ancestor', baseCommitId, candidateCommitId,
  ], { signal, errorCode: 'CANDIDATE_DIVERGED', label: 'candidate ancestry check' })
  const message = await git(repository, ['log', '-1', '--format=%s', candidateCommitId], { signal })
  const diff = await checkedProcess('git', [
    '-C',
    repository,
    'diff',
    '--no-ext-diff',
    '--binary',
    '--full-index',
    `${baseCommitId}..${candidateCommitId}`,
  ], { signal, errorCode: 'CANDIDATE_DIFF_FAILED', label: 'candidate diff freeze' })
  const paths = await checkedProcess('git', [
    '-C', repository, 'diff', '--name-only', '-z', `${baseCommitId}..${candidateCommitId}`,
  ], { signal, errorCode: 'CANDIDATE_DIFF_FAILED', label: 'candidate changed paths' })
  const changedPaths = []
  for (const path of paths.stdout.toString('utf8').split('\u0000').filter(Boolean)) {
    const object = await runProcess('git', [
      '-C', repository, 'rev-parse', '--verify', `${candidateCommitId}:${path}`,
    ], { signal })
    changedPaths.push(Object.freeze({
      path,
      state: object.code === 0 ? 'present' : 'deleted',
      objectId: object.code === 0 ? object.stdout.toString('utf8').trim() : null,
    }))
  }
  if (changedPaths.length === 0) fail('CANDIDATE_MISSING', 'candidate commit has no changed path')
  return Object.freeze({
    baseCommitId,
    baseTreeId: await git(repository, ['rev-parse', `${baseCommitId}^{tree}`], { signal }),
    candidateCommitId,
    candidateTreeId: await git(repository, ['rev-parse', `${candidateCommitId}^{tree}`], {
      signal,
    }),
    diffSha256: sha256(diff.stdout),
    changedPaths: Object.freeze(changedPaths.toSorted((left, right) => (
      left.path.localeCompare(right.path)
    ))),
    commitMessage: message,
  })
}

function deliveryReviewAttention(config, delivery, stageRunId) {
  return Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: identity(config, 'attention:delivery-review'),
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    stageRunId,
    type: 'delivery_approval',
    title: 'Approve the exact evaluated candidate and Verdict',
    context: 'Review the current passing Verdict, candidate identity, and direct evidence.',
    options: Object.freeze([Object.freeze({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'approve-delivery',
      label: 'Approve delivery',
      description: 'Deliver the exact current candidate and passing Verdict.',
    })]),
    assignedTo: 'evaluation-human',
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: delivery.updatedAtMillis,
    resolvedAtMillis: null,
  })
}

function safeRuntimeProjection(delivery, runtimeEvents) {
  const projection = new DeliveryRuntimeProjection({ delivery }).replay(runtimeEvents)
  return Object.freeze({
    schemaVersion: projection.schemaVersion,
    deliveryId: projection.deliveryId,
    deliveryRevision: projection.deliveryRevision,
    stages: projection.stages.map(stage => Object.freeze({
      stageRun: stage.stageRun,
      sessions: stage.sessions.map(session => Object.freeze({
        binding: session.binding,
        asOfSequence: session.asOfSequence,
        plan: session.plan === null ? null : Object.freeze({
          items: session.plan.items,
          complete: session.plan.complete,
          firstEvent: session.plan.firstEvent,
          latestEvent: session.plan.latestEvent,
        }),
        agents: session.agents,
        agentEdges: session.agentEdges,
        activities: session.activities.map(activity => Object.freeze({
          callId: activity.callId,
          activityType: activity.activityType,
          status: activity.status,
          outcome: activity.outcome,
          exitCode: activity.exitCode,
          firstEvent: activity.firstEvent,
          latestEvent: activity.latestEvent,
        })),
        interactions: session.interactions.map(interaction => Object.freeze({
          id: interaction.id,
          operationId: interaction.operationId,
          interactionType: interaction.interactionType,
          blocking: interaction.blocking,
          status: interaction.status,
          requestedEvent: interaction.requestedEvent,
          resolvedEvent: interaction.resolvedEvent,
        })),
        failures: session.failures.map(failure => Object.freeze({
          code: failure.code,
          event: failure.event,
        })),
        recovery: session.recovery,
        diff: session.diff === null ? null : Object.freeze({
          changedFiles: session.diff.changedFiles,
          additions: session.diff.additions,
          deletions: session.diff.deletions,
          event: session.diff.event,
        }),
        usage: session.usage,
        evidenceLinks: session.evidenceLinks,
        attentionCandidates: session.attentionCandidates.map(candidate => Object.freeze({
          type: candidate.type,
          blocking: candidate.blocking,
          status: candidate.status,
          stageRunId: candidate.stageRunId,
          sessionBindingId: candidate.sessionBindingId,
          sourceRef: candidate.sourceRef,
        })),
      })),
    })),
  })
}

function safeError(error, phase) {
  const code = typeof error?.code === 'string' ? error.code : 'RUNTIME_FAILURE'
  return Object.freeze({
    phase,
    code,
    category: error instanceof LiveEvaluationBudgetError
      ? 'budget'
      : error instanceof LiveEvaluationError
        ? 'evaluation'
        : 'runtime',
  })
}

async function checkpointDelivery(journal, delivery, budget, extra = {}) {
  await journal.patch({
    budget: budget.snapshot,
    delivery,
    ...extra,
  })
}

/**
 * Run the fixed Delivery-stage evaluator. It controls business stages only;
 * DSH and the single embedded Codex kernel retain every Agent/tool decision.
 */
export async function runLiveEvaluation(options) {
  if (!isRecord(options)
    || options.optIn !== true
    || typeof options.outputDirectory !== 'string'
    || options.outputDirectory.trim().length === 0) {
    fail('OPT_IN_REQUIRED', 'live evaluation requires optIn: true and an output directory')
  }
  const allowedOptionKeys = new Set([
    'optIn',
    'config',
    'outputDirectory',
    'signal',
    'environment',
  ])
  if (Object.keys(options).some(key => !allowedOptionKeys.has(key))) {
    fail('INVALID_OPTIONS', 'live evaluation options contain an unsupported field')
  }
  if (options.environment !== undefined && !isRecord(options.environment)) {
    fail('INVALID_OPTIONS', 'live evaluation environment must be a plain object')
  }
  if (options.signal !== undefined
    && (typeof options.signal?.aborted !== 'boolean'
      || typeof options.signal?.addEventListener !== 'function'
      || typeof options.signal?.removeEventListener !== 'function')) {
    fail('INVALID_OPTIONS', 'live evaluation signal must be an AbortSignal')
  }
  const config = parseLiveEvaluationConfig(options.config)
  const environment = options.environment ?? process.env
  const secrets = selectedSecrets(config, environment)
  if (containsSecret(config, secrets)) {
    fail('RAW_CREDENTIAL_REJECTED', 'live evaluation config contains credential material')
  }
  const outputDirectory = resolve(options.outputDirectory)
  await mkdir(outputDirectory, { recursive: true })
  const runRoot = join(outputDirectory, config.runId)
  let lock
  try {
    lock = await open(`${runRoot}.lock`, 'wx', 0o600)
  } catch (error) {
    const code = typeof error?.code === 'string' ? error.code : null
    return fail(
      code === 'EEXIST' ? 'RUN_EXISTS' : 'OUTPUT_INIT_FAILED',
      code === 'EEXIST'
        ? `evaluation run ${config.runId} already exists`
        : 'evaluation output lock could not be created',
      error,
    )
  }
  try {
    await mkdir(runRoot, { recursive: false })
  } catch (error) {
    await lock.close().catch(() => undefined)
    await rm(`${runRoot}.lock`, { force: true }).catch(() => undefined)
    const code = typeof error?.code === 'string' ? error.code : null
    return fail(
      code === 'EEXIST' ? 'RUN_EXISTS' : 'OUTPUT_INIT_FAILED',
      code === 'EEXIST'
        ? `evaluation run ${config.runId} already exists`
        : 'evaluation result directory could not be created',
      error,
    )
  }
  const startedAtMillis = Date.now()
  const budget = new LiveEvaluationBudget(config.budgets, startedAtMillis)
  let journal
  try {
    journal = await LiveEvaluationJournal.create(runRoot, {
      schemaVersion: LIVE_EVALUATION_SCHEMA_VERSION,
      runId: config.runId,
      state: 'running',
      phase: 'initializing',
      phaseHistory: [{ phase: 'initializing', atMillis: startedAtMillis }],
      startedAtMillis,
      updatedAtMillis: startedAtMillis,
      finishedAtMillis: null,
      configSha256: configDigest(config),
      projectionVersion: config.projectionVersion,
      inputs: {
        deliverySpec: config.deliverySpec,
        deliverySpecSha256: sha256(stableJson(config.deliverySpec)),
        solution: config.solution,
        solutionSha256: sha256(stableJson(config.solution)),
        humanDecisions: config.humanDecisions,
        humanDecisionsSha256: sha256(stableJson(config.humanDecisions)),
        execution: config.execution,
        executionSha256: sha256(stableJson(config.execution)),
      },
      provider: {
        catalog: 'dsh-pi-ai',
        route: config.provider.route,
        model: config.provider.model,
        credentialRef: config.provider.apiKeyEnv,
        endpointOverride: config.provider.baseURL,
        reasoningEffort: config.provider.reasoningEffort,
        modelInfo: null,
      },
      repository: {
        sourcePath: config.repository.sourcePath,
        expectedCommit: config.repository.expectedCommit,
        workspace: join(runRoot, 'workspace'),
        reviewWorkspace: join(runRoot, 'review-workspace'),
        baseCommitId: null,
        baseTreeId: null,
      },
      sourceIdentities: null,
      preflight: null,
      budget: budget.snapshot,
      delivery: null,
      candidate: null,
      runtimeProjection: null,
      measures: null,
      error: null,
    }, secrets)
  } catch (error) {
    await lock.close().catch(() => undefined)
    await rm(`${runRoot}.lock`, { force: true }).catch(() => undefined)
    await rm(runRoot, { recursive: true, force: true }).catch(() => undefined)
    return fail('OUTPUT_INIT_FAILED', 'initial evaluation result could not be written', error)
  }
  let runtime
  let delivery = null
  let candidate = null
  const abortController = new AbortController()
  const externalSignal = options.signal
  const forwardAbort = () => abortController.abort(externalSignal.reason)
  externalSignal?.addEventListener('abort', forwardAbort, { once: true })
  if (externalSignal?.aborted) forwardAbort()
  const wallTimer = setTimeout(() => {
    abortController.abort(new LiveEvaluationBudgetError(
      'WALL_TIME_BUDGET_EXCEEDED',
      'evaluation wall-time budget is exhausted',
    ))
    runtime?.cancelAll('evaluation wall-time budget exhausted')
  }, config.budgets.maxWallTimeMillis)

  try {
    await journal.phase('source-identity')
    const identities = await sourceIdentities()
    await journal.patch({ sourceIdentities: identities })

    await journal.phase('preflight')
    const preflight = await runLiveEvaluationPreflight(abortController.signal)
    await journal.patch({ preflight })

    await journal.phase('repository')
    const repository = await verifyAndCloneRepository(config, runRoot, abortController.signal)
    await journal.patch({
      repository: {
        ...journal.state.repository,
        baseCommitId: repository.baseCommitId,
        baseTreeId: repository.baseTreeId,
      },
    })

    const proof = randomBytes(32).toString('base64url')
    const service = new StrongFlowService({
      home: join(runRoot, 'home'),
      authenticator: createStrongFlowDeliveryLocalProofAuthenticator({
        localSessionProof: proof,
        localSessionActorId: 'evaluation-human',
      }),
    })
    runtime = await LiveDshRuntime.create({
      home: join(runRoot, 'home'),
      workspace: repository.workspace,
      provider: config.provider,
      budget,
      environment,
    })
    await journal.patch({
      provider: {
        ...journal.state.provider,
        modelInfo: runtime.modelInfo,
      },
    })
    abortController.signal.addEventListener('abort', () => {
      runtime.cancelAll('evaluation interrupted')
    }, { once: true })

    const created = await serviceCall(service, 'createDelivery', {
      requestId: requestId(config, 'create-delivery'),
      spec: draftSpec(config.deliverySpec),
      tasks: [],
    })
    delivery = await serviceCall(service, 'updateDeliverySpec', {
      requestId: requestId(config, 'approve-delivery-spec'),
      deliveryId: created.id,
      expectedRevision: created.revision,
      spec: config.deliverySpec,
    })
    await checkpointDelivery(journal, delivery, budget)

    await journal.phase('planning')
    const planningStageRunId = identity(config, 'stage:planning')
    delivery = await startStage(service, config, delivery, {
      stageRunId: planningStageRunId,
      stage: 'planning',
      actorType: 'codex',
      role: 'planner',
    })
    const planner = await runtime.createRole({
      sessionId: identity(config, 'session:planner'),
      role: 'planner',
      stageRunId: planningStageRunId,
      bindingId: identity(config, 'binding:planner'),
    })
    delivery = await bindRole(service, config, delivery, planner)
    await checkpointDelivery(journal, delivery, budget)
    await planner.turn(plannerPrompt(config))
    await disposeCompletedRoleSession(planner)
    await checkpointDelivery(journal, delivery, budget)

    await journal.phase('plan-review')
    const planReviewStageRunId = identity(config, 'stage:plan-review')
    const planAttention = createStrongFlowPlanReviewAttention({
      delivery,
      attentionItemId: identity(config, 'attention:plan-review'),
      reviewStageRunId: planReviewStageRunId,
      assignedTo: 'evaluation-human',
      solution: config.solution,
      risks: [],
      unresolvedItems: [],
      preparedAtMillis: delivery.updatedAtMillis,
    })
    delivery = await startStage(service, config, delivery, {
      stageRunId: planReviewStageRunId,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      attention: planAttention,
    })
    delivery = await bindHuman(
      service,
      config,
      delivery,
      planReviewStageRunId,
      'plan-review',
    )
    const planContext = parseStrongFlowPlanReviewContextText(planAttention.context)
    const planDecision = createStrongFlowPlanReviewDecision({
      context: planContext,
      action: config.humanDecisions.planReview.action,
      comments: config.humanDecisions.planReview.comments,
      requestedChanges: config.humanDecisions.planReview.requestedChanges,
    })
    delivery = await serviceCall(service, 'resolveAttention', {
      requestId: requestId(config, 'approve-plan-review'),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      attentionItemId: planAttention.id,
      status: 'resolved',
      resolution: JSON.stringify(planDecision),
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof },
    })
    await checkpointDelivery(journal, delivery, budget)

    await journal.phase('executing')
    const executionStageRunId = identity(config, 'stage:executing')
    delivery = await startStage(service, config, delivery, {
      stageRunId: executionStageRunId,
      stage: 'executing',
      actorType: 'codex',
      role: 'executor',
    })
    const executor = await runtime.createRole({
      sessionId: identity(config, 'session:executor'),
      role: 'executor',
      stageRunId: executionStageRunId,
      bindingId: identity(config, 'binding:executor'),
    })
    delivery = await bindRole(service, config, delivery, executor)
    await checkpointDelivery(journal, delivery, budget)
    await executor.turn(executorPrompt(config))
    await disposeCompletedRoleSession(executor)
    const repositoryCandidate = await freezeCandidateFacts(
      repository.workspace,
      repository.baseCommitId,
      config.execution.commitMessage,
      abortController.signal,
    )
    if (repositoryCandidate.commitMessage !== config.execution.commitMessage) {
      fail('CANDIDATE_COMMIT_MISMATCH', 'candidate freeze did not use the pinned commit message')
    }
    await prepareReviewWorkspace(
      repository.workspace,
      repository.reviewWorkspace,
      repositoryCandidate.candidateCommitId,
      abortController.signal,
    )

    await journal.phase('reviewing')
    const reviewerStageRunId = identity(config, 'stage:reviewer')
    delivery = await startStage(service, config, delivery, {
      stageRunId: reviewerStageRunId,
      stage: 'verifying',
      actorType: 'codex',
      role: 'reviewer',
    })
    const reviewer = await runtime.createRole({
      sessionId: identity(config, 'session:reviewer'),
      role: 'reviewer',
      stageRunId: reviewerStageRunId,
      bindingId: identity(config, 'binding:reviewer'),
      workspace: repository.reviewWorkspace,
    })
    delivery = await bindRole(service, config, delivery, reviewer)
    candidate = freezeDeliveryCandidate(delivery, {
      producerStageRunId: executionStageRunId,
      producerSessionBindingId: executor.bindingId,
      baseCommitId: repositoryCandidate.baseCommitId,
      baseTreeId: repositoryCandidate.baseTreeId,
      candidateCommitId: repositoryCandidate.candidateCommitId,
      candidateTreeId: repositoryCandidate.candidateTreeId,
      diffSha256: repositoryCandidate.diffSha256,
      changedPaths: repositoryCandidate.changedPaths,
    })
    await checkpointDelivery(journal, delivery, budget, { candidate })
    let acceptance = freezeAcceptanceVerificationInput(delivery)
    const reviewerAssignment = createIndependentVerificationAssignment({
      delivery,
      acceptance,
      candidate,
      stageRunId: reviewer.stageRunId,
      sessionBindingId: reviewer.bindingId,
    })
    await reviewer.turn(firstVerificationPrompt(reviewerAssignment))
    await assertReviewWorkspace(
      repository.reviewWorkspace,
      repositoryCandidate.candidateCommitId,
      abortController.signal,
    )
    let runtimeEvents = await collectAllEvents(join(runRoot, 'home'), delivery)
    const reviewerEvidence = evidenceForBinding(delivery, runtimeEvents, reviewer.bindingId)
    runtimeEvents = await collectStructuredVerificationResult({
      session: reviewer,
      assignment: reviewerAssignment,
      evidence: reviewerEvidence,
      delivery,
      acceptance,
      candidate,
      readRuntimeEvents: () => collectAllEvents(join(runRoot, 'home'), delivery),
      assertCandidate: () => assertReviewWorkspace(
        repository.reviewWorkspace,
        repositoryCandidate.candidateCommitId,
        abortController.signal,
      ),
    })
    await disposeCompletedRoleSession(reviewer)
    await checkpointDelivery(journal, delivery, budget)

    await journal.phase('verifying')
    const verifierStageRunId = identity(config, 'stage:verifier')
    delivery = await startStage(service, config, delivery, {
      stageRunId: verifierStageRunId,
      stage: 'verifying',
      actorType: 'codex',
      role: 'verifier',
    })
    const verifier = await runtime.createRole({
      sessionId: identity(config, 'session:verifier'),
      role: 'verifier',
      stageRunId: verifierStageRunId,
      bindingId: identity(config, 'binding:verifier'),
      workspace: repository.reviewWorkspace,
    })
    delivery = await bindRole(service, config, delivery, verifier)
    acceptance = freezeAcceptanceVerificationInput(delivery)
    const verifierAssignment = createIndependentVerificationAssignment({
      delivery,
      acceptance,
      candidate,
      stageRunId: verifier.stageRunId,
      sessionBindingId: verifier.bindingId,
    })
    await checkpointDelivery(journal, delivery, budget)
    await verifier.turn(firstVerificationPrompt(verifierAssignment))
    await assertReviewWorkspace(
      repository.reviewWorkspace,
      repositoryCandidate.candidateCommitId,
      abortController.signal,
    )
    runtimeEvents = await collectAllEvents(join(runRoot, 'home'), delivery)
    const verifierEvidence = evidenceForBinding(delivery, runtimeEvents, verifier.bindingId)
    runtimeEvents = await collectStructuredVerificationResult({
      session: verifier,
      assignment: verifierAssignment,
      evidence: verifierEvidence,
      delivery,
      acceptance,
      candidate,
      readRuntimeEvents: () => collectAllEvents(join(runRoot, 'home'), delivery),
      assertCandidate: () => assertReviewWorkspace(
        repository.reviewWorkspace,
        repositoryCandidate.candidateCommitId,
        abortController.signal,
      ),
    })
    await disposeCompletedRoleSession(verifier)
    delivery = await serviceCall(service, 'submitVerdict', {
      requestId: requestId(config, 'submit-verdict'),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      candidate,
      runtimeEvents,
      requiredRoles: verificationRoles,
    })
    await checkpointDelivery(journal, delivery, budget, {
      candidate,
      runtimeProjection: safeRuntimeProjection(delivery, runtimeEvents),
    })
    if (delivery.status !== 'ready-to-deliver' || delivery.verdict?.status !== 'pass') {
      fail('DELIVERY_NOT_READY', 'independent verification did not produce a passing Delivery')
    }

    await journal.phase('delivery-review')
    const deliveryReviewStageRunId = identity(config, 'stage:delivery-review')
    const finalAttention = deliveryReviewAttention(config, delivery, deliveryReviewStageRunId)
    delivery = await startStage(service, config, delivery, {
      stageRunId: deliveryReviewStageRunId,
      stage: 'delivery-review',
      actorType: 'human',
      role: 'approver',
      attention: finalAttention,
    })
    delivery = await bindHuman(
      service,
      config,
      delivery,
      deliveryReviewStageRunId,
      'delivery-review',
    )
    delivery = await serviceCall(service, 'resolveAttention', {
      requestId: requestId(config, 'approve-delivery'),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      attentionItemId: finalAttention.id,
      status: 'resolved',
      resolution: config.humanDecisions.deliveryReview.resolution,
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof },
    })
    runtimeEvents = await collectAllEvents(join(runRoot, 'home'), delivery)
    const finishedAtMillis = Date.now()
    const completedPatch = {
      state: 'completed',
      finishedAtMillis,
      budget: budget.snapshot,
      delivery,
      candidate,
      runtimeProjection: safeRuntimeProjection(delivery, runtimeEvents),
      error: null,
    }
    await journal.phase('completed', {
      ...completedPatch,
      measures: measureLiveEvaluationResult({
        ...journal.state,
        ...completedPatch,
      }),
    })
    return Object.freeze({ path: journal.path, result: journal.state })
  } catch (error) {
    const aborted = abortController.signal.aborted
    const budgetFailure = error instanceof LiveEvaluationBudgetError
      || abortController.signal.reason instanceof LiveEvaluationBudgetError
    const state = budgetFailure ? 'budget-exceeded' : aborted ? 'interrupted' : 'failed'
    let failureProjection = journal.state.runtimeProjection
    if (delivery !== null) {
      try {
        const runtimeEvents = await collectAllEvents(join(runRoot, 'home'), delivery)
        failureProjection = safeRuntimeProjection(delivery, runtimeEvents)
      } catch {
        // Keep the last safely written projection if the incomplete run cannot be replayed.
      }
    }
    const failedPatch = {
      state,
      finishedAtMillis: Date.now(),
      budget: budget.snapshot,
      delivery: delivery ?? journal.state.delivery,
      candidate: candidate ?? journal.state.candidate,
      runtimeProjection: failureProjection,
      error: safeError(
        abortController.signal.reason instanceof Error
          ? abortController.signal.reason
          : error,
        journal.state.phase,
      ),
    }
    await journal.phase(state, {
      ...failedPatch,
      measures: measureLiveEvaluationResult({
        ...journal.state,
        ...failedPatch,
      }),
    }).catch(() => undefined)
    throw Object.assign(error instanceof Error ? error : new Error(String(error)), {
      evaluationResultPath: journal.path,
    })
  } finally {
    clearTimeout(wallTimer)
    externalSignal?.removeEventListener('abort', forwardAbort)
    await runtime?.close().catch(() => undefined)
    await lock.close().catch(() => undefined)
    await rm(`${runRoot}.lock`, { force: true }).catch(() => undefined)
  }
}
