import { mkdtempSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  CodexRuntimeProjector,
  DshModelPort,
  DshRuntimeProjection,
  RuntimeApprovalRouter,
} from '../../packages/dsh-profile/dist/index.js'
import { WinWinCodeKernel } from '../../packages/native/dist/index.js'

const root = mkdtempSync(join(tmpdir(), 'winwincode-native-model-'))
const home = join(root, 'home')
const cwd = join(root, 'workspace')
mkdirSync(cwd)

const calls = []
const credential = 'TOKEN-native-model-fixture-secret'
let callSequence = 0
const llm = {
  credential,
  async prepareCall(config, signal) {
    const preparedSequence = callSequence
    callSequence += 1
    return {
      config: Object.freeze({ ...config }),
      stream(options) {
        calls.push({
          provider: options.provider,
          model: options.model,
          reasoningEffort: options.reasoningEffort,
          toolNames: options.tools?.map(tool => tool.name) ?? [],
          hasToolResult: options.messages.some(message => message.content.some(
            block => block.type === 'tool-result',
          )),
          signalMatches: options.signal === signal,
        })
        return preparedSequence === 0 ? toolCall(options) : finalAnswer()
      },
    }
  },
}

async function* toolCall(options) {
  const tool = options.tools?.find(candidate => candidate.name === 'exec_command')
  if (tool === undefined) {
    yield {
      type: 'finish',
      reason: {
        kind: 'error',
        failure: {
          code: 'FIXTURE_TOOL_MISSING',
          message: 'Codex did not expose exec_command through the DSH model request',
        },
      },
    }
    return
  }
  const argumentsJson = JSON.stringify({
    cmd: 'printf native-model-port-tool-output',
    sandbox_permissions: 'require_escalated',
    justification: 'Exercise the WinWinCode approval callback fixture.',
  })
  yield { type: 'block-start', index: 0, blockType: 'tool-call' }
  yield {
    type: 'tool-call-delta',
    index: 0,
    id: 'fixture-tool-call',
    name: tool.name,
    argumentsDelta: argumentsJson,
  }
  yield {
    type: 'block-end',
    index: 0,
    block: {
      type: 'tool-call',
      id: 'fixture-tool-call',
      name: tool.name,
      arguments: argumentsJson,
    },
  }
  yield {
    type: 'usage',
    usage: { inputTokens: 25, outputTokens: 8 },
  }
  yield { type: 'finish', reason: { kind: 'tool-calls' } }
}

async function* finalAnswer() {
  yield { type: 'block-start', index: 0, blockType: 'reasoning' }
  yield { type: 'reasoning-delta', index: 0, text: 'The fixture command completed.' }
  yield { type: 'block-start', index: 1, blockType: 'text' }
  yield { type: 'text-delta', index: 1, text: 'model port ' }
  yield { type: 'text-delta', index: 1, text: 'complete' }
  yield {
    type: 'block-end',
    index: 0,
    block: { type: 'reasoning', text: 'The fixture command completed.' },
  }
  yield {
    type: 'block-end',
    index: 1,
    block: { type: 'text', text: 'model port complete' },
  }
  yield {
    type: 'usage',
    usage: { inputTokens: 40, outputTokens: 12, reasoningTokens: 4 },
  }
  yield { type: 'finish', reason: { kind: 'stop' } }
}

const kernel = new WinWinCodeKernel({
  home,
  modelPort: new DshModelPort(llm),
})

try {
  const session = await kernel.createSession({
    cwd,
    provider: 'deepseek-compatible',
    model: 'fixture-coder',
  })
  const rawEvents = []
  while (true) {
    const startup = await kernel.pollEvent(session.sessionId, 10)
    if (startup.status !== 'event') break
    rawEvents.push(startup.event)
  }
  const normalizer = new CodexRuntimeProjector({
    sessionId: session.sessionId,
    roleId: 'chat',
    kernelStreamId: 'native-stream-1',
  })
  const projection = new DshRuntimeProjection({
    sessionId: session.sessionId,
    roleId: 'chat',
  })
  const approvalRouter = new RuntimeApprovalRouter(kernel, projection)
  const approvalResponses = []
  const normalized = []
  const dshSessionAppends = []
  for (const raw of rawEvents) {
    const event = normalizer.ingest(raw)
    if (event === undefined) continue
    normalized.push(event)
    dshSessionAppends.push(...projection.apply(event).sessionAppends)
  }
  const submission = await kernel.submitTurn(
    session.sessionId,
    'Run the fixture command and report when it completes.',
  )
  const events = []
  const deadline = Date.now() + 15_000
  while (Date.now() < deadline) {
    const poll = await kernel.pollEvent(session.sessionId, 1_000)
    if (poll.status !== 'event') continue
    rawEvents.push(poll.event)
    const normalizedEvent = normalizer.ingest(poll.event)
    if (normalizedEvent !== undefined) {
      normalized.push(normalizedEvent)
      dshSessionAppends.push(...projection.apply(normalizedEvent).sessionAppends)
      if (normalizedEvent.kind === 'approval.requested') {
        const approvalId = normalizedEvent.source.approvalId
        if (approvalId === undefined) throw new Error('approval request lacks its source identity')
        approvalResponses.push({
          approvalId,
          submissionId: await approvalRouter.resolve({
            approvalId,
            decision: { kind: 'approved' },
          }),
        })
      }
    }
    events.push({
      kind: poll.event.kind,
      payloadType: poll.event.payload?.msg?.type,
      payload: poll.event.payload,
    })
    if (poll.event.kind === 'turn_complete') break
  }
  await kernel.closeSession(session.sessionId)
  const persisted = session.rolloutPath === undefined
    ? ''
    : readFileSync(session.rolloutPath, 'utf8')
  const replayNormalizer = new CodexRuntimeProjector({
    sessionId: session.sessionId,
    roleId: 'chat',
    kernelStreamId: 'native-stream-1',
  })
  const replayEvents = replayNormalizer.replay(rawEvents)
  const replayProjection = new DshRuntimeProjection({
    sessionId: session.sessionId,
    roleId: 'chat',
  })
  const replayAppends = replayProjection.replay(replayEvents)
  const report = {
    submission,
    calls,
    eventKinds: events.map(event => event.kind),
    payloadTypes: events.map(event => event.payloadType),
    agentMessages: events
      .filter(event => event.kind === 'agent_message')
      .map(event => event.payload?.msg?.message),
    errors: events
      .filter(event => event.kind === 'error')
      .map(event => event.payload?.msg?.message),
    normalizedKinds: normalized.map(event => event.kind),
    dshSessionAppendTypes: dshSessionAppends.map(event => event.type),
    approvalResponses,
    projectionReplayMatches:
      JSON.stringify(projection.snapshot) === JSON.stringify(replayProjection.snapshot),
    sessionReplayMatches: JSON.stringify(dshSessionAppends) === JSON.stringify(replayAppends),
    sourceIdentitiesComplete: normalized.every(event => (
      event.source.sessionId === session.sessionId
      && event.source.roleId === 'chat'
      && event.source.kernelStreamId === 'native-stream-1'
      && /^\d+$/u.test(event.source.kernelSequence)
      && event.source.submissionId.length > 0
    )),
    credentialPresent: `${JSON.stringify(events)}\n${JSON.stringify(normalized)}\n${persisted}`
      .includes(credential),
  }
  process.stdout.write(`${JSON.stringify(report)}\n`)
  await kernel.shutdown()
} finally {
  await kernel.shutdown()
  rmSync(root, { force: true, recursive: true })
}
