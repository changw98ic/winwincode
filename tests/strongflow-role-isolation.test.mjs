import assert from 'node:assert/strict'
import { mkdir, mkdtemp, realpath, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  CandidateId,
  JobId,
  SourceSnapshotId,
  StageRunId,
  STRONGFLOW_ROLE_IDS,
  STRONGFLOW_ROLE_TOOLS,
  StrongFlowWorkspaceId,
  VerificationSnapshotId,
  createStrongFlowRoleConfiguration,
} from '../packages/contracts/dist/index.js'
import {
  DshModelPort,
} from '../packages/dsh-profile/dist/index.js'
import {
  WinWinCodeKernel,
} from '../packages/native/dist/index.js'
import {
  EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
  StrongFlowRoleRunner,
  StrongFlowRoleSessionManager,
} from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)
const HASH_B = 'b'.repeat(64)
const HASH_C = 'c'.repeat(64)

const modelCatalog = Object.freeze([
  Object.freeze({
    provider: 'fixture-general-provider',
    model: 'fixture-general-model',
    reasoningEfforts: Object.freeze(['medium']),
  }),
  Object.freeze({
    provider: 'fixture-architecture-provider',
    model: 'fixture-architecture-model',
    reasoningEfforts: Object.freeze(['high']),
  }),
  Object.freeze({
    provider: 'fixture-verification-provider',
    model: 'fixture-verification-model',
    reasoningEfforts: Object.freeze(['high']),
  }),
])

const roleConfiguration = createStrongFlowRoleConfiguration(
  Object.fromEntries(STRONGFLOW_ROLE_IDS.map((roleId, index) => {
    const special = roleId === 'solution'
      ? {
          provider: 'fixture-architecture-provider',
          model: 'fixture-architecture-model',
          reasoningEffort: 'high',
        }
      : roleId === 'adversarial-verifier'
        ? {
            provider: 'fixture-verification-provider',
            model: 'fixture-verification-model',
            reasoningEffort: 'high',
          }
        : {
            provider: 'fixture-general-provider',
            model: 'fixture-general-model',
            reasoningEffort: 'medium',
          }
    return [roleId, {
      modelRoute: { provider: special.provider, model: special.model },
      reasoningEffort: special.reasoningEffort,
      budget: {
        maxTurns: 1,
        maxWallTimeMillis: 2_000 + index,
        maxTotalTokens: 50 + index,
        maxCostUsdMicros: 10_000 + index,
      },
    }]
  })),
  modelCatalog,
)

function deferred() {
  let resolvePromise = () => {}
  const promise = new Promise(resolve => {
    resolvePromise = resolve
  })
  return { promise, resolve: resolvePromise }
}

class AsyncEventChannel {
  values = []
  waiters = []
  closed = false

  push(value) {
    if (this.closed) return
    const waiter = this.waiters.shift()
    if (waiter === undefined) this.values.push(value)
    else waiter.finish({ done: false, value })
  }

  close() {
    if (this.closed) return
    this.closed = true
    for (const waiter of this.waiters.splice(0)) {
      waiter.finish({ done: true, value: undefined })
    }
  }

  next(signal) {
    const value = this.values.shift()
    if (value !== undefined) return Promise.resolve({ done: false, value })
    if (this.closed || signal?.aborted === true) {
      return Promise.resolve({ done: true, value: undefined })
    }
    return new Promise(resolve => {
      let settled = false
      const abort = () => finish({ done: true, value: undefined })
      const finish = result => {
        if (settled) return
        settled = true
        signal?.removeEventListener('abort', abort)
        const index = this.waiters.indexOf(waiter)
        if (index >= 0) this.waiters.splice(index, 1)
        resolve(result)
      }
      const waiter = { finish }
      this.waiters.push(waiter)
      signal?.addEventListener('abort', abort, { once: true })
    })
  }
}

