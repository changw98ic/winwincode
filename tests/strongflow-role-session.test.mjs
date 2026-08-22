import assert from 'node:assert/strict'
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rm,
  stat,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  CandidateId,
  JobId,
  SourceSnapshotId,
  StageRunId,
  StrongFlowWorkspaceId,
  VerificationSnapshotId,
  STRONGFLOW_ROLE_IDS,
  createStrongFlowRoleConfiguration,
  strongFlowPermissionPolicyForRole,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowRoleSessionError,
  StrongFlowRoleSessionManager,
} from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)
const HASH_B = 'b'.repeat(64)
const HASH_C = 'c'.repeat(64)

const modelCatalog = Object.freeze([
  Object.freeze({
    provider: 'fixture-provider',
    model: 'fixture-model',
    reasoningEfforts: Object.freeze(['medium']),
  }),
  Object.freeze({
    provider: 'fixture-provider',
    model: 'fixture-verifier',
    reasoningEfforts: Object.freeze(['high']),
  }),
])

function roleAssignments(overrides = {}) {
  return Object.fromEntries(STRONGFLOW_ROLE_IDS.map((roleId, index) => [
    roleId,
    {
      modelRoute: {
        provider: 'fixture-provider',
        model: roleId === 'adversarial-verifier'
          ? 'fixture-verifier'
          : 'fixture-model',
      },
      reasoningEffort: roleId === 'adversarial-verifier' ? 'high' : 'medium',
      budget: {
        maxTurns: 8 + index + (overrides[roleId]?.maxTurnsDelta ?? 0),
        maxWallTimeMillis: 60_000 + index,
        maxTotalTokens: 10_000 + index,
        maxCostUsdMicros: 1_000_000 + index,
      },
    },
  ]))
}

function roleConfiguration(overrides) {
  return createStrongFlowRoleConfiguration(roleAssignments(overrides), modelCatalog)
}

function deferred() {
  let resolvePromise = () => {}
  let rejectPromise = () => {}
  const promise = new Promise((resolve, reject) => {
    resolvePromise = resolve
    rejectPromise = reject
  })
  return { promise, resolve: resolvePromise, reject: rejectPromise }
}

function waitForStop(signal, close) {
  if (signal?.aborted === true) return Promise.resolve()
  const aborted = new Promise(resolve => signal?.addEventListener('abort', resolve, { once: true }))
  return Promise.race([close.promise, aborted])
}

function effectivePolicyFor(options) {
  const authority = options.governedAuthority
  assert.ok(authority, 'StrongFlow must provide native authority before session creation')
  return Object.freeze({
    schemaVersion: 1,
    authority: 'codex-core',
    roleId: authority.roleId,
    permissionPreset: authority.permissionPreset,
    workspaceMode: authority.workspaceMode,
    workspaceRoot: authority.workspaceRoot,
    visibleTools: Object.freeze([...authority.visibleTools]),
    filesystem: authority.workspaceMode === 'candidate-write'
      ? 'managed-workspace-write'
      : 'managed-read-only',
    network: 'restricted',
    process: 'dynamic-tools-only',
    environment: 'empty',
    approvalPolicy: 'on-request',
    approvalsReviewer: 'user',
    loginShell: false,
    environmentSelections: Object.freeze([]),
    instructionSources: Object.freeze([]),
  })
}

class FakeKernel {
  constructor(home, sequences = [1n]) {
    this.home = home
    this.sequences = sequences
  }

  creates = []
  resumes = []
  subscriptions = []
  submissions = []
  interrupts = []
  closes = []
  sessions = new Map()
  nextSession = 1

  async createSession(options) {
    this.creates.push(structuredClone(options))
    return this.#newSession('created', options)
  }

  async resumeSession(options) {
    this.resumes.push(structuredClone(options))
    return this.#newSession('resumed', options)
  }

  async submitTurn(sessionId, text) {
    this.submissions.push({ sessionId, text })
    return Object.freeze({ status: 'started', turnId: `turn-${this.submissions.length}` })
  }

  async interrupt(sessionId) {
    this.interrupts.push(sessionId)
    return `interrupt-${sessionId}`
  }

  async resolveApproval() {
    return 'approval-resolved'
  }

  async resolveDynamicTool() {
    return 'dynamic-tool-resolved'
  }

  async closeSession(sessionId) {
    this.closes.push(sessionId)
    this.sessions.get(sessionId)?.close.resolve()
  }

