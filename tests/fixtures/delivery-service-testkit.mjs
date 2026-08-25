import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import vm from 'node:vm'

import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import LlmRuntime, {
  LlmAdapter,
  createUserMessage,
} from '@deepseek-ai/dsh-llm'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ApprovalService from '@deepseek-ai/dsh-user-approval'
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'

import {
  DELIVERY_SCHEMA_VERSION,
  materializeStrongFlowDeliveryRequest,
  parseStrongFlowPlanReviewContextText,
} from '../../packages/contracts/dist/index.js'
import {
  CodexRuntimeProjector,
  DeliveryRecoveryError,
  DshRuntimeProjection,
  RuntimeApprovalRouter,
  RuntimeProjectionError,
  RuntimeSessionLedger,
  WinWinCodeAgentFactory,
  reconcileDeliveryAfterRestart,
} from '../../packages/dsh-profile/dist/index.js'
import { WinWinCodeKernel } from '../../packages/native/dist/index.js'
import {
  DeliveryRuntimeProjection,
  DeliveryRuntimeProjectionError,
  DeliveryStore,
  StrongFlowService,
  StrongFlowServiceInvoker,
  createStrongFlowDeliveryLocalProofAuthenticator,
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  freezeDeliveryCandidate,
} from '../../packages/strongflow/dist/index.js'

export const DELIVERY_FIXTURE_BASE_TIME = 2_900_000_000_000
export const DELIVERY_FIXTURE_UI_PROOF = 'fixture-local-session-proof-value'
export const DELIVERY_FIXTURE_CLI_PROOF = 'fixture-local-peer-proof-value'

const DEFAULT_DELIVERY_ID = 'dlv_01J00000000000000000000007'
const DEFAULT_MODEL = 'fixture-coder'
const DEFAULT_PROVIDER = 'fixture'
const CREDENTIAL_ENVIRONMENT_PATTERN = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu
const CANDIDATE_TEST_TIMEOUT_MILLIS = 60_000
const ADOPT_DSH_RUNTIME = Symbol('adoptDshRuntime')

function immutable(value) {
  const clone = structuredClone(value)
  const pending = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function checked(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    cwd: options.cwd,
    encoding: 'utf8',
    env: options.env ?? process.env,
    timeout: options.timeout ?? 30_000,
  })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null || result.status !== 0) {
    throw new Error([
      `${command} ${arguments_.join(' ')} failed`,
      `signal=${result.signal ?? 'none'}`,
      `status=${String(result.status)}`,
      result.stderr.trim(),
      result.stdout.trim(),
    ].filter(Boolean).join('\n'))
  }
  return result.stdout
}

function git(repository, ...arguments_) {
  return checked('git', arguments_, { cwd: repository }).trim()
}

function textBlocks(messages) {
  return messages.flatMap(message => message.content.flatMap(block => (
    block.type === 'text' ? [block.text] : []
  )))
}

function assistantMessages(agent) {
  return agent.session.events.flatMap(event => {
    if (event.type !== 'assistant/message') return []
    return event.data.message.content.flatMap(block => (
      block.type === 'text' ? [block.text] : []
    ))
  })
}

function requestFailure(response) {
  if (response.ok) return null
  return Object.freeze({
    code: response.error.code,
    currentRevision: response.error.currentRevision,
  })
}

function readOnlySessionConfiguration() {
  return {
    approval_policy: 'on-request',
    approvals_reviewer: 'user',
    permission_profile: {
      type: 'managed',
      file_system: {
        type: 'restricted',
        entries: [{
          path: { type: 'special', value: { kind: 'root' } },
          access: 'read',
        }],
      },
      network: 'restricted',
    },
  }
}

export function keylessFixtureEnvironment(environment = process.env) {
  return Object.fromEntries(Object.entries(environment).filter(([name]) => (
    !CREDENTIAL_ENVIRONMENT_PATTERN.test(name)
  )))
}

export function kernelFixtureEvent(
  sequence,
  type,
  data = {},
  submissionId = 'fixture-submission',
) {
  const payload = { id: submissionId, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

/**
 * Deterministic DSH provider stream. It replaces only the external model
 * provider; DSH and the embedded Codex kernel still own the real Agent loop.
 */
export class ScriptedDshModelAdapter extends LlmAdapter {
  calls = []

  #script

  constructor(script) {
    super()
    if (!Array.isArray(script) || script.length === 0) {
      throw new TypeError('scripted DSH adapter requires at least one response')
    }
    this.#script = script.map(entry => immutable({
      text: entry.text,
      usage: entry.usage ?? { inputTokens: 12, outputTokens: 8 },
      finishReason: entry.finishReason ?? 'stop',
      failure: entry.failure ?? null,
    }))
  }

  async *stream(options) {
    const entry = this.#script[this.calls.length]
    if (entry === undefined) {
      throw new Error('scripted DSH adapter response queue is exhausted')
    }
    const prompt = textBlocks(options.messages).at(-1) ?? ''
    this.calls.push(immutable({
      provider: options.provider,
      model: options.model,
      maxTokens: options.maxTokens ?? null,
      purpose: options.purpose ?? null,
      prompt,
      answer: entry.text,
      usage: entry.usage,
    }))
    if (entry.failure !== null) {
      yield {
        type: 'finish',
        reason: {
          kind: 'error',
          failure: entry.failure,
        },
      }
      return
    }
    yield { type: 'block-start', index: 0, blockType: 'text' }
    yield { type: 'text-delta', index: 0, text: entry.text }
    yield {
      type: 'block-end',
      index: 0,
      block: { type: 'text', text: entry.text },
    }
    yield { type: 'usage', usage: entry.usage }
    yield { type: 'finish', reason: { kind: entry.finishReason } }
  }

  get remainingResponses() {
    return this.#script.length - this.calls.length
  }
}

/** Real DSH composition over one embedded Codex kernel and a scripted provider. */
export class ScriptedDshFixtureRuntime {
  #adapter
  #closed = false
  #closePromise
  #context
  #handles = new Set()
  #kernel
  #releaseOwner

  constructor(options) {
    this.home = resolve(options.home)
    this.workspace = resolve(options.workspace)
    this.#adapter = new ScriptedDshModelAdapter(options.script)
    this.#context = new Context()
    this.#kernel = undefined
  }

  static async create(options) {
    if (typeof options.owner?.[ADOPT_DSH_RUNTIME] !== 'function') {
      throw new TypeError('scripted DSH fixture runtime requires its filesystem owner')
    }
    const runtime = new ScriptedDshFixtureRuntime(options)
    runtime.#releaseOwner = options.owner[ADOPT_DSH_RUNTIME](runtime)
    try {
      await runtime.#start()
      return runtime
    } catch (error) {
      try {
        await runtime.close()
      } catch (cleanupError) {
        throw new AggregateError(
          [error, cleanupError],
          'scripted DSH fixture runtime startup and cleanup failed',
        )
      }
      throw error
    }
  }

