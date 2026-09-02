import assert from 'node:assert/strict'
import { readFile, readdir } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { join, relative, resolve, sep } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const inventoryPath = join(
  root,
  'docs/decisions/0028-control-plane-worker-migration.inventory.json',
)
const TARGET_SOURCE_ROOTS = Object.freeze([
  'schema/winwincode/v1',
  'apps/client/src',
  'packages/contracts/src',
  'packages/strongflow/src',
  'crates/helper/src',
  'crates/kernel/src',
  'crates/winwincode-api/src',
  'crates/winwincode-audit/src',
  'crates/winwincode-backup/src',
  'crates/winwincode-cli/src',
  'crates/winwincode-drill/src',
  'crates/winwincode-evidence-export/src',
  'crates/winwincode-integration/src',
  'crates/winwincode-object-store/src',
  'crates/winwincode-postgres/src',
  'crates/winwincode-test-assets/src',
  'crates/winwincode-control-plane/src',
  'crates/winwincode-codex/src',
  'crates/winwincode-delivery/src',
  'crates/winwincode-domain/src',
  'crates/winwincode-execution-port/src',
  'crates/winwincode-local/src',
  'crates/winwincode-observability/src',
  'crates/winwincode-publication/src',
  'crates/winwincode-repository-context/src',
  'crates/winwincode-server/src',
  'crates/winwincode-session/src',
  'crates/winwincode-storage/src',
  'crates/winwincode-worker/src',
])
const REQUIRED_SCENARIOS = Object.freeze([
  'success',
  'failure',
  'cancel',
  'recovery',
  'approval',
  'close',
])
const REQUIRED_CALLER_KINDS = Object.freeze([
  'composition',
  'presentation',
  'release',
  'test',
])
const REQUIRED_BEHAVIOR_KINDS = Object.freeze([
  'persistence',
  'event',
  'error',
])
const TARGET_OWNERS = new Set([
  'canonical-schema',
  'control-plane',
  'execution-worker',
  'typescript-web',
])
const DISPOSITIONS = new Set(['retain'])

async function json(path) {
  return JSON.parse(await readFile(join(root, path), 'utf8'))
}

async function filesBelow(path) {
  const directory = join(root, path)
  const entries = await readdir(directory, { withFileTypes: true })
  const files = []
  for (const entry of entries) {
    const absolute = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...await filesBelow(relative(root, absolute)))
    else if (entry.isFile() && /\.(?:rs|ts|tsx|js|json)$/u.test(entry.name)) {
      files.push(relative(root, absolute).split(sep).join('/'))
    }
  }
  return files
}

function duplicate(values) {
  return values.find((value, index) => values.indexOf(value) !== index)
}

test('inventory names the current apps/client and Rust source roots', async () => {
  const inventory = await json('docs/decisions/0028-control-plane-worker-migration.inventory.json')

  assert.equal(inventory.schemaVersion, 1)
  assert.deepEqual(inventory.architecture, {
    presentation: 'apps/client',
    businessAuthority: 'crates/winwincode-control-plane',
    executionCoordinator: 'crates/winwincode-worker',
    executionAuthority: 'crates/kernel',
    executionSeam: 'crates/winwincode-execution-port',
    serverBoundary: 'crates/winwincode-server',
    localComposition: 'crates/winwincode-local',
    helperExecutable: 'crates/helper',
  })
  assert.deepEqual(inventory.sourceRoots, TARGET_SOURCE_ROOTS)
  assert.ok(inventory.surfaces.length > 0)

  const surfaceIds = inventory.surfaces.map(surface => surface.id)
  assert.equal(duplicate(surfaceIds), undefined)
  const listedPaths = inventory.surfaces.flatMap(surface => surface.sourcePaths)
  assert.equal(duplicate(listedPaths), undefined)
  for (const sourceRoot of TARGET_SOURCE_ROOTS) assert.ok(existsSync(join(root, sourceRoot)))
  for (const sourcePath of listedPaths) assert.ok(existsSync(join(root, sourcePath)), sourcePath)

  const actualSourceFiles = (await Promise.all(TARGET_SOURCE_ROOTS.map(filesBelow))).flat()
  const listedSourceFiles = listedPaths.filter(path => /\.(?:rs|ts|tsx|js|json)$/u.test(path))
  assert.deepEqual([...new Set(listedSourceFiles)].sort(), [...new Set(actualSourceFiles)].sort())
})