  async *events(sessionId, options = {}) {
    this.subscriptions.push(sessionId)
    const session = this.sessions.get(sessionId)
    if (session === undefined) throw new Error(`unknown fake session ${sessionId}`)
    for (const sequence of this.sequences) {
      yield Object.freeze({
        sequence,
        kind: 'fixture.event',
        payload: Object.freeze({ type: 'fixture.event', sequence: sequence.toString() }),
        rawJson: JSON.stringify({ type: 'fixture.event', sequence: sequence.toString() }),
      })
    }
    await waitForStop(options.signal, session.close)
  }

  crash(sessionId) {
    this.sessions.get(sessionId)?.close.resolve()
  }

  #newSession(source, options) {
    const ordinal = this.nextSession++
    const sessionId = `${source}-kernel-${ordinal}`
    this.sessions.set(sessionId, { close: deferred() })
    return Object.freeze({
      sessionId,
      rolloutPath: join(this.home, `${source}-rollout-${ordinal}.jsonl`),
      effectivePolicy: effectivePolicyFor(options),
    })
  }
}

class MissingRolloutKernel extends FakeKernel {
  async createSession(options) {
    this.creates.push(structuredClone(options))
    return Object.freeze({
      sessionId: 'missing-rollout-kernel',
      effectivePolicy: effectivePolicyFor(options),
    })
  }
}

class BrokenEventsKernel extends FakeKernel {
  events() {
    throw new Error('fixture event subscription failed')
  }
}

class AlteredEvidenceKernel extends FakeKernel {
  async createSession(options) {
    const info = await super.createSession(options)
    return Object.freeze({
      ...info,
      effectivePolicy: Object.freeze({
        ...info.effectivePolicy,
        network: 'enabled',
      }),
    })
  }
}

class MissingEvidenceKernel extends FakeKernel {
  async createSession(options) {
    const info = await super.createSession(options)
    return Object.freeze({
      sessionId: info.sessionId,
      rolloutPath: info.rolloutPath,
    })
  }
}

class RecordingInstaller {
  constructor(options = {}) {
    this.options = options
  }

  requests = []
  disposals = []

  async install(request) {
    this.requests.push(request)
    await this.options.onInstall?.(request)
    if (this.options.failure !== undefined) throw this.options.failure
    return Object.freeze({
      contextId: this.options.contextId ?? request.context.contextId,
      handleEvent: event => this.options.onEvent?.(event),
      dispose: async disposal => {
        this.disposals.push(disposal)
        await this.options.onDispose?.(disposal)
      },
    })
  }
}

async function fixture(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-role-session-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const workspaceRoot = join(home, 'workspace-fixture')
  const source = join(workspaceRoot, 'source')
  const candidate = join(workspaceRoot, 'candidate')
  await mkdir(source, { recursive: true })
  await mkdir(candidate, { recursive: true })
  const verification = {}
  const outputs = {}
  for (const roleId of ['reviewer', 'verifier', 'adversarial-verifier']) {
    verification[roleId] = join(workspaceRoot, 'verification', roleId)
    outputs[roleId] = join(workspaceRoot, 'verification-output', roleId)
    await mkdir(verification[roleId], { recursive: true })
    await mkdir(outputs[roleId], { recursive: true })
  }
  const canonicalVerification = Object.fromEntries(await Promise.all(
    Object.entries(verification).map(async ([roleId, path]) => [roleId, await realpath(path)]),
  ))
  const canonicalOutputs = Object.fromEntries(await Promise.all(
    Object.entries(outputs).map(async ([roleId, path]) => [roleId, await realpath(path)]),
  ))
  return {
    home,
    workspaceRoot: await realpath(workspaceRoot),
    source: await realpath(source),
    candidate: await realpath(candidate),
    verification: canonicalVerification,
    outputs: canonicalOutputs,
    workspaceId: StrongFlowWorkspaceId(`workspace-sha256-${HASH_A}`),
    sourceSnapshotId: SourceSnapshotId(`source-sha256-${HASH_B}`),
    candidateId: CandidateId('candidate-role-session-fixture'),
    verificationSnapshotId: VerificationSnapshotId(`verification-sha256-${HASH_C}`),
  }
}

