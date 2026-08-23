import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { homedir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'

import { strongFlowRoleSessionPolicy } from '@winwincode/contracts'
import {
  WinWinCodeKernel,
  nativePackageName,
  resolveReleaseTarget,
} from '@winwincode/native'

function findTool(tools, name, namespace) {
  for (const tool of tools ?? []) {
    if (tool?.type === 'namespace' && Array.isArray(tool.tools)) {
      const nested = findTool(tool.tools, name, tool.name)
      if (nested !== undefined) return nested
    }
    if (tool?.name === name) return { name, ...(namespace === undefined ? {} : { namespace }) }
  }
  return undefined
}

function toolOutputs(request) {
  const input = Array.isArray(request.request?.input) ? request.request.input : []
  return input
    .filter(item => item?.type === 'function_call_output' || item?.type === 'custom_tool_call_output')
    .map(item => item.output)
}

function approvalFromEvent(sessionId, event) {
  const message = event.payload?.msg
  if (message?.type === 'exec_approval_request') {
    return {
      sessionId,
      kind: 'exec',
      operationId: message.approval_id ?? message.call_id ?? message.id,
      ...(typeof message.turn_id === 'string' ? { turnId: message.turn_id } : {}),
      decision: { kind: 'approved' },
    }
  }
  if (message?.type === 'apply_patch_approval_request') {
    return {
      sessionId,
      kind: 'patch',
      operationId: message.approval_id ?? message.call_id ?? message.id,
      ...(typeof message.turn_id === 'string' ? { turnId: message.turn_id } : {}),
      decision: { kind: 'approved' },
    }
  }
  return undefined
}

async function drain(kernel, sessionId, eventKinds, timeoutMillis = 20) {
  const events = []
  while (true) {
    const poll = await kernel.pollEvent(sessionId, timeoutMillis)
    if (poll.status !== 'event') return events
    events.push(poll.event)
    eventKinds.push(poll.event.kind)
  }
}

async function runTurn(kernel, sessionId, text, eventKinds, errors) {
  await kernel.submitTurn(sessionId, text)
  const events = []
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    const poll = await kernel.pollEvent(sessionId, 1_000)
    if (poll.status !== 'event') continue
    events.push(poll.event)
    eventKinds.push(poll.event.kind)
    const approval = approvalFromEvent(sessionId, poll.event)
    if (approval !== undefined) await kernel.resolveApproval(approval)
    if (poll.event.kind === 'error') {
      errors.push(poll.event.payload?.msg?.message ?? 'kernel error')
    }
    if (poll.event.kind === 'turn_complete') return events
  }
  throw new Error(`installed native session ${sessionId} did not complete`)
}

function configuredMessage(events) {
  return events.find(event => event.payload?.msg?.type === 'session_configured')?.payload?.msg
}

function isReadOnlyProfile(message) {
  const profile = message?.permission_profile
  const entries = profile?.file_system?.entries
  return message?.approval_policy === 'on-request'
    && message?.approvals_reviewer === 'user'
    && profile?.type === 'managed'
    && profile?.network === 'restricted'
    && profile?.file_system?.type === 'restricted'
    && Array.isArray(entries)
    && entries.some(entry => entry?.access === 'read')
    && entries.every(entry => entry?.access !== 'write')
}

function isWorkspaceWriteProfile(message) {
  const profile = message?.permission_profile
  const entries = profile?.file_system?.entries
  return message?.approval_policy === 'on-request'
    && message?.approvals_reviewer === 'user'
    && profile?.type === 'managed'
    && profile?.network === 'restricted'
    && profile?.file_system?.type === 'restricted'
    && Array.isArray(entries)
    && entries.some(entry => entry?.access === 'write')
}

const root = resolve(import.meta.dirname, '..')
const home = join(root, 'home')
const workspace = join(root, 'workspace')
const blockedPath = join(
  homedir(),
  `.winwincode-sandbox-escape-${process.pid}-${basename(root)}`,
)
const allowedPath = join(workspace, 'sandbox-smoke.txt')
mkdirSync(home, { recursive: true })
mkdirSync(workspace, { recursive: true })
rmSync(blockedPath, { force: true })
writeFileSync(
  join(home, 'config.toml'),
  'approval_policy = "never"\ndefault_permissions = ":workspace"\n',
)

