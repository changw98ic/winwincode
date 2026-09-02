import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import { createUserMessage } from '@deepseek-ai/dsh-llm'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ApprovalService from '@deepseek-ai/dsh-user-approval'

import {
  RuntimeSessionLedger,
} from '../packages/dsh-profile/dist/index.js'
import {
  WinWinCodeAgentFactoryFixture,
} from '../packages/dsh-profile/dist/agent-factory-test-support.js'
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
    for (const waiter of this.waiters.splice(0)) {
      waiter({ value: undefined, done: true })
    }
  }

  async next() {
    const value = this.values.shift()
    if (value !== undefined) return { value, done: false }
    if (this.closed) return { value: undefined, done: true }
    return new Promise(resolve => { this.waiters.push(resolve) })
  }
}

class NativePortFixtureKernel {
  sessions = new Map()
  submissions = []
  interrupts = []
  nextSession = 0
  submitted = Promise.withResolvers()

  constructor(mode, failure = undefined) {
    this.mode = mode
    this.failure = failure
  }

  async createSession(options) {
    const sessionId = `native-session-${++this.nextSession}`
    this.sessions.set(sessionId, { options, queue: new EventQueue() })
    return { sessionId, rolloutPath: `/native/${sessionId}.jsonl` }
  }

  async resumeSession(options) {
    return this.createSession(options)
  }

  async forkSession(options) {
    return this.createSession(options)
  }

  async submitTurn(sessionId, text) {
    const state = this.sessions.get(sessionId)
    assert.ok(state)
    const turnId = `native-turn-${this.submissions.length + 1}`
    this.submissions.push({ sessionId, text })
    this.submitted.resolve()
    if (this.mode === 'submit-error') throw this.failure
    const events = [rawEvent(1, 'task_started', { turn_id: turnId })]
    if (this.mode === 'complete') {
      events.push(
        rawEvent(2, 'user_message', { message: text }),
        rawEvent(3, 'agent_message_content_delta', {
          turn_id: turnId,
          item_id: 'message-1',
          delta: 'native ',
        }),
        rawEvent(4, 'agent_message', { message: 'native answer' }),
        rawEvent(5, 'task_complete', {
          turn_id: turnId,
          last_agent_message: 'native answer',
          error: null,
        }),
      )
    }
    queueMicrotask(() => {
      for (const event of events) state.queue.push(event)
    })
    return { status: 'started', turnId }
  }

  async steer() {
    return { status: 'not_submitted', reason: 'fixture has no steering window' }
  }

  async interrupt(sessionId) {
    const state = this.sessions.get(sessionId)
    assert.ok(state)
    this.interrupts.push(sessionId)
    state.queue.push(rawEvent(2, 'turn_aborted', { turn_id: 'native-turn-1' }))
    if (this.failure !== undefined) throw this.failure
    return 'native-interrupt-receipt'
  }

  async resolveApproval() {
    return 'native-approval-receipt'
  }

  async listSessions() {
    return [...this.sessions.keys()]
  }

  async closeSession(sessionId) {
    this.sessions.get(sessionId)?.queue.close()
  }

  async shutdown() {
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

async function mount(t, kernel) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-rust-model-port-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const ctx = new Context()
  const llmAccess = { count: 0 }
  const forbiddenLlm = new Proxy(Object.create(null), {
    get() {
      llmAccess.count += 1
      throw new Error('production AgentFactory accessed ctx.llm')
    },
  })
  Object.defineProperty(ctx, 'llm', {
    configurable: true,
    get() {
      llmAccess.count += 1
      return forbiddenLlm
    },
  })

  await ctx.plugin(SessionStore)
  await ctx.plugin(SystemPrompt)
  await ctx.plugin(AgentRegistry)
  await ctx.plugin(ApprovalService, { policy: 'never' })
  const errors = []
  ctx.on('agent/error', ({ error }) => { errors.push(error) })
  let kernelOptions
  const plugin = (pluginCtx) => {
    new WinWinCodeAgentFactoryFixture(
      pluginCtx,
      { home, roleId: 'chat' },
      options => {
        kernelOptions = structuredClone(options)
        return kernel
      },
    )
  }
  plugin.inject = ['agents', 'sessions', 'systemPrompt', 'approval']
  await ctx.plugin(plugin)
  t.after(() => ctx.fiber.dispose())
  return { ctx, errors, home, kernelOptions, llmAccess }
}

async function createAgent(ctx, home, sessionId) {
  return ctx.agents.create({
    sessionId: SessionId(sessionId),
    meta: { cwd: home },
    agentOptions: { provider: 'canonical', model: 'rust-provider' },
  })
}

function userMessage(text) {
  return createUserMessage({
    content: [{ type: 'text', text }],
    source: { kind: 'user' },
  })
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (predicate()) return
    await new Promise(resolve => { setTimeout(resolve, 1) })
  }
  assert.fail('timed out waiting for the native kernel event boundary')
}

test('production AgentFactory sends turns only through native kernel options', async t => {
  const kernel = new NativePortFixtureKernel('complete')
  const mounted = await mount(t, kernel)
  const handle = await createAgent(mounted.ctx, mounted.home, 'native-only-turn')

  handle.agent.followup(userMessage('use the canonical Rust model port'))
  await handle.agent.whenIdle()

  assert.deepEqual(mounted.kernelOptions, { home: mounted.home })
  assert.equal(Object.hasOwn(mounted.kernelOptions, 'modelPort'), false)
  assert.equal(mounted.llmAccess.count, 0)
  assert.deepEqual(kernel.submissions, [{
    sessionId: 'native-session-1',
    text: 'use the canonical Rust model port',
  }])
  assert.deepEqual(mounted.errors, [])
  await handle.dispose()
})

test('native kernel cancellation is exact and never falls back to ctx.llm', async t => {
  const interruptFailure = new Error('native provider cancellation receipt failed')
  const kernel = new NativePortFixtureKernel('pending', interruptFailure)
  const mounted = await mount(t, kernel)
  const handle = await createAgent(mounted.ctx, mounted.home, 'native-cancel-turn')

  handle.agent.followup(userMessage('cancel the canonical Rust request'))
  await kernel.submitted.promise
  await waitFor(() => handle.agent.session.events.some(event => event.type === 'turn/start'))
  handle.agent.cancel({ kind: 'user' })
  await handle.agent.whenIdle()
  await waitFor(() => handle.agent.session.events.some(event => event.type === 'turn/end'))

  assert.deepEqual(kernel.interrupts, ['native-session-1'])
  assert.equal(mounted.errors.includes(interruptFailure), true)
  assert.equal(mounted.llmAccess.count, 0)
  const snapshot = await RuntimeSessionLedger.open(
    mounted.home,
    'native-cancel-turn',
  ).then(ledger => ledger.read())
  assert.equal(snapshot.events.at(-1).kind, 'turn.aborted')
  await handle.dispose()
})

test('native kernel errors retain their identity without consulting ctx.llm', async t => {
  const nativeFailure = new Error('canonical Rust Provider Gateway unavailable')
  const kernel = new NativePortFixtureKernel('submit-error', nativeFailure)
  const mounted = await mount(t, kernel)
  const handle = await createAgent(mounted.ctx, mounted.home, 'native-error-turn')

  handle.agent.followup(userMessage('surface the canonical provider error'))
  await handle.agent.whenIdle()

  assert.equal(mounted.errors.length, 1)
  assert.equal(mounted.errors[0], nativeFailure)
  assert.equal(mounted.llmAccess.count, 0)
  assert.deepEqual(kernel.interrupts, [])
  await handle.dispose()
})