function workspaceFor(fixtureValue, roleId, stageRunId) {
  const base = {
    roleId,
    stageRunId,
    workspaceId: fixtureValue.workspaceId,
    sourceSnapshotId: fixtureValue.sourceSnapshotId,
  }
  if (['requirements', 'solution', 'planner'].includes(roleId)) {
    return Object.freeze({
      ...base,
      mode: 'source-read-only',
      path: fixtureValue.source,
    })
  }
  if (['executor', 'remediator'].includes(roleId)) {
    return Object.freeze({
      ...base,
      mode: 'candidate-write',
      path: fixtureValue.candidate,
      ...(roleId === 'remediator' ? { candidateId: fixtureValue.candidateId } : {}),
    })
  }
  return Object.freeze({
    ...base,
    mode: 'candidate-read-only',
    path: fixtureValue.verification[roleId],
    temporaryOutputPath: fixtureValue.outputs[roleId],
    candidateId: fixtureValue.candidateId,
    verificationSnapshotId: fixtureValue.verificationSnapshotId,
  })
}

function assignmentFor(fixtureValue, roleId, identity = 'shared') {
  const stageRunId = StageRunId(`stage-run-${identity}`)
  return Object.freeze({
    jobId: JobId(`job-${identity}`),
    stageRunId,
    attemptId: AttemptId(`attempt-${identity}`),
    roleId,
    workspace: workspaceFor(fixtureValue, roleId, stageRunId),
  })
}

function managerOptions(fixtureValue, kernel, installer, configuration = roleConfiguration()) {
  let time = 1_900_000_000_000
  return {
    home: fixtureValue.home,
    kernel,
    installer,
    roleConfiguration: configuration,
    modelCatalog,
    now: () => time++,
  }
}

function expectSessionError(code) {
  return error => error instanceof StrongFlowRoleSessionError && error.code === code
}

function sessionDirectoryFor(home, lineageId) {
  return join(home, 'strongflow-role-sessions', lineageId.replace('kernel-lineage-sha256-', ''))
}

function deadPid() {
  for (const pid of [2_147_483_647, 2_147_483_646, 999_999_999]) {
    try {
      process.kill(pid, 0)
    } catch (error) {
      if (error?.code === 'ESRCH') return pid
    }
  }
  throw new Error('test host has no known absent process id')
}

test('publishes a role session only after its full context is installed and stored', async t => {
  const value = await fixture(t)
  const kernel = new FakeKernel(value.home)
  const entered = deferred()
  const release = deferred()
  let manager
  const installer = new RecordingInstaller({
    async onInstall(request) {
      assert.equal(manager.listSessions().length, 0)
      assert.ok(kernel.subscriptions.includes(request.kernel.kernelSessionId))
      entered.resolve()
      await release.promise
    },
  })
  manager = new StrongFlowRoleSessionManager(managerOptions(value, kernel, installer))
  const assignment = assignmentFor(value, 'requirements', 'publication')

  const creating = manager.create(assignment)
  await entered.promise
  assert.deepEqual(manager.listSessions(), [])
  const rootEntries = await readdir(join(value.home, 'strongflow-role-sessions'))
  assert.equal(rootEntries.some(entry => /^[a-f0-9]{64}$/u.test(entry)), false)

  release.resolve()
  const session = await creating
  assert.equal(manager.listSessions().length, 1)
  assert.equal(session.context.jobId, assignment.jobId)
  assert.equal(session.context.stageRunId, assignment.stageRunId)
  assert.equal(session.context.attemptId, assignment.attemptId)
  assert.equal(session.context.roleSpec.id, 'requirements')
  assert.equal(session.context.roleSpec.modelRoute.provider, 'fixture-provider')
  assert.equal(session.context.roleSpec.reasoningEffort, 'medium')
  assert.equal(session.context.workspace.path, value.source)
  assert.ok(Object.isFrozen(session.context))
  assert.ok(Object.isFrozen(session.context.roleSpec))
  assert.ok(Object.isFrozen(strongFlowPermissionPolicyForRole('requirements')))
  assert.ok(Object.isFrozen(session.context.roleSpec.budget))
  assert.ok(Object.isFrozen(session.context.workspace))
  assert.equal(kernel.creates.length, 1)
  assert.equal(kernel.creates[0].cwd, value.source)
  assert.equal(kernel.creates[0].provider, 'fixture-provider')
  assert.equal(kernel.creates[0].model, 'fixture-model')
  assert.equal(kernel.creates[0].governedAuthority.roleId, 'requirements')
  assert.deepEqual(
    kernel.creates[0].governedAuthority.visibleTools,
    strongFlowPermissionPolicyForRole('requirements').tools.allowed,
  )

  const event = await session.events()[Symbol.asyncIterator]().next()
  assert.equal(event.done, false)
  assert.equal(event.value.event.sequence, 1n)
  assert.equal(event.value.contextId, session.context.contextId)
  assert.equal(event.value.kernelSessionLineageId, session.context.kernelSessionLineageId)

  const directory = sessionDirectoryFor(value.home, session.context.kernelSessionLineageId)
  const storedContext = JSON.parse(await readFile(join(directory, 'context.json'), 'utf8'))
  assert.deepEqual(storedContext, session.context)
  await session.dispose()
  assert.equal(session.state, 'closed')
  assert.deepEqual(manager.listSessions(), [])
  assert.deepEqual(kernel.closes, [session.kernel.kernelSessionId])
  assert.deepEqual(installer.disposals, [{
    outcome: 'completed',
    reason: 'StrongFlow role session completed and was disposed',
  }])
  await assert.rejects(stat(join(directory, 'owner.json')), error => error?.code === 'ENOENT')
  const lifecycle = (await readFile(join(directory, 'lifecycle.jsonl'), 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line))
  assert.deepEqual(lifecycle.map(record => record.recordType), [
    'kernel.accepted',
    'session.terminal',
  ])
  assert.equal(lifecycle[1].outcome, 'completed')
  await assert.rejects(manager.resume(assignment), expectSessionError('SESSION_TERMINAL'))
})

