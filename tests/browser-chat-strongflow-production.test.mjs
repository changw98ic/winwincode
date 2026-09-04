import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { basename, dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import {
  prepareControlledRepository,
  serverTargetDirectory,
  verifyApiProductionSourceSeal,
  writeHelperReleaseManifest,
} from '../scripts/run-api-production-vertical.mjs'

import {
  certificate,
  chromeBinary,
  closeServer,
  command,
  DevTools,
  evaluate,
  freePort,
  listen,
  startStandaloneServer,
  staticClientServer,
  stopChild,
  waitForGlobal,
  waitForServer,
} from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')
const artifactDirectory = resolve(root, 'test-results/browser-chat-strongflow-production')
const rules = JSON.parse(readFileSync(
  resolve(root, 'docs/contracts/browser-chat-strongflow-production.rules.json'),
  'utf8',
))
const expectedBrowserRule = 'Chrome DevTools Protocol over a real headless Chrome or Chromium process'
const expectedClientRule = 'apps/client production-built modules and production CSS served from a standalone HTTPS origin with one browser-gate entry module'
const expectedServerRule = 'winwincode-server production binary served from a separate HTTPS origin'
const expectedAssertions = [
  'the default Client surface is Chat and mounts the canonical Chat page',
  'Chat creates one ProductSession and refreshes from its real Control Plane event stream',
  'Chat reaches Completed with a non-empty completed assistant message produced by the local Worker and provider path',
  'the Chat terminal projection survives a deep-link navigation and a full browser reload byte-for-byte',
  'StrongFlow starts from the canonical clarifying Delivery revision and uses its generated StageRun binding',
  'StrongFlow waits for scheduler updates and clicks only the visible production solution, task, verdict, Attention, and final Delivery controls',
  'every clicked StrongFlow control is followed by a higher Delivery revision rendered from the real event subscription',
  'StrongFlow reaches delivered with a frozen candidate, a passing verdict, completed tasks, no open Attention, and planner, executor, reviewer, and verifier stages',
  'the delivered StrongFlow deep link survives a full browser reload with the same revision and authenticated session',
  'Chat and StrongFlow open real WebSocket subscriptions only to the configured Control Plane origin',
  'all browser traffic is confined to the standalone Client origin and configured Control Plane origin',
  'no Host or DSH backend request is issued',
  'the bootstrap proof is absent from URL, DOM, browser storage, cookie visibility, console, network URLs, and Server output',
  'initial and terminal Chat and StrongFlow screenshots plus one bounded normalized trace are written under test-results/browser-chat-strongflow-production',
]
const expectedArtifacts = [
  'test-results/browser-chat-strongflow-production/chat-initial.png',
  'test-results/browser-chat-strongflow-production/chat-terminal.png',
  'test-results/browser-chat-strongflow-production/strongflow-initial.png',
  'test-results/browser-chat-strongflow-production/strongflow-terminal.png',
  'test-results/browser-chat-strongflow-production/trace.json',
]
assert.equal(rules.browser, expectedBrowserRule)
assert.equal(rules.client, expectedClientRule)
assert.equal(rules.server, expectedServerRule)
assert.equal(rules.schemaVersion, 'winwincode.browser-chat-strongflow-production.v2')
assert.deepEqual(rules.assertions, expectedAssertions)
assert.deepEqual(rules.artifacts, expectedArtifacts)
assert.deepEqual(rules.forbiddenRequestPathPatterns, [
  '(?:/dsh(?:/|$))',
  '(?:/host(?:/|$))',
])
const forbiddenRequestPaths = rules.forbiddenRequestPathPatterns.map(pattern => new RegExp(pattern, 'u'))
const artifactFiles = rules.artifacts.map(path => resolve(root, path))
assert.equal(new Set(artifactFiles.map(dirname)).size, 1)
assert.equal(dirname(artifactFiles[0]), artifactDirectory)

function forbiddenRequestUrl(value) {
  try {
    const path = decodeURIComponent(new URL(value).pathname)
    return forbiddenRequestPaths.some(pattern => pattern.test(path))
  } catch {
    return true
  }
}

function directoryContainsMarker(directory, marker) {
  const needle = Buffer.from(marker)
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name)
    if (entry.isDirectory()) {
      if (directoryContainsMarker(path, marker)) return true
    } else if (entry.isFile() && readFileSync(path).includes(needle)) {
      return true
    }
  }
  return false
}

