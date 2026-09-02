import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import LlmRuntime, {
  LlmAdapter,
  createUserMessage,
} from '@deepseek-ai/dsh-llm'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ApprovalService from '@deepseek-ai/dsh-user-approval'

import {
  RuntimeSessionLedger,
  chatSurface,
} from './dsh-profile/index.mjs'
import { WinWinCodeAgentFactoryFixture } from './dsh-profile/agent-factory-test-support.mjs'
import { WinWinCodeKernel } from '../../packages/native/dist/index.js'
import { strongFlowSurface } from '../../packages/strongflow/dist/index.js'

function textBlocks(messages) {
  return messages.flatMap(message => message.content.flatMap(block => (
    block.type === 'text' ? [block.text] : []
  )))
}

class KeylessFixtureAdapter extends LlmAdapter {
  calls = []

  async *stream(options) {
    const prompt = textBlocks(options.messages).at(-1) ?? ''
    const answer = prompt.includes('requirements')
      ? 'requirements role complete'
      : 'stock chat complete'
    this.calls.push({
      provider: options.provider,
      model: options.model,
      prompt,
      answer,
    })
    yield { type: 'block-start', index: 0, blockType: 'text' }
    yield { type: 'text-delta', index: 0, text: answer }
    yield {
      type: 'block-end',
      index: 0,
      block: { type: 'text', text: answer },
    }
    yield {
      type: 'usage',
      usage: { inputTokens: 8, outputTokens: 4 },
    }
    yield { type: 'finish', reason: { kind: 'stop' } }
  }
}

function assistantMessages(agent) {
  return agent.session.events.flatMap(event => {
    if (event.type !== 'assistant/message') return []
    return event.data.message.content.flatMap(block => (
      block.type === 'text' ? [block.text] : []
    ))
  })
}

const root = mkdtempSync(join(tmpdir(), 'winwincode-product-smoke-'))
const home = join(root, 'home')
const workspace = join(root, 'workspace')
mkdirSync(workspace)

const ctx = new Context()
const adapter = new KeylessFixtureAdapter()
let kernel
let kernelCreations = 0
let chat
let requirements

try {
  await ctx.plugin(LlmRuntime)
  await ctx.plugin(SessionStore)
  await ctx.plugin(SystemPrompt)
  await ctx.plugin(AgentRegistry)
  await ctx.plugin(ApprovalService, { policy: 'never' })

  const adapterPlugin = pluginCtx => {
    pluginCtx.llm.registerAdapter(['fixture'], adapter)
  }
  adapterPlugin.inject = ['llm']
  await ctx.plugin(adapterPlugin)

  const factoryPlugin = pluginCtx => {
    new WinWinCodeAgentFactoryFixture(
      pluginCtx,
      { home, roleId: 'chat' },
      options => {
        kernelCreations += 1
        kernel = new WinWinCodeKernel(options)
        return kernel
      },
    )
  }
  factoryPlugin.inject = ['agents', 'sessions', 'llm', 'systemPrompt', 'approval']
  await ctx.plugin(factoryPlugin)

  chat = await ctx.agents.create({
    sessionId: SessionId('stock-chat-smoke'),
    meta: { cwd: workspace },
    agentOptions: { provider: 'fixture', model: 'fixture-coder' },
  })
  chat.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'complete the stock chat smoke' }],
    source: { kind: 'user' },
  }))
  await chat.agent.whenIdle()

  requirements = await ctx.agents.create({
    sessionId: SessionId('requirements-role-smoke'),
    meta: { cwd: workspace, agentPreset: 'requirements' },
    agentOptions: { provider: 'fixture', model: 'fixture-coder' },
  })
  requirements.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'complete the requirements role smoke' }],
    source: { kind: 'user' },
  }))
  await requirements.agent.whenIdle()

  const [chatSnapshot, requirementsSnapshot] = await Promise.all([
    RuntimeSessionLedger.open(home, chat.agent.id).then(ledger => ledger.read()),
    RuntimeSessionLedger.open(home, requirements.agent.id).then(ledger => ledger.read()),
  ])
  const kernelSessionIds = [
    chatSnapshot.manifest.kernelSessionId,
    requirementsSnapshot.manifest.kernelSessionId,
  ]
  const report = {
    surfaces: [chatSurface, strongFlowSurface],
    kernelCreations,
    kernelSessionIds,
    roles: [
      chatSnapshot.manifest.roleId,
      requirementsSnapshot.manifest.roleId,
    ],
    calls: adapter.calls,
    assistantMessages: [
      ...assistantMessages(chat.agent),
      ...assistantMessages(requirements.agent),
    ],
    runtimeEvents: {
      chat: chatSnapshot.events.map(event => event.kind),
      requirements: requirementsSnapshot.events.map(event => event.kind),
    },
    roleSourcesMatch: chatSnapshot.events.every(event => event.source.roleId === 'chat')
      && requirementsSnapshot.events.every(event => (
        event.source.roleId === 'requirements'
      )),
    credentialEnvironment: Object.keys(process.env).filter(name => (
      /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/u.test(name)
      && process.env[name] !== undefined
      && process.env[name] !== ''
    )),
  }

  await Promise.all([chat.dispose(), requirements.dispose()])
  chat = undefined
  requirements = undefined
  await ctx.fiber.dispose()
  const shutdown = await kernel.shutdown()
  report.shutdown = shutdown
  process.stdout.write(`${JSON.stringify(report)}\n`)
} finally {
  await chat?.dispose().catch(() => undefined)
  await requirements?.dispose().catch(() => undefined)
  await ctx.fiber.dispose().catch(() => undefined)
  await kernel?.shutdown().catch(() => undefined)
  rmSync(root, { force: true, recursive: true })
}
