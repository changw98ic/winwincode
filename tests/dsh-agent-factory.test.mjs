import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { Context } from '@deepseek-ai/cordis'
import AgentRegistry, { installModelSelection } from '@deepseek-ai/dsh-agent'
import LlmRuntime, { createUserMessage } from '@deepseek-ai/dsh-llm'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import JsonlSessionPersistence from '@deepseek-ai/dsh-session-persistence-jsonl'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ApprovalService from '@deepseek-ai/dsh-user-approval'

import { strongFlowRoleSessionPolicy } from '../packages/contracts/dist/index.js'
import {
  RuntimeSessionLedger,
  WinWinCodeAgentFactory,
} from '../packages/dsh-profile/dist/index.js'

function rawEvent(sequence, type, data = {}, submissionId = 'submission-1') {
  const envelope = { id: submissionId, msg: { type, ...data } }
  return {
    sequence: BigInt(sequence),
    kind: type,
    payload: envelope,
    rawJson: JSON.stringify(envelope),
  }
}

class EventQueue {
  values = []
  waiters = []
  closed = false

  push(value) {
    const waiter = this.waiters.shift()
    if (waiter === undefined) this.values.push(value)
    else waiter({ value, done: false })
  }

  close() {
    this.closed = true
    for (const waiter of this.waiters.splice(0)) waiter({ value: undefined, done: true })
  }

  async next() {
    const value = this.values.shift()
    if (value !== undefined) return { value, done: false }
    if (this.closed) return { value: undefined, done: true }
    return new Promise(resolve => { this.waiters.push(resolve) })
  }
}

class FixtureKernel {
  sessions = new Map()
  submissions = []
  resumes = []
  nextSession = 0
  shutdownCalls = 0

  async createSession(options) {
    return this.openSession(`kernel-${++this.nextSession}`, options)
  }

  async resumeSession(options) {
    this.resumes.push(options)
    return this.openSession(`kernel-${++this.nextSession}`, options)
  }

  async forkSession(options) {
    return this.openSession(`kernel-${++this.nextSession}`, options)
  }

  openSession(sessionId, options) {
    const rolloutPath = options.rolloutPath ?? `/runtime/${sessionId}.jsonl`
    this.sessions.set(sessionId, { queue: new EventQueue(), rolloutPath, options })
    return { sessionId, rolloutPath }
  }

  async submitTurn(sessionId, text) {
    const state = this.sessions.get(sessionId)
    assert.ok(state)
    const turnId = `turn-${this.submissions.length + 1}`
    this.submissions.push({ sessionId, text, options: state.options })
    const events = [
      rawEvent(1, 'task_started', { turn_id: turnId }),
      rawEvent(2, 'user_message', { message: text }),
      rawEvent(3, 'agent_message_content_delta', {
        turn_id: turnId,
        item_id: 'message-1',
        delta: 'embedded ',
      }),
      rawEvent(4, 'agent_message', { message: 'embedded answer' }),
      rawEvent(5, 'task_complete', {
        turn_id: turnId,
        last_agent_message: 'embedded answer',
        error: null,
      }),
    ]
    queueMicrotask(() => { for (const event of events) state.queue.push(event) })
    return { status: 'started', turnId }
  }

  async steer() {
    return { status: 'not_submitted', reason: 'fixture has no active steering window' }
  }

  async interrupt() {
    return 'interrupt-fixture'
  }

  async resolveApproval() {
    return 'approval-fixture'
  }

  async closeSession(sessionId) {
    this.sessions.get(sessionId)?.queue.close()
  }

  async shutdown() {
    this.shutdownCalls += 1
    for (const state of this.sessions.values()) state.queue.close()
    return { completed: [], submitFailed: [], timedOut: [] }
  }

  async *events(sessionId) {
    const state = this.sessions.get(sessionId)
    assert.ok(state)
    while (true) {
      const next = await state.queue.next()
      if (next.done) return
      yield next.value
    }
  }
}

async function mount(home, kernel, persistenceRoot) {
  const ctx = new Context()
  await ctx.plugin(LlmRuntime)
  await ctx.plugin(SessionStore)
  await ctx.plugin(SystemPrompt)
  await ctx.plugin(AgentRegistry)
  await ctx.plugin(ApprovalService, { policy: 'never' })
  if (persistenceRoot !== undefined) {
    await ctx.plugin(JsonlSessionPersistence, { root: persistenceRoot })
  }
  let factory
  const plugin = (pluginCtx) => {
    factory = new WinWinCodeAgentFactory(
      pluginCtx,
      { home, roleId: 'chat' },
      () => kernel,
    )
  }
  plugin.inject = ['agents', 'sessions', 'llm', 'systemPrompt', 'approval']
  await ctx.plugin(plugin)
  assert.ok(factory)
  return ctx
}