function credentialShapeRisks(value, path = '$') {
  if (Array.isArray(value)) {
    return value.flatMap(item => credentialShapeRisks(item, `${path}[]`))
  }
  if (value === null || typeof value !== 'object') return []
  const risks = []
  for (const [key, child] of Object.entries(value)) {
    const childPath = `${path}.${key}`
    const normalized = key.replaceAll(/[^A-Za-z0-9]/gu, '').toLowerCase()
    const allowedReference = [
      'credentialreferenceid',
      'credentialreferenceids',
      'credentialref',
    ].includes(normalized)
    const safePlaceholder = child === null || (
      typeof child === 'string'
      && [
        '[redacted]',
        '<redacted>',
        'redacted',
        'credential-reference',
        'reference-only',
        'dsh-reference-only',
      ].includes(child.trim().toLowerCase())
    )
    if (!allowedReference && [
      'apikey',
      'authorization',
      'credential',
      'credentials',
      'password',
      'passwd',
      'privatekey',
      'secret',
      'clientsecret',
      'token',
      'accesstoken',
      'refreshtoken',
      'idtoken',
      'sessiontoken',
      'vaultlocator',
      'credentiallocator',
      'providercredential',
      'secretmaterial',
    ].includes(normalized) && !safePlaceholder) {
      risks.push({ path: childPath, rule: 'forbidden-field' })
    }
    if (normalized === 'secretstate'
      && !['available', 'revoked', 'missing', 'unavailable'].includes(child)) {
      risks.push({ path: childPath, rule: 'forbidden-secret-state' })
    }
    if (typeof child === 'string') {
      const text = child.toLowerCase()
      const rules = [
        ['private-key', /-----begin (?:rsa |openssh )?private key-----/u],
        ['bearer', /\bbearer\s+(?!\[redacted|<redacted>)[^\s,;"'}\]]+/u],
        ['provider-token', /(?:sk-[a-z0-9_-]{16,}|gh[pous]_[a-z0-9_-]{20,}|github_pat_[a-z0-9_]{20,}|xox[bp]-[a-z0-9-]{10,}|npm_[a-z0-9]{20,})/u],
        ['url-userinfo', /\b[a-z][a-z0-9+.-]*:\/\/[^/\s:@]+:[^/\s@]+@/u],
        ['assignment', /(?:api_key|apikey|authorization|client_secret|password|private_key|secret|token)\s*[=:]\s*(?!\[redacted|<redacted>|redacted\b)[^\s,;"'}\]]+/u],
      ]
      for (const [rule, pattern] of rules) {
        if (pattern.test(text)) risks.push({ path: childPath, rule })
      }
    }
    risks.push(...credentialShapeRisks(child, childPath))
  }
  return risks
}

function eventHubDiagnostic(directory) {
  const database = resolve(directory, 'server-data/event-hub/server-event-hub.sqlite3')
  if (!existsSync(database)) return [{ status: 'absent' }]
  const result = spawnSync(process.env.WWC_SQLITE3_BIN ?? 'sqlite3', [
    '-readonly',
    '-json',
    database,
    `SELECT event_id AS eventId, event_type_json AS eventType, occurred_at_json AS occurredAt,
            payload_json AS payload, scope_json AS scope, sequence, source_json AS source,
            stream_json AS stream, topic
       FROM hub_events
      ORDER BY rowid DESC
      LIMIT 12`,
  ], { encoding: 'utf8' })
  if (result.status !== 0) return [{ status: 'unavailable' }]
  try {
    return JSON.parse(result.stdout).map(row => {
      const frame = {
        event: JSON.parse(row.payload),
        eventId: row.eventId,
        occurredAt: JSON.parse(row.occurredAt),
        scope: JSON.parse(row.scope),
        sequence: row.sequence,
        source: JSON.parse(row.source),
        stream: JSON.parse(row.stream),
        type: 'event.v1',
      }
      return {
        eventType: JSON.parse(row.eventType),
        risks: credentialShapeRisks(frame),
        sequence: row.sequence,
        topic: row.topic,
      }
    })
  } catch {
    return [{ status: 'invalid' }]
  }
}

function diagnosticText(value, seen = new Set()) {
  if (typeof value === 'string') return value
  if (value === null || typeof value !== 'object' || seen.has(value)) return ''
  seen.add(value)
  return Object.values(value).map(item => diagnosticText(item, seen)).join('\n')
}

async function remoteObjectText(devtools, sessionId, remoteObject, seen = new Set()) {
  if (remoteObject === null || typeof remoteObject !== 'object') {
    return diagnosticText(remoteObject)
  }
  const text = [diagnosticText({
    description: remoteObject.description,
    type: remoteObject.type,
    unserializableValue: remoteObject.unserializableValue,
    value: remoteObject.value,
  })]
  if (typeof remoteObject.objectId !== 'string' || seen.has(remoteObject.objectId)) {
    return text.join('\n')
  }
  seen.add(remoteObject.objectId)
  const properties = await devtools.send('Runtime.getProperties', {
    objectId: remoteObject.objectId,
    ownProperties: true,
    accessorPropertiesOnly: false,
    generatePreview: false,
  }, sessionId)
  for (const property of [
    ...(properties.result ?? []),
    ...(properties.internalProperties ?? []),
    ...(properties.privateProperties ?? []),
  ]) {
    text.push(property.name ?? '')
    if (property.value !== undefined) {
      text.push(await remoteObjectText(devtools, sessionId, property.value, seen))
    }
  }
  return text.join('\n')
}

function normalizedUrl(value, clientOrigin, controlUrl) {
  return value
    .replace(clientOrigin, 'CLIENT_ORIGIN')
    .replace(controlUrl, 'CONTROL_ORIGIN')
}

function normalizedHash(value) {
  if (value.startsWith('#/chat')) return '#/chat?session=PRODUCT_SESSION_ID'
  if (value.startsWith('#/strongflow')) {
    return '#/strongflow?delivery=DELIVERY_ID&session=PRODUCT_SESSION_ID&stageRun=STAGE_RUN_ID'
  }
  return value
}

function hasTerminalAssistant(messages) {
  return messages.some(message => (
    message.role === 'assistant'
    && message.state === 'completed'
    && message.content.trim().length > 0
  ))
}

function canonicalTrace({
  setup,
  strongflowInitial,
  chatTerminal,
  strongflowTerminal,
  strongflowReload,
  chatAfterStrongFlowReload,
  chatReload,
  requests,
  webSockets,
  clientOrigin,
  controlUrl,
}) {
  const network = [...new Set(requests.map(request => (
    `${request.method} ${normalizedUrl(request.url, clientOrigin, controlUrl)}`
  )))].sort()
  const controlSocketUrl = controlUrl.replace(/^https:/u, 'wss:')
  return {
    schemaVersion: rules.schemaVersion,
    flow: {
      defaultSurface: 'chat',
      setup: {
        chatHash: normalizedHash(setup.chatHash),
        chatHeading: setup.chatHeading,
        chatMessages: setup.chatMessages,
        chatStatus: setup.chatStatus,
        deliveryRevision: setup.deliveryRevision,
        deliveryStatus: setup.deliveryStatus,
      },
      strongflowInitial: {
        deliveryHeading: strongflowInitial.deliveryHeading,
        deliveryStatus: strongflowInitial.deliveryStatus,
        revision: strongflowInitial.revision,
        stageCount: strongflowInitial.stageCount,
        status: strongflowInitial.status,
        hash: normalizedHash(strongflowInitial.hash),
      },
      chatTerminal: {
        messages: chatTerminal.messages,
        status: chatTerminal.status,
        hash: normalizedHash(chatTerminal.hash),
      },
      strongflowTerminal: {
        actions: strongflowTerminal.actions,
        candidatePresent: strongflowTerminal.candidatePresent,
        deliveryStatus: strongflowTerminal.deliveryStatus,
        observations: strongflowTerminal.observations,
        openAttentionCount: strongflowTerminal.openAttentionCount,
        revision: strongflowTerminal.revision,
        stageRoles: strongflowTerminal.stageRoles,
        status: strongflowTerminal.status,
        taskStatuses: strongflowTerminal.taskStatuses,
        transportFailures: strongflowTerminal.transportFailures,
        verdictCriteria: strongflowTerminal.verdictCriteria,
        verdictStatus: strongflowTerminal.verdictStatus,
        hash: normalizedHash(strongflowTerminal.hash),
      },
      strongflowReload: {
        deliveryStatus: strongflowReload.deliveryStatus,
        revision: strongflowReload.revision,
        status: strongflowReload.status,
        hash: normalizedHash(strongflowReload.hash),
      },
      chatAfterStrongFlowReload: {
        messages: chatAfterStrongFlowReload.messages,
        status: chatAfterStrongFlowReload.status,
        hash: normalizedHash(chatAfterStrongFlowReload.hash),
      },
      chatReload: {
        messages: chatReload.messages,
        status: chatReload.status,
        hash: normalizedHash(chatReload.hash),
      },
    },
    network,
    webSockets: [...new Set(webSockets.map(url => (
      url.replace(controlSocketUrl, 'CONTROL_SOCKET')
    )))].sort(),
    assertions: rules.assertions,
  }
}

test('real browser runs default Chat and StrongFlow through the production Client and Server', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the real-browser product gate')
  if (process.env.WWC_BROWSER_SKIP_BUILD !== '1') {
    command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
    command(root, 'cargo', [
      'build', '-p', 'winwincode-server', '--bin', 'winwincode-server', '--locked', '--offline',
    ])
    command(root, 'cargo', [
      'build', '--release', '-p', 'winwincode-kernel-helper', '--locked', '--offline',
    ])
  }
  const targetDirectory = serverTargetDirectory(root)
  const serverBinary = resolve(targetDirectory, 'debug/winwincode-server')
  const helperExecutable = resolve(targetDirectory, 'debug/winwincode-kernel-helper')
  const releaseHelper = resolve(targetDirectory, 'release/winwincode-kernel-helper')
  if (process.env.WWC_BROWSER_SKIP_BUILD !== '1') {
    assert.equal(existsSync(releaseHelper), true, `release helper is missing: ${releaseHelper}`)
    copyFileSync(releaseHelper, helperExecutable)
    chmodSync(helperExecutable, 0o755)
  }
  assert.equal(existsSync(serverBinary), true, `Server binary is missing: ${serverBinary}`)
  assert.equal(existsSync(helperExecutable), true, `kernel helper is missing: ${helperExecutable}`)
  const helperReleaseManifest = writeHelperReleaseManifest(root, helperExecutable)
  if (process.env.WWC_BROWSER_SKIP_BUILD === '1') {
    verifyApiProductionSourceSeal({
      root,
      serverBinary,
      helperExecutable,
    })
  }
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-product-'))
  rmSync(artifactDirectory, { recursive: true, force: true })
  mkdirSync(artifactDirectory, { recursive: true })
  const certificateFiles = certificate(root, directory)
  const proof = randomBytes(32).toString('base64url')
  const actionSigningKeyHex = randomBytes(32).toString('hex')
  const controlledRepository = prepareControlledRepository({ fixtureDirectory: directory })
  let controlUrl = ''
  let clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-chat-strongflow-client.mjs',
    configuration: () => ({
      forbiddenRequestPathPatterns: rules.forbiddenRequestPathPatterns,
      repositoryBaseline: controlledRepository.revision,
      serverUrl: controlUrl,
    }),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  const controlPort = await freePort()
  const serverErrors = []
  let standalone = null
  let chrome = null
  let devtools = null
  t.after(async () => {
    devtools?.close()
    const cleanup = await Promise.allSettled([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      ...(standalone === null ? [] : [stopChild(standalone, 'SIGINT')]),
      ...(clientServer === null ? [] : [closeServer(clientServer)]),
    ])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false, 'browser gate temporary resources must be removed')
    const failure = cleanup.find(result => result.status === 'rejected')
    if (failure !== undefined && failure.status === 'rejected') throw failure.reason
  })
  ;({ child: standalone, controlUrl } = startStandaloneServer({
    root,
    certificateFiles,
    clientOrigin,
    controlPort,
    directory,
    proof,
    errors: serverErrors,
    actionSigningKeyHex,
    checkoutRevision: controlledRepository.revision,
    helperExecutable,
    helperReleaseManifest,
    repositoryRoot: controlledRepository.repository,
    serverBinary,
    sourceRoot: controlledRepository.sourceRoot,
  }))
  await waitForServer(controlUrl, standalone, serverErrors)
  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  chrome = launched.chrome
  devtools = launched.devtools
  const requests = []
  const webSockets = []
  const webSocketFrames = []
  const browserDiagnostics = []
  const browserDiagnosticReads = []
  const uncaughtExceptions = []
  const errorLogEntries = []
  let authenticationEstablished = false
  let sessionId = null
  function collectRemoteDiagnostics(remoteObjects) {
    if (sessionId === null) return
    browserDiagnosticReads.push(Promise.all(
      remoteObjects.map(remoteObject => remoteObjectText(devtools, sessionId, remoteObject)),
    ).then(values => values.join('\n')).catch(error => diagnosticText(error)))
  }
  devtools.on('Network.requestWillBeSent', event => { requests.push(event.request) })
  devtools.on('Network.webSocketCreated', event => { webSockets.push(event.url) })
  devtools.on('Network.webSocketFrameReceived', event => {
    try {
      const frame = JSON.parse(event.response.payloadData)
      webSocketFrames.push({
        errorCode: frame.error?.error?.code ?? frame.error?.code ?? null,
        errorReason: frame.error?.error?.details?.reason
          ?? frame.error?.details?.reason
          ?? null,
        errorRetryable: frame.error?.error?.retryable ?? frame.error?.retryable ?? null,
        eventType: frame.event?.type ?? null,
        sequence: frame.sequence ?? null,
        type: frame.type ?? 'unknown',
      })
    } catch {
      webSocketFrames.push({ type: 'unparseable' })
    }
  })
  devtools.on('Runtime.consoleAPICalled', event => {
    browserDiagnostics.push(diagnosticText(event.args))
    collectRemoteDiagnostics(event.args ?? [])
  })
  devtools.on('Runtime.exceptionThrown', event => {
    uncaughtExceptions.push(event)
    browserDiagnostics.push(diagnosticText(event))
    collectRemoteDiagnostics(event.exceptionDetails?.exception === undefined
      ? []
      : [event.exceptionDetails.exception])
  })
  devtools.on('Log.entryAdded', event => {
    if (event.entry?.level === 'error') {
      errorLogEntries.push({
        beforeAuthentication: !authenticationEstablished,
        entry: event.entry,
      })
    }
    browserDiagnostics.push(diagnosticText(event))
    collectRemoteDiagnostics(event.entry?.args ?? [])
  })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  ;({ sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true }))
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Network.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Log.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 1000,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', { url: clientOrigin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runChatStrongFlowSetup')

  async function evaluateGate(expression) {
    try {
      return await evaluate(devtools, sessionId, expression)
    } catch (error) {
      if (browserDiagnosticReads.length > 0) {
        browserDiagnostics.push(...await Promise.all(browserDiagnosticReads.splice(0)))
      }
      const diagnostics = [
        ...serverErrors,
        ...browserDiagnostics,
        JSON.stringify({ webSocketFrames: webSocketFrames.slice(-25) }),
        JSON.stringify({ eventHub: eventHubDiagnostic(directory) }),
      ].join('\n')
        .replaceAll(proof, '[redacted]')
        .replaceAll(actionSigningKeyHex, '[redacted]')
        .trim()
        .slice(-8_000)
      if (diagnostics.length > 0 && error instanceof Error) {
        error.message = `${error.message}\nGate diagnostics:\n${diagnostics}`
      }
      throw error
    }
  }

  async function capture(name) {
    const screenshot = await devtools.send('Page.captureScreenshot', {
      format: 'png',
      captureBeyondViewport: true,
    }, sessionId)
    writeFileSync(resolve(artifactDirectory, name), Buffer.from(screenshot.data, 'base64'))
  }

  async function proofLeakScan() {
    return evaluateGate(`({
      url: location.href,
      dom: document.documentElement.outerHTML,
      localStorage: JSON.stringify(localStorage),
      sessionStorage: JSON.stringify(sessionStorage),
      cookie: document.cookie,
    })`)
  }

  const setup = await evaluateGate(
    `globalThis.runChatStrongFlowSetup(${JSON.stringify(proof)})`,
  )
  authenticationEstablished = true
  assert.equal(setup.chatHash, '#/chat?session=psn_01J00000000000000000000001')
  assert.equal(setup.chatHeading, 'Browser production Chat')
  assert.ok(['Running', 'Ready', 'Completed'].includes(setup.chatStatus), JSON.stringify(setup))
  assert.ok(setup.chatMessages.some(message => (
    message.content === 'Run the deterministic local browser workflow.'
    && message.role === 'user'
    && message.state === 'completed'
  )), JSON.stringify(setup))
  assert.equal(setup.deliveryRevision, 2)
  assert.equal(setup.deliveryStatus, 'clarifying')
  assert.equal(setup.legacyBackendRequests.length, 0)
  assert.equal(Object.values(await proofLeakScan()).some(value => value.includes(proof)), false)
  await capture('chat-initial.png')

  const strongflowInitial = await evaluateGate(
    'globalThis.openStrongFlowBrowserFixture()',
  )
  assert.equal(
    strongflowInitial.deliveryHeading,
    'Browser production StrongFlow',
    JSON.stringify(strongflowInitial),
  )
  assert.equal(strongflowInitial.error, '')
  assert.equal(strongflowInitial.status, 'Waiting for your input')
  assert.equal(strongflowInitial.deliveryStatus, 'clarifying')
  assert.ok(strongflowInitial.revision >= setup.deliveryRevision)
  const [strongflowRoute, strongflowQuery = ''] = strongflowInitial.hash.split('?', 2)
  const strongflowParameters = new URLSearchParams(strongflowQuery)
  assert.equal(strongflowRoute, '#/strongflow')
  assert.equal(strongflowParameters.get('delivery'), setup.deliveryId)
  assert.match(strongflowParameters.get('session') ?? '', /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
  assert.match(strongflowParameters.get('stageRun') ?? '', /^run_[0-9A-HJKMNP-TV-Z]{26}$/u)
  assert.ok(strongflowInitial.stageCount >= 1, JSON.stringify(strongflowInitial))
  assert.equal(strongflowInitial.legacyBackendRequests.length, 0)
  await capture('strongflow-initial.png')

  const chatTerminal = await evaluateGate(
    'globalThis.waitForTerminalChatBrowserFixture()',
  )
  assert.equal(chatTerminal.heading, 'Browser production Chat')
  assert.ok(['Ready', 'Completed'].includes(chatTerminal.status))
  assert.equal(hasTerminalAssistant(chatTerminal.messages), true)
  assert.equal(chatTerminal.authSessionBytes, setup.authSessionBytes)
  assert.ok(chatTerminal.runtimeSessions.some(session => (
    session.stageRunId === null
    && session.asOfSequence > 0
    && typeof session.workerSessionId === 'string'
    && typeof session.codexThreadId === 'string'
  )), JSON.stringify(chatTerminal.runtimeSessions))
  assert.equal(chatTerminal.legacyBackendRequests.length, 0)
  await capture('chat-terminal.png')

  const strongflowTerminal = await evaluateGate(
    'globalThis.runStrongFlowToDelivered()',
  )
  assert.equal(strongflowTerminal.deliveryHeading, 'Browser production StrongFlow')
  assert.equal(strongflowTerminal.status, 'Completed')
  assert.equal(strongflowTerminal.deliveryStatus, 'delivered')
  assert.equal(strongflowTerminal.candidatePresent, true)
  assert.equal(strongflowTerminal.verdictStatus, 'pass')
  assert.ok(strongflowTerminal.verdictCriteria.length > 0, JSON.stringify(strongflowTerminal))
  assert.equal(strongflowTerminal.verdictCriteria.every(status => status === 'pass'), true)
  assert.equal(strongflowTerminal.openAttentionCount, 0)
  assert.ok(strongflowTerminal.taskStatuses.length > 0, JSON.stringify(strongflowTerminal))
  assert.equal(strongflowTerminal.taskStatuses.every(status => status === 'completed'), true)
  for (const role of ['executor', 'planner', 'reviewer', 'verifier']) {
    assert.ok(strongflowTerminal.stageRoles.includes(role), `missing ${role}: ${JSON.stringify(
      strongflowTerminal.stageRoles,
    )}`)
    assert.ok(strongflowTerminal.stageProvenance.some(stage => (
      stage.role === role
      && stage.actorType === 'codex'
      && stage.status === 'succeeded'
      && stage.bindingReady
    )), `missing succeeded bound ${role}: ${JSON.stringify(strongflowTerminal.stageProvenance)}`)
  }
  const candidateProducer = strongflowTerminal.stageProvenance.find(stage => (
    stage.id === strongflowTerminal.candidateProducerStageRunId
  ))
  assert.equal(candidateProducer?.role, 'executor')
  assert.equal(candidateProducer?.status, 'succeeded')
  assert.equal(candidateProducer?.bindingReady, true)
  for (const action of [
    'approve-solution',
    'approve-tasks',
    'submit-verdict',
    'advance-delivery',
    'resolve-attention',
  ]) {
    assert.ok(strongflowTerminal.actions.some(item => item.action === action), `missing ${action}`)
  }
  assert.equal(strongflowTerminal.actions.every(action => (
    action.toRevision > action.fromRevision
  )), true)
  assert.equal(strongflowTerminal.legacyBackendRequests.length, 0)
  await capture('strongflow-terminal.png')
  assert.equal(Object.values(await proofLeakScan()).some(value => value.includes(proof)), false)

  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForGlobal(devtools, sessionId, 'inspectTerminalStrongFlowAfterReload')
  const strongflowReload = await evaluateGate(
    'globalThis.inspectTerminalStrongFlowAfterReload()',
  )
  assert.equal(strongflowReload.status, 'Completed')
  assert.equal(strongflowReload.deliveryStatus, 'delivered')
  assert.equal(strongflowReload.revision, strongflowTerminal.revision)
  assert.equal(strongflowReload.hash, strongflowTerminal.hash)
  assert.equal(strongflowReload.authSessionBytes, setup.authSessionBytes)
  assert.equal(strongflowReload.canonicalDeliveryBytes, strongflowTerminal.canonicalDeliveryBytes)
  assert.equal(strongflowReload.legacyBackendRequests.length, 0)

  const chatAfterStrongFlowReload = await evaluateGate(
    'globalThis.openTerminalChatAfterStrongFlowReload()',
  )
  assert.ok(['Ready', 'Completed'].includes(chatAfterStrongFlowReload.status))
  assert.equal(hasTerminalAssistant(chatAfterStrongFlowReload.messages), true)
  assert.deepEqual(chatAfterStrongFlowReload.messages, chatTerminal.messages)
  assert.equal(chatAfterStrongFlowReload.authSessionBytes, setup.authSessionBytes)
  assert.equal(chatAfterStrongFlowReload.canonicalMessagesBytes, chatTerminal.canonicalMessagesBytes)
  assert.equal(chatAfterStrongFlowReload.legacyBackendRequests.length, 0)

  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForGlobal(devtools, sessionId, 'inspectTerminalChatAfterReload')
  const chatReload = await evaluateGate(
    'globalThis.inspectTerminalChatAfterReload()',
  )
  assert.ok(['Ready', 'Completed'].includes(chatReload.status))
  assert.equal(hasTerminalAssistant(chatReload.messages), true)
  assert.deepEqual(chatReload.messages, chatTerminal.messages)
  assert.equal(chatReload.authSessionBytes, setup.authSessionBytes)
  assert.equal(chatReload.canonicalMessagesBytes, chatTerminal.canonicalMessagesBytes)
  assert.equal(chatReload.legacyBackendRequests.length, 0)

  const browserLeakScan = await proofLeakScan()
  for (let offset = 0; offset < browserDiagnosticReads.length;) {
    const pending = browserDiagnosticReads.slice(offset)
    offset = browserDiagnosticReads.length
    browserDiagnostics.push(...await Promise.all(pending))
  }
  assert.equal(Object.values(browserLeakScan).some(value => value.includes(proof)), false)
  for (const marker of [proof, actionSigningKeyHex]) {
    assert.equal(serverErrors.join('').includes(marker), false)
    assert.equal(directoryContainsMarker(resolve(directory, 'server-data'), marker), false)
  }
  assert.equal(browserDiagnostics.some(message => message.includes(proof)), false)
  assert.deepEqual(uncaughtExceptions, [], diagnosticText(uncaughtExceptions))
  assert.equal(strongflowTerminal.transportFailures.every(failure => (
    failure.operation === 'command'
      ? failure.code === 'TRUSTED_FACTS_UNAVAILABLE'
        && failure.kind === 'server'
      : failure.operation === 'query'
        ? failure.code === 'READ_CURSOR_EXPIRED'
          && failure.kind === 'server'
        : false
  )), true, JSON.stringify(strongflowTerminal.transportFailures))
  const expectedCommandErrorLogs = strongflowTerminal.transportFailures.filter(failure => (
    failure.operation === 'command'
  )).length
  const expectedQueryErrorLogs = strongflowTerminal.transportFailures.filter(failure => (
    failure.operation === 'query'
  )).length
  const transientErrorLogEntries = errorLogEntries.filter(({ beforeAuthentication, entry }) => {
    if (beforeAuthentication || entry.source !== 'network') return false
    try {
      const path = new URL(entry.url).pathname
      return (path === '/api/v1/commands' && /\b503\b/u.test(entry.text ?? ''))
        || (path === '/api/v1/queries' && /\b(?:409|410|503)\b/u.test(entry.text ?? ''))
    } catch {
      return false
    }
  })
  assert.equal(
    transientErrorLogEntries.filter(({ entry }) => (
      new URL(entry.url).pathname === '/api/v1/commands'
    )).length,
    expectedCommandErrorLogs,
  )
  assert.equal(
    transientErrorLogEntries.filter(({ entry }) => (
      new URL(entry.url).pathname === '/api/v1/queries'
    )).length,
    expectedQueryErrorLogs,
  )
  const unexpectedErrorLogEntries = errorLogEntries.filter(({ beforeAuthentication, entry }) => {
    if (transientErrorLogEntries.some(candidate => candidate.entry === entry)) return false
    if (!beforeAuthentication || entry.source !== 'network' || !/\b401\b/u.test(entry.text ?? '')) {
      return true
    }
    try {
      return new URL(entry.url).pathname !== '/api/v1/auth/session'
    } catch {
      return true
    }
  })
  assert.deepEqual(unexpectedErrorLogEntries, [], diagnosticText(unexpectedErrorLogEntries))
  assert.equal(requests.some(request => request.url.includes(proof)), false)
  assert.equal(webSockets.some(url => url.includes(proof)), false)
  const resourceUrls = [
    ...setup.resources,
    ...strongflowInitial.resources,
    ...chatTerminal.resources,
    ...strongflowTerminal.resources,
    ...strongflowReload.resources,
    ...chatAfterStrongFlowReload.resources,
    ...chatReload.resources,
  ]
  assert.equal([
    ...requests.map(request => request.url),
    ...webSockets,
    ...resourceUrls,
  ].some(forbiddenRequestUrl), false)
  assert.equal(
    requests.every(request => (
      new URL(request.url).origin === clientOrigin
      || new URL(request.url).origin === controlUrl
      || request.url === 'about:blank'
    )),
    true,
  )
  const controlSocketUrl = controlUrl.replace(/^https:/u, 'wss:')
  assert.ok(webSockets.length > 0, 'the real Client did not open its Control Plane WebSocket')
  assert.equal(webSockets.every(url => url.startsWith(controlSocketUrl)), true)

  const trace = canonicalTrace({
    setup,
    strongflowInitial,
    chatTerminal,
    strongflowTerminal,
    strongflowReload,
    chatAfterStrongFlowReload,
    chatReload,
    requests,
    webSockets,
    clientOrigin,
    controlUrl,
  })
  writeFileSync(resolve(artifactDirectory, 'trace.json'), `${JSON.stringify(trace, null, 2)}\n`)
  for (const artifact of artifactFiles.filter(path => path.endsWith('.png'))) {
    assert.ok(readFileSync(artifact).length > 1_000, basename(artifact))
  }
  assert.ok(readFileSync(artifactFiles.find(path => path.endsWith('/trace.json'))).length > 0)
})
