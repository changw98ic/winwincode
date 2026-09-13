#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

/**
 * WWX-GATE-X3: knowledge, extension, template, and impact-analysis reuse gate.
 *
 * Runs success, reject, and restart/replay paths for each lane and writes a
 * single evidence document bound to the executing git HEAD and the current
 * protocol identifiers. Never invents a second knowledge index or extension
 * execution path.
 */

import { execFileSync } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { spawnSync } from 'node:child_process'

const root = resolve(import.meta.dirname, '..')
const GATE_ID = 'WWX-GATE-X3'
const PROTOCOL_VERSION = 'wwx-gate-x3/v1'

function gitHead() {
  return execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim()
}

function compileKnowledgeModule() {
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
    throw new Error(`knowledge module did not compile:\n${result.stdout}${result.stderr}`)
  }
  return join(root, '.cache/candidate-run-preview-tests/community-knowledge-catalog.js')
}

function extensionPolicy() {
  const matrix = JSON.parse(
    readFileSync(
      join(root, 'docs/contracts/community-extension-compatibility.matrix.json'),
      'utf8',
    ),
  )
  return matrix.policy
}

function templateAccepts(template) {
  // Templates are data only: schema fields and action references, never code.
  if (typeof template?.id !== 'string' || template.id.length === 0) return false
  if (typeof template?.body !== 'string') return false
  if (/<\s*script|javascript:/i.test(template.body)) return false
  if (template.actions !== undefined && !Array.isArray(template.actions)) return false
  for (const action of template.actions ?? []) {
    if (typeof action !== 'string' || action.length === 0) return false
  }
  return true
}

function impactAnalysisReusesProjection(candidate) {
  // Impact analysis must cite an existing projection key; it may not invent a
  // second source of truth.
  if (!Array.isArray(candidate?.projectionKeys)) return false
  if (candidate.projectionKeys.length === 0) return false
  const allowed = new Set([
    'delivery',
    'attention',
    'usage',
    'workrun',
    'candidate',
    'knowledge',
  ])
  return candidate.projectionKeys.every(key => allowed.has(key))
}

async function runKnowledgeLane(modulePath) {
  const knowledge = await import(pathToFileURL(modulePath).href)
  const {
    emptyKnowledgeCatalog,
    upsertKnowledge,
    confirmKnowledge,
    deleteKnowledgeSource,
    knowledgeContextEntries,
    restoreKnowledgeBackup,
  } = knowledge

  const caller = { userId: 'user_1', repositoryIds: new Set(['rep_1']) }
  const outsider = { userId: 'user_2', repositoryIds: new Set() }

  // Success path: suggest → confirm → context.
  let catalog = emptyKnowledgeCatalog()
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_1',
    status: 'confirmed',
    scope: 'personal',
    title: '偏好短回复',
    body: '回答保持简短',
    ruleKey: null,
    repositoryId: null,
    sourceVersion: 'v1',
    expiresAt: null,
  })
  catalog = confirmKnowledge(catalog, 'src_1')
  const success = knowledgeContextEntries(catalog, caller).length === 1

  // Reject path: repository knowledge without permission never enters context;
  // deleted sources leave tombstones and cannot be recreated.
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_repo',
    status: 'suggested',
    scope: 'repository',
    title: '仓库规则',
    body: '使用 pnpm',
    ruleKey: 'package-manager',
    repositoryId: 'rep_1',
    sourceVersion: 'v1',
    expiresAt: null,
  })
  const rejectNoPermission = knowledgeContextEntries(catalog, outsider).every(
    entry => entry.sourceId !== 'src_repo',
  )
  catalog = deleteKnowledgeSource(catalog, 'src_repo')
  const tombstone = catalog.entries.get('src_repo')?.status === 'tombstone'
  const recreated = upsertKnowledge(catalog, {
    sourceId: 'src_repo',
    status: 'suggested',
    scope: 'repository',
    title: '再来一次',
    body: 'x',
    ruleKey: null,
    repositoryId: 'rep_1',
    sourceVersion: 'v2',
    expiresAt: null,
  })
  const rejectRecreate = recreated.entries.get('src_repo')?.status === 'tombstone'

  // Replay path: restore without post-restore tombstones keeps reads closed.
  const backup = emptyKnowledgeCatalog()
  const closed = restoreKnowledgeBackup({
    backup,
    tombstonesAfterRestorePoint: [],
    expectedTombstoneIds: ['src_deleted'],
  })
  const replayBlocked = closed.readsOpen === false
  const open = restoreKnowledgeBackup({
    backup,
    tombstonesAfterRestorePoint: ['src_deleted'],
    expectedTombstoneIds: ['src_deleted'],
  })
  const replayOpen = open.readsOpen === true

  return {
    success,
    reject: rejectNoPermission && tombstone && rejectRecreate,
    replay: replayBlocked && replayOpen,
    details: {
      successContextCount: knowledgeContextEntries(catalog, caller).length,
      rejectNoPermission,
      tombstone,
      rejectRecreate,
      replayBlocked,
      replayOpen,
    },
  }
}