test('rolls back native and installed resources when setup is not accepted', async t => {
  const cases = [
    {
      name: 'installer failure',
      installer: () => new RecordingInstaller({ failure: new Error('fixture setup failed') }),
      code: 'SESSION_SETUP_FAILED',
      disposalCount: 0,
    },
    {
      name: 'wrong installed context',
      installer: () => new RecordingInstaller({
        contextId: `role-context-sha256-${'f'.repeat(64)}`,
      }),
      code: 'CONTEXT_INSTALLATION_MISMATCH',
      disposalCount: 1,
    },
  ]

  for (const setupCase of cases) {
    await t.test(setupCase.name, async t => {
      const value = await fixture(t)
      const kernel = new FakeKernel(value.home)
      const installer = setupCase.installer()
      const manager = new StrongFlowRoleSessionManager(
        managerOptions(value, kernel, installer),
      )
      const assignment = assignmentFor(value, 'solution', setupCase.name.replaceAll(' ', '-'))

      await assert.rejects(manager.create(assignment), expectSessionError(setupCase.code))
      assert.equal(manager.listSessions().length, 0)
      assert.equal(kernel.creates.length, 1)
      assert.deepEqual(kernel.closes, ['created-kernel-1'])
      assert.equal(installer.disposals.length, setupCase.disposalCount)
      const rootEntries = await readdir(join(value.home, 'strongflow-role-sessions'))
      assert.equal(rootEntries.some(entry => /^[a-f0-9]{64}$/u.test(entry)), false)
      await assert.rejects(manager.resume(assignment), expectSessionError('SESSION_NOT_FOUND'))
    })
  }
})

test('closes a native session that returns an invalid setup identity', async t => {
  const value = await fixture(t)
  const kernel = new MissingRolloutKernel(value.home)
  const manager = new StrongFlowRoleSessionManager(managerOptions(
    value,
    kernel,
    new RecordingInstaller(),
  ))
  const assignment = assignmentFor(value, 'planner', 'missing-rollout')

  await assert.rejects(manager.create(assignment), expectSessionError('SESSION_SETUP_FAILED'))
  assert.deepEqual(kernel.closes, ['missing-rollout-kernel'])
  assert.equal(manager.listSessions().length, 0)
  const rootEntries = await readdir(join(value.home, 'strongflow-role-sessions'))
  assert.equal(rootEntries.some(entry => /^[a-f0-9]{64}$/u.test(entry)), false)
})