  get calls() {
    return immutable(this.#adapter.calls)
  }

  get remainingResponses() {
    return this.#adapter.remainingResponses
  }

  get closed() {
    return this.#closed
  }

  async #start() {
    await this.#context.plugin(LlmRuntime)
    await this.#context.plugin(SessionStore)
    await this.#context.plugin(SystemPrompt)
    await this.#context.plugin(AgentRegistry)
    await this.#context.plugin(ApprovalService, { policy: 'never' })

    const adapter = this.#adapter
    const adapterPlugin = pluginContext => {
      pluginContext.llm.registerAdapter([DEFAULT_PROVIDER], adapter)
    }
    adapterPlugin.inject = ['llm']
    await this.#context.plugin(adapterPlugin)

    const runtime = this
    const factoryPlugin = pluginContext => {
      new WinWinCodeAgentFactory(
        pluginContext,
        { home: runtime.home, roleId: 'chat' },
        options => {
          if (runtime.#kernel !== undefined) {
            throw new Error('fixture attempted to create a second embedded Codex kernel')
          }
          runtime.#kernel = new WinWinCodeKernel(options)
          return runtime.#kernel
        },
      )
    }
    factoryPlugin.inject = ['agents', 'sessions', 'llm', 'systemPrompt', 'approval']
    await this.#context.plugin(factoryPlugin)
  }

  async runRole(options) {
    const handle = await this.#context.agents.create({
      sessionId: SessionId(options.sessionId),
      meta: { cwd: this.workspace, agentPreset: options.roleId },
      agentOptions: {
        provider: DEFAULT_PROVIDER,
        model: DEFAULT_MODEL,
        maxTokens: options.maxTokens,
      },
    })
    this.#handles.add(handle)
    handle.agent.followup(createUserMessage({
      content: [{ type: 'text', text: options.prompt }],
      source: { kind: 'user' },
    }))
    await handle.agent.whenIdle()
    const stored = await RuntimeSessionLedger.open(this.home, handle.agent.id)
      .then(ledger => ledger.read())
    const result = immutable({
      dshSessionId: handle.agent.id,
      codexSessionId: stored.manifest.kernelSessionId,
      roleId: stored.manifest.roleId,
      rolloutPath: stored.manifest.rolloutPath,
      configuredMaxTokens: handle.agent.options.maxTokens ?? null,
      events: stored.events,
      assistantMessages: assistantMessages(handle.agent),
    })
    await handle.dispose()
    this.#handles.delete(handle)
    return result
  }

  async close() {
    this.#closePromise ??= this.#close()
    return this.#closePromise
  }

  async #close() {
    const failures = []
    for (const handle of [...this.#handles]) {
      try {
        await handle.dispose()
        this.#handles.delete(handle)
      } catch (error) {
        failures.push(error)
      }
    }
    try {
      await this.#context.fiber.dispose()
    } catch (error) {
      failures.push(error)
    }
    try {
      await this.#kernel?.shutdown()
    } catch (error) {
      failures.push(error)
    }
    if (failures.length > 0) {
      throw new AggregateError(failures, 'scripted DSH fixture runtime cleanup failed')
    }
    this.#closed = true
    this.#releaseOwner?.()
  }
}

export class DeterministicFixtureClock {
  #value

  constructor(start = DELIVERY_FIXTURE_BASE_TIME + 100) {
    if (!Number.isSafeInteger(start) || start < 0) {
      throw new TypeError('fixture clock start must be a non-negative safe integer')
    }
    this.#value = start
    this.now = this.now.bind(this)
  }

  now() {
    this.#value += 1
    return this.#value
  }

  peek() {
    return this.#value
  }
}

async function initializeRepository(repository) {
  await mkdir(join(repository, 'src'), { recursive: true })
  await mkdir(join(repository, 'test'), { recursive: true })
  await writeFile(join(repository, 'src', 'value.mjs'), "export const value = 'before'\n")
  await writeFile(join(repository, 'test', 'value.test.mjs'), [
    "import assert from 'node:assert/strict'",
    "import test from 'node:test'",
    "import { value } from '../src/value.mjs'",
    '',
    "test('fixture candidate', () => { assert.equal(value, 'after') })",
    '',
  ].join('\n'))
  await writeFile(join(repository, 'package.json'), `${JSON.stringify({
    name: 'winwincode-delivery-fixture-repository',
    private: true,
    type: 'module',
    scripts: { test: 'node --test' },
  }, null, 2)}\n`)
  git(repository, 'init', '--initial-branch=main')
  git(repository, 'config', 'user.name', 'WinWinCode Fixture')
  git(repository, 'config', 'user.email', 'fixture@winwincode.invalid')
  git(repository, 'add', '--all')
  checked('git', ['commit', '-m', 'Create deterministic fixture baseline'], {
    cwd: repository,
    env: {
      ...process.env,
      GIT_AUTHOR_DATE: '2025-01-01T00:00:00Z',
      GIT_COMMITTER_DATE: '2025-01-01T00:00:00Z',
    },
  })
  return Object.freeze({
    baseCommitId: git(repository, 'rev-parse', 'HEAD'),
    baseTreeId: git(repository, 'rev-parse', 'HEAD^{tree}'),
  })
}

async function readRepositoryIdentity(repository) {
  return Object.freeze({
    baseCommitId: git(repository, 'rev-list', '--max-parents=0', 'HEAD'),
    baseTreeId: git(repository, 'rev-parse', `${git(repository, 'rev-list', '--max-parents=0', 'HEAD')}^{tree}`),
  })
}

async function createCandidateCommit(repository, options = {}) {
  const value = options.value ?? 'after'
  const expectedTestPass = options.expectedTestPass ?? true
  const baseCommitId = options.baseCommitId
    ?? git(repository, 'rev-list', '--max-parents=0', 'HEAD')
  await writeFile(join(repository, 'src', 'value.mjs'), `export const value = '${value}'\n`)
  git(repository, 'add', '--all')
  checked('git', ['commit', '-m', options.message ?? 'Implement deterministic fixture candidate'], {
    cwd: repository,
    env: {
      ...process.env,
      GIT_AUTHOR_DATE: options.commitDate ?? '2025-01-02T00:00:00Z',
      GIT_COMMITTER_DATE: options.commitDate ?? '2025-01-02T00:00:00Z',
    },
  })
  const candidateCommitId = git(repository, 'rev-parse', 'HEAD')
  const diff = checked('git', [
    'diff',
    '--no-ext-diff',
    '--binary',
    '--full-index',
    `${baseCommitId}..${candidateCommitId}`,
  ], { cwd: repository })
  const verification = spawnSync(process.execPath, ['--test'], {
    cwd: repository,
    encoding: 'utf8',
    env: Object.fromEntries(Object.entries(keylessFixtureEnvironment()).filter(([name]) => (
      name !== 'NODE_TEST_CONTEXT'
    ))),
    timeout: CANDIDATE_TEST_TIMEOUT_MILLIS,
  })
  if (verification.error !== undefined) throw verification.error
  if (verification.signal !== null || verification.status === null) {
    throw new Error(verification.stderr || verification.stdout || 'fixture candidate test did not settle')
  }
  const testPassed = verification.status === 0
  if (testPassed !== expectedTestPass) {
    throw new Error([
      `fixture candidate test unexpectedly ${testPassed ? 'passed' : 'failed'}`,
      verification.stderr,
      verification.stdout,
    ].filter(Boolean).join('\n'))
  }
  return Object.freeze({
    baseCommitId,
    baseTreeId: git(repository, 'rev-parse', `${baseCommitId}^{tree}`),
    candidateCommitId,
    candidateTreeId: git(repository, 'rev-parse', `${candidateCommitId}^{tree}`),
    diff,
    diffSha256: createHash('sha256').update(diff).digest('hex'),
    changedPaths: Object.freeze([Object.freeze({
      path: 'src/value.mjs',
      state: 'present',
      objectId: git(repository, 'rev-parse', `${candidateCommitId}:src/value.mjs`),
    })]),
    testPassed,
    testExitCode: verification.status,
    testOutput: `${verification.stdout}${verification.stderr}`,
  })
}

