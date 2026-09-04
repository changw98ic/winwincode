// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, join, resolve } from 'node:path'
import test from 'node:test'

import {
  certificate,
  chromeBinary,
  closeServer,
  command,
  DevTools,
  evaluate,
  freePort,
  listen,
  staticClientServer,
  stopChild,
  waitForGlobal,
} from './fixtures/real-browser-harness.mjs'
import {
  buildFirstRunDiagnostic,
  FIRST_RUN_DIAGNOSTIC_SCHEMA_VERSION,
  scanFirstRunDiagnostic,
} from './fixtures/browser-first-run-diagnostics.mjs'

const root = resolve(import.meta.dirname, '..')
const artifactDirectory = resolve(root, 'test-results/first-run-strongflow-browser')
const IDENTIFIER = /^[a-z]{3}_[0-9A-HJKMNP-TV-Z]{26}$/u
const STRONGFLOW_ROUTE = new RegExp(
  '^#/strongflow\\?delivery=dlv_[0-9A-HJKMNP-TV-Z]{26}'
  + '&session=psn_[0-9A-HJKMNP-TV-Z]{26}'
  + '&stageRun=run_[0-9A-HJKMNP-TV-Z]{26}&view=unified'
  + '&organizationId=org_00000000000000000000000001'
  + '&workspaceId=wsp_00000000000000000000000001'
  + '&projectId=prj_00000000000000000000000001'
  + '&repositoryId=rep_00000000000000000000000001$',
  'u',
)
const SECRET_VALUES = Object.freeze([
  'first-run-browser-bootstrap-proof',
  'rejected-first-run-bootstrap-proof',
  'vault-locator-secret-marker',
])
const ASSERTIONS = Object.freeze([
  'the first run signs in with one bootstrap proof that is never echoed anywhere',
  'the first run chooses an authorized repository Scope through the Scope selector',
  'the first requirement submission fails once and the Chat retry entry recovers it',
  'the first run selects an available model route before it can create a Chat',
  'the first Chat session and its requirement are created through canonical commands',
  'a mid-flow reload restores the same ProductSession and requirement from the URL',
  'the confirmed Chat requirement becomes one Delivery that opens StrongFlow',
  'the StrongFlow deep link survives a full reload with the same Delivery subscription',
  'the first-run readiness checklist reaches 6 of 6 complete',
  'every key command carries a fresh requestId, an exact expectedRevision, and the selected Scope',
  'the bootstrap proof and the planted vault locator stay out of DOM, URL, storage, console and artifacts',
])

function commandCalls(observation) {
  return observation.commands.map(call => ({
    requestId: call.requestId,
    command: call.command,
    expectedRevision: call.expectedRevision,
    scope: call.scope,
    payload: call.payload,
  }))
}

function messagesOf(observation, productSessionId) {
  return observation.queries
    .filter(call => call.query === 'session.messages.list')
    .filter(call => call.parameters.productSessionId === productSessionId)
}

