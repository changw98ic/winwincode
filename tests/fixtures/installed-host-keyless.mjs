import { mkdirSync } from 'node:fs'
import { realpathSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

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
} from './dist/index.js'
import { WinWinCodeKernel } from '@winwincode/native'
import { strongFlowSurface } from '@winwincode/strongflow'

import { WinWinCodeAgentFactoryFixture } from './dist/agent-factory-test-support.js'
function requiredEnvironment(name) {
  const value = process.env[name]
  if (value === undefined || value.length === 0) throw new Error(`${name} is required`)
  return value
}

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
      ? 'installed requirements role complete'
      : 'installed stock chat complete'
    this.calls.push({ provider: options.provider, model: options.model, prompt, answer })
    yield { type: 'block-start', index: 0, blockType: 'text' }
    yield { type: 'text-delta', index: 0, text: answer }
    yield { type: 'block-end', index: 0, block: { type: 'text', text: answer } }
    yield { type: 'usage', usage: { inputTokens: 8, outputTokens: 4 } }
    yield { type: 'finish', reason: { kind: 'stop' } }
  }
}

function assistantMessages(agent) {
  return agent.session.events.flatMap(event => (
    event.type === 'assistant/message'
      ? event.data.message.content.flatMap(block => block.type === 'text' ? [block.text] : [])
      : []
  ))
}

const home = requiredEnvironment('WINWINCODE_SMOKE_HOME')
const workspace = requiredEnvironment('WINWINCODE_SMOKE_WORKSPACE')
mkdirSync(workspace, { recursive: true })
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
  const adapterPlugin = pluginCtx => pluginCtx.llm.registerAdapter(['fixture'], adapter)
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
    sessionId: SessionId('installed-stock-chat-smoke'),
    meta: { cwd: workspace },
    agentOptions: { provider: 'fixture', model: 'fixture-coder' },
  })
  chat.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'complete the installed stock chat smoke' }],
    source: { kind: 'user' },
  }))
  await chat.agent.whenIdle()

  requirements = await ctx.agents.create({
    sessionId: SessionId('installed-requirements-smoke'),
    meta: { cwd: workspace, agentPreset: 'requirements' },
    agentOptions: { provider: 'fixture', model: 'fixture-coder' },
  })
  requirements.agent.followup(createUserMessage({
    content: [{ type: 'text', text: 'complete the installed requirements smoke' }],
    source: { kind: 'user' },
  }))
  await requirements.agent.whenIdle()

  const ledgers = await Promise.all([
    RuntimeSessionLedger.open(home, chat.agent.id).then(ledger => ledger.read()),
    RuntimeSessionLedger.open(home, requirements.agent.id).then(ledger => ledger.read()),
  ])
  const report = {
    surfaces: [chatSurface, strongFlowSurface],
    kernelCreations,
    nativeTarget: kernel.target,
    kernelSessionIds: ledgers.map(snapshot => snapshot.manifest.kernelSessionId),
    roles: ledgers.map(snapshot => snapshot.manifest.roleId),
    calls: adapter.calls,
    assistantMessages: [
      ...assistantMessages(chat.agent),
      ...assistantMessages(requirements.agent),
    ],
    eventKinds: ledgers.map(snapshot => snapshot.events.map(event => event.kind)),
    modulePaths: [
      realpathSync(fileURLToPath(import.meta.resolve('./dist/index.js'))),
      realpathSync(fileURLToPath(import.meta.resolve('@winwincode/native'))),
      realpathSync(fileURLToPath(import.meta.resolve('@winwincode/strongflow'))),
    ],
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
  report.shutdown = await kernel.shutdown()
  process.stdout.write(`${JSON.stringify(report)}\n`)
} finally {
  await chat?.dispose().catch(() => undefined)
  await requirements?.dispose().catch(() => undefined)
  await ctx.fiber.dispose().catch(() => undefined)
  await kernel?.shutdown().catch(() => undefined)
}