function solutionFixture(suffix = 'current') {
  return Object.freeze({
    id: `solution-${suffix}`,
    summary: 'Use the approved DeliverySpec to change one local module and verify it.',
    approach: Object.freeze([
      'Keep DSH as the interaction and model-provider shell.',
      'Use the embedded Codex kernel as the only execution authority.',
      'Bind direct evidence to the exact frozen candidate.',
    ]),
    components: Object.freeze([Object.freeze({
      id: `component-${suffix}`,
      label: 'Fixture value module',
      responsibility: 'Produce the observable value required by the acceptance criterion.',
      kind: 'component',
      trustBoundary: 'Local fixture repository',
      unresolved: false,
      repositoryPathPrefixes: Object.freeze(['src']),
    })]),
    connections: Object.freeze([Object.freeze({
      id: `connection-${suffix}`,
      from: 'platform:codex-core',
      to: `component-${suffix}`,
      label: 'Implements the approved local change',
    })]),
  })
}

function planDecision(attention, overrides = {}) {
  const context = parseStrongFlowPlanReviewContextText(attention.context)
  return createStrongFlowPlanReviewDecision({
    context,
    action: overrides.action ?? 'approve',
    comments: overrides.comments ?? 'Approve the exact current fixture review set.',
    requestedChanges: overrides.requestedChanges ?? [],
  })
}

function roleRuntimeEvents(
  delivery,
  candidate,
  role,
  outcome = 'pass',
  stageRunId = null,
) {
  const run = stageRunId === null
    ? delivery.stageRuns.findLast(entry => (
      entry.stage === 'verifying' && entry.role === role
    ))
    : delivery.stageRuns.find(entry => (
      entry.id === stageRunId && entry.stage === 'verifying' && entry.role === role
    ))
  if (run === undefined) throw new Error(`fixture has no ${role} verification StageRun`)
  const binding = delivery.sessionBindings.find(entry => entry.stageRunId === run.id)
  if (binding?.dshSessionId === null || binding?.codexSessionId === null || binding === undefined) {
    throw new Error(`fixture has no complete ${role} SessionBinding`)
  }
  const evidenceType = role === 'reviewer' ? 'command' : 'test'
  const status = outcome === 'fail'
    ? 'failed'
    : outcome === 'timed-out'
      ? 'timed-out'
      : outcome === 'policy-denied'
        ? 'sandbox-denied'
        : 'completed'
  const verdict = outcome === 'fail'
    ? 'fail'
    : outcome === 'timed-out' || outcome === 'policy-denied'
      ? 'infra_error'
      : outcome === 'inconclusive'
        ? 'inconclusive'
        : 'pass'
  const directEvidence = outcome === 'inconclusive'
    ? []
    : [{ type: evidenceType, event_id: `${binding.dshSessionId}@3` }]
  const submissionId = `submission-${role}-${run.id}`
  return new CodexRuntimeProjector({
    sessionId: binding.dshSessionId,
    kernelSessionId: binding.codexSessionId,
    roleId: role,
    kernelStreamId: `stream-${binding.dshSessionId}`,
  }).replay([
    kernelFixtureEvent(1, 'session_configured', {
      session_id: binding.codexSessionId,
      thread_id: binding.codexSessionId,
      model_provider_id: DEFAULT_PROVIDER,
      model: DEFAULT_MODEL,
      occurred_at_ms: binding.boundAtMillis,
      ...readOnlySessionConfiguration(),
    }, submissionId),
    kernelFixtureEvent(2, 'task_started', {
      turn_id: `turn-${role}`,
      started_at_ms: binding.boundAtMillis,
    }, submissionId),
    kernelFixtureEvent(3, 'item_completed', {
      turn_id: `turn-${role}`,
      completed_at_ms: binding.boundAtMillis,
      item: {
        type: 'CommandExecution',
        id: `check-${role}`,
        command: role === 'reviewer'
          ? ['git', 'diff', '--check']
          : ['node', '--test'],
        status,
        ...(status === 'sandbox-denied' ? {} : { exit_code: status === 'completed' ? 0 : 1 }),
        ...(status === 'timed-out' ? { timed_out: true } : {}),
      },
    }, submissionId),
    kernelFixtureEvent(4, 'agent_message', {
      turn_id: `turn-${role}`,
      occurred_at_ms: binding.boundAtMillis,
      phase: 'final_answer',
      message: JSON.stringify({
        protocol: 'winwincode.independent-verification-result.v1',
        delivery_spec_id: delivery.spec.id,
        delivery_spec_revision: delivery.spec.revision,
        candidate_ref: candidate.candidateRef,
        findings: [{
          finding_id: `finding-${role}-${outcome}-${run.attempt}`,
          criterion_id: delivery.spec.acceptanceCriteria[0].id,
          verdict,
          explanation: `${role} produced the controlled ${outcome} fixture result.`,
          evidence_sources: directEvidence,
        }],
      }),
    }, submissionId),
    kernelFixtureEvent(5, 'task_complete', {
      turn_id: `turn-${role}`,
      completed_at_ms: binding.boundAtMillis,
      last_agent_message: `${role} fixture complete`,
      error: null,
    }, submissionId),
  ])
}