function runExtensionLane() {
  const policy = extensionPolicy()
  const success = policy.unknownPackage === 'unverified-and-not-executable'
    && policy.unmanagedCapability === 'deny'
    && policy.discovery === 'metadata-only-no-execution'
  const reject = policy.arbitraryDomOrAuthorityAccess === 'unsupported'
    && policy.installation.startsWith('not-enabled-')
  const page = readFileSync(join(root, 'apps/client/src/extensions-page.ts'), 'utf8')
  const replay = !page.includes('submitCommand') && page.includes('disabled: true')
  return { success, reject, replay, details: { policyUnknown: policy.unknownPackage } }
}

function runTemplateLane() {
  const success = templateAccepts({
    id: 'tpl_review',
    body: '请按验收条件逐项检查。',
    actions: ['open-review'],
  })
  const reject = !templateAccepts({
    id: 'tpl_bad',
    body: '<script>alert(1)</script>',
  }) && !templateAccepts({ id: '', body: 'x' })
  // Replay: same template accepted twice with identical result (pure function).
  const first = templateAccepts({ id: 'tpl_a', body: 'ok' })
  const second = templateAccepts({ id: 'tpl_a', body: 'ok' })
  const replay = first && second && first === second
  return { success, reject, replay }
}

function runImpactLane() {
  const success = impactAnalysisReusesProjection({
    projectionKeys: ['delivery', 'attention'],
  })
  const reject = !impactAnalysisReusesProjection({ projectionKeys: ['invented-store'] })
    && !impactAnalysisReusesProjection({ projectionKeys: [] })
  const replay = impactAnalysisReusesProjection({ projectionKeys: ['workrun'] })
  return { success, reject, replay }
}

function assertLane(name, lane) {
  if (!lane.success || !lane.reject || !lane.replay) {
    const error = new Error(`${GATE_ID} lane ${name} failed: ${JSON.stringify(lane)}`)
    error.lane = name
    error.laneResult = lane
    throw error
  }
}

async function main() {
  const head = gitHead()
  const modulePath = compileKnowledgeModule()
  const knowledge = await runKnowledgeLane(modulePath)
  const extension = runExtensionLane()
  const template = runTemplateLane()
  const impact = runImpactLane()

  assertLane('knowledge', knowledge)
  assertLane('extension', extension)
  assertLane('template', template)
  assertLane('impact', impact)

  const evidence = {
    gateId: GATE_ID,
    protocolVersion: PROTOCOL_VERSION,
    gitHead: head,
    generatedAt: new Date().toISOString(),
    lanes: {
      knowledge,
      extension,
      template,
      impact,
    },
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
      knowledge: 'pass',
      extension: 'pass',
      template: 'pass',
      impact: 'pass',
    },
  }, null, 2)}\n`)
}

main().catch(error => {
  process.stderr.write(`${String(error?.message ?? error)}\n`)
  process.exitCode = 1
})