function kernelEvent(sequence, type, data, turnId) {
  const payload = { id: turnId, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

class InstrumentedRoleKernel {
  constructor(home, options = {}) {
    this.home = home
    this.options = options
  }

  creates = []
  resumes = []
  submissions = []
  interrupts = []
  closes = []
  sessions = new Map()
  nextSession = 1

  async createSession(options) {
    this.creates.push(structuredClone(options))
    return this.#newSession('created')
  }

  async resumeSession(options) {
    this.resumes.push(structuredClone(options))
    return this.#newSession('resumed')
  }

  bindContext(kernelSessionId, context) {
    const session = this.sessions.get(kernelSessionId)
    if (session === undefined) throw new Error(`unknown fixture session ${kernelSessionId}`)
    if (session.context !== undefined) throw new Error('fixture context was installed twice')
    session.context = context
  }

  async submitTurn(kernelSessionId, text) {
    const session = this.sessions.get(kernelSessionId)
    if (session?.context === undefined) throw new Error('fixture role context is not installed')
    const role = session.context.roleSpec
    const turnId = `turn-${kernelSessionId}`
    this.submissions.push({ kernelSessionId, roleId: role.id, text })
    const roleIndex = STRONGFLOW_ROLE_IDS.indexOf(role.id)
    const normalTokens = 20 + roleIndex
    const totalTokens = this.options.exceedTokenBudget === true
      ? role.budget.maxTotalTokens + 1
      : normalTokens
    const artifacts = role.requiredOutputArtifacts.map(kind => ({
      kind,
      artifact: {
        artifactKind: kind,
        producerRole: role.id,
        ...(this.options.hiddenSolutionRole === role.id && kind === 'REQUIREMENT_SPEC'
          ? { solutionDesign: { hidden: true } }
          : {}),
      },
    }))
    session.channel.push(kernelEvent(1, 'task_started', {
      turn_id: turnId,
      started_at: 100,
    }, turnId))
    session.channel.push(kernelEvent(2, 'token_count', {
      info: {
        total_token_usage: {
          input_tokens: totalTokens - 4,
          cached_input_tokens: 2,
          cache_write_input_tokens: 0,
          output_tokens: 4,
          reasoning_output_tokens: 1,
          total_tokens: totalTokens,
        },
        last_token_usage: {
          input_tokens: totalTokens - 4,
          output_tokens: 4,
          total_tokens: totalTokens,
        },
      },
      rate_limits: null,
    }, turnId))
    session.channel.push(kernelEvent(3, 'task_complete', {
      turn_id: turnId,
      last_agent_message: JSON.stringify({ schemaVersion: 1, artifacts }),
      error: null,
      completed_at: 101,
    }, turnId))
    return Object.freeze({ status: 'started', turnId })
  }

  async interrupt(kernelSessionId) {
    this.interrupts.push(kernelSessionId)
    return `interrupt-${kernelSessionId}`
  }

  async closeSession(kernelSessionId) {
    this.closes.push(kernelSessionId)
    this.sessions.get(kernelSessionId)?.channel.close()
  }

  async *events(kernelSessionId, options = {}) {
    const session = this.sessions.get(kernelSessionId)
    if (session === undefined) throw new Error(`unknown fixture session ${kernelSessionId}`)
    while (true) {
      const next = await session.channel.next(options.signal)
      if (next.done) return
      yield next.value
    }
  }

  #newSession(source) {
    const ordinal = this.nextSession++
    const sessionId = `${source}-role-kernel-${ordinal}`
    this.sessions.set(sessionId, {
      channel: new AsyncEventChannel(),
      context: undefined,
    })
    return Object.freeze({
      sessionId,
      rolloutPath: join(this.home, `${sessionId}.jsonl`),
    })
  }
}

class InstrumentedContextInstaller {
  constructor(kernel) {
    this.kernel = kernel
  }

  requests = []
  disposals = []
  byKernelSession = new Map()

  async install(request) {
    this.requests.push(request)
    this.byKernelSession.set(request.kernel.kernelSessionId, request.context)
    this.kernel.bindContext(request.kernel.kernelSessionId, request.context)
    return Object.freeze({
      contextId: request.context.contextId,
      dispose: disposal => {
        this.disposals.push({
          roleId: request.context.roleSpec.id,
          kernelSessionId: request.kernel.kernelSessionId,
          ...disposal,
        })
      },
    })
  }

  probeTool(kernelSessionId, tool) {
    const context = this.byKernelSession.get(kernelSessionId)
    if (context === undefined) throw new Error(`no installed fixture context ${kernelSessionId}`)
    return Object.freeze({
      roleId: context.roleSpec.id,
      tool,
      allowed: context.roleSpec.allowedTools.includes(tool),
      workspaceMode: context.workspace.mode,
      workspacePath: context.workspace.path,
    })
  }
}

class RecordingNativeContextInstaller {
  requests = []
  disposals = []

  async install(request) {
    this.requests.push(request)
    return Object.freeze({
      contextId: request.context.contextId,
      dispose: disposal => {
        this.disposals.push({
          roleId: request.context.roleSpec.id,
          kernelSessionId: request.kernel.kernelSessionId,
          ...disposal,
        })
      },
    })
  }
}

class MemoryRoleRecorder {
  events = []
  results = []
  flushes = 0

  appendKernelEvent(event) {
    this.events.push(event)
  }

  finish(result) {
    this.results.push(result)
  }

  flush() {
    this.flushes += 1
  }
}

async function fixture(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-role-isolation-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const source = join(home, 'source')
  const candidate = join(home, 'candidate')
  await mkdir(source)
  await mkdir(candidate)
  const verification = {}
  const outputs = {}
  for (const roleId of ['reviewer', 'verifier', 'adversarial-verifier']) {
    verification[roleId] = join(home, 'verification', roleId)
    outputs[roleId] = join(home, 'verification-output', roleId)
    await mkdir(verification[roleId], { recursive: true })
    await mkdir(outputs[roleId], { recursive: true })
  }
  return Object.freeze({
    home,
    source: await realpath(source),
    candidate: await realpath(candidate),
    verification: Object.freeze(Object.fromEntries(await Promise.all(
      Object.entries(verification).map(async ([roleId, path]) => [roleId, await realpath(path)]),
    ))),
    outputs: Object.freeze(Object.fromEntries(await Promise.all(
      Object.entries(outputs).map(async ([roleId, path]) => [roleId, await realpath(path)]),
    ))),
    workspaceId: StrongFlowWorkspaceId(`workspace-sha256-${HASH_A}`),
    sourceSnapshotId: SourceSnapshotId(`source-sha256-${HASH_B}`),
    candidateId: CandidateId('candidate-role-isolation-fixture'),
    verificationSnapshotId: VerificationSnapshotId(`verification-sha256-${HASH_C}`),
  })
}

function workspaceFor(value, roleId, stageRunId) {
  const base = {
    roleId,
    stageRunId,
    workspaceId: value.workspaceId,
    sourceSnapshotId: value.sourceSnapshotId,
  }
  if (['requirements', 'solution', 'planner'].includes(roleId)) {
    return Object.freeze({
      ...base,
      mode: 'source-read-only',
      path: value.source,
    })
  }
  if (['executor', 'remediator'].includes(roleId)) {
    return Object.freeze({
      ...base,
      mode: 'candidate-write',
      path: value.candidate,
      ...(roleId === 'remediator' ? { candidateId: value.candidateId } : {}),
    })
  }
  return Object.freeze({
    ...base,
    mode: 'candidate-read-only',
    path: value.verification[roleId],
    temporaryOutputPath: value.outputs[roleId],
    candidateId: value.candidateId,
    verificationSnapshotId: value.verificationSnapshotId,
  })
}

function assignmentFor(value, roleId, suffix = 'matrix') {
  const stageRunId = StageRunId(`stage-run-${suffix}`)
  return Object.freeze({
    jobId: JobId(`job-${suffix}`),
    stageRunId,
    attemptId: AttemptId(`attempt-${suffix}`),
    roleId,
    workspace: workspaceFor(value, roleId, stageRunId),
  })
}

function roleInputs(spec, privateMarker) {
  return spec.acceptedInputArtifacts.map((kind, index) => Object.freeze({
    artifactId: `${spec.id}-${kind.toLowerCase().replaceAll('_', '-')}-${index}`,
    kind,
    value: Object.freeze({ privateMarker, inputKind: kind }),
  }))
}

function roleValidators(spec, options = {}) {
  return spec.requiredOutputArtifacts.map(kind => Object.freeze({
    kind,
    validate(value, context) {
      assert.equal(context.roleSession.roleSpec.id, spec.id)
      assert.deepEqual(Object.keys(value).sort(), ['artifactKind', 'producerRole'])
      assert.equal(value.artifactKind, kind)
      assert.equal(value.producerRole, spec.id)
      if (options.rejectHiddenSolution === true && 'solutionDesign' in value) {
        throw new Error('RequirementSpec contains hidden solution design')
      }
      return Object.freeze({ artifactKind: kind, producerRole: spec.id })
    },
  }))
}

function roleRunner(recorder) {
  return new StrongFlowRoleRunner({
    recorder,
    costAccountant: {
      costUsdMicros({ tokenUsage }) {
        return tokenUsage.totalTokens * 3
      },
    },
  })
}

function manager(value, kernel, installer) {
  let now = 2_000_000_000_000
  return new StrongFlowRoleSessionManager({
    home: value.home,
    kernel,
    installer,
    roleConfiguration,
    modelCatalog,
    now: () => now++,
  })
}

test('all eight roles keep separate model, prompt, tool, workspace, and artifact scopes', async t => {
  const value = await fixture(t)
  const kernel = new InstrumentedRoleKernel(value.home)
  const installer = new InstrumentedContextInstaller(kernel)
  const sessionManager = manager(value, kernel, installer)
  const sessions = await Promise.all(STRONGFLOW_ROLE_IDS.map(roleId => (
    sessionManager.create(assignmentFor(value, roleId))
  )))
  const privateMarkers = Object.fromEntries(
    STRONGFLOW_ROLE_IDS.map(roleId => [roleId, `PRIVATE_CONTEXT_${roleId}`]),
  )
  const recorders = new Map()
  const results = await Promise.all(sessions.map(async session => {
    const recorder = new MemoryRoleRecorder()
    recorders.set(session.context.roleSpec.id, recorder)
    const result = await roleRunner(recorder).run({
      session,
      inputs: roleInputs(
        session.context.roleSpec,
        privateMarkers[session.context.roleSpec.id],
      ),
      validators: roleValidators(session.context.roleSpec),
      budgetBaseline: EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
    })
    return [session, result]
  }))

  assert.equal(new Set(sessions.map(session => session.context.contextId)).size, 8)
  assert.equal(new Set(sessions.map(session => session.kernel.kernelSessionId)).size, 8)
  assert.equal(sessionManager.listSessions().length, 0)
  assert.equal(installer.requests.length, 8)
  assert.equal(installer.disposals.length, 8)
  assert.ok(installer.disposals.every(disposal => disposal.outcome === 'completed'))

  for (const [session, result] of results) {
    const spec = session.context.roleSpec
    assert.equal(result.outcome, 'succeeded', spec.id)
    assert.deepEqual(Object.keys(result.artifacts), spec.requiredOutputArtifacts, spec.id)
    assert.equal(result.usage.turnsStarted, 1)
    assert.ok(result.usage.tokenUsage.totalTokens <= spec.budget.maxTotalTokens)
    assert.ok(result.usage.costUsdMicros <= spec.budget.maxCostUsdMicros)
    assert.equal(result.eventInterval.generation, 1)
    assert.equal(result.eventInterval.firstSequence, '1')
    assert.equal(result.eventInterval.lastSequence, '3')
    assert.ok(Object.isFrozen(session.context))
    assert.ok(Object.isFrozen(session.context.roleSpec))
    assert.ok(Object.isFrozen(session.context.workspace))

    const submission = kernel.submissions.find(entry => (
      entry.kernelSessionId === session.kernel.kernelSessionId
    ))
    assert.ok(submission)
    assert.ok(submission.text.includes(privateMarkers[spec.id]))
    for (const otherRoleId of STRONGFLOW_ROLE_IDS) {
      if (otherRoleId !== spec.id) {
        assert.equal(submission.text.includes(privateMarkers[otherRoleId]), false)
      }
    }

    for (const tool of STRONGFLOW_ROLE_TOOLS) {
      const probe = installer.probeTool(session.kernel.kernelSessionId, tool)
      assert.equal(probe.allowed, spec.allowedTools.includes(tool), `${spec.id}/${tool}`)
      assert.equal(probe.workspaceMode, spec.workspaceMode)
      assert.equal(probe.workspacePath, session.context.workspace.path)
    }
    const candidateWriter = ['executor', 'remediator'].includes(spec.id)
    assert.equal(spec.allowedTools.includes('candidate.patch'), candidateWriter)
    assert.equal(spec.workspaceMode === 'candidate-write', candidateWriter)
    assert.equal(spec.sandboxPolicy.filesystem === 'candidate-write', candidateWriter)

    const expectedCreate = kernel.creates.find(call => (
      call.provider === spec.modelRoute.provider
      && call.model === spec.modelRoute.model
      && call.cwd === session.context.workspace.path
    ))
    assert.ok(expectedCreate, spec.id)
    const recorder = recorders.get(spec.id)
    assert.equal(recorder.events.length, 3)
    assert.equal(recorder.results.length, 1)
    assert.equal(recorder.flushes, 1)
  }

  const requirementResult = results.find(([session]) => (
    session.context.roleSpec.id === 'requirements'
  ))[1]
  const solutionResult = results.find(([session]) => (
    session.context.roleSpec.id === 'solution'
  ))[1]
  assert.deepEqual(Object.keys(requirementResult.artifacts), ['REQUIREMENT_SPEC'])
  assert.deepEqual(Object.keys(solutionResult.artifacts), [
    'SOLUTION_DESIGN',
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    'PROCESS_FLOW_DIAGRAM',
  ])
})

test('scripted DSH model streams drive all eight roles through the embedded Codex kernel', async t => {
  const value = await fixture(t)
  const calls = []
  const llm = {
    async prepareCall(config, signal) {
      return {
        config: Object.freeze({ ...config }),
        stream(options) {
          const userMessage = options.messages.findLast(message => message.role === 'user')
          const prompt = userMessage?.content
            .filter(block => block.type === 'text')
            .map(block => block.text)
            .join('\n')
          assert.ok(prompt)
          const marker = 'IDENTIFIED_INPUT_ARTIFACTS:\n'
          const markerIndex = prompt.lastIndexOf(marker)
          assert.ok(markerIndex >= 0)
          const assignment = JSON.parse(prompt.slice(markerIndex + marker.length))
          calls.push({
            provider: options.provider,
            model: options.model,
            roleId: assignment.roleId,
            contextId: assignment.contextId,
            privateMarkers: assignment.inputs.map(input => input.value.privateMarker),
            signalMatches: options.signal === signal,
          })
          return scriptedRoleOutput(assignment)
        },
      }
    },
  }
  async function* scriptedRoleOutput(assignment) {
    const text = JSON.stringify({
      schemaVersion: 1,
      artifacts: assignment.requiredOutputArtifacts.map(kind => ({
        kind,
        artifact: { artifactKind: kind, producerRole: assignment.roleId },
      })),
    })
    yield { type: 'block-start', index: 0, blockType: 'text' }
    yield { type: 'text-delta', index: 0, text }
    yield { type: 'block-end', index: 0, block: { type: 'text', text } }
    yield { type: 'usage', usage: { inputTokens: 24, outputTokens: 12 } }
    yield { type: 'finish', reason: { kind: 'stop' } }
  }

  const kernel = new WinWinCodeKernel({
    home: join(value.home, 'native-kernel'),
    modelPort: new DshModelPort(llm),
  })
  const installer = new RecordingNativeContextInstaller()
  const sessionManager = manager(value, kernel, installer)
  try {
    const sessions = await Promise.all(STRONGFLOW_ROLE_IDS.map(roleId => (
      sessionManager.create(assignmentFor(value, roleId, 'native-dsh-matrix'))
    )))
    const results = await Promise.all(sessions.map(async session => {
      const recorder = new MemoryRoleRecorder()
      return roleRunner(recorder).run({
        session,
        inputs: roleInputs(
          session.context.roleSpec,
          `NATIVE_DSH_PRIVATE_${session.context.roleSpec.id}`,
        ),
        validators: roleValidators(session.context.roleSpec),
        budgetBaseline: EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
      })
    }))

    assert.equal(results.length, 8)
    assert.ok(results.every(result => result.outcome === 'succeeded'))
    assert.equal(calls.length, 8)
    assert.equal(new Set(calls.map(call => call.contextId)).size, 8)
    assert.ok(calls.every(call => call.signalMatches))
    assert.deepEqual(new Set(calls.map(call => call.roleId)), new Set(STRONGFLOW_ROLE_IDS))
    for (const call of calls) {
      const spec = roleConfiguration.roles.find(role => role.id === call.roleId)
      assert.ok(spec)
      assert.equal(call.provider, spec.modelRoute.provider)
      assert.equal(call.model, spec.modelRoute.model)
      assert.ok(call.privateMarkers.every(marker => (
        marker === `NATIVE_DSH_PRIVATE_${call.roleId}`
      )))
    }
    assert.equal(sessionManager.listSessions().length, 0)
    assert.equal(installer.requests.length, 8)
    assert.equal(installer.disposals.length, 8)
    assert.ok(installer.disposals.every(disposal => disposal.outcome === 'completed'))
  } finally {
    await kernel.shutdown()
  }
})

test('every role terminates and releases its session when its own token budget is exceeded', async t => {
  const value = await fixture(t)
  const kernel = new InstrumentedRoleKernel(value.home, { exceedTokenBudget: true })
  const installer = new InstrumentedContextInstaller(kernel)
  const sessionManager = manager(value, kernel, installer)
  const sessions = await Promise.all(STRONGFLOW_ROLE_IDS.map(roleId => (
    sessionManager.create(assignmentFor(value, roleId, 'budget-matrix'))
  )))

  const results = await Promise.all(sessions.map(async session => {
    const recorder = new MemoryRoleRecorder()
    return roleRunner(recorder).run({
      session,
      inputs: roleInputs(session.context.roleSpec, `BUDGET_${session.context.roleSpec.id}`),
      validators: roleValidators(session.context.roleSpec),
      budgetBaseline: EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
    })
  }))

  for (const [index, result] of results.entries()) {
    const roleId = STRONGFLOW_ROLE_IDS[index]
    assert.equal(result.outcome, 'budget-exceeded', roleId)
    assert.equal(result.failure.code, 'TOKEN_BUDGET_EXCEEDED', roleId)
    assert.equal(result.eventInterval.firstSequence, '1')
    assert.equal(result.eventInterval.lastSequence, '2')
    assert.equal(result.eventInterval.eventCount, 2)
  }
  assert.equal(sessionManager.listSessions().length, 0)
  assert.equal(kernel.interrupts.length, 8)
  assert.equal(kernel.closes.length, 8)
  assert.equal(installer.disposals.length, 8)
  assert.ok(installer.disposals.every(disposal => disposal.outcome === 'failed'))
})

test('planner cannot start without the exact human review artifact', async t => {
  const value = await fixture(t)
  const kernel = new InstrumentedRoleKernel(value.home)
  const installer = new InstrumentedContextInstaller(kernel)
  const sessionManager = manager(value, kernel, installer)
  const roles = ['planner']
  const sessions = await Promise.all(roles.map(roleId => (
    sessionManager.create(assignmentFor(value, roleId, 'approval-bypass'))
  )))

  for (const session of sessions) {
    const inputs = roleInputs(session.context.roleSpec, `APPROVAL_${session.context.roleSpec.id}`)
      .filter(input => input.kind !== 'HUMAN_REVIEW_RECORD')
    const recorder = new MemoryRoleRecorder()
    const result = await roleRunner(recorder).run({
      session,
      inputs,
      validators: roleValidators(session.context.roleSpec),
      budgetBaseline: EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
    })
    assert.equal(result.outcome, 'failed')
    assert.equal(result.failure.code, 'INPUT_ARTIFACT_MISMATCH')
    assert.equal(result.eventInterval.eventCount, 0)
  }
  assert.equal(kernel.submissions.length, 0)
  assert.equal(sessionManager.listSessions().length, 0)
  assert.equal(kernel.interrupts.length, 0)
  assert.equal(kernel.closes.length, 1)
  assert.ok(installer.disposals.every(disposal => disposal.outcome === 'failed'))
})

test('RequirementSpec validation rejects a model attempt to hide solution design', async t => {
  const value = await fixture(t)
  const kernel = new InstrumentedRoleKernel(value.home, {
    hiddenSolutionRole: 'requirements',
  })
  const installer = new InstrumentedContextInstaller(kernel)
  const sessionManager = manager(value, kernel, installer)
  const session = await sessionManager.create(
    assignmentFor(value, 'requirements', 'hidden-solution'),
  )
  const recorder = new MemoryRoleRecorder()
  const result = await roleRunner(recorder).run({
    session,
    inputs: roleInputs(session.context.roleSpec, 'HIDDEN_SOLUTION_ATTEMPT'),
    validators: roleValidators(session.context.roleSpec, { rejectHiddenSolution: true }),
    budgetBaseline: EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
  })

  assert.equal(result.outcome, 'failed')
  assert.equal(result.failure.code, 'ARTIFACT_INVALID')
  assert.equal(result.eventInterval.eventCount, 3)
  assert.equal(kernel.interrupts.length, 0)
  assert.equal(kernel.closes.length, 1)
  assert.equal(installer.disposals[0].outcome, 'failed')
})
