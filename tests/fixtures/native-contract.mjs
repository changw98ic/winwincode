import { mkdtempSync, mkdirSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  KernelError,
  WinWinCodeKernel,
} from '../../packages/native/dist/index.js'

async function captureKernelError(action) {
  try {
    await action()
  } catch (error) {
    if (error instanceof KernelError) return error.code
    throw error
  }
  throw new Error('expected a KernelError')
}

const root = mkdtempSync(join(tmpdir(), 'winwincode-native-contract-'))
const home = join(root, 'home')
const cwd = join(root, 'workspace')
mkdirSync(cwd)

let modelAbortCount = 0
let modelCallStarted = false
const blockingModelPort = {
  async *stream(request, signal) {
    modelCallStarted = true
    yield { type: 'created' }
    yield { type: 'server_model', model: request.request.model }
    await new Promise(resolvePromise => signal.addEventListener(
      'abort',
      resolvePromise,
      { once: true },
    ))
    modelAbortCount += 1
  },
}

const kernel = new WinWinCodeKernel({
  home,
  eventCapacity: 1,
  modelPort: blockingModelPort,
})
const report = {
  buildInfo: kernel.buildInfo,
}

try {
  const source = await kernel.createSession({
    cwd,
    provider: 'fixture',
    model: 'fixture-model',
  })
  report.source = source

  const eventStream = kernel.events(source.sessionId, { timeoutMillis: 20 })
  const firstResult = await eventStream.next()
  if (firstResult.done) throw new Error('source event stream closed before its startup event')
  report.firstEvent = {
    sequence: firstResult.value.sequence.toString(),
    kind: firstResult.value.kind,
    payloadType: firstResult.value.payload?.msg?.type,
  }

  const duplicateStream = kernel.events(source.sessionId, { timeoutMillis: 20 })
  report.duplicateSubscriberCode = await captureKernelError(() => duplicateStream.next())
  await eventStream.return()

  report.timeoutPoll = await kernel.pollEvent(source.sessionId, 20)
  report.sessionsAfterCreate = await kernel.listSessions()
  report.emptySubmitCode = await captureKernelError(
    () => kernel.submitTurn(source.sessionId, '   '),
  )
  report.emptySteerCode = await captureKernelError(
    () => kernel.steer({
      sessionId: source.sessionId,
      expectedTurnId: 'not-active',
      text: '   ',
    }),
  )
  report.idleInterruptSubmissionId = await kernel.interrupt(source.sessionId)
  report.idleApprovalSubmissionId = await kernel.resolveApproval({
    sessionId: source.sessionId,
    kind: 'exec',
    operationId: 'fixture-approval',
    decision: { kind: 'abort' },
  })
  report.invalidApprovalCode = await captureKernelError(
    () => kernel.resolveApproval({
      sessionId: source.sessionId,
      kind: 'exec',
      operationId: 'fixture-approval',
      decision: { kind: 'denied', rejection: '   ' },
    }),
  )

  const fork = await kernel.forkSession({ sourceSessionId: source.sessionId })
  report.fork = fork
  const forkEvents = []
  while (true) {
    const poll = await kernel.pollEvent(fork.sessionId, 20)
    if (poll.status !== 'event') break
    forkEvents.push({ sequence: poll.event.sequence.toString(), kind: poll.event.kind })
  }
  report.forkEvents = forkEvents
  const forkClosedPoll = kernel.pollEvent(fork.sessionId, 5_000)
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  await kernel.closeSession(fork.sessionId)
  report.forkClosedPoll = await forkClosedPoll

  const submission = await kernel.submitTurn(source.sessionId, 'Reply with one word.')
  if (submission.turnId === undefined) throw new Error('turn submission did not return a turn ID')
  report.submission = submission
  report.steering = await kernel.steer({
    sessionId: source.sessionId,
    expectedTurnId: submission.turnId,
    text: 'Reply with two words.',
  })

  const turnEvents = []
  const modelStartDeadline = Date.now() + 5_000
  while (!modelCallStarted && Date.now() < modelStartDeadline) {
    const poll = await kernel.pollEvent(source.sessionId, 100)
    if (poll.status !== 'event') continue
    turnEvents.push({
      sequence: poll.event.sequence.toString(),
      kind: poll.event.kind,
      payloadType: poll.event.payload?.msg?.type,
    })
  }
  if (!modelCallStarted) throw new Error('model stream did not start')
  report.turnEvents = turnEvents
  report.activeInterruptSubmissionId = await kernel.interrupt(source.sessionId)
  const modelAbortDeadline = Date.now() + 5_000
  const postInterruptKinds = []
  while (modelAbortCount === 0 && Date.now() < modelAbortDeadline) {
    const poll = await kernel.pollEvent(source.sessionId, 20)
    if (poll.status === 'event') postInterruptKinds.push(poll.event.kind)
  }
  if (modelAbortCount === 0) {
    throw new Error(
      `model stream did not receive cancellation; events=${postInterruptKinds.join(',')}`,
    )
  }

  const rolloutPath = source.rolloutPath
  if (rolloutPath === undefined) throw new Error('source session did not create a rollout')
  await kernel.closeSession(source.sessionId)
  const resumed = await kernel.resumeSession({
    rolloutPath,
    cwd,
    provider: 'fixture',
    model: 'fixture-model',
  })
  report.resumed = resumed
  await kernel.closeSession(resumed.sessionId)
  report.modelAbortCount = modelAbortCount

  report.shutdown = await kernel.shutdown()
  report.secondShutdown = await kernel.shutdown()
  report.afterShutdownCode = await captureKernelError(() => kernel.listSessions())
} finally {
  await kernel.shutdown()
  rmSync(root, { force: true, recursive: true })
}

process.stdout.write(`${JSON.stringify(report)}\n`)
