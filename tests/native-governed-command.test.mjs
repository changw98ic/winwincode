import assert from 'node:assert/strict'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  GOVERNED_COMMAND_SCHEMA_VERSION,
  GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
  KernelError,
  WinWinCodeKernel,
} from '../packages/native/dist/index.js'

function executorAuthority(workspaceRoot, overrides = {}) {
  return Object.freeze({
    schemaVersion: GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
    roleId: 'executor',
    permissionPreset: 'candidate-write',
    workspaceMode: 'candidate-write',
    workspaceRoot,
    systemInstructions: 'Execute only the exact approved fixture command.',
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

function command(sessionId, commandId, argv, cwd, overrides = {}) {
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
  assert.notEqual(address, null)
  assert.equal(typeof address, 'object')
  return address.port
}

test('native governed commands enforce process, filesystem, network, env, and cancellation', async t => {
  const root = mkdtempSync(join(tmpdir(), 'winwincode-governed-command-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const home = join(root, 'home')
  const workspacePath = join(root, 'workspace')
  mkdirSync(workspacePath)
  const workspace = realpathSync(workspacePath)
  const outside = join(root, 'outside.txt')
  const secret = 'TOKEN-native-governed-secret'
  writeFileSync(join(workspace, '.env'), `${secret}\n`)
  process.env.WINWINCODE_UNRELATED_SECRET = secret
  t.after(() => delete process.env.WINWINCODE_UNRELATED_SECRET)

  const kernel = new WinWinCodeKernel({
    home,
    modelPort: { async *stream() { yield { type: 'created' } } },
  })
  t.after(() => kernel.shutdown())
  const session = await kernel.createSession({
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
    governedAuthority: executorAuthority(workspace),
  })

  const environment = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'environment',
    ['/usr/bin/env'],
    workspace,
  ))
  assert.equal(environment.status, 'exited')
  assert.equal(environment.exitCode, 0)
  assert.equal(environment.stdout.includes('WINWINCODE_UNRELATED_SECRET'), false)
  assert.equal(environment.stdout.includes(secret), false)
  assert.deepEqual(environment.environmentNames, [
    'CI', 'HOME', 'LANG', 'NO_COLOR', 'PATH', 'TEMP', 'TMP', 'TMPDIR',
  ])
  assert.equal(environment.network, 'restricted')
  assert.equal(
    environment.sandbox,
    process.platform === 'darwin' ? 'macos-seatbelt' : 'linux-seccomp',
  )

  const inside = join(workspace, 'inside.txt')
  const writeInside = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'write-inside',
    ['/bin/bash', '-c', `printf governed > ${JSON.stringify(inside)}`],
    workspace,
  ))
  assert.equal(writeInside.status, 'exited')
  assert.equal(writeInside.exitCode, 0)
  assert.equal(readFileSync(inside, 'utf8'), 'governed')

  const writeOutside = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'write-outside',
    ['/bin/bash', '-c', `printf escaped > ${JSON.stringify(outside)}`],
    workspace,
  ))
  assert.equal(writeOutside.status, 'sandbox-denied')
  assert.equal(existsSync(outside), false)

  const readCredential = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'read-credential',
    ['/bin/bash', '-c', `while IFS= read -r line; do printf '%s' "$line"; done < ${JSON.stringify(join(workspace, '.env'))}`],
    workspace,
  ))
  assert.equal(readCredential.status, 'sandbox-denied')
  assert.equal(`${readCredential.stdout}${readCredential.stderr}`.includes(secret), false)

  let networkConnections = 0
  const server = createServer(socket => {
    networkConnections += 1
    socket.end()
  })
  const port = await listen(server)
  t.after(() => new Promise(resolvePromise => server.close(resolvePromise)))
  const remoteWrite = join(workspace, 'remote-write.txt')
  const network = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'network',
    [
      '/bin/bash',
      '-c',
      `printf x > /dev/tcp/127.0.0.1/${port} && printf remote > ${JSON.stringify(remoteWrite)}`,
    ],
    workspace,
  ))
  assert.equal(network.status, 'sandbox-denied')
  assert.equal(networkConnections, 0)
  assert.equal(existsSync(remoteWrite), false)

  const timeoutResult = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'timeout',
    ['/bin/bash', '-c', 'while :; do :; done'],
    workspace,
    { timeoutMillis: 50 },
  ))
  assert.equal(timeoutResult.status, 'timed-out')

  const pending = kernel.executeGovernedCommand(command(
    session.sessionId,
    'cancel',
    ['/bin/bash', '-c', 'while :; do :; done'],
    workspace,
  ))
  await new Promise(resolvePromise => setTimeout(resolvePromise, 100))
  await kernel.cancelGovernedCommand(session.sessionId, 'cancel')
  assert.equal((await pending).status, 'cancelled')

  await assert.rejects(
    kernel.executeGovernedCommand(command(
      session.sessionId,
      'credential-environment',
      ['/usr/bin/env'],
      workspace,
      { environment: { TOKEN: secret } },
    )),
    error => error instanceof KernelError && error.code === 'GOVERNED_COMMAND_POLICY_DENIED',
  )
  await assert.rejects(
    kernel.executeGovernedCommand(command(
      session.sessionId,
      'credential-argument',
      ['/usr/bin/env', `TOKEN=${secret}`],
      workspace,
    )),
    error => error instanceof KernelError && error.code === 'GOVERNED_COMMAND_POLICY_DENIED',
  )
  const closePending = kernel.executeGovernedCommand(command(
    session.sessionId,
    'close-cancel',
    ['/bin/bash', '-c', 'while :; do :; done'],
    workspace,
  ))
  await new Promise(resolvePromise => setTimeout(resolvePromise, 100))
  await kernel.closeSession(session.sessionId)
  assert.equal((await closePending).status, 'cancelled')

  const ordinary = await kernel.createSession({
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
  })
  await assert.rejects(
    kernel.executeGovernedCommand(command(
      ordinary.sessionId,
      'ordinary',
      ['/usr/bin/true'],
      workspace,
    )),
    error => error instanceof KernelError && error.code === 'GOVERNED_COMMAND_POLICY_DENIED',
  )
  await kernel.closeSession(ordinary.sessionId)
})

test('read-only governed roles cannot modify their assigned workspace', async t => {
  const root = mkdtempSync(join(tmpdir(), 'winwincode-governed-readonly-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const workspacePath = join(root, 'workspace')
  mkdirSync(workspacePath)
  const workspace = realpathSync(workspacePath)
  const kernel = new WinWinCodeKernel({
    home: join(root, 'home'),
    modelPort: { async *stream() { yield { type: 'created' } } },
  })
  t.after(() => kernel.shutdown())
  const session = await kernel.createSession({
    cwd: workspace,
    provider: 'fixture-provider',
    model: 'fixture-model',
    governedAuthority: executorAuthority(workspace, {
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
  const target = join(workspace, 'forbidden.txt')
  const result = await kernel.executeGovernedCommand(command(
    session.sessionId,
    'readonly-write',
    ['/bin/bash', '-c', `printf denied > ${JSON.stringify(target)}`],
    workspace,
    { tool: 'test.run' },
  ))
  assert.equal(result.status, 'sandbox-denied')
  assert.equal(existsSync(target), false)
})
