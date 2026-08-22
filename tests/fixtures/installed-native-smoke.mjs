import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { createServer } from 'node:net'
import { homedir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'

import {
  GOVERNED_COMMAND_SCHEMA_VERSION,
  GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
  KernelError,
  WinWinCodeKernel,
  nativePackageName,
  resolveReleaseTarget,
} from '@winwincode/native'

function governedAuthority(workspaceRoot, overrides = {}) {
  return Object.freeze({
    schemaVersion: GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
    roleId: 'executor',
    permissionPreset: 'candidate-write',
    workspaceMode: 'candidate-write',
    workspaceRoot,
    systemInstructions: 'Execute only the installed-package governed smoke grant.',
    reasoningEffort: 'medium',
    visibleTools: Object.freeze([
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
      'candidate.diff',
      'command.run',
      'test.run',
      'candidate.patch',
    ]),
    ...overrides,
  })
}

function governedCommand(sessionId, commandId, argv, cwd, overrides = {}) {
  return Object.freeze({
    schemaVersion: GOVERNED_COMMAND_SCHEMA_VERSION,
    sessionId,
    commandId,
    tool: 'command.run',
    argv: Object.freeze(argv),
    cwd,
    environment: Object.freeze({ LANG: 'C.UTF-8' }),
    timeoutMillis: 10_000,
    outputLimitBytes: 1_048_576,
    ...overrides,
  })
}

async function listen(server) {
  await new Promise((resolvePromise, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolvePromise)
  })
  const address = server.address()
  if (address === null || typeof address !== 'object') throw new Error('network fixture has no port')
  return address.port
}

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
const governedSecret = 'TOKEN-installed-governed-secret'
const governedCredentialPath = join(workspace, '.env')
const governedInsidePath = join(workspace, 'governed-inside.txt')
const governedOutsidePath = join(root, 'governed-outside.txt')
const governedRemotePath = join(workspace, 'governed-remote.txt')
writeFileSync(governedCredentialPath, `${governedSecret}\n`)
process.env.WINWINCODE_INSTALLED_UNRELATED_SECRET = governedSecret
let networkConnections = 0
const networkServer = createServer(socket => {
  networkConnections += 1
  socket.end()
})
const networkPort = await listen(networkServer)

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

  const governedSession = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
    governedAuthority: governedAuthority(workspace),
  })
  const governedEnvironment = await kernel.executeGovernedCommand(governedCommand(
    governedSession.sessionId,
    'installed-environment',
    ['/usr/bin/env'],
    workspace,
  ))
  const governedWrite = await kernel.executeGovernedCommand(governedCommand(
    governedSession.sessionId,
    'installed-write',
    ['/bin/bash', '-c', `printf governed > ${JSON.stringify(governedInsidePath)}`],
    workspace,
  ))
  const governedOutside = await kernel.executeGovernedCommand(governedCommand(
    governedSession.sessionId,
    'installed-outside',
    ['/bin/bash', '-c', `printf escaped > ${JSON.stringify(governedOutsidePath)}`],
    workspace,
  ))
  const governedCredential = await kernel.executeGovernedCommand(governedCommand(
    governedSession.sessionId,
    'installed-credential',
    [
      '/bin/bash',
      '-c',
      `while IFS= read -r line; do printf '%s' "$line"; done < ${JSON.stringify(governedCredentialPath)}`,
    ],
    workspace,
  ))
  const governedNetwork = await kernel.executeGovernedCommand(governedCommand(
    governedSession.sessionId,
    'installed-network',
    [
      '/bin/bash',
      '-c',
      `printf x > /dev/tcp/127.0.0.1/${networkPort} && printf remote > ${JSON.stringify(governedRemotePath)}`,
    ],
    workspace,
  ))
  const governedTimeout = await kernel.executeGovernedCommand(governedCommand(
    governedSession.sessionId,
    'installed-timeout',
    ['/bin/bash', '-c', 'while :; do :; done'],
    workspace,
    { timeoutMillis: 50 },
  ))
  await kernel.closeSession(governedSession.sessionId)

  const readOnlySession = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
    governedAuthority: governedAuthority(workspace, {
      roleId: 'verifier',
      permissionPreset: 'snapshot-verify',
      workspaceMode: 'candidate-read-only',
      visibleTools: Object.freeze([
        'artifact.read',
        'artifact.write',
        'workspace.read',
        'code.search',
        'candidate.diff',
        'command.run',
        'test.run',
      ]),
    }),
  })
  const readOnlyTarget = join(workspace, 'read-only-write.txt')
  const readOnlyWrite = await kernel.executeGovernedCommand(governedCommand(
    readOnlySession.sessionId,
    'installed-read-only',
    ['/bin/bash', '-c', `printf denied > ${JSON.stringify(readOnlyTarget)}`],
    workspace,
    { tool: 'test.run' },
  ))
  await kernel.closeSession(readOnlySession.sessionId)

  const ordinarySession = await kernel.createSession({
    cwd: workspace,
    provider: 'keyless-fixture',
    model: 'keyless-fixture-model',
  })
  let ordinaryDenied = false
  try {
    await kernel.executeGovernedCommand(governedCommand(
      ordinarySession.sessionId,
      'installed-ordinary',
      ['/usr/bin/true'],
      workspace,
    ))
  } catch (error) {
    ordinaryDenied = error instanceof KernelError
      && error.code === 'GOVERNED_COMMAND_POLICY_DENIED'
  }
  await kernel.closeSession(ordinarySession.sessionId)
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
    governed: {
      sandbox: governedEnvironment.sandbox,
      network: governedEnvironment.network,
      environmentSecretExcluded: !governedEnvironment.stdout.includes(governedSecret)
        && !governedEnvironment.stdout.includes('WINWINCODE_INSTALLED_UNRELATED_SECRET'),
      environmentNames: governedEnvironment.environmentNames,
      workspaceWriteSucceeded: governedWrite.status === 'exited'
        && governedWrite.exitCode === 0
        && readFileSync(governedInsidePath, 'utf8') === 'governed',
      outsideWriteBlocked: governedOutside.status === 'sandbox-denied'
        && !existsSync(governedOutsidePath),
      credentialReadBlocked: governedCredential.status === 'sandbox-denied'
        && !`${governedCredential.stdout}${governedCredential.stderr}`.includes(governedSecret),
      networkBlocked: governedNetwork.status === 'sandbox-denied'
        && networkConnections === 0
        && !existsSync(governedRemotePath),
      timeoutStopped: governedTimeout.status === 'timed-out',
      readOnlyWriteBlocked: readOnlyWrite.status === 'sandbox-denied'
        && !existsSync(readOnlyTarget),
      ordinaryDenied,
    },
    eventKinds,
    diagnosticEvents,
    errors,
  }
  process.stdout.write(`${JSON.stringify(report)}\n`)
} finally {
  delete process.env.WINWINCODE_INSTALLED_UNRELATED_SECRET
  await new Promise(resolvePromise => networkServer.close(resolvePromise))
  await kernel.shutdown()
  rmSync(blockedPath, { force: true })
}
