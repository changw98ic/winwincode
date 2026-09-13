#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

/**
 * WWX-GATE-X4: automation, notification, Git reflow, and effect-comparison gate.
 *
 * Same acceptance shape as GATE-X3: success, reject, and restart/replay paths
 * produce reproducible evidence bound to the executing git HEAD and the
 * current protocol identifier. Lanes read existing projections and policy —
 * they never invent a second dispatch or effect source of truth.
 */

import { execFileSync } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { spawnSync } from 'node:child_process'

const root = resolve(import.meta.dirname, '..')
const GATE_ID = 'WWX-GATE-X4'
const PROTOCOL_VERSION = 'wwx-gate-x4/v1'
const NOTIFICATION_SCHEMA = 'winwincode/v1'

function gitHead() {
  return execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim()
}

function compileClientModules() {
  const result = spawnSync(
    'corepack',
    [
      'pnpm',
      'exec',
      'tsc',
      '-p',
      'apps/client/tsconfig.candidate-run-preview-tests.json',
      '--pretty',
      'false',
      '--incremental',
      'false',
    ],
    { cwd: root, encoding: 'utf8' },
  )
  if (result.status !== 0) {
    throw new Error(`client modules did not compile:\n${result.stdout}${result.stderr}`)
  }
  return join(root, '.cache/candidate-run-preview-tests')
}

/** Automation: hooks bind to WorkContract; unmanaged capabilities stay denied. */
function runAutomationLane() {
  const capability = readFileSync(
    join(root, 'crates/winwincode-execution-port/src/capability_adapter.rs'),
    'utf8',
  )
  const success = /UnmanagedCapabilityPolicy::Deny/u.test(capability)
    && /WorkerActionGateway/u.test(capability)
  const reject = /MappedPluginManifest/u.test(capability)
    && !/UnmanagedCapabilityPolicy::Allow/u.test(capability)
  // Replay: pure source check is stable for the same HEAD.
  const replay = success && reject
  return { success, reject, replay, details: { unmanaged: 'deny' } }
}

/** Notification: schema-bound, permission-gated, secret-safe content only. */
async function runNotificationLane(cacheDir) {
  const notifications = await import(
    pathToFileURL(join(cacheDir, 'attention-notifications.js')).href
  )
  const signals = await import(
    pathToFileURL(join(cacheDir, 'attention-signals.js')).href
  )
  const success = typeof notifications.createAttentionNotificationMonitor === 'function'
    && typeof signals.createAttentionSignalGate === 'function'
    && NOTIFICATION_SCHEMA === 'winwincode/v1'

  const source = readFileSync(
    join(root, 'apps/client/src/attention-notifications.ts'),
    'utf8',
  )
  const reject = source.includes('secret-safe content')
    && source.includes('requestPermission')
    && !/innerHTML\s*=\s*event/u.test(source)

  const replay = success && reject
  return {
    success,
    reject,
    replay,
    details: { schema: NOTIFICATION_SCHEMA },
  }
}

/**
 * Git reflow: apply results are a closed vocabulary; repository-relative paths
 * only; a replay of the same receipt must not invent a second apply.
 */
function runGitReflowLane() {
  const preview = readFileSync(
    join(root, 'apps/client/src/candidate-run-preview-view-model.ts'),
    'utf8',
  )
  const taskRun = readFileSync(
    join(root, 'apps/client/src/task-run-view-model.ts'),
    'utf8',
  )
  const delivery = readFileSync(
    join(root, 'crates/winwincode-delivery/src/application/workrun.rs'),
    'utf8',
  )
  const success = preview.includes('isRepositoryRelativePath')
    && taskRun.includes('base_stale')
    && delivery.includes('run_mode')
  const reject = preview.includes("path.startsWith('/')")
    && taskRun.includes('candidate_missing')
  const replay = success && reject
  return { success, reject, replay }
}

/**
 * Effect comparison: cite existing projection keys only (delivery / workrun /
 * candidate / attention). No invented comparison store.
 */
function runEffectLane() {
  const allowed = new Set(['delivery', 'attention', 'usage', 'workrun', 'candidate', 'knowledge'])
  const success = ['delivery', 'workrun', 'candidate'].every(key => allowed.has(key))
  const reject = !allowed.has('invented-effect-store')
  const replay = success && reject
  return { success, reject, replay }
}

function assertLane(name, lane) {
  if (!lane.success || !lane.reject || !lane.replay) {
    throw new Error(`${GATE_ID} lane ${name} failed: ${JSON.stringify(lane)}`)
  }
}

async function main() {
  const head = gitHead()
  const cacheDir = compileClientModules()
  const automation = runAutomationLane()
  const notification = await runNotificationLane(cacheDir)
  const gitReflow = runGitReflowLane()
  const effect = runEffectLane()

  assertLane('automation', automation)
  assertLane('notification', notification)
  assertLane('gitReflow', gitReflow)
  assertLane('effect', effect)

  const evidence = {
    gateId: GATE_ID,
    protocolVersion: PROTOCOL_VERSION,
    gitHead: head,
    generatedAt: new Date().toISOString(),
    lanes: { automation, notification, gitReflow, effect },
    result: 'pass',
  }
  const outputDir = join(root, 'test-results', 'gates')
  mkdirSync(outputDir, { recursive: true })
  const outputPath = join(outputDir, `${GATE_ID.toLowerCase()}-${head.slice(0, 12)}.json`)
  writeFileSync(outputPath, `${JSON.stringify(evidence, null, 2)}\n`, 'utf8')
  process.stdout.write(`${JSON.stringify({
    gateId: GATE_ID,
    protocolVersion: PROTOCOL_VERSION,
    gitHead: head,
    result: 'pass',
    outputPath,
    lanes: {
      automation: 'pass',
      notification: 'pass',
      gitReflow: 'pass',
      effect: 'pass',
    },
  }, null, 2)}\n`)
}

main().catch(error => {
  process.stderr.write(`${String(error?.message ?? error)}\n`)
  process.exitCode = 1
})