test('rejects missing or partial kernel enforcement before publication', async t => {
  for (const [name, Kernel] of [
    ['missing-enforcement', MissingEvidenceKernel],
    ['altered-enforcement', AlteredEvidenceKernel],
  ]) {
    await t.test(name, async t => {
      const value = await fixture(t)
      const kernel = new Kernel(value.home)
      const installer = new RecordingInstaller()
      const manager = new StrongFlowRoleSessionManager(managerOptions(value, kernel, installer))
      const assignment = assignmentFor(value, 'executor', name)

      await assert.rejects(
        manager.create(assignment),
        expectSessionError('ENFORCEMENT_UNAVAILABLE'),
      )
      assert.deepEqual(kernel.closes, ['created-kernel-1'])
      assert.equal(installer.requests.length, 0)
      assert.equal(manager.listSessions().length, 0)
      const rootEntries = await readdir(join(value.home, 'strongflow-role-sessions'))
      assert.equal(rootEntries.some(entry => /^[a-f0-9]{64}$/u.test(entry)), false)
    })
  }
})

test('does not publish a session whose ordered event subscription fails', async t => {
  const value = await fixture(t)
  const kernel = new BrokenEventsKernel(value.home)
  const installer = new RecordingInstaller()
  const manager = new StrongFlowRoleSessionManager(managerOptions(value, kernel, installer))
  const assignment = assignmentFor(value, 'planner', 'event-subscription')

  await assert.rejects(manager.create(assignment), expectSessionError('EVENT_STREAM_FAILED'))
  assert.equal(installer.requests.length, 0)
  assert.deepEqual(kernel.closes, ['created-kernel-1'])
  assert.equal(manager.listSessions().length, 0)
})

test('resumes an abandoned generation with the exact stored role and workspace snapshot', async t => {
  const value = await fixture(t)
  const assignment = assignmentFor(value, 'verifier', 'resume')
  const firstKernel = new FakeKernel(value.home)
  const firstInstaller = new RecordingInstaller()
  const firstManager = new StrongFlowRoleSessionManager(
    managerOptions(value, firstKernel, firstInstaller),
  )
  const first = await firstManager.create(assignment)
  firstKernel.crash(first.kernel.kernelSessionId)

  const secondKernel = new FakeKernel(value.home)
  const secondInstaller = new RecordingInstaller()
  const secondManager = new StrongFlowRoleSessionManager(
    managerOptions(value, secondKernel, secondInstaller),
  )
  await assert.rejects(secondManager.resume(assignment), expectSessionError('SESSION_ACTIVE'))
  assert.equal(secondKernel.resumes.length, 0)

  const directory = sessionDirectoryFor(value.home, first.context.kernelSessionLineageId)
  const ownerPath = join(directory, 'owner.json')
  const owner = JSON.parse(await readFile(ownerPath, 'utf8'))
  owner.pid = deadPid()
  await writeFile(ownerPath, `${JSON.stringify(owner, null, 2)}\n`, 'utf8')

  const resumed = await secondManager.resume(assignment)
  assert.equal(resumed.kernel.generation, 2)
  assert.equal(resumed.kernel.source, 'resume')
  assert.equal(resumed.context.contextId, first.context.contextId)
  assert.deepEqual(resumed.context.roleSpec, first.context.roleSpec)
  assert.deepEqual(resumed.context.workspace, first.context.workspace)
  assert.equal(secondKernel.resumes.length, 1)
  assert.equal(secondKernel.resumes[0].rolloutPath, first.kernel.rolloutPath)
  assert.equal(secondKernel.resumes[0].cwd, value.verification.verifier)
  assert.equal(secondKernel.resumes[0].provider, 'fixture-provider')
  assert.equal(secondKernel.resumes[0].model, 'fixture-model')
  assert.equal(secondKernel.resumes[0].governedAuthority.roleId, 'verifier')
  assert.equal(secondInstaller.requests[0].source, 'resume')
  assert.equal(secondInstaller.requests[0].context.contextId, first.context.contextId)
  const event = await resumed.events()[Symbol.asyncIterator]().next()
  assert.equal(event.value.generation, 2)
  assert.equal(event.value.kernelStreamId, resumed.kernel.kernelStreamId)
  await resumed.dispose()
})