test('first-run diagnostics keep identifiers and drop every other value', () => {
  const poisoned = {
    page: { url: 'https://client.localhost:8443/#/chat?session=psn_a', hash: '#/chat?session=psn_a' },
    identity: {
      status: 'signed-in',
      actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
      authorizedScopes: [],
    },
    workspace: {
      sessionCount: 1,
      deliveryId: 'dlv_00000000000000000000000001',
      defaultModelRoute: { providerId: 'p', modelId: 'm', credentialReferenceId: 'crd_x' },
      note: 'first-run-browser-bootstrap-proof',
    },
    commands: [{
      requestId: 'req_00000000000000000000000001',
      command: 'session.create',
      expectedRevision: 0,
      scope: {
        kind: 'repository',
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
      },
      payload: {
        productSessionId: 'psn_00000000000000000000000001',
        title: 'New Chat',
        modelRoute: {
          providerId: 'alternate-provider',
          modelId: 'alternate-model',
          credentialReferenceId: 'crd_00000000000000000000000002',
        },
      },
    }],
    queries: [],
    subscriptions: [],
    console: [
      'bootstrap-proof-first-run-browser-vertical leaked',
      'rejected-first-run-bootstrap-proof and vault-locator-secret-marker leaked',
    ],
    secrets: {
      bootstrapProof: 'first-run-browser-bootstrap-proof',
      secretMarker: 'vault-locator-secret-marker',
      submittedProofs: ['first-run-browser-bootstrap-proof'],
    },
  }
  const artifact = buildFirstRunDiagnostic({
    phase: 'unit',
    failure: 'first-run-browser-bootstrap-proof in a failure message',
    observation: poisoned,
    clientOrigin: 'https://client.localhost:8443',
    assertions: [...ASSERTIONS],
    secretValues: SECRET_VALUES,
  })
  assert.equal(artifact.schemaVersion, FIRST_RUN_DIAGNOSTIC_SCHEMA_VERSION)
  assert.equal(artifact.page.url, 'CLIENT_ORIGIN/#/chat?session=psn_a')
  assert.equal(artifact.failure.message, '[redacted] in a failure message')
  const serialized = JSON.stringify(artifact)
  for (const secret of SECRET_VALUES) {
    assert.equal(serialized.includes(secret), false, secret)
  }
  // Negative control: an artifact that kept the observation verbatim must be rejected,
  // which proves the fingerprints really do detect the run's secret material.
  const unsanitized = JSON.stringify(poisoned)
  for (const secret of SECRET_VALUES) {
    assert.equal(unsanitized.includes(secret), true, secret)
  }
  const rejected = scanFirstRunDiagnostic(Buffer.from(unsanitized), {
    label: 'unit/unsanitized.json',
    secretValues: SECRET_VALUES,
  })
  assert.equal(rejected.status, 'rejected')
  assert.equal(
    rejected.findings.some(finding => finding.rule === 'fingerprint.exact-secret'),
    true,
    JSON.stringify(rejected.findings),
  )
  assert.equal(artifact.commands.length, 1)
  assert.deepEqual(artifact.commands[0].payload, {
    productSessionId: 'psn_00000000000000000000000001',
    title: '<redacted>',
    modelRoute: {
      providerId: '<redacted>',
      modelId: '<redacted>',
      credentialReferenceId: 'crd_00000000000000000000000002',
    },
  })
  assert.deepEqual(artifact.commands[0].scope, {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  })
  assert.equal(artifact.console.length, 2)
  assert.equal(artifact.console[1], '[redacted] and [redacted] leaked')
  assert.deepEqual(artifact.secrets, undefined)

  const clean = scanFirstRunDiagnostic(Buffer.from(JSON.stringify(artifact)), {
    label: 'unit/diagnostics.json',
    secretValues: SECRET_VALUES,
  })
  assert.equal(clean.status, 'passed', JSON.stringify(clean.findings))
})