function candidateWriterKernelEvents(
  delivery,
  stageRunId,
  diff,
  testPassed = true,
) {
  const run = delivery.stageRuns.find(entry => entry.id === stageRunId)
  const binding = delivery.sessionBindings.find(entry => entry.stageRunId === run?.id)
  if (run === undefined
    || (run.stage !== 'executing' && run.stage !== 'reworking')
    || binding?.dshSessionId === null
    || binding?.codexSessionId === null
    || binding === undefined) {
    throw new Error(`fixture candidate-writer StageRun ${stageRunId} is not completely bound`)
  }
  const eventSuffix = `${run.role}-${String(run.attempt)}`
  const submissionId = `submission-${eventSuffix}`
  return [
    kernelFixtureEvent(1, 'session_configured', {
      session_id: binding.codexSessionId,
      thread_id: binding.codexSessionId,
      model_provider_id: DEFAULT_PROVIDER,
      model: DEFAULT_MODEL,
    }, submissionId),
    kernelFixtureEvent(2, 'task_started', {
      turn_id: 'turn-executor',
      started_at_ms: run.startedAtMillis,
    }, submissionId),
    kernelFixtureEvent(3, 'plan_update', {
      explanation: 'Project the Codex-owned plan without taking over scheduling.',
      plan: [
        { step: 'Inspect the local fixture', status: 'completed' },
        { step: 'Produce the candidate', status: 'completed' },
        { step: 'Wait for independent verification', status: 'in_progress' },
      ],
    }, submissionId),
    kernelFixtureEvent(4, 'collab_agent_spawn_end', {
      call_id: `spawn-fixture-${eventSuffix}`,
      sender_thread_id: binding.codexSessionId,
      new_thread_id: `codex-fixture-subagent-${eventSuffix}`,
      new_agent_nickname: `fixture-${run.role}-reviewer`,
      new_agent_role: 'review',
      prompt: 'Inspect the local candidate.',
      model: DEFAULT_MODEL,
      reasoning_effort: 'medium',
      status: 'completed',
    }, submissionId),
    kernelFixtureEvent(5, 'item_completed', {
      turn_id: 'turn-executor',
      item: {
        type: 'CommandExecution',
        id: `test-fixture-candidate-${eventSuffix}`,
        command: [process.execPath, '--test'],
        status: testPassed ? 'completed' : 'failed',
        exit_code: testPassed ? 0 : 1,
      },
    }, submissionId),
    kernelFixtureEvent(6, 'turn_diff', { unified_diff: diff }, submissionId),
    kernelFixtureEvent(7, 'token_count', {
      info: {
        total_token_usage: { input_tokens: 30, output_tokens: 12, total_tokens: 42 },
        last_token_usage: { input_tokens: 30, output_tokens: 12, total_tokens: 42 },
      },
      rate_limits: null,
    }, submissionId),
    kernelFixtureEvent(8, 'task_complete', {
      turn_id: 'turn-executor',
      last_agent_message: 'Candidate produced.',
      error: null,
    }, submissionId),
  ]
}

function executorKernelEvents(delivery, diff) {
  return candidateWriterKernelEvents(
    delivery,
    'stage-fixture-executor',
    diff,
    true,
  )
}

async function appendKernelEvents(home, delivery, stageRunId, sourceEvents) {
  const run = delivery.stageRuns.find(entry => entry.id === stageRunId)
  const binding = delivery.sessionBindings.find(entry => entry.stageRunId === stageRunId)
  if (run === undefined
    || binding?.dshSessionId === null
    || binding?.codexSessionId === null
    || binding === undefined) {
    throw new Error(`cannot append runtime events for unbound StageRun ${stageRunId}`)
  }
  const streamId = `stream-${binding.dshSessionId}`
  const ledger = await RuntimeSessionLedger.create({
    home,
    dshSessionId: binding.dshSessionId,
    roleId: run.role,
    cwd: delivery.spec.repository.locator,
    kernelSessionId: binding.codexSessionId,
    kernelStreamId: streamId,
    rolloutPath: join(home, 'fixture-rollouts', `${binding.dshSessionId}.jsonl`),
    provider: DEFAULT_PROVIDER,
    model: DEFAULT_MODEL,
  })
  const projector = new CodexRuntimeProjector({
    sessionId: binding.dshSessionId,
    kernelSessionId: binding.codexSessionId,
    roleId: run.role,
    kernelStreamId: streamId,
  })
  const runtimeEvents = projector.replay(sourceEvents)
  for (const event of runtimeEvents) await ledger.appendEvent(event)
  return runtimeEvents
}

function loadStrongFlowClient() {
  let registration
  const bundlePath = resolve(
    import.meta.dirname,
    '..',
    '..',
    'packages',
    'strongflow',
    'dist',
    'client.js',
  )
  return readFile(bundlePath, 'utf8').then((source) => {
    vm.runInNewContext(source, {
      Symbol,
      structuredClone,
      window: {
        __ModuleLoader__: {
          load(value) { registration = value },
        },
      },
    })
    if (registration?.id !== '@winwincode/strongflow') {
      throw new Error('StrongFlow production browser bundle did not register')
    }
    return registration.factory((id) => {
      if (id === 'react') return React
      throw new Error(`unexpected StrongFlow browser dependency: ${id}`)
    })
  })
}

export async function renderFixtureDeliveryProjection(input) {
  const client = await loadStrongFlowClient()
  return renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery: input.delivery,
      diagramExecution: input.diagramExecution ?? null,
      runtimeExecution: input.runtimeExecution ?? null,
      sessionId: input.sessionId,
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))
}

/**
 * Reusable offline test environment over the production contracts, DSH
 * projection, embedded kernel bridge, StrongFlow service, store, and UI bundle.
 */
export class DeliveryServiceFixtureTestkit {
  #cleanupPromise
  #dshRuntimes = new Set()
  #ownsRoot
  #runtimeEvents = []

  constructor(options) {
    this.root = resolve(options.root)
    this.home = join(this.root, 'home')
    this.repository = join(this.root, 'repository')
    this.repositoryLocator = options.repositoryLocator ?? this.repository
    this.deliveryId = options.deliveryId ?? DEFAULT_DELIVERY_ID
    this.clock = new DeterministicFixtureClock(options.clockStart)
    this.repositoryIdentity = options.repositoryIdentity
    this.#ownsRoot = options.ownsRoot
    this.mutationTrace = []
    this.diagramFacts = { runtimeEvents: Object.freeze([]), candidate: null }
    this.#replaceService()
  }

  static async create(options = {}) {
    const ownsRoot = options.root === undefined
    const root = options.root === undefined
      ? await mkdtemp(join(tmpdir(), 'winwincode-delivery-testkit-'))
      : resolve(options.root)
    await mkdir(root, { recursive: true })
    const repository = join(root, 'repository')
    let repositoryIdentity
    try {
      repositoryIdentity = await readRepositoryIdentity(repository)
    } catch {
      repositoryIdentity = await initializeRepository(repository)
    }
    await mkdir(join(root, 'home'), { recursive: true })
    return new DeliveryServiceFixtureTestkit({
      root,
      ownsRoot,
      deliveryId: options.deliveryId,
      clockStart: options.clockStart ?? DELIVERY_FIXTURE_BASE_TIME + 100,
      repositoryLocator: options.repositoryLocator,
      repositoryIdentity,
    })
  }

  [ADOPT_DSH_RUNTIME](runtime) {
    if (this.#cleanupPromise !== undefined) {
      throw new Error('Delivery testkit cleanup already started')
    }
    if (runtime.home !== this.home || runtime.workspace !== this.repository) {
      throw new TypeError('scripted DSH runtime must use its owning testkit paths')
    }
    this.#dshRuntimes.add(runtime)
    return () => this.#dshRuntimes.delete(runtime)
  }

