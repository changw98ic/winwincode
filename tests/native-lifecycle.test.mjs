import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import test from 'node:test'

import {
  KernelError,
  WinWinCodeKernel,
  nativePackageName,
  resolveReleaseTarget,
} from '../packages/native/dist/index.js'
import { createRequire } from 'node:module'
import { dirname } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const target = resolveReleaseTarget()
const require = createRequire(resolve(root, 'packages/native/dist/index.js'))
const prebuildRoot = dirname(require.resolve(`${nativePackageName(target)}/build-info.json`))
const binding = resolve(prebuildRoot, 'winwincode_native.node')
const helper = resolve(prebuildRoot, 'winwincode-kernel-helper')

test('a created native session closes without crashing its host process', () => {
  const child = spawnSync(process.execPath, [
    resolve(root, 'tests/fixtures/native-close-session.mjs'),
    binding,
    helper,
  ], {
    cwd: root,
    encoding: 'utf8',
    timeout: 20_000,
  })

  assert.equal(child.signal, null, `native child terminated by ${child.signal ?? 'no signal'}`)
  assert.equal(child.status, 0, child.stderr || child.stdout)
})

test('TypeScript owns the complete embedded kernel lifecycle contract', () => {
  const child = spawnSync(process.execPath, [
    resolve(root, 'tests/fixtures/native-contract.mjs'),
  ], {
    cwd: root,
    encoding: 'utf8',
    timeout: 20_000,
  })

  assert.equal(child.signal, null, `native child terminated by ${child.signal ?? 'no signal'}`)
  assert.equal(child.status, 0, child.stderr || child.stdout)
  const report = JSON.parse(child.stdout.trim().split('\n').at(-1))

  assert.deepEqual(report.buildInfo, {
    interfaceVersion: 3,
    codexTag: 'rust-v0.149.0',
    codexCommit: '758ef40f50c1a458425c7cfbf1eb12cbc07af0b0',
    patchSet: [
      'upstream/patches/codex/0001-export-client-mcp-extensions.patch',
      'upstream/patches/codex/0002-inject-model-stream-transport.patch',
      'upstream/patches/codex/0003-export-config-builder.patch',
      'upstream/patches/codex/0004-resume-with-caller-options.patch',
    ],
    eventCapacity: 16,
  })
  assert.match(report.source.sessionId, /^[0-9a-f-]{36}$/u)
  assert.ok(report.source.rolloutPath.endsWith('.jsonl'))
  assert.deepEqual(report.firstEvent, {
    sequence: '1',
    kind: 'mcp_startup_complete',
    payloadType: 'mcp_startup_complete',
  })
  assert.equal(report.duplicateSubscriberCode, 'EVENT_SUBSCRIBER_EXISTS')
  assert.deepEqual(report.timeoutPoll, { status: 'timeout' })
  assert.deepEqual(report.sessionsAfterCreate, [report.source.sessionId])
  assert.equal(report.emptySubmitCode, 'EMPTY_INPUT')
  assert.equal(report.emptySteerCode, 'EMPTY_INPUT')
  assert.match(report.idleInterruptSubmissionId, /^[0-9a-f-]{36}$/u)
  assert.match(report.idleApprovalSubmissionId, /^[0-9a-f-]{36}$/u)
  assert.equal(report.invalidApprovalCode, 'INVALID_APPROVAL_RESPONSE')

  assert.notEqual(report.fork.sessionId, report.source.sessionId)
  assert.ok(report.fork.rolloutPath.endsWith('.jsonl'))
  assert.ok(report.forkEvents.length >= 1)
  assert.equal(report.forkEvents[0].sequence, '1')
  assert.equal(report.forkEvents[0].kind, 'mcp_startup_complete')
  assert.deepEqual(report.forkClosedPoll, { status: 'closed' })

  assert.equal(report.submission.status, 'started')
  assert.match(report.submission.turnId, /^[0-9a-f-]{36}$/u)
  assert.deepEqual(report.steering, {
    status: 'steered',
    turnId: report.submission.turnId,
  })
  assert.ok(report.turnEvents.some(event => event.kind === 'turn_started'))
  assert.ok(report.turnEvents.every((event, index, events) => (
    index === 0 || BigInt(event.sequence) > BigInt(events[index - 1].sequence)
  )))
  const turnStarted = report.turnEvents.find(event => event.kind === 'turn_started')
  assert.equal(turnStarted.payloadType, 'task_started')
  assert.match(report.activeInterruptSubmissionId, /^[0-9a-f-]{36}$/u)

  assert.equal(report.resumed.sessionId, report.source.sessionId)
  assert.equal(report.resumed.rolloutPath, report.source.rolloutPath)
  assert.ok(report.modelAbortCount >= 1)
  assert.deepEqual(report.shutdown, {
    completed: [],
    submitFailed: [],
    timedOut: [],
  })
  assert.deepEqual(report.secondShutdown, report.shutdown)
  assert.equal(report.afterShutdownCode, 'KERNEL_CLOSED')
})

test('native loader returns a typed error for a missing artifact', () => {
  assert.throws(
    () => new WinWinCodeKernel({
      home: resolve(root, '.cache', 'missing-native-home'),
      nativeDirectory: resolve(root, '.cache', 'missing-native-artifacts'),
      modelPort: {
        async *stream() {
          yield { type: 'created' }
        },
      },
    }),
    error => error instanceof KernelError && error.code === 'NATIVE_LOAD_FAILED',
  )
})