test('a real browser runs the first-use vertical from sign-in into StrongFlow', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the first-run vertical')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  rmSync(artifactDirectory, { recursive: true, force: true })
  mkdirSync(artifactDirectory, { recursive: true })
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-first-run-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-first-run-strongflow.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  let chrome = null
  let devtools = null
  let sessionId = null
  t.after(async () => {
    devtools?.close()
    await Promise.all([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
  })

  async function capture(name) {
    const screenshot = await devtools.send('Page.captureScreenshot', {
      format: 'png',
      captureBeyondViewport: true,
    }, sessionId)
    writeFileSync(join(artifactDirectory, name), Buffer.from(screenshot.data, 'base64'))
  }

  async function writeFailureArtifacts(phase, error) {
    const failure = error instanceof Error ? error.message : String(error)
    try {
      const observation = await evaluate(devtools, sessionId, 'globalThis.firstRunObservation()')
      const diagnostic = buildFirstRunDiagnostic({
        phase,
        failure,
        observation,
        clientOrigin,
        assertions: [...ASSERTIONS],
      })
      writeFileSync(
        join(artifactDirectory, 'diagnostics.json'),
        `${JSON.stringify(diagnostic, null, 2)}\n`,
      )
    } catch (diagnosticError) {
      const diagnostic = buildFirstRunDiagnostic({
        phase,
        failure,
        observation: {},
        clientOrigin,
        assertions: [...ASSERTIONS],
        secretValues: SECRET_VALUES,
      })
      diagnostic.observationError = diagnosticError instanceof Error
        ? diagnosticError.message
        : String(diagnosticError)
      writeFileSync(
        join(artifactDirectory, 'diagnostics.json'),
        `${JSON.stringify(diagnostic, null, 2)}\n`,
      )
    }
    try {
      const screenshot = await devtools.send('Page.captureScreenshot', {
        format: 'png',
        captureBeyondViewport: true,
      }, sessionId)
      writeFileSync(join(artifactDirectory, 'failure.png'), Buffer.from(screenshot.data, 'base64'))
    } catch {}
  }

  async function gate(expression, phase) {
    try {
      return await evaluate(devtools, sessionId, expression)
    } catch (error) {
      await writeFailureArtifacts(phase, error)
      throw error
    }
  }

  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  chrome = launched.chrome
  devtools = launched.devtools
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  ;({ sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true }))
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 1000,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', { url: `${clientOrigin}/#/chat` }, sessionId)
  await waitForGlobal(devtools, sessionId, 'firstRunReady')

  // First contact: no browser session exists, so the shell offers the write-only
  // sign-in form and keeps every surface closed until the proof is accepted.
  const signIn = await gate('globalThis.firstRunSignIn()', 'sign-in')
  assert.equal(signIn.unsigned.status, 'Sign in required')
  assert.equal(signIn.unsigned.slot, 'Sign in to open this workspace.')
  assert.equal(signIn.unsigned.chatMounted, false)
  assert.equal(signIn.unsigned.scopeSelectorMounted, false)
  assert.equal(signIn.unsigned.checklistHidden, true)
  assert.deepEqual(Object.values(signIn.unsigned.secrets), [false, false, false, false])
  assert.equal(signIn.rejected.status, 'Sign in required')
  assert.equal(signIn.rejected.error, 'The bootstrap proof was rejected or expired.')
  assert.equal(signIn.rejected.diagnosticLeak, false)
  assert.deepEqual(Object.values(signIn.rejected.secrets), [false, false, false, false])
  assert.equal(signIn.signedIn.status, 'signed-in')
  assert.deepEqual(signIn.signedIn.actor, { kind: 'user', id: 'usr_00000000000000000000000001' })
  assert.equal(signIn.signedIn.authorizedScopeCount, 2)
  assert.deepEqual(Object.values(signIn.signedIn.secrets), [false, false, false, false])

  // The first run owns no Scope yet: two repositories are authorized, so the shell
  // requires an explicit choice before any surface mounts.
  const scope = await gate('globalThis.firstRunChooseScope()', 'scope-selection')
  assert.equal(scope.before.hash, '#/chat')
  assert.equal(scope.before.chatMounted, false)
  assert.equal(scope.before.slot, 'Choose an authorized repository Scope to open this workspace.')
  assert.equal(scope.before.checklist.summary, 'First-run setup · 0 of 6 complete')
  const beforeScopeItem = scope.before.checklist.items.find(item => item.id === 'repository-scope')
  assert.equal(beforeScopeItem.status, 'attention')
  assert.equal(beforeScopeItem.reason, 'Choose an authorized repository Scope with the Scope selector.')
  assert.deepEqual(Object.values(scope.before.secrets), [false, false, false, false])
  assert.deepEqual(scope.after.scopeParameters, {
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  })
  assert.equal(scope.after.repositoryScopeItem.status, 'ready')
  assert.equal(scope.after.repositoryScopeItem.reason, 'Complete')
  assert.deepEqual(Object.values(scope.after.secrets), [false, false, false, false])
  await capture('first-run-scope.png')

  // With a Scope the checklist can check every server fact except the first Chat.
  const checklist = await gate('globalThis.firstRunChecklistAfterScope()', 'first-run-checklist')
  assert.equal(checklist.summary, 'First-run setup · 5 of 6 complete')
  const checklistItems = Object.fromEntries(checklist.items.map(item => [item.id, item]))
  assert.deepEqual(Object.keys(checklistItems), [
    'repository-scope',
    'model-route',
    'credential-reference',
    'server-worker-health',
    'helper-availability',
    'first-chat-delivery',
  ])
  assert.equal(checklistItems['repository-scope'].status, 'ready')
  assert.equal(checklistItems['model-route'].status, 'ready')
  assert.equal(checklistItems['credential-reference'].status, 'ready')
  assert.equal(checklistItems['server-worker-health'].status, 'ready')
  assert.equal(checklistItems['helper-availability'].status, 'ready')
  assert.equal(checklistItems['first-chat-delivery'].status, 'attention')
  assert.match(checklistItems['first-chat-delivery'].reason, /No Chat session exists yet/u)
  assert.match(checklistItems['first-chat-delivery'].fix, /Start your first Chat/u)

  // No default route is configured, so the first run must pick one before Chat opens.
  const model = await gate('globalThis.firstRunChooseModelRoute()', 'model-route-selection')
  assert.equal(model.before.status, 'Choose a model route')
  assert.equal(model.before.newSessionDisabled, true)
  assert.equal(model.before.model, 'Choose an available model route')
  assert.equal(model.chosen.status, 'Ready for a new Chat')
  assert.match(model.chosen.model, /Alternate Model/u)
  assert.equal(model.chosen.newSessionDisabled, false)

  // The first requirement submission fails once; the Chat retry entry keeps the
  // draft and the resubmitted requirement creates the first Chat content.
  const created = await gate('globalThis.firstRunCreateChat()', 'first-chat')
  assert.equal(created.before.heading, 'Chat')
  assert.equal(created.created.heading, 'New Chat')
  assert.equal(created.created.status, 'Ready')
  assert.deepEqual(created.created.messages, [])
  assert.equal(created.failed.status, 'Ready')
  assert.equal(
    created.failed.error,
    'The Chat server could not be reached. Check the connection and retry.',
  )
  assert.equal(created.failed.retryHidden, false)
  const chatSessionId = new URLSearchParams(created.failed.hash.split('?')[1]).get('session')
  assert.match(chatSessionId, IDENTIFIER)
  assert.equal(created.recovered.retryHidden, true)
  assert.equal(created.recovered.status, 'Ready')
  assert.deepEqual(created.recovered.messages, [])
  assert.deepEqual(created.delivered.messages, [{
    role: 'user',
    state: 'completed',
    content: 'Deliver the deterministic first-run vertical.',
  }])
  await capture('first-run-chat.png')
  // Each reload starts a new page and therefore a new Control Plane facade, so the
  // Chat and Delivery command records have to be read in their own page instances.
  const chatObservation = await gate('globalThis.firstRunObservation()', 'chat-observation')

  // A reload in the middle of the first run must restore the same ProductSession.
  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForGlobal(devtools, sessionId, 'firstRunReady')
  const restoredChat = await gate('globalThis.firstRunRestoreChat()', 'chat-restore')
  assert.equal(restoredChat.hash, created.created.hash)
  assert.equal(restoredChat.heading, 'New Chat')
  assert.deepEqual(restoredChat.messages, [{
    role: 'user',
    state: 'completed',
    content: 'Deliver the deterministic first-run vertical.',
  }])
  const continued = await gate('globalThis.firstRunContinueRestoredChat()', 'chat-continue')
  assert.equal(continued.status, 'Ready')

  // The confirmed requirement becomes exactly one Delivery, which opens StrongFlow.
  const conversion = await gate('globalThis.firstRunConvertDelivery()', 'delivery-conversion')
  assert.equal(conversion.draft.sourceSession, chatSessionId)
  assert.equal(conversion.draft.title, 'New Chat')
  assert.equal(conversion.draft.goal, 'Deliver the deterministic first-run vertical.')
  assert.match(conversion.draft.scope, /rep_00000000000000000000000001/u)
  assert.equal(conversion.draft.model, 'alternate-provider / alternate-model')
  assert.match(conversion.strongflow.hash, STRONGFLOW_ROUTE)
  const strongflowDeliveryId = new URLSearchParams(
    conversion.strongflow.hash.split('?')[1],
  ).get('delivery')
  const strongflowSessionId = new URLSearchParams(
    conversion.strongflow.hash.split('?')[1],
  ).get('session')
  const strongflowStageRunId = new URLSearchParams(
    conversion.strongflow.hash.split('?')[1],
  ).get('stageRun')
  assert.equal(conversion.strongflow.heading, 'First-run Delivery')
  assert.match(conversion.strongflow.status, /Waiting for your input/u)
  assert.deepEqual(conversion.strongflow.deliveryIds, [strongflowDeliveryId])
  await capture('first-run-strongflow.png')

  // The command and subscription record is read before the reload below, because a
  // reload starts a new page and therefore a new Control Plane facade.
  const deliveryObservation = await gate(
    'globalThis.firstRunObservation()',
    'delivery-observation',
  )
  const observation = {
    ...deliveryObservation,
    commands: [...chatObservation.commands, ...deliveryObservation.commands],
    queries: [...chatObservation.queries, ...deliveryObservation.queries],
    subscriptions: [...chatObservation.subscriptions, ...deliveryObservation.subscriptions],
  }

  // The StrongFlow deep link survives a full reload with the same subscription.
  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForGlobal(devtools, sessionId, 'firstRunReady')
  const restoredStrongFlow = await gate(
    'globalThis.firstRunRestoreStrongFlow()',
    'strongflow-restore',
  )
  assert.equal(restoredStrongFlow.hash, conversion.strongflow.hash)
  assert.equal(restoredStrongFlow.deliveryParameter, strongflowDeliveryId)
  assert.equal(restoredStrongFlow.sessionParameter, strongflowSessionId)
  assert.equal(restoredStrongFlow.stageRunParameter, strongflowStageRunId)
  assert.equal(restoredStrongFlow.heading, 'First-run Delivery')
  assert.match(restoredStrongFlow.status, /Waiting for your input/u)
  assert.deepEqual(restoredStrongFlow.deliveryIds, [strongflowDeliveryId])

  // The first-run checklist closes once the first Chat and Delivery both exist.
  const finalChecklist = await gate('globalThis.firstRunFinalChecklist()', 'final-checklist')
  assert.equal(finalChecklist.summary, 'First-run setup complete · 6 of 6 complete')
  assert.equal(finalChecklist.items.every(item => item.status === 'ready'), true)
  assert.equal(finalChecklist.items.every(item => item.fix === null), true)

  const secrets = await gate('globalThis.firstRunSecretScan()', 'secret-scan')
  assert.deepEqual(Object.values(secrets), [false, false, false, false])

  const commands = commandCalls(observation)
  assert.deepEqual(commands.map(item => item.command), [
    'session.create',
    'chat.submit',
    'chat.submit',
    'delivery.create',
    'delivery.advance',
  ])
  for (const item of commands) {
    assert.match(item.requestId, IDENTIFIER, item.command)
    assert.deepEqual(item.scope, {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    }, item.command)
  }
  assert.equal(new Set(commands.map(item => item.requestId)).size, commands.length)
  const [sessionCreate, failedSubmit, retrySubmit, deliveryCreate, deliveryAdvance] = commands
  assert.equal(sessionCreate.expectedRevision, 0)
  assert.match(sessionCreate.payload.productSessionId, IDENTIFIER)
  assert.equal(sessionCreate.payload.productSessionId, chatSessionId)
  assert.equal(sessionCreate.payload.projectId, 'prj_00000000000000000000000001')
  assert.equal(sessionCreate.payload.repositoryId, 'rep_00000000000000000000000001')
  assert.equal(sessionCreate.payload.title, 'New Chat')
  assert.deepEqual(sessionCreate.payload.modelRoute, {
    providerId: 'alternate-provider',
    modelId: 'alternate-model',
    credentialReferenceId: 'crd_00000000000000000000000002',
  })
  // The retried requirement submission carries a fresh requestId and the same
  // exact revision binding, so only one of the two attempts can land.
  for (const submit of [failedSubmit, retrySubmit]) {
    assert.equal(submit.command, 'chat.submit')
    assert.equal(submit.expectedRevision, 1)
    assert.deepEqual(submit.payload, {
      productSessionId: chatSessionId,
      message: 'Deliver the deterministic first-run vertical.',
    })
  }
  assert.notEqual(failedSubmit.requestId, retrySubmit.requestId)
  assert.equal(deliveryCreate.expectedRevision, 0)
  assert.equal(deliveryCreate.payload.deliveryId, strongflowDeliveryId)
  assert.deepEqual(deliveryCreate.payload.spec, {
    acceptanceCriteria: [{
      id: 'criterion:1',
      required: true,
      title: 'The first-run vertical reaches StrongFlow.',
    }],
    baseRevision: '0123456789abcdef0123456789abcdef01234567',
    constraints: [],
    goal: 'Deliver the deterministic first-run vertical.',
    outOfScope: [],
    publicationTarget: null,
    repositoryId: 'rep_00000000000000000000000001',
    scope: ['Deliver the deterministic first-run vertical.'],
    sourceProductSessionId: chatSessionId,
    title: 'First-run Delivery',
  })
  assert.deepEqual(deliveryCreate.payload.tasks, [])
  assert.equal(deliveryAdvance.expectedRevision, 1)
  assert.deepEqual(deliveryAdvance.payload, { deliveryId: strongflowDeliveryId })

  const subscriptions = observation.subscriptions.map(item => item.subscription)
  assert.ok(subscriptions.some(item => (
    item.stream.kind === 'product-session' && item.stream.productSessionId === chatSessionId
  )), JSON.stringify(subscriptions))
  assert.ok(subscriptions.some(item => (
    item.stream.kind === 'delivery' && item.stream.deliveryId === strongflowDeliveryId
  )), JSON.stringify(subscriptions))
  for (const item of subscriptions) {
    // A subscription is bound to the selected repository Scope, or to its Project
    // request pool for the model-route stream; never to any other workspace.
    assert.ok(
      ['repository', 'project'].includes(item.scope.kind)
        && item.scope.organizationId === 'org_00000000000000000000000001'
        && item.scope.workspaceId === 'wsp_00000000000000000000000001'
        && item.scope.projectId === 'prj_00000000000000000000000001'
        && (item.scope.kind === 'project'
          || item.scope.repositoryId === 'rep_00000000000000000000000001'),
      JSON.stringify(item.scope),
    )
  }
  assert.deepEqual(observation.identity.actor, {
    kind: 'user',
    id: 'usr_00000000000000000000000001',
  })
  assert.deepEqual(observation.identity.authorizedScopes, [
    {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    },
    {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000002',
      workspaceId: 'wsp_00000000000000000000000002',
      projectId: 'prj_00000000000000000000000002',
      repositoryId: 'rep_00000000000000000000000002',
    },
  ])
  const { availabilityReads, ...workspace } = observation.workspace
  assert.ok(availabilityReads >= 1, String(availabilityReads))
  assert.deepEqual(workspace, {
    sessionCount: 1,
    messageCount: 1,
    submittedRequirements: 2,
    deliveryId: strongflowDeliveryId,
    deliveryRevision: 2,
    deliveryStatus: 'clarifying',
    defaultModelRoute: null,
  })
  const sessionReads = observation.queries.filter(call => call.query === 'session.get')
  assert.ok(sessionReads.length >= 2, JSON.stringify(observation.workspace))
  assert.deepEqual(messagesOf(observation, chatSessionId).length >= 2, true)
  for (const read of messagesOf(observation, chatSessionId)) {
    assert.equal(read.scope.repositoryId, 'rep_00000000000000000000000001')
  }

  // Failure diagnostics exist and stay secret-safe; the fingerprints prove the gate
  // would reject an artifact that carried the bootstrap proof or the vault locator.
  await writeFailureArtifacts('artifact-self-check', new Error('Intentional artifact self-check.'))
  const diagnosticPath = join(artifactDirectory, 'diagnostics.json')
  const failurePath = join(artifactDirectory, 'failure.png')
  assert.equal(existsSync(diagnosticPath), true, 'diagnostics.json')
  assert.equal(existsSync(failurePath), true, 'failure.png')
  assert.ok(readFileSync(failurePath).length > 1_000, 'failure.png')
  const diagnostic = JSON.parse(readFileSync(diagnosticPath, 'utf8'))
  assert.equal(diagnostic.schemaVersion, FIRST_RUN_DIAGNOSTIC_SCHEMA_VERSION)
  assert.equal(diagnostic.failure.message, 'Intentional artifact self-check.')
  assert.equal(diagnostic.phase, 'artifact-self-check')
  assert.equal(diagnostic.page.url.startsWith('CLIENT_ORIGIN/#/'), true)
  const diagnosticReport = scanFirstRunDiagnostic(readFileSync(diagnosticPath), {
    label: 'test-results/first-run-strongflow-browser/diagnostics.json',
    secretValues: SECRET_VALUES,
  })
  assert.equal(diagnosticReport.status, 'passed', JSON.stringify(diagnosticReport.findings))
  for (const artifact of ['first-run-scope.png', 'first-run-chat.png', 'first-run-strongflow.png']) {
    assert.ok(readFileSync(join(artifactDirectory, artifact)).length > 1_000, basename(artifact))
  }
})
