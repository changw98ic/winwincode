import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { homedir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'

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
// GitHub's Linux runners deny loopback setup inside a new network namespace.
// This smoke verifies the filesystem boundary, so it keeps network access unchanged.
writeFileSync(
  join(home, 'config.toml'),
  process.platform === 'linux'
    ? `approval_policy = "never"
default_permissions = "workspace-smoke"

[permissions.workspace-smoke]
extends = ":workspace"

[permissions.workspace-smoke.network]
enabled = true
mode = "full"
`
    : 'approval_policy = "never"\ndefault_permissions = ":workspace"\n',
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
      yield {
        type: 'completed',
        responseId: request.requestId,
        endTurn: false,
      }
      return
    }

    const outputs = toolOutputs(request)
    receivedToolOutputs = outputs
    toolResultSeen = outputs.some(output => JSON.stringify(output).includes(marker))
    const item = {
      type: 'message',
      id: 'installed-smoke-message',
      role: 'assistant',
      content: [{ type: 'output_text', text: 'installed native smoke complete' }],
      phase: 'final_answer',
    }
    yield { type: 'output_item_added', item: { ...item, content: [] } }
    yield { type: 'output_text_delta', delta: 'installed native smoke complete' }
    yield { type: 'output_item_done', item }
    yield {
      type: 'completed',
      responseId: request.requestId,
      endTurn: true,
    }
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
const diagnosticEvents = []
const errors = []

try {
  const session = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
  })
  while (true) {
    const startup = await kernel.pollEvent(session.sessionId, 20)
    if (startup.status !== 'event') break
    eventKinds.push(startup.event.kind)
  }
  await kernel.submitTurn(session.sessionId, 'Run the installed native sandbox smoke.')
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    const poll = await kernel.pollEvent(session.sessionId, 1_000)
    if (poll.status !== 'event') continue
    eventKinds.push(poll.event.kind)
    if (['warning', 'exec_command_begin', 'exec_command_end'].includes(poll.event.kind)) {
      diagnosticEvents.push({ kind: poll.event.kind, payload: poll.event.payload })
    }
    const approval = approvalFromEvent(session.sessionId, poll.event)
    if (approval !== undefined) await kernel.resolveApproval(approval)
    if (poll.event.kind === 'error') errors.push(poll.event.payload?.msg?.message ?? 'kernel error')
    if (poll.event.kind === 'turn_complete') break
  }
  await kernel.closeSession(session.sessionId)
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
    eventKinds,
    diagnosticEvents,
    errors,
  }
  process.stdout.write(`${JSON.stringify(report)}\n`)
} finally {
  await kernel.shutdown()
  rmSync(blockedPath, { force: true })
}