test('resumes every abandoned role without sharing its context or kernel generation', async t => {
  const value = await fixture(t)
  const firstKernel = new FakeKernel(value.home)
  const firstInstaller = new RecordingInstaller()
  const firstManager = new StrongFlowRoleSessionManager(
    managerOptions(value, firstKernel, firstInstaller),
  )
  const assignments = STRONGFLOW_ROLE_IDS.map(roleId => (
    assignmentFor(value, roleId, 'all-role-resume')
  ))
  const firstSessions = await Promise.all(
    assignments.map(assignment => firstManager.create(assignment)),
  )
  for (const session of firstSessions) {
    firstKernel.crash(session.kernel.kernelSessionId)
    const ownerPath = join(
      sessionDirectoryFor(value.home, session.context.kernelSessionLineageId),
      'owner.json',
    )
    const owner = JSON.parse(await readFile(ownerPath, 'utf8'))
    owner.pid = deadPid()
    await writeFile(ownerPath, `${JSON.stringify(owner, null, 2)}\n`, 'utf8')
  }

  const resumedKernel = new FakeKernel(value.home)
  const resumedInstaller = new RecordingInstaller()
  const resumedManager = new StrongFlowRoleSessionManager(
    managerOptions(value, resumedKernel, resumedInstaller),
  )
  const resumedSessions = await Promise.all(
    assignments.map(assignment => resumedManager.resume(assignment)),
  )

  assert.equal(new Set(resumedSessions.map(session => session.context.contextId)).size, 8)
  assert.equal(new Set(resumedSessions.map(session => session.kernel.kernelSessionId)).size, 8)
  assert.ok(resumedSessions.every(session => session.kernel.generation === 2))
  assert.ok(resumedSessions.every(session => session.kernel.source === 'resume'))
  assert.deepEqual(
    resumedSessions.map(session => session.context.contextId),
    firstSessions.map(session => session.context.contextId),
  )
  assert.deepEqual(
    resumedSessions.map(session => session.context.roleSpec.id),
    STRONGFLOW_ROLE_IDS,
  )
  assert.deepEqual(
    resumedInstaller.requests.map(request => request.source),
    STRONGFLOW_ROLE_IDS.map(() => 'resume'),
  )
  assert.equal(resumedKernel.resumes.length, 8)

  await Promise.all(resumedSessions.map(session => session.dispose()))
  assert.equal(resumedManager.listSessions().length, 0)
  assert.equal(resumedInstaller.disposals.length, 8)
  assert.equal(resumedKernel.closes.length, 8)
})

test('rejects changed role or workspace configuration before resuming the kernel', async t => {
  const value = await fixture(t)
  const assignment = assignmentFor(value, 'requirements', 'snapshot-check')
  const firstKernel = new FakeKernel(value.home)
  const firstManager = new StrongFlowRoleSessionManager(managerOptions(
    value,
    firstKernel,
    new RecordingInstaller(),
  ))
  const first = await firstManager.create(assignment)
  firstKernel.crash(first.kernel.kernelSessionId)

  const changedRoleKernel = new FakeKernel(value.home)
  const changedRoleManager = new StrongFlowRoleSessionManager(managerOptions(
    value,
    changedRoleKernel,
    new RecordingInstaller(),
    roleConfiguration({ requirements: { maxTurnsDelta: 1 } }),
  ))
  await assert.rejects(
    changedRoleManager.resume(assignment),
    expectSessionError('ROLE_SNAPSHOT_MISMATCH'),
  )
  assert.equal(changedRoleKernel.resumes.length, 0)

  const alternateSource = join(value.workspaceRoot, 'alternate-source')
  await mkdir(alternateSource)
  const changedWorkspaceAssignment = Object.freeze({
    ...assignment,
    workspace: Object.freeze({ ...assignment.workspace, path: alternateSource }),
  })
  const changedWorkspaceKernel = new FakeKernel(value.home)
  const changedWorkspaceManager = new StrongFlowRoleSessionManager(managerOptions(
    value,
    changedWorkspaceKernel,
    new RecordingInstaller(),
  ))
  await assert.rejects(
    changedWorkspaceManager.resume(changedWorkspaceAssignment),
    expectSessionError('CONTEXT_SNAPSHOT_MISMATCH'),
  )
  assert.equal(changedWorkspaceKernel.resumes.length, 0)
})