const requests = []
let toolResultSeen = false
let receivedToolOutputs = []
const marker = 'installed-native-sandbox-smoke'
const modelPort = {
  async *stream(request) {
    requests.push(request)
    yield { type: 'created' }
    yield { type: 'server_model', model: request.request.model }
    if (requests.length === 1) {
      const tool = findTool(request.request.tools, 'exec_command')
      if (tool === undefined) throw new Error('installed package did not expose exec_command')
      const item = {
        type: 'function_call',
        id: 'installed-smoke-function-call',
        name: tool.name,
        ...(tool.namespace === undefined ? {} : { namespace: tool.namespace }),
        arguments: JSON.stringify({
          cmd: [
            'set -eu',
            `printf ${marker} > sandbox-smoke.txt`,
            `if printf blocked > ${JSON.stringify(blockedPath)} 2>/dev/null; then exit 97; fi`,
            'cat sandbox-smoke.txt',
          ].join('; '),
          workdir: workspace,
          yield_time_ms: 10_000,
        }),
        call_id: 'installed-smoke-call',
      }
      yield { type: 'output_item_added', item: { ...item, arguments: '' } }
      yield { type: 'output_item_done', item }
      yield { type: 'completed', responseId: request.requestId, endTurn: false }
      return
    }

    const outputs = toolOutputs(request)
    if (requests.length === 2) {
      receivedToolOutputs = outputs
      toolResultSeen = outputs.some(output => JSON.stringify(output).includes(marker))
    }
    const item = {
      type: 'message',
      id: `installed-smoke-message-${requests.length}`,
      role: 'assistant',
      content: [{ type: 'output_text', text: 'installed native smoke complete' }],
      phase: 'final_answer',
    }
    yield { type: 'output_item_added', item: { ...item, content: [] } }
    yield { type: 'output_text_delta', delta: 'installed native smoke complete' }
    yield { type: 'output_item_done', item }
    yield { type: 'completed', responseId: request.requestId, endTurn: true }
  },
}

const target = resolveReleaseTarget()
const packageName = nativePackageName(target)
const require = createRequire(import.meta.url)
const packageBuildInfoPath = require.resolve(`${packageName}/build-info.json`)
const nativePrebuildRoot = dirname(packageBuildInfoPath)
const packageBuildInfo = JSON.parse(readFileSync(packageBuildInfoPath, 'utf8'))
const kernel = new WinWinCodeKernel({ home, modelPort })
const eventKinds = []
const errors = []

try {
  const session = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
  })
  await drain(kernel, session.sessionId, eventKinds)
  await runTurn(
    kernel,
    session.sessionId,
    'Run the installed native sandbox smoke.',
    eventKinds,
    errors,
  )
  await kernel.closeSession(session.sessionId)

  const readOnlySession = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
    rolePolicy: strongFlowRoleSessionPolicy('requirements'),
  })
  const readOnlyEvents = await drain(kernel, readOnlySession.sessionId, eventKinds)
  readOnlyEvents.push(...await runTurn(
    kernel,
    readOnlySession.sessionId,
    'Summarize the delivery requirements.',
    eventKinds,
    errors,
  ))
  const readOnlyConfigured = configuredMessage(readOnlyEvents)
  const roleRequestText = JSON.stringify(requests.at(-1)?.request)
  await kernel.closeSession(readOnlySession.sessionId)

  const writerSession = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
    rolePolicy: strongFlowRoleSessionPolicy('executor'),
  })
  const writerEvents = await drain(kernel, writerSession.sessionId, eventKinds)
  const writerConfigured = configuredMessage(writerEvents)
  await kernel.closeSession(writerSession.sessionId)

  const report = {
    target,
    packageName,
    packageBuildInfo,
    kernelBuildInfo: kernel.buildInfo,
    requests: requests.length,
    toolResultSeen,
    receivedToolOutputs,
    workspaceWriteSucceeded: existsSync(allowedPath)
      && readFileSync(allowedPath, 'utf8') === marker,
    parentWriteBlocked: !existsSync(blockedPath),
    sandboxHelperBundled: process.platform !== 'linux'
      || existsSync(require.resolve(`${packageName}/codex-linux-sandbox`)),
    bubblewrapBundled: process.platform !== 'linux'
      || (
        existsSync(join(nativePrebuildRoot, 'codex-resources', 'bwrap'))
        && (statSync(join(nativePrebuildRoot, 'codex-resources', 'bwrap')).mode & 0o111) !== 0
      ),
    rolePolicy: {
      readOnlyObserved: isReadOnlyProfile(readOnlyConfigured),
      workspaceWriteObserved: isWorkspaceWriteProfile(writerConfigured),
      codexCapabilitiesPresent: ['exec_command', 'update_plan', 'spawn_agent'].every(
        name => roleRequestText.includes(name),
      ),
    },
    eventKinds,
    errors,
  }
  process.stdout.write(`${JSON.stringify(report)}\n`)
} finally {
  await kernel.shutdown()
  rmSync(blockedPath, { force: true })
}