  #replaceService() {
    this.authenticator = createStrongFlowDeliveryLocalProofAuthenticator({
      localSessionProof: DELIVERY_FIXTURE_UI_PROOF,
      localPeerProof: DELIVERY_FIXTURE_CLI_PROOF,
      localSessionActorId: 'fixture-ui-reviewer',
      localPeerActorId: 'fixture-cli-reviewer',
    })
    this.service = new StrongFlowService({
      home: this.home,
      authenticator: this.authenticator,
      clock: this.clock.now,
      executionSource: {
        read: async () => this.diagramFacts,
      },
    })
    this.invoker = new StrongFlowServiceInvoker(this.service)
  }

  restart(clockStart = this.clock.peek() + 1_000) {
    this.clock = new DeterministicFixtureClock(clockStart)
    this.#replaceService()
    return this.service
  }

  setDiagramFacts(runtimeEvents, candidate = null) {
    this.diagramFacts = immutable({ runtimeEvents, candidate })
  }

  spec(revision, suffix = `v${String(revision)}`) {
    return immutable({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `spec-${this.deliveryId}-${suffix}`,
      deliveryId: this.deliveryId,
      revision,
      title: `Deterministic Delivery ${suffix}`,
      goal: 'Prove the canonical Delivery path from reviewed goal to direct evidence.',
      scope: ['One local repository value change'],
      outOfScope: ['A second Agent scheduler', 'A generic task tracker'],
      constraints: [
        'Codex remains the execution authority',
        'DSH remains the interaction and model-provider shell',
      ],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: `criterion-${this.deliveryId}-${suffix}`,
        description: 'The local fixture exports the reviewed value.',
        verificationMethod: 'Run the local Node test against the frozen candidate.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: this.repositoryLocator,
      },
      baseRevision: this.repositoryIdentity.baseCommitId,
      maxReworkAttempts: 2,
      createdAtMillis: DELIVERY_FIXTURE_BASE_TIME + revision,
    })
  }

  async request(operation, requestId, payload) {
    const request = materializeStrongFlowDeliveryRequest(operation, requestId, payload)
    const response = await this.invoker.invoke(request)
    this.mutationTrace.push(Object.freeze({
      operation,
      requestId,
      ok: response.ok,
      revision: response.ok ? response.result.delivery.revision : null,
      error: requestFailure(response),
    }))
    return response
  }

  async requireSuccess(operation, requestId, payload) {
    const response = await this.request(operation, requestId, payload)
    if (!response.ok) {
      throw new Error(`${operation} failed with ${response.error.code}: ${response.error.message}`)
    }
    return response.result.delivery
  }

  async preparePlanReview(options = {}) {
    const draftSpec = this.spec(1, 'draft')
    const created = await this.requireSuccess('createDelivery', 'fixture:create', {
      spec: draftSpec,
      tasks: [],
    })
    const duplicate = await this.request('createDelivery', 'fixture:create:conflict', {
      spec: { ...draftSpec, title: 'Conflicting duplicate Delivery' },
      tasks: [],
    })
    if (duplicate.ok || duplicate.error.code !== 'DELIVERY_CONFLICT') {
      throw new Error('fixture duplicate Delivery was not rejected')
    }

    const approvedSpec = this.spec(2, 'approved')
    const ready = await this.requireSuccess('updateDeliverySpec', 'fixture:update-spec', {
      deliveryId: this.deliveryId,
      expectedRevision: created.revision,
      spec: approvedSpec,
    })
    const staleUpdate = await this.request('updateDeliverySpec', 'fixture:update-spec:stale', {
      deliveryId: this.deliveryId,
      expectedRevision: created.revision,
      spec: this.spec(3, 'stale'),
    })
    if (staleUpdate.ok || staleUpdate.error.code !== 'REVISION_CONFLICT') {
      throw new Error('fixture stale DeliverySpec update was not rejected')
    }

    const earlyExecution = await this.request('startStage', 'fixture:start:too-early', {
      deliveryId: this.deliveryId,
      expectedRevision: ready.revision,
      stageRunId: 'stage-fixture-too-early',
      deliveryTaskId: null,
      stage: 'executing',
      actorType: 'codex',
      role: 'executor',
      attention: null,
    })
    if (earlyExecution.ok || earlyExecution.error.code !== 'WRONG_DELIVERY_STATE') {
      throw new Error('fixture execution started before plan review')
    }

    const planning = await this.requireSuccess('startStage', 'fixture:start:planning', {
      deliveryId: this.deliveryId,
      expectedRevision: ready.revision,
      stageRunId: 'stage-fixture-planning',
      deliveryTaskId: null,
      stage: 'planning',
      actorType: 'codex',
      role: 'planner',
      attention: null,
    })
    const badBinding = await this.request('bindSession', 'fixture:bind:unknown', {
      deliveryId: this.deliveryId,
      expectedRevision: planning.revision,
      bindingId: 'binding-fixture-unknown',
      stageRunId: 'stage-fixture-unknown',
      dshSessionId: 'dsh-fixture-unknown',
      codexSessionId: 'codex-fixture-unknown',
    })
    if (badBinding.ok || badBinding.error.code !== 'INVALID_REQUEST') {
      throw new Error('fixture accepted a SessionBinding for an unknown StageRun')
    }

    let plannerSession = {
      dshSessionId: 'dsh-fixture-planner',
      codexSessionId: 'codex-fixture-planner',
    }
    if (options.dshRuntime !== undefined) {
      plannerSession = await options.dshRuntime.runRole({
        sessionId: 'dsh-fixture-planner',
        roleId: 'planner',
        prompt: 'Prepare the deterministic fixture solution.',
        maxTokens: options.plannerMaxTokens ?? 128,
      })
    } else {
      this.#runtimeEvents.push(...await appendKernelEvents(
        this.home,
        {
          ...planning,
          sessionBindings: [{
            schemaVersion: DELIVERY_SCHEMA_VERSION,
            id: 'binding-fixture-planner-shadow',
            deliveryId: this.deliveryId,
            stageRunId: 'stage-fixture-planning',
            ...plannerSession,
            boundAtMillis: planning.updatedAtMillis,
          }],
        },
        'stage-fixture-planning',
        [
          kernelFixtureEvent(1, 'task_started', { turn_id: 'turn-fixture-planner' }),
          kernelFixtureEvent(2, 'plan_update', {
            explanation: 'Keep the solution separate from the DeliverySpec.',
            plan: [{ step: 'Prepare the review set', status: 'completed' }],
          }),
          kernelFixtureEvent(3, 'task_complete', {
            turn_id: 'turn-fixture-planner',
            last_agent_message: 'Solution prepared.',
            error: null,
          }),
        ],
      ))
    }
    const boundPlanning = await this.requireSuccess('bindSession', 'fixture:bind:planning', {
      deliveryId: this.deliveryId,
      expectedRevision: planning.revision,
      bindingId: 'binding-fixture-planning',
      stageRunId: 'stage-fixture-planning',
      dshSessionId: plannerSession.dshSessionId,
      codexSessionId: plannerSession.codexSessionId,
    })
    const attention = createStrongFlowPlanReviewAttention({
      delivery: boundPlanning,
      attentionItemId: 'attention-fixture-plan-review',
      reviewStageRunId: 'stage-fixture-plan-review',
      assignedTo: 'fixture-ui-reviewer',
      solution: solutionFixture('approved'),
      risks: ['A stale approval must not unlock execution.'],
      unresolvedItems: [],
      preparedAtMillis: boundPlanning.updatedAtMillis,
    })
    const waiting = await this.requireSuccess('startStage', 'fixture:start:plan-review', {
      deliveryId: this.deliveryId,
      expectedRevision: boundPlanning.revision,
      stageRunId: 'stage-fixture-plan-review',
      deliveryTaskId: null,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      attention,
    })
    const boundReview = await this.requireSuccess('bindSession', 'fixture:bind:plan-review', {
      deliveryId: this.deliveryId,
      expectedRevision: waiting.revision,
      bindingId: 'binding-fixture-plan-review',
      stageRunId: 'stage-fixture-plan-review',
      dshSessionId: 'dsh-fixture-plan-review',
      codexSessionId: null,
    })
    return Object.freeze({
      delivery: boundReview,
      attention,
      decision: planDecision(attention),
    })
  }

  async approvePlan(review, options = {}) {
    const decision = options.decision ?? review.decision
    return this.request('resolveAttention', options.requestId ?? 'fixture:approve:plan', {
      deliveryId: this.deliveryId,
      expectedRevision: review.delivery.revision,
      attentionItemId: review.attention.id,
      status: decision.action === 'approve' ? 'resolved' : 'dismissed',
      resolution: JSON.stringify(decision),
      remediation: null,
      channel: 'local-ui',
      authentication: {
        scheme: 'local-session',
        proof: DELIVERY_FIXTURE_UI_PROOF,
      },
    })
  }

  async preparePlanRevision(current, options = {}) {
    const prefix = options.prefix ?? 'revised'
    const planningStageRunId = `stage-fixture-${prefix}-planning`
    const planning = await this.requireSuccess('startStage', `fixture:${prefix}:start:planning`, {
      deliveryId: this.deliveryId,
      expectedRevision: current.revision,
      stageRunId: planningStageRunId,
      deliveryTaskId: null,
      stage: 'planning',
      actorType: 'codex',
      role: 'planner',
      attention: null,
    })
    const plannerSession = options.dshRuntime === undefined
      ? {
        dshSessionId: `dsh-fixture-${prefix}-planner`,
        codexSessionId: `codex-fixture-${prefix}-planner`,
      }
      : await options.dshRuntime.runRole({
        sessionId: `dsh-fixture-${prefix}-planner`,
        roleId: 'planner',
        prompt: 'Revise the deterministic fixture solution from the human review note.',
        maxTokens: options.plannerMaxTokens ?? 128,
      })
    const boundPlanning = await this.requireSuccess(
      'bindSession',
      `fixture:${prefix}:bind:planning`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: planning.revision,
        bindingId: `binding-fixture-${prefix}-planning`,
        stageRunId: planningStageRunId,
        dshSessionId: plannerSession.dshSessionId,
        codexSessionId: plannerSession.codexSessionId,
      },
    )
    if (options.dshRuntime === undefined) {
      this.#runtimeEvents.push(...await appendKernelEvents(
        this.home,
        boundPlanning,
        planningStageRunId,
        [
          kernelFixtureEvent(1, 'task_started', {
            turn_id: `turn-fixture-${prefix}-planner`,
          }),
          kernelFixtureEvent(2, 'plan_update', {
            explanation: 'Revise only the reviewed solution; keep the DeliverySpec unchanged.',
            plan: [{ step: 'Apply the requested solution correction', status: 'completed' }],
          }),
          kernelFixtureEvent(3, 'task_complete', {
            turn_id: `turn-fixture-${prefix}-planner`,
            last_agent_message: 'Revised solution prepared.',
            error: null,
          }),
        ],
      ))
    }
    const attentionItemId = `attention-fixture-${prefix}-plan-review`
    const reviewStageRunId = `stage-fixture-${prefix}-plan-review`
    const attention = createStrongFlowPlanReviewAttention({
      delivery: boundPlanning,
      attentionItemId,
      reviewStageRunId,
      assignedTo: 'fixture-ui-reviewer',
      solution: solutionFixture(prefix),
      risks: ['The superseded review set must remain unusable.'],
      unresolvedItems: [],
      preparedAtMillis: boundPlanning.updatedAtMillis,
    })
    const waiting = await this.requireSuccess(
      'startStage',
      `fixture:${prefix}:start:plan-review`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: boundPlanning.revision,
        stageRunId: reviewStageRunId,
        deliveryTaskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        attention,
      },
    )
    const boundReview = await this.requireSuccess(
      'bindSession',
      `fixture:${prefix}:bind:plan-review`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: waiting.revision,
        bindingId: `binding-fixture-${prefix}-plan-review`,
        stageRunId: reviewStageRunId,
        dshSessionId: `dsh-fixture-${prefix}-plan-review`,
        codexSessionId: null,
      },
    )
    return Object.freeze({
      delivery: boundReview,
      attention,
      decision: planDecision(attention, {
        comments: 'Approve the exact revised fixture review set.',
      }),
    })
  }

  async prepareVerification(approvedPlan) {
    const executing = await this.requireSuccess('startStage', 'fixture:start:execution', {
      deliveryId: this.deliveryId,
      expectedRevision: approvedPlan.revision,
      stageRunId: 'stage-fixture-executor',
      deliveryTaskId: null,
      stage: 'executing',
      actorType: 'codex',
      role: 'executor',
      attention: null,
    })
    const boundExecutor = await this.requireSuccess('bindSession', 'fixture:bind:execution', {
      deliveryId: this.deliveryId,
      expectedRevision: executing.revision,
      bindingId: 'binding-fixture-executor',
      stageRunId: 'stage-fixture-executor',
      dshSessionId: 'dsh-fixture-executor',
      codexSessionId: 'codex-fixture-executor',
    })
    const repositoryCandidate = await createCandidateCommit(this.repository)
    const executorEvents = await appendKernelEvents(
      this.home,
      boundExecutor,
      'stage-fixture-executor',
      executorKernelEvents(boundExecutor, repositoryCandidate.diff),
    )
    this.#runtimeEvents.push(...executorEvents)
    this.setDiagramFacts(this.#runtimeEvents)
    const executingProjection = await this.service.getDeliveryProjection(this.deliveryId)

    const reviewing = await this.requireSuccess('startStage', 'fixture:start:reviewer', {
      deliveryId: this.deliveryId,
      expectedRevision: boundExecutor.revision,
      stageRunId: 'stage-fixture-reviewer',
      deliveryTaskId: null,
      stage: 'verifying',
      actorType: 'codex',
      role: 'reviewer',
      attention: null,
    })
    const boundReviewer = await this.requireSuccess('bindSession', 'fixture:bind:reviewer', {
      deliveryId: this.deliveryId,
      expectedRevision: reviewing.revision,
      bindingId: 'binding-fixture-reviewer',
      stageRunId: 'stage-fixture-reviewer',
      dshSessionId: 'dsh-fixture-reviewer',
      codexSessionId: 'codex-fixture-reviewer',
    })
    const candidate = freezeDeliveryCandidate(boundReviewer, {
      producerStageRunId: 'stage-fixture-executor',
      producerSessionBindingId: 'binding-fixture-executor',
      baseCommitId: repositoryCandidate.baseCommitId,
      baseTreeId: repositoryCandidate.baseTreeId,
      candidateCommitId: repositoryCandidate.candidateCommitId,
      candidateTreeId: repositoryCandidate.candidateTreeId,
      diffSha256: repositoryCandidate.diffSha256,
      changedPaths: repositoryCandidate.changedPaths,
    })
    this.setDiagramFacts(this.#runtimeEvents, candidate)
    const finishedProjection = await this.service.getDeliveryProjection(this.deliveryId)

    const verifying = await this.requireSuccess('startStage', 'fixture:start:verifier', {
      deliveryId: this.deliveryId,
      expectedRevision: boundReviewer.revision,
      stageRunId: 'stage-fixture-verifier',
      deliveryTaskId: null,
      stage: 'verifying',
      actorType: 'codex',
      role: 'verifier',
      attention: null,
    })
    const boundVerifier = await this.requireSuccess('bindSession', 'fixture:bind:verifier', {
      deliveryId: this.deliveryId,
      expectedRevision: verifying.revision,
      bindingId: 'binding-fixture-verifier',
      stageRunId: 'stage-fixture-verifier',
      dshSessionId: 'dsh-fixture-verifier',
      codexSessionId: 'codex-fixture-verifier',
    })
    return Object.freeze({
      delivery: boundVerifier,
      candidate,
      repositoryCandidate,
      executingProjection,
      finishedProjection,
      reviewerStageRunId: 'stage-fixture-reviewer',
      verifierStageRunId: 'stage-fixture-verifier',
    })
  }

  async prepareCandidateVerification(current, options = {}) {
    const prefix = options.prefix ?? 'scenario'
    const writerStage = options.writerStage ?? 'executing'
    const writerRole = writerStage === 'reworking' ? 'remediator' : 'executor'
    const writerStageRunId = `stage-fixture-${prefix}-${writerRole}`
    const writerBindingId = `binding-fixture-${prefix}-${writerRole}`
    const startedWriter = await this.requireSuccess(
      'startStage',
      `fixture:${prefix}:start:${writerStage}`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: current.revision,
        stageRunId: writerStageRunId,
        deliveryTaskId: options.deliveryTaskId ?? null,
        stage: writerStage,
        actorType: 'codex',
        role: writerRole,
        attention: null,
      },
    )
    const boundWriter = await this.requireSuccess(
      'bindSession',
      `fixture:${prefix}:bind:${writerStage}`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: startedWriter.revision,
        bindingId: writerBindingId,
        stageRunId: writerStageRunId,
        dshSessionId: `dsh-fixture-${prefix}-${writerRole}`,
        codexSessionId: `codex-fixture-${prefix}-${writerRole}`,
      },
    )
    const repositoryCandidate = await createCandidateCommit(this.repository, {
      baseCommitId: this.repositoryIdentity.baseCommitId,
      value: options.value ?? 'after',
      expectedTestPass: options.expectedTestPass ?? true,
      message: options.message ?? `Implement ${prefix} fixture candidate`,
      commitDate: options.commitDate,
    })
    const writerEvents = await appendKernelEvents(
      this.home,
      boundWriter,
      writerStageRunId,
      candidateWriterKernelEvents(
        boundWriter,
        writerStageRunId,
        repositoryCandidate.diff,
        repositoryCandidate.testPassed,
      ),
    )
    this.#runtimeEvents.push(...writerEvents)
    this.setDiagramFacts(this.#runtimeEvents)
    const executingProjection = await this.service.getDeliveryProjection(this.deliveryId)

    const reviewerStageRunId = `stage-fixture-${prefix}-reviewer`
    const reviewing = await this.requireSuccess(
      'startStage',
      `fixture:${prefix}:start:reviewer`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: boundWriter.revision,
        stageRunId: reviewerStageRunId,
        deliveryTaskId: options.deliveryTaskId ?? null,
        stage: 'verifying',
        actorType: 'codex',
        role: 'reviewer',
        attention: null,
      },
    )
    const boundReviewer = await this.requireSuccess(
      'bindSession',
      `fixture:${prefix}:bind:reviewer`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: reviewing.revision,
        bindingId: `binding-fixture-${prefix}-reviewer`,
        stageRunId: reviewerStageRunId,
        dshSessionId: `dsh-fixture-${prefix}-reviewer`,
        codexSessionId: `codex-fixture-${prefix}-reviewer`,
      },
    )
    const candidate = freezeDeliveryCandidate(boundReviewer, {
      producerStageRunId: writerStageRunId,
      producerSessionBindingId: writerBindingId,
      baseCommitId: repositoryCandidate.baseCommitId,
      baseTreeId: repositoryCandidate.baseTreeId,
      candidateCommitId: repositoryCandidate.candidateCommitId,
      candidateTreeId: repositoryCandidate.candidateTreeId,
      diffSha256: repositoryCandidate.diffSha256,
      changedPaths: repositoryCandidate.changedPaths,
    })
    this.setDiagramFacts(this.#runtimeEvents, candidate)
    const finishedProjection = await this.service.getDeliveryProjection(this.deliveryId)

    const verifierStageRunId = `stage-fixture-${prefix}-verifier`
    const verifying = await this.requireSuccess(
      'startStage',
      `fixture:${prefix}:start:verifier`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: boundReviewer.revision,
        stageRunId: verifierStageRunId,
        deliveryTaskId: options.deliveryTaskId ?? null,
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        attention: null,
      },
    )
    const boundVerifier = await this.requireSuccess(
      'bindSession',
      `fixture:${prefix}:bind:verifier`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: verifying.revision,
        bindingId: `binding-fixture-${prefix}-verifier`,
        stageRunId: verifierStageRunId,
        dshSessionId: `dsh-fixture-${prefix}-verifier`,
        codexSessionId: `codex-fixture-${prefix}-verifier`,
      },
    )
    return Object.freeze({
      delivery: boundVerifier,
      candidate,
      repositoryCandidate,
      executingProjection,
      finishedProjection,
      writerStageRunId,
      writerBindingId,
      reviewerStageRunId,
      verifierStageRunId,
    })
  }

  async verificationEvents(prepared, outcomes = {}) {
    const reviewer = roleRuntimeEvents(
      prepared.delivery,
      prepared.candidate,
      'reviewer',
      outcomes.reviewer ?? 'pass',
      prepared.reviewerStageRunId ?? 'stage-fixture-reviewer',
    )
    const verifier = roleRuntimeEvents(
      prepared.delivery,
      prepared.candidate,
      'verifier',
      outcomes.verifier ?? 'pass',
      prepared.verifierStageRunId ?? 'stage-fixture-verifier',
    )
    for (const [stageRunId, events] of [
      [prepared.reviewerStageRunId ?? 'stage-fixture-reviewer', reviewer],
      [prepared.verifierStageRunId ?? 'stage-fixture-verifier', verifier],
    ]) {
      const run = prepared.delivery.stageRuns.find(entry => entry.id === stageRunId)
      const binding = prepared.delivery.sessionBindings.find(entry => entry.stageRunId === stageRunId)
      const ledger = await RuntimeSessionLedger.create({
        home: this.home,
        dshSessionId: binding.dshSessionId,
        roleId: run.role,
        cwd: this.repository,
        kernelSessionId: binding.codexSessionId,
        kernelStreamId: `stream-${binding.dshSessionId}`,
        rolloutPath: join(this.home, 'fixture-rollouts', `${binding.dshSessionId}.jsonl`),
        provider: DEFAULT_PROVIDER,
        model: DEFAULT_MODEL,
      })
      for (const event of events) await ledger.appendEvent(event)
    }
    this.#runtimeEvents.push(...reviewer, ...verifier)
    this.setDiagramFacts(this.#runtimeEvents, prepared.candidate)
    return Object.freeze([...reviewer, ...verifier])
  }

  async prepareDeliveryReview(current, options = {}) {
    const prefix = options.prefix ?? 'final'
    const stageRunId = `stage-fixture-${prefix}-delivery-review`
    const attention = immutable({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `attention-fixture-${prefix}-delivery-review`,
      deliveryId: this.deliveryId,
      deliverySpecId: current.spec.id,
      stageRunId,
      type: 'delivery_approval',
      title: 'Approve the current verified fixture candidate',
      context: 'Review the current passing Verdict and its direct evidence.',
      options: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'approve-delivery',
        label: 'Approve delivery',
        description: 'Deliver the exact current candidate and Verdict.',
      }, {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'annotate-rework',
        label: 'Request annotated rework',
        description: 'Return the exact current candidate to bounded rework.',
      }],
      assignedTo: 'fixture-ui-reviewer',
      blocking: true,
      status: 'open',
      resolution: null,
      resolvedBy: null,
      createdAtMillis: current.updatedAtMillis,
      resolvedAtMillis: null,
    })
    const reviewing = await this.requireSuccess(
      'startStage',
      `fixture:${prefix}:start:delivery-review`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: current.revision,
        stageRunId,
        deliveryTaskId: null,
        stage: 'delivery-review',
        actorType: 'human',
        role: 'approver',
        attention,
      },
    )
    const bound = await this.requireSuccess(
      'bindSession',
      `fixture:${prefix}:bind:delivery-review`,
      {
        deliveryId: this.deliveryId,
        expectedRevision: reviewing.revision,
        bindingId: `binding-fixture-${prefix}-delivery-review`,
        stageRunId,
        dshSessionId: `dsh-fixture-${prefix}-delivery-review`,
        codexSessionId: null,
      },
    )
    return Object.freeze({ delivery: bound, attention })
  }

  async approveDelivery(review, options = {}) {
    return this.request('resolveAttention', options.requestId ?? 'fixture:approve:delivery', {
      deliveryId: this.deliveryId,
      expectedRevision: review.delivery.revision,
      attentionItemId: review.attention.id,
      status: 'resolved',
      resolution: options.resolution ?? 'Approve the exact current candidate and passing Verdict.',
      remediation: null,
      channel: 'local-ui',
      authentication: {
        scheme: 'local-session',
        proof: DELIVERY_FIXTURE_UI_PROOF,
      },
    })
  }

  async submitVerdict(prepared, runtimeEvents, options = {}) {
    return this.request('submitVerdict', options.requestId ?? 'fixture:submit:verdict', {
      deliveryId: this.deliveryId,
      expectedRevision: prepared.delivery.revision,
      candidate: options.candidate ?? prepared.candidate,
      runtimeEvents,
      requiredRoles: ['reviewer', 'verifier'],
    })
  }

  async runtimeProjection(delivery) {
    const events = []
    for (const binding of delivery.sessionBindings) {
      if (binding.dshSessionId === null || binding.codexSessionId === null) continue
      try {
        const stored = await RuntimeSessionLedger.open(this.home, binding.dshSessionId)
          .then(ledger => ledger.read())
        events.push(...stored.events)
      } catch {
        continue
      }
    }
    return new DeliveryRuntimeProjection({ delivery }).replay(events)
  }

  async recover(liveCodexSessionIds = []) {
    return reconcileDeliveryAfterRestart({
      home: this.home,
      deliveryId: this.deliveryId,
      codex: {
        async listSessions() { return liveCodexSessionIds },
      },
    })
  }

  async stored() {
    return DeliveryStore.open(this.home, this.deliveryId).then(store => store.read())
  }

  async cleanup() {
    this.#cleanupPromise ??= this.#cleanup()
    return this.#cleanupPromise
  }

  async #cleanup() {
    for (const runtime of [...this.#dshRuntimes]) await runtime.close()
    if (this.#dshRuntimes.size !== 0) {
      throw new Error('Delivery testkit still owns an active DSH runtime')
    }
    if (this.#ownsRoot) await rm(this.root, { recursive: true, force: true })
  }
}

export async function exerciseFixturePolicyDenial() {
  const sessionId = 'dsh-fixture-policy'
  const kernelSessionId = 'codex-fixture-policy'
  const normalizer = new CodexRuntimeProjector({
    sessionId,
    kernelSessionId,
    roleId: 'executor',
    kernelStreamId: 'stream-fixture-policy',
  })
  const projection = new DshRuntimeProjection({ sessionId, roleId: 'executor' })
  const event = normalizer.ingest(kernelFixtureEvent(1, 'exec_approval_request', {
    call_id: 'operation-fixture-policy',
    approval_id: 'approval-fixture-policy',
    turn_id: 'turn-fixture-policy',
    command: ['git', 'status'],
    cwd: '/fixture/repository',
    parsed_cmd: [],
  }))
  projection.apply(event)
  const responses = []
  const router = new RuntimeApprovalRouter({
    async resolveApproval(response) {
      responses.push(response)
      return 'submission-fixture-policy-denied'
    },
  }, projection)
  const submissionId = await router.resolve({
    approvalId: 'approval-fixture-policy',
    decision: { kind: 'denied', rejection: 'Fixture policy denied this operation.' },
  })
  return immutable({ submissionId, responses })
}

export function assertMalformedFixtureProjection() {
  const missing = new CodexRuntimeProjector({
    sessionId: 'dsh-fixture-malformed',
    kernelSessionId: 'codex-fixture-malformed',
    roleId: 'executor',
    kernelStreamId: 'stream-fixture-malformed',
  })
  try {
    missing.ingest(kernelFixtureEvent(2, 'task_started', { turn_id: 'turn-malformed' }))
  } catch (error) {
    if (error instanceof RuntimeProjectionError
      && error.code === 'EVENT_SEQUENCE_MISSING') return error.code
    throw error
  }
  throw new Error('malformed fixture projection was accepted')
}

export function assertForeignDeliveryProjection(delivery) {
  const projector = new CodexRuntimeProjector({
    sessionId: 'dsh-fixture-foreign',
    kernelSessionId: 'codex-fixture-foreign',
    roleId: 'executor',
    kernelStreamId: 'stream-fixture-foreign',
  })
  const event = projector.ingest(kernelFixtureEvent(1, 'task_started', {
    turn_id: 'turn-fixture-foreign',
  }))
  try {
    new DeliveryRuntimeProjection({ delivery }).apply(event)
  } catch (error) {
    if (error instanceof DeliveryRuntimeProjectionError
      && error.code === 'RUNTIME_SESSION_UNBOUND') return error.code
    throw error
  }
  throw new Error('foreign runtime event was accepted by Delivery projection')
}

export function isRecoveryFailure(error, code) {
  return error instanceof DeliveryRecoveryError && error.code === code
}