test('DSH AgentFactory runs a stock chat turn through one embedded kernel session', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-agent-factory-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const kernel = new FixtureKernel()
  const ctx = await mount(home, kernel)
  t.after(() => ctx.fiber.dispose())

  const handle = await ctx.agents.create({
    sessionId: SessionId('dsh-chat-1'),
    meta: { cwd: home },
    agentOptions: { provider: 'deepseek', model: 'deepseek-chat' },
  })
  handle.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'hello from DSH' }],
    source: { kind: 'user' },
  }))
  await handle.agent.whenIdle()

  assert.deepEqual(kernel.submissions.map(call => [call.sessionId, call.text]), [
    ['kernel-1', 'hello from DSH'],
  ])
  assert.notEqual(kernel.submissions[0].sessionId, handle.agent.id)
  const eventTypes = handle.agent.session.events.map(event => event.type)
  assert.deepEqual(eventTypes.filter(type => [
    'turn/start',
    'step/start',
    'user/message',
    'assistant/chunk',
    'assistant/message',
    'step/end',
    'turn/end',
  ].includes(type)), [
    'turn/start',
    'step/start',
    'user/message',
    'assistant/chunk',
    'assistant/message',
    'step/end',
    'turn/end',
  ])
  const header = handle.agent.session.requestHeader()
  assert.equal(header.config.provider, 'deepseek')
  assert.equal(header.config.model, 'deepseek-chat')
  const assistant = handle.agent.session.events.find(event => event.type === 'assistant/message')
  assert.equal(assistant.data.message.content[0].text, 'embedded answer')
  assert.equal(assistant.sourceEventSeqs.length, 1)

  const ledger = await RuntimeSessionLedger.open(home, 'dsh-chat-1')
  const snapshot = await ledger.read()
  assert.equal(snapshot.manifest.kernelSessionId, 'kernel-1')
  assert.equal(snapshot.manifest.roleId, 'chat')
  assert.equal(snapshot.events.at(-1).kind, 'turn.completed')
  assert.equal(snapshot.records.filter(record => record.recordType === 'runtime.event').length, 5)

  await handle.dispose()
  assert.equal(ctx.agents.get(SessionId('dsh-chat-1')), undefined)
  assert.equal(ctx.sessions.get(SessionId('dsh-chat-1')), undefined)
})

test('stock Chat and a StrongFlow role share one embedded kernel with distinct identities', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-agent-roles-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const kernel = new FixtureKernel()
  const ctx = await mount(home, kernel)
  t.after(() => ctx.fiber.dispose())

  const chat = await ctx.agents.create({
    sessionId: SessionId('dsh-shared-chat'),
    meta: { cwd: home },
    agentOptions: { provider: 'fixture', model: 'fixture-coder' },
  })
  const requirements = await ctx.agents.create({
    sessionId: SessionId('dsh-shared-requirements'),
    meta: { cwd: home, agentPreset: 'requirements' },
    agentOptions: { provider: 'fixture', model: 'fixture-coder' },
  })

  chat.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'chat turn' }],
    source: { kind: 'user' },
  }))
  requirements.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'requirements turn' }],
    source: { kind: 'user' },
  }))
  await Promise.all([chat.agent.whenIdle(), requirements.agent.whenIdle()])

  assert.equal(kernel.nextSession, 2)
  assert.deepEqual(kernel.submissions.map(call => call.text).sort(), [
    'chat turn',
    'requirements turn',
  ])
  const chatLedger = await RuntimeSessionLedger.open(home, 'dsh-shared-chat')
  const requirementsLedger = await RuntimeSessionLedger.open(
    home,
    'dsh-shared-requirements',
  )
  const [chatSnapshot, requirementsSnapshot] = await Promise.all([
    chatLedger.read(),
    requirementsLedger.read(),
  ])
  assert.equal(chatSnapshot.manifest.roleId, 'chat')
  assert.equal(requirementsSnapshot.manifest.roleId, 'requirements')
  assert.equal(kernel.sessions.get('kernel-1').options.rolePolicy, undefined)
  assert.deepEqual(
    kernel.sessions.get('kernel-2').options.rolePolicy,
    strongFlowRoleSessionPolicy('requirements'),
  )
  assert.match(
    kernel.sessions.get('kernel-2').options.rolePolicy.developerInstructions,
    /DeliverySpec/u,
  )
  assert.ok(chatSnapshot.events.every(event => event.source.roleId === 'chat'))
  assert.ok(requirementsSnapshot.events.every(event => (
    event.source.roleId === 'requirements'
  )))

  await Promise.all([chat.dispose(), requirements.dispose()])
})