test('creates a distinct immutable native context for every canonical role', async t => {
  const value = await fixture(t)
  const kernel = new FakeKernel(value.home)
  const installer = new RecordingInstaller()
  const manager = new StrongFlowRoleSessionManager(managerOptions(value, kernel, installer))
  const sessions = []
  for (const roleId of STRONGFLOW_ROLE_IDS) {
    sessions.push(await manager.create(assignmentFor(value, roleId, 'all-roles')))
  }

  assert.equal(manager.listSessions().length, STRONGFLOW_ROLE_IDS.length)
  assert.equal(new Set(sessions.map(session => session.context.kernelSessionLineageId)).size, 8)
  assert.equal(new Set(sessions.map(session => session.context.contextId)).size, 8)
  assert.equal(new Set(sessions.map(session => session.context.roleSpec.id)).size, 8)
  assert.equal(new Set(sessions.map(session => session.context.roleSpec.systemInstructions)).size, 8)
  assert.equal(new Set(sessions.map(session => session.kernel.kernelSessionId)).size, 8)
  assert.deepEqual(installer.requests.map(request => request.context.roleSpec.id), STRONGFLOW_ROLE_IDS)
  const adversarial = sessions.find(
    session => session.context.roleSpec.id === 'adversarial-verifier',
  )
  assert.equal(adversarial.context.roleSpec.modelRoute.model, 'fixture-verifier')
  assert.equal(adversarial.context.roleSpec.reasoningEffort, 'high')

  await Promise.all(sessions.map(session => session.dispose()))
  assert.equal(manager.listSessions().length, 0)
  assert.equal(installer.disposals.length, 8)
  assert.equal(kernel.closes.length, 8)
})

test('cancellation interrupts the exact kernel session and records a terminal result', async t => {
  const value = await fixture(t)
  const kernel = new FakeKernel(value.home)
  const installer = new RecordingInstaller()
  const manager = new StrongFlowRoleSessionManager(managerOptions(value, kernel, installer))
  const assignment = assignmentFor(value, 'executor', 'cancel')
  const session = await manager.create(assignment)

  await session.cancel('User cancelled this execution attempt')
  assert.equal(session.state, 'cancelled')
  assert.deepEqual(kernel.interrupts, [session.kernel.kernelSessionId])
  assert.deepEqual(kernel.closes, [session.kernel.kernelSessionId])
  assert.deepEqual(installer.disposals, [{
    outcome: 'cancelled',
    reason: 'User cancelled this execution attempt',
  }])
  assert.equal(manager.listSessions().length, 0)
  await assert.rejects(manager.resume(assignment), expectSessionError('SESSION_TERMINAL'))
})

test('submits one governed turn through the exact native session and can record role failure', async t => {
  const value = await fixture(t)
  const kernel = new FakeKernel(value.home)
  const installer = new RecordingInstaller()
  const manager = new StrongFlowRoleSessionManager(managerOptions(value, kernel, installer))
  const session = await manager.create(assignmentFor(value, 'requirements', 'submission'))

  const submission = await session.submitTurn('Produce the exact RequirementSpec envelope.')
  assert.deepEqual(submission, { status: 'started', turnId: 'turn-1' })
  assert.deepEqual(kernel.submissions, [{
    sessionId: session.kernel.kernelSessionId,
    text: 'Produce the exact RequirementSpec envelope.',
  }])
  await assert.rejects(
    session.submitTurn('A second turn is outside this governed assignment.'),
    expectSessionError('TURN_ALREADY_SUBMITTED'),
  )

  await session.fail('Role output failed schema validation', { interrupt: true })
  assert.equal(session.state, 'failed')
  assert.deepEqual(kernel.interrupts, [session.kernel.kernelSessionId])
  assert.deepEqual(kernel.closes, [session.kernel.kernelSessionId])
  assert.deepEqual(installer.disposals, [{
    outcome: 'failed',
    reason: 'Role output failed schema validation',
  }])
  await assert.rejects(
    manager.resume(assignmentFor(value, 'requirements', 'submission')),
    expectSessionError('SESSION_TERMINAL'),
  )
})

test('teardown releases a session even when its bounded event buffer is full', async t => {
  const value = await fixture(t)
  const kernel = new FakeKernel(value.home, [1n, 2n, 3n])
  const installer = new RecordingInstaller()
  const manager = new StrongFlowRoleSessionManager({
    ...managerOptions(value, kernel, installer),
    eventBufferCapacity: 1,
  })
  const session = await manager.create(assignmentFor(value, 'solution', 'full-buffer'))
  await new Promise(resolve => setImmediate(resolve))

  await session.dispose()
  assert.equal(session.state, 'closed')
  assert.deepEqual(kernel.closes, [session.kernel.kernelSessionId])
  assert.equal(installer.disposals.length, 1)
})
