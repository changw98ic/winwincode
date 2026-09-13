import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readdirSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { pathToFileURL } from 'node:url'

const root = resolve(import.meta.dirname, '..')

const compiler = spawnSync(
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
assert.equal(
  compiler.status,
  0,
  `GATE-X3 knowledge module did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const knowledge = await import(
  pathToFileURL(resolve(root, '.cache/candidate-run-preview-tests/community-knowledge-catalog.js')).href
)

const {
  emptyKnowledgeCatalog,
  upsertKnowledge,
  confirmKnowledge,
  deleteKnowledgeSource,
  knowledgeContextEntries,
  restoreKnowledgeBackup,
  resolveKnowledgeRule,
} = knowledge

test('ADR-0034 success: only confirmed knowledge enters context', () => {
  let catalog = emptyKnowledgeCatalog()
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_1',
    status: 'confirmed',
    scope: 'personal',
    title: 't',
    body: 'b',
    ruleKey: null,
    repositoryId: null,
    sourceVersion: 'v1',
    expiresAt: null,
  })
  // Create always lands in suggested first.
  assert.equal(catalog.entries.get('src_1').status, 'suggested')
  catalog = confirmKnowledge(catalog, 'src_1')
  const caller = { userId: 'u1', repositoryIds: new Set() }
  const context = knowledgeContextEntries(catalog, caller)
  assert.equal(context.length, 1)
  assert.equal(context[0].status, 'confirmed')
})

test('ADR-0034 reject: permission before search; delete leaves tombstone', () => {
  let catalog = emptyKnowledgeCatalog()
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_repo',
    status: 'suggested',
    scope: 'repository',
    title: 'repo',
    body: 'rule',
    ruleKey: 'pm',
    repositoryId: 'rep_1',
    sourceVersion: 'v1',
    expiresAt: null,
  })
  catalog = confirmKnowledge(catalog, 'src_repo')
  const outsider = { userId: 'u2', repositoryIds: new Set() }
  assert.equal(knowledgeContextEntries(catalog, outsider).length, 0)
  const member = { userId: 'u1', repositoryIds: new Set(['rep_1']) }
  assert.equal(knowledgeContextEntries(catalog, member).length, 1)

  catalog = deleteKnowledgeSource(catalog, 'src_repo')
  assert.equal(catalog.entries.get('src_repo').status, 'tombstone')
  assert.equal(catalog.entries.get('src_repo').body, null)
  const recreated = upsertKnowledge(catalog, {
    sourceId: 'src_repo',
    status: 'suggested',
    scope: 'repository',
    title: 'again',
    body: 'x',
    ruleKey: null,
    repositoryId: 'rep_1',
    sourceVersion: 'v2',
    expiresAt: null,
  })
  assert.equal(recreated.entries.get('src_repo').status, 'tombstone')
})

test('ADR-0034 source version change forces reconfirmation', () => {
  let catalog = emptyKnowledgeCatalog()
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_v',
    status: 'suggested',
    scope: 'personal',
    title: 't',
    body: 'b',
    ruleKey: null,
    repositoryId: null,
    sourceVersion: 'v1',
    expiresAt: null,
  })
  catalog = confirmKnowledge(catalog, 'src_v')
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_v',
    status: 'confirmed',
    scope: 'personal',
    title: 't',
    body: 'b2',
    ruleKey: null,
    repositoryId: null,
    sourceVersion: 'v2',
    expiresAt: null,
  })
  assert.equal(catalog.entries.get('src_v').status, 'needs_reconfirmation')
  const caller = { userId: 'u1', repositoryIds: new Set() }
  assert.equal(knowledgeContextEntries(catalog, caller).length, 0)
})

test('ADR-0034 restore replay: missing tombstones keep reads closed', () => {
  const backup = emptyKnowledgeCatalog()
  const blocked = restoreKnowledgeBackup({
    backup,
    tombstonesAfterRestorePoint: [],
    expectedTombstoneIds: ['src_x'],
  })
  assert.equal(blocked.readsOpen, false)
  const open = restoreKnowledgeBackup({
    backup,
    tombstonesAfterRestorePoint: ['src_x'],
    expectedTombstoneIds: ['src_x'],
  })
  assert.equal(open.readsOpen, true)
})

test('repository rules override personal rules of the same key', () => {
  let catalog = emptyKnowledgeCatalog()
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_p',
    status: 'suggested',
    scope: 'personal',
    title: 'personal',
    body: 'npm',
    ruleKey: 'pm',
    repositoryId: null,
    sourceVersion: 'v1',
    expiresAt: null,
  })
  catalog = upsertKnowledge(catalog, {
    sourceId: 'src_r',
    status: 'suggested',
    scope: 'repository',
    title: 'repo',
    body: 'pnpm',
    ruleKey: 'pm',
    repositoryId: 'rep_1',
    sourceVersion: 'v1',
    expiresAt: null,
  })
  catalog = confirmKnowledge(catalog, 'src_p')
  catalog = confirmKnowledge(catalog, 'src_r')
  const caller = { userId: 'u1', repositoryIds: new Set(['rep_1']) }
  const winner = resolveKnowledgeRule(catalog, caller, 'pm')
  assert.equal(winner?.sourceId, 'src_r')
})

test('gate runner passes success/reject/replay and binds evidence to HEAD', () => {
  const result = spawnSync(
    'node',
    ['scripts/run-wwx-gate-x3.mjs'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(result.status, 0, result.stdout + result.stderr)
  const payload = JSON.parse(result.stdout)
  assert.equal(payload.gateId, 'WWX-GATE-X3')
  assert.equal(payload.result, 'pass')
  assert.match(payload.gitHead, /^[0-9a-f]{40}$/)
  assert.equal(payload.protocolVersion, 'wwx-gate-x3/v1')
  const evidence = JSON.parse(readFileSync(payload.outputPath, 'utf8'))
  assert.equal(evidence.gitHead, payload.gitHead)
  for (const lane of ['knowledge', 'extension', 'template', 'impact']) {
    assert.equal(evidence.lanes[lane].success, true, lane)
    assert.equal(evidence.lanes[lane].reject, true, lane)
    assert.equal(evidence.lanes[lane].replay, true, lane)
  }
  const gateFiles = readdirSync(join(root, 'test-results/gates'))
  assert.ok(gateFiles.some(name => name.startsWith('wwx-gate-x3-')))
})