test('the DSH model selector reopens Codex on the selected provider without a second loop', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-route-switch-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const kernel = new FixtureKernel()
  const ctx = await mount(home, kernel)
  t.after(() => ctx.fiber.dispose())

  const handle = await ctx.agents.create({
    sessionId: SessionId('dsh-route-switch'),
    meta: { cwd: home },
    agentOptions: { provider: 'deepseek', model: 'deepseek-chat' },
    setup(agentCtx) {
      installModelSelection(agentCtx, {
        current: { provider: 'anthropic', model: 'claude-sonnet-4-6' },
        assembled: undefined,
      })
    },
  })
  handle.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'use the selected route' }],
    source: { kind: 'user' },
  }))
  await handle.agent.whenIdle()

  assert.equal(kernel.resumes.length, 1)
  assert.equal(kernel.resumes[0].provider, 'anthropic')
  assert.equal(kernel.resumes[0].model, 'claude-sonnet-4-6')
  assert.deepEqual(kernel.submissions.map(call => call.sessionId), ['kernel-2'])
  assert.equal(handle.agent.session.requestHeader().config.provider, 'anthropic')
  assert.equal(handle.agent.session.requestHeader().config.model, 'claude-sonnet-4-6')
  const ledger = await RuntimeSessionLedger.open(home, 'dsh-route-switch')
  const snapshot = await ledger.read()
  assert.equal(snapshot.manifest.kernelSessionId, 'kernel-2')
  assert.equal(snapshot.records.filter(record => record.recordType === 'kernel.lifecycle').length, 2)

  await handle.dispose()
})

test('a persisted DSH session resumes from its sidecar and keeps normalized history monotonic', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-resume-home-'))
  const persistenceRoot = await mkdtemp(join(tmpdir(), 'winwincode-dsh-persistence-'))
  t.after(async () => {
    await rm(home, { recursive: true, force: true })
    await rm(persistenceRoot, { recursive: true, force: true })
  })

  const firstKernel = new FixtureKernel()
  const firstCtx = await mount(home, firstKernel, persistenceRoot)
  const first = await firstCtx.agents.create({
    sessionId: SessionId('dsh-resume-1'),
    meta: { cwd: home },
    agentOptions: { provider: 'deepseek', model: 'deepseek-chat' },
  })
  first.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'first turn' }],
    source: { kind: 'user' },
  }))
  await first.agent.whenIdle()
  assert.equal(await firstCtx.sessions.flush(first.agent.session), true)
  await first.dispose()
  await firstCtx.fiber.dispose()

  const secondKernel = new FixtureKernel()
  const secondCtx = await mount(home, secondKernel, persistenceRoot)
  t.after(() => secondCtx.fiber.dispose())
  const resumed = await secondCtx.agents.resume({
    resumeSessionId: SessionId('dsh-resume-1'),
    agentOptions: { provider: 'deepseek', model: 'deepseek-chat' },
  })
  resumed.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'second turn' }],
    source: { kind: 'user' },
  }))
  await resumed.agent.whenIdle()

  assert.equal(secondKernel.resumes.length, 1)
  assert.equal(secondKernel.resumes[0].rolloutPath, '/runtime/kernel-1.jsonl')
  assert.deepEqual(secondKernel.submissions.map(call => call.text), ['second turn'])
  const ledger = await RuntimeSessionLedger.open(home, 'dsh-resume-1')
  const snapshot = await ledger.read()
  assert.equal(snapshot.events.length, 10)
  assert.deepEqual(snapshot.events.map(event => event.cursor.sequence), [
    '1', '2', '3', '4', '5', '6', '7', '8', '9', '10',
  ])
  assert.deepEqual(
    resumed.agent.session.events
      .filter(event => event.type === 'user/message')
      .map(event => event.data.content[0].text),
    ['first turn', 'second turn'],
  )

  await resumed.dispose()
})
