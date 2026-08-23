import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { strongFlowRoleSessionPolicy } from '../packages/contracts/dist/index.js'
import {
  KernelError,
  WinWinCodeKernel,
} from '../packages/native/dist/index.js'

async function collectAvailableEvents(kernel, sessionId) {
  const events = []
  while (true) {
    const poll = await kernel.pollEvent(sessionId, 10)
    if (poll.status !== 'event') return events
    events.push(poll.event)
  }
}

test('role-scoped create and resume use Codex permissions and retain Codex capabilities', async t => {
  const root = mkdtempSync(join(tmpdir(), 'winwincode-native-role-'))
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
          id: 'role-session-answer',
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

  const rolePolicy = strongFlowRoleSessionPolicy('requirements')
  const created = await kernel.createSession({
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
    rolePolicy,
  })
  assert.ok(created.rolloutPath)
  const events = [...await collectAvailableEvents(kernel, created.sessionId)]
  const submission = await kernel.submitTurn(created.sessionId, 'Record the fixture requirement.')
  assert.equal(submission.status, 'started')
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    const poll = await kernel.pollEvent(created.sessionId, 250)
    if (poll.status !== 'event') continue
    events.push(poll.event)
    if (poll.event.kind === 'turn_complete') break
  }
  assert.ok(events.some(event => event.kind === 'turn_complete'))
  const configured = events.find(event => event.payload?.msg?.type === 'session_configured')
  assert.ok(configured)
  assert.equal(configured.payload.msg.approval_policy, 'on-request')
  assert.equal(configured.payload.msg.approvals_reviewer, 'user')
  assert.equal(configured.payload.msg.permission_profile.type, 'managed')
  assert.equal(configured.payload.msg.permission_profile.network, 'restricted')
  assert.equal(configured.payload.msg.permission_profile.file_system.type, 'restricted')
  assert.ok(configured.payload.msg.permission_profile.file_system.entries.every(
    entry => entry.access !== 'write',
  ))
  assert.equal(modelRequests.length, 1)
  const requestText = JSON.stringify(modelRequests[0].request)
  for (const capability of ['exec_command', 'update_plan', 'spawn_agent']) {
    assert.match(requestText, new RegExp(capability, 'u'))
  }
  await kernel.closeSession(created.sessionId)

  const resumed = await kernel.resumeSession({
    rolloutPath: created.rolloutPath,
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
    rolePolicy,
  })
  assert.equal(resumed.sessionId, created.sessionId)
  await kernel.closeSession(resumed.sessionId)

  await assert.rejects(
    kernel.createSession({
      cwd: workspace,
      provider: 'fixture-provider',
      model: 'fixture-model',
      rolePolicy: { ...rolePolicy, workspaceMode: 'candidate-write' },
    }),
    error => error instanceof KernelError && error.code === 'INVALID_ARGUMENT',
  )
  await assert.rejects(
    kernel.createSession({
      cwd: workspace,
      provider: 'fixture-provider',
      model: 'fixture-model',
      rolePolicy: { ...rolePolicy, unexpected: true },
    }),
    error => error instanceof KernelError && error.code === 'INVALID_ARGUMENT',
  )
  assert.deepEqual(await kernel.listSessions(), [])
})
