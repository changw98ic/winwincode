#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const GATE_ID = 'WWX-GATE-X4'
const PROTOCOL_VERSION = 'wwx-gate-x4/v1'
const lanes = [
  {
    name: 'automation',
    command: 'cargo',
    args: ['test', '-p', 'winwincode-control-plane', '--test', 'automation_recipe', '--locked'],
    coverage: ['success', 'reject', 'restart-replay'],
  },
  {
    name: 'notification',
    command: 'node',
    args: ['--test', 'tests/attention-notifications-client.test.mjs'],
    coverage: ['success', 'reject', 'replay'],
  },
  {
    name: 'gitReflow',
    command: 'cargo',
    args: ['test', '-p', 'winwincode-control-plane', '--test', 'candidate_git_release_vertical', '--locked'],
    coverage: ['success', 'reject', 'restart-replay'],
  },
  {
    name: 'effect',
    command: 'cargo',
    args: ['test', '-p', 'winwincode-execution-port', '--test', 'performance_comparison', '--locked'],
    coverage: ['success', 'reject', 'replay'],
  },
]

function sha256(value) {
  return createHash('sha256').update(value).digest('hex')
}

function run(command, args) {
  const startedAt = Date.now()
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
  })
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed:\n${result.stdout}${result.stderr}`)
  }
  return {
    status: 'passed',
    exitCode: result.status,
    durationMs: Date.now() - startedAt,
    stdoutSha256: sha256(result.stdout),
    stderrSha256: sha256(result.stderr),
  }
}

const headResult = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' })
if (headResult.status !== 0) throw new Error('git HEAD lookup failed')
const gitHead = headResult.stdout.trim()
const results = Object.fromEntries(lanes.map(lane => [
  lane.name,
  {
    command: [lane.command, ...lane.args],
    coverage: lane.coverage,
    ...run(lane.command, lane.args),
  },
]))
const evidence = {
  gateId: GATE_ID,
  protocolVersion: PROTOCOL_VERSION,
  gitHead,
  generatedAt: new Date().toISOString(),
  lanes: results,
  result: 'pass',
}
const outputDir = join(root, 'test-results', 'gates')
mkdirSync(outputDir, { recursive: true })
const outputPath = join(outputDir, `${GATE_ID.toLowerCase()}-${gitHead.slice(0, 12)}.json`)
writeFileSync(outputPath, `${JSON.stringify(evidence, null, 2)}\n`)
process.stdout.write(`${JSON.stringify({
  gateId: GATE_ID,
  protocolVersion: PROTOCOL_VERSION,
  gitHead,
  result: 'pass',
  outputPath,
  lanes: Object.fromEntries(lanes.map(lane => [lane.name, 'pass'])),
}, null, 2)}\n`)