test('inventory surfaces have one phase, owner, target module and observable contract', async () => {
  const inventory = await json('docs/decisions/0028-control-plane-worker-migration.inventory.json')
  const graph = await json('docs/decisions/0028-control-plane-worker-target-graph.json')
  const graphPaths = new Set(graph.nodes.map(node => node.path))
  const baselineIds = new Set(inventory.behaviorBaselines.map(baseline => baseline.id))
  for (const surface of inventory.surfaces) {
    assert.match(surface.id, /^[a-z][a-z0-9-]+$/u)
    assert.ok(surface.sourcePaths.length > 0)
    assert.ok(surface.entrySymbols.length > 0)
    assert.ok(surface.targetOwners.length > 0)
    assert.ok(surface.targetOwners.every(owner => TARGET_OWNERS.has(owner)))
    assert.match(surface.phase, /^(?:[1-6]|enterprise)$/u)
    assert.ok(DISPOSITIONS.has(surface.disposition))
    assert.ok(surface.observableContracts.length > 0)
    assert.ok(surface.behaviorBaselineIds.length > 0)
    assert.ok(surface.behaviorBaselineIds.every(id => baselineIds.has(id)))
    assert.ok(surface.targetModules.length > 0)
    for (const target of surface.targetModules) assert.ok(graphPaths.has(target), target)
  }
  assert.deepEqual(inventory.upstreamPackages, [])
  assert.deepEqual(inventory.temporaryAdapters, [])
  assert.deepEqual(inventory.removedCapabilities, [])
  const serialized = JSON.stringify(inventory)
  for (const oldPath of [
    'apps/host',
    'apps/web',
    'packages/dsh-profile',
    'packages/native',
    'crates/native',
    'dsh-profile',
    'napi-kernel-bridge',
  ]) assert.equal(serialized.includes(oldPath), false, oldPath)
})

test('inventory behavior baselines cover success, failure, cancel, recovery, approval and close', async () => {
  const inventory = await json('docs/decisions/0028-control-plane-worker-migration.inventory.json')
  const baselineIds = inventory.behaviorBaselines.map(baseline => baseline.id)
  assert.equal(duplicate(baselineIds), undefined)
  assert.deepEqual(
    [...new Set(inventory.behaviorBaselines.map(baseline => baseline.scenario))].sort(),
    [...REQUIRED_SCENARIOS].sort(),
  )
  for (const baseline of inventory.behaviorBaselines) {
    assert.ok(REQUIRED_SCENARIOS.includes(baseline.scenario))
    assert.ok(baseline.expectedFacts.length > 0)
    assert.ok(existsSync(join(root, baseline.testFile)), baseline.testFile)
  }
  const referencedBaselineIds = new Set(
    inventory.surfaces.flatMap(surface => surface.behaviorBaselineIds),
  )
  assert.deepEqual([...referencedBaselineIds].sort(), [...baselineIds].sort())
})

test('inventory callers cover Client, Server, Local, Rust tests and release checks', async () => {
  const inventory = await json('docs/decisions/0028-control-plane-worker-migration.inventory.json')
  const callerIds = inventory.callers.map(caller => caller.id)
  assert.equal(duplicate(callerIds), undefined)
  assert.deepEqual(
    [...new Set(inventory.callers.map(caller => caller.kind))].sort(),
    [...REQUIRED_CALLER_KINDS].sort(),
  )
  const surfaceIds = new Set(inventory.surfaces.map(surface => surface.id))
  const baselineIds = new Set(inventory.behaviorBaselines.map(baseline => baseline.id))
  for (const caller of inventory.callers) {
    assert.ok(REQUIRED_CALLER_KINDS.includes(caller.kind))
    assert.ok(caller.sourcePaths.length > 0)
    assert.ok(caller.currentEntryPoints.length > 0)
    assert.ok(caller.targetOwners.length > 0)
    assert.ok(caller.targetOwners.every(owner => TARGET_OWNERS.has(owner)))
    assert.ok(caller.surfaceIds.length > 0)
    assert.ok(caller.surfaceIds.every(id => surfaceIds.has(id)))
    assert.ok(caller.behaviorBaselineIds.length > 0)
    assert.ok(caller.behaviorBaselineIds.every(id => baselineIds.has(id)))
    for (const sourcePath of caller.sourcePaths) assert.ok(existsSync(join(root, sourcePath)), sourcePath)
  }
})

test('inventory freezes persistence, event and error observations independently', async () => {
  const inventory = await json('docs/decisions/0028-control-plane-worker-migration.inventory.json')
  const contractIds = inventory.observableBehaviors.map(contract => contract.id)
  assert.equal(duplicate(contractIds), undefined)
  assert.deepEqual(
    [...new Set(inventory.observableBehaviors.map(contract => contract.kind))].sort(),
    [...REQUIRED_BEHAVIOR_KINDS].sort(),
  )
  const baselineIds = new Set(inventory.behaviorBaselines.map(baseline => baseline.id))
  for (const contract of inventory.observableBehaviors) {
    assert.ok(REQUIRED_BEHAVIOR_KINDS.includes(contract.kind))
    assert.ok(contract.currentFacts.length > 0)
    assert.ok(contract.targetFacts.length > 0)
    assert.ok(contract.behaviorBaselineIds.length > 0)
    assert.ok(contract.behaviorBaselineIds.every(id => baselineIds.has(id)))
  }
})

test('inventory paths are repository-relative and stay inside declared roots', async () => {
  const inventory = await json('docs/decisions/0028-control-plane-worker-migration.inventory.json')
  for (const path of inventory.surfaces.flatMap(surface => surface.sourcePaths)) {
    assert.equal(path.includes('\\'), false)
    assert.equal(path.startsWith('/'), false)
    assert.equal(path.split('/').includes('..'), false)
    assert.ok(TARGET_SOURCE_ROOTS.some(sourceRoot => (
      path === sourceRoot || path.startsWith(`${sourceRoot}/`)
    )), path)
  }
})
