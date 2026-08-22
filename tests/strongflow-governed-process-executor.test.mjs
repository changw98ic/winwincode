import assert from 'node:assert/strict'
import test from 'node:test'

import {
  StrongFlowExactProcessGrantAuthorizer,
  StrongFlowGovernedProcessExecutor,
  StrongFlowRoleToolExecutionError,
} from '../packages/strongflow/dist/index.js'

function request(overrides = {}) {
  return Object.freeze({
    jobId: 'job-process',
    stageRunId: 'stage-process',
    attemptId: 'attempt-process',
    roleId: 'executor',
    contextId: 'context-process',
    kernelSessionLineageId: 'lineage-process',
    kernelSessionId: 'session-process',
    kernelStreamId: 'stream-process',
    kernelSequence: '12',
    turnId: 'turn-process',
    callId: 'call-process',
    tool: 'command.run',
    arguments: Object.freeze({
      argv: Object.freeze(['/usr/bin/true']),
      cwd: '.',
    }),
    resolvedWorkspacePaths: Object.freeze(['/fixture/workspace']),
    excludedWorkspacePatterns: Object.freeze(['**/.env']),
    signal: new AbortController().signal,
    ...overrides,
  })
}

function grant(overrides = {}) {
  return Object.freeze({
    schemaVersion: 1,
    grantId: 'grant-process',
    jobId: 'job-process',
    stageRunId: 'stage-process',
    attemptId: 'attempt-process',
    roleId: 'executor',
    contextId: 'context-process',
    kernelSessionId: 'session-process',
    tool: 'command.run',
    argv: Object.freeze(['/usr/bin/true']),
    cwd: '/fixture/workspace',
    environment: Object.freeze({ LANG: 'C.UTF-8' }),
    timeoutMillis: 10_000,
    outputLimitBytes: 1_048_576,
    ...overrides,
  })
}

function fixture(options = {}) {
  const nativeCalls = []
  const cancellations = []
  const delegated = []
  const audit = []
  const kernel = {
    async executeGovernedCommand(value) {
      nativeCalls.push(structuredClone(value))
      if (options.nativeError !== undefined) throw options.nativeError
      return options.result ?? {
        schemaVersion: 1,
        sessionId: value.sessionId,
        commandId: value.commandId,
        status: 'exited',
        exitCode: 0,
        stdout: 'ok\n',
        stderr: '',
        sandbox: 'macos-seatbelt',
        network: 'restricted',
        environmentNames: ['CI', 'HOME', 'LANG', 'NO_COLOR', 'PATH', 'TEMP', 'TMP', 'TMPDIR'],
      }
    },
    async cancelGovernedCommand(sessionId, commandId) {
      cancellations.push({ sessionId, commandId })
    },
  }
  const executor = new StrongFlowGovernedProcessExecutor({
    kernel,
    grants: options.authorizer ?? new StrongFlowExactProcessGrantAuthorizer(options.grants ?? []),
    delegate: {
      async execute(value) {
        delegated.push(value)
        return { delegated: true }
      },
    },
    securityAudit: {
      append(event) {
        if (options.auditError !== undefined) throw options.auditError
        audit.push(structuredClone(event))
      },
    },
  })
  return { executor, nativeCalls, cancellations, delegated, audit }
}

test('blocks a process request with no exact trusted grant before native execution', async () => {
  const value = fixture()
  await assert.rejects(
    value.executor.execute(request()),
    error => error instanceof StrongFlowRoleToolExecutionError
      && error.kind === 'policy-denied'
      && error.code === 'PROCESS_GRANT_REQUIRED',
  )
  assert.equal(value.nativeCalls.length, 0)
  assert.deepEqual(value.audit.map(event => [event.type, event.outcome]), [[
    'strongflow.security.process.denied',
    'policy-denied',
  ]])
})

test('executes only an exact grant and records digests instead of command output', async () => {
  const value = fixture({ grants: [grant()] })
  const result = await value.executor.execute(request())
  assert.equal(value.nativeCalls.length, 1)
  assert.deepEqual(value.nativeCalls[0].argv, ['/usr/bin/true'])
  assert.deepEqual(value.nativeCalls[0].environment, { LANG: 'C.UTF-8' })
  assert.equal(result.status, 'completed')
  assert.equal(result.stdout, 'ok\n')
  assert.deepEqual(value.audit.map(event => event.type), [
    'strongflow.security.process.authorized',
    'strongflow.security.process.completed',
  ])
  assert.equal(JSON.stringify(value.audit).includes('ok\\n'), false)
  assert.match(value.audit[1].facts.stdoutSha256, /^sha256:[a-f0-9]{64}$/u)
})

test('keeps native sandbox denial distinct from ordinary task failure', async () => {
  const denied = fixture({
    grants: [grant()],
    result: {
      schemaVersion: 1,
      sessionId: 'session-process',
      commandId: 'fixture',
      status: 'sandbox-denied',
      exitCode: 1,
      stdout: '',
      stderr: 'operation not permitted',
      sandbox: 'macos-seatbelt',
      network: 'restricted',
      environmentNames: [],
    },
  })
  await assert.rejects(
    denied.executor.execute(request()),
    error => error instanceof StrongFlowRoleToolExecutionError
      && error.kind === 'sandbox-denied'
      && error.code === 'PROCESS_SANDBOX_DENIED',
  )

  const failed = fixture({
    grants: [grant()],
    result: {
      schemaVersion: 1,
      sessionId: 'session-process',
      commandId: 'fixture',
      status: 'exited',
      exitCode: 2,
      stdout: '',
      stderr: 'test failed',
      sandbox: 'macos-seatbelt',
      network: 'restricted',
      environmentNames: [],
    },
  })
  await assert.rejects(
    failed.executor.execute(request()),
    error => error instanceof StrongFlowRoleToolExecutionError
      && error.kind === 'task-failed'
      && error.code === 'PROCESS_EXIT_NONZERO',
  )
})

test('rejects a mismatched grant and an audit failure without native execution', async () => {
  const mismatch = fixture({
    authorizer: { authorize: async () => grant({ argv: ['/usr/bin/false'] }) },
  })
  await assert.rejects(
    mismatch.executor.execute(request()),
    error => error instanceof StrongFlowRoleToolExecutionError
      && error.code === 'PROCESS_GRANT_MISMATCH',
  )
  assert.equal(mismatch.nativeCalls.length, 0)

  const auditFailure = fixture({ grants: [grant()], auditError: new Error('disk failure') })
  await assert.rejects(
    auditFailure.executor.execute(request()),
    error => error instanceof StrongFlowRoleToolExecutionError
      && error.code === 'SECURITY_AUDIT_FAILED',
  )
  assert.equal(auditFailure.nativeCalls.length, 0)
})

test('delegates non-process tools without granting a child process', async () => {
  const value = fixture({ grants: [grant()] })
  const result = await value.executor.execute(request({
    tool: 'workspace.read',
    arguments: Object.freeze({ path: 'README.md' }),
  }))
  assert.deepEqual(result, { delegated: true })
  assert.equal(value.delegated.length, 1)
  assert.equal(value.nativeCalls.length, 0)
})
