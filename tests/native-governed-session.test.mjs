import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
  KernelError,
  WinWinCodeKernel,
} from '../packages/native/dist/index.js'

function authority(workspaceRoot, overrides = {}) {
  return Object.freeze({
    schemaVersion: GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
    roleId: 'requirements',
    permissionPreset: 'definition-read',
    workspaceMode: 'source-read-only',
    workspaceRoot,
    systemInstructions: 'Collect requirements without designing the solution.',
    reasoningEffort: 'medium',
    visibleTools: Object.freeze([
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
    ]),
    ...overrides,
  })
}

test('native create and resume preserve one exact governed role authority', async t => {
  const root = mkdtempSync(join(tmpdir(), 'winwincode-native-governed-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const home = join(root, 'home')
  const workspacePath = join(root, 'workspace')
  mkdirSync(workspacePath)
  const workspace = realpathSync(workspacePath)
  const modelRequests = []
  const kernel = new WinWinCodeKernel({
    home,
    modelPort: {
      async *stream(request) {
        modelRequests.push(request)
        yield { type: 'created' }
        yield { type: 'server_model', model: request.request.model }
        const item = {
          type: 'message',
          id: 'governed-session-answer',
          role: 'assistant',
          content: [{ type: 'output_text', text: 'requirements recorded' }],
          phase: 'final_answer',
        }
        yield { type: 'output_item_added', item: { ...item, content: [] } }
        yield { type: 'output_text_delta', delta: 'requirements recorded' }
        yield { type: 'output_item_done', item }
        yield { type: 'completed', responseId: request.requestId, endTurn: true }
      },
    },
  })
  t.after(() => kernel.shutdown())

  const governedAuthority = authority(workspace)
  const created = await kernel.createSession({
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
    governedAuthority,
  })
  assert.ok(created.rolloutPath)
  assert.deepEqual(created.effectivePolicy, {
    schemaVersion: 1,
    authority: 'codex-core',
    roleId: 'requirements',
    permissionPreset: 'definition-read',
    workspaceMode: 'source-read-only',
    workspaceRoot: workspace,
    visibleTools: governedAuthority.visibleTools,
    filesystem: 'managed-read-only',
    network: 'restricted',
    process: 'dynamic-tools-with-governed-command-api',
    environment: 'empty',
    governedProcess: 'platform-sandbox-required',
    governedProcessNetwork: 'restricted',
    governedProcessEnvironment: 'explicit-allowlist',
    credentials: 'dsh-reference-only',
    approvalPolicy: 'on-request',
    approvalsReviewer: 'user',
    loginShell: false,
    environmentSelections: [],
    instructionSources: [],
  })
  const startup = await kernel.pollEvent(created.sessionId, 5_000)
  assert.equal(startup.status, 'event')
  assert.equal(startup.event.kind, 'mcp_startup_complete')
  const submission = await kernel.submitTurn(created.sessionId, 'Record the fixture requirement.')
  assert.equal(submission.status, 'started')
  const deadline = Date.now() + 5_000
  let completed = false
  while (!completed && Date.now() < deadline) {
    const poll = await kernel.pollEvent(created.sessionId, 250)
    completed = poll.status === 'event' && poll.event.kind === 'turn_complete'
  }
  assert.equal(completed, true)
  assert.equal(modelRequests.length, 1)
  await kernel.closeSession(created.sessionId)

  const resumed = await kernel.resumeSession({
    rolloutPath: created.rolloutPath,
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
    governedAuthority,
  })
  assert.equal(resumed.sessionId, created.sessionId)
  assert.deepEqual(resumed.effectivePolicy, created.effectivePolicy)
  await kernel.closeSession(resumed.sessionId)

  for (const drift of [
    { permissionPreset: 'candidate-write' },
    { visibleTools: governedAuthority.visibleTools.slice(0, -1) },
  ]) {
    await assert.rejects(
      kernel.createSession({
        cwd: workspace,
        provider: 'fixture-provider',
        model: 'fixture-model',
        governedAuthority: authority(workspace, drift),
      }),
      error => error instanceof KernelError && error.code === 'INVALID_GOVERNED_AUTHORITY',
    )
  }
  await assert.rejects(
    kernel.createSession({
      cwd: workspace,
      provider: 'fixture-provider',
      model: 'fixture-model',
      governedAuthority: authority(workspace, { unexpected: true }),
    }),
    error => error instanceof KernelError && error.code === 'INVALID_ARGUMENT',
  )
  assert.deepEqual(await kernel.listSessions(), [])
})
