import assert from 'node:assert/strict'
import { readFile, readdir } from 'node:fs/promises'
import { dirname, join, relative, resolve, sep } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const inventoryPath = join(
  root,
  'docs/decisions/0028-control-plane-worker-migration.inventory.json',
)

const SOURCE_ROOTS = Object.freeze([
  'apps/host/src',
  'packages/contracts/src',
  'packages/dsh-profile/src',
  'packages/native/src',
  'packages/strongflow/src',
  'crates/kernel/src',
  'crates/native/src',
])
const PACKAGE_MANIFESTS = Object.freeze([
  'package.json',
  'apps/host/package.json',
  'packages/dsh-profile/package.json',
  'packages/strongflow/package.json',
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
  'chat',
  'strongflow',
  'cli',
  'test',
  'release',
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
const DISPOSITIONS = new Set([
  'delete',
  'presentation',
  'retain',
  'split',
  'translate',
])
const ADAPTER_REMOVAL_TASK_BY_PHASE = Object.freeze({
  5: 'winwincode-9c4.16.5.7',
  6: 'winwincode-9c4.16.6.6',
})

async function json(path) {
  return JSON.parse(await readFile(join(root, path), 'utf8'))
}

async function filesBelow(path) {
  const directory = join(root, path)
  const entries = await readdir(directory, { withFileTypes: true })
  const files = []
  for (const entry of entries) {
    const absolute = join(directory, entry.name)
    if (entry.isDirectory()) {
      files.push(...await filesBelow(relative(root, absolute)))
    } else if (entry.isFile() && /\.(?:rs|ts)$/u.test(entry.name)) {
      files.push(relative(root, absolute).split(sep).join('/'))
    }
  }
  return files
}

function duplicate(values) {
  return values.find((value, index) => values.indexOf(value) !== index)
}

test('migration inventory assigns every current backend source to one target owner', async () => {
  const inventory = await json(relative(root, inventoryPath))

  assert.equal(inventory.schemaVersion, 1)
  assert.deepEqual(inventory.architecture, {
    presentation: 'typescript-web',
    businessAuthority: 'control-plane',
    executionCoordinator: 'execution-worker',
    executionAuthority: 'codex-core',
    executionSeam: 'execution-port',
    localDeployment: 'same-process-modules',
    enterpriseDeployment: 'separate-processes',
  })
  assert.deepEqual(inventory.sourceRoots, SOURCE_ROOTS)
  assert.ok(Array.isArray(inventory.surfaces) && inventory.surfaces.length > 0)

  const surfaceIds = inventory.surfaces.map(surface => surface.id)
  assert.equal(duplicate(surfaceIds), undefined)
  const listedFiles = inventory.surfaces.flatMap(surface => surface.sourcePaths)
  assert.equal(duplicate(listedFiles), undefined)

  for (const surface of inventory.surfaces) {
    assert.match(surface.id, /^[a-z][a-z0-9-]+$/u)
    assert.ok(Array.isArray(surface.sourcePaths) && surface.sourcePaths.length > 0)
    assert.ok(Array.isArray(surface.entrySymbols) && surface.entrySymbols.length > 0)
    assert.ok(Array.isArray(surface.targetOwners) && surface.targetOwners.length > 0)
    assert.ok(surface.targetOwners.every(owner => TARGET_OWNERS.has(owner)))
    assert.ok(Array.isArray(surface.targetModules) && surface.targetModules.length > 0)
    assert.match(surface.phase, /^[1-6]$/u)
    assert.ok(DISPOSITIONS.has(surface.disposition))
    assert.ok(Array.isArray(surface.observableContracts)
      && surface.observableContracts.length > 0)
    assert.ok(Array.isArray(surface.behaviorBaselineIds)
      && surface.behaviorBaselineIds.length > 0)

    const source = (await Promise.all(surface.sourcePaths.map(path => (
      readFile(join(root, path), 'utf8')
    )))).join('\n')
    for (const symbol of surface.entrySymbols) {
      assert.ok(source.includes(symbol), `${surface.id} is missing entry symbol ${symbol}`)
    }
  }

  const actualFiles = (await Promise.all(SOURCE_ROOTS.map(filesBelow))).flat().sort()
  assert.deepEqual([...listedFiles].sort(), actualFiles)
})

test('migration inventory covers every declared DeepSeek dependency', async () => {
  const inventory = await json(relative(root, inventoryPath))
  const actualPackages = new Set()
  for (const path of PACKAGE_MANIFESTS) {
    const manifest = await json(path)
    for (const section of [
      'dependencies',
      'devDependencies',
      'optionalDependencies',
      'peerDependencies',
    ]) {
      for (const name of Object.keys(manifest[section] ?? {})) {
        if (name.startsWith('@deepseek-ai/')) actualPackages.add(name)
      }
    }
  }

  const listedPackages = inventory.upstreamPackages.map(entry => entry.name)
  assert.equal(duplicate(listedPackages), undefined)
  assert.deepEqual([...listedPackages].sort(), [...actualPackages].sort())
  for (const entry of inventory.upstreamPackages) {
    assert.match(entry.phase, /^[1-6]$/u)
    assert.ok(DISPOSITIONS.has(entry.disposition))
    assert.ok(entry.target.length > 0)
  }
})

test('migration behavior baselines pin success, failure, cancel, recovery, approval, and close', async () => {
  const inventory = await json(relative(root, inventoryPath))
  const baselineIds = inventory.behaviorBaselines.map(baseline => baseline.id)
  assert.equal(duplicate(baselineIds), undefined)
  assert.deepEqual(
    [...new Set(inventory.behaviorBaselines.map(baseline => baseline.scenario))].sort(),
    [...REQUIRED_SCENARIOS].sort(),
  )

  for (const baseline of inventory.behaviorBaselines) {
    assert.ok(REQUIRED_SCENARIOS.includes(baseline.scenario))
    assert.ok(Array.isArray(baseline.expectedFacts) && baseline.expectedFacts.length > 0)
    const testSource = await readFile(join(root, baseline.testFile), 'utf8')
    assert.ok(
      testSource.includes(baseline.testName),
      `${baseline.id} test name is missing from ${baseline.testFile}`,
    )
  }

  const referencedBaselineIds = new Set(
    inventory.surfaces.flatMap(surface => surface.behaviorBaselineIds),
  )
  assert.deepEqual([...referencedBaselineIds].sort(), [...baselineIds].sort())
})

test('Chat, StrongFlow, CLI, tests, and release callers are tied to observable entry points', async () => {
  const inventory = await json(relative(root, inventoryPath))
  const callerIds = inventory.callers.map(caller => caller.id)
  assert.equal(duplicate(callerIds), undefined)
  assert.deepEqual(
    [...new Set(inventory.callers.map(caller => caller.kind))].sort(),
    [...REQUIRED_CALLER_KINDS].sort(),
  )

  const surfaceById = new Map(inventory.surfaces.map(surface => [surface.id, surface]))
  const baselineIds = new Set(inventory.behaviorBaselines.map(baseline => baseline.id))
  for (const caller of inventory.callers) {
    assert.ok(REQUIRED_CALLER_KINDS.includes(caller.kind))
    assert.ok(caller.sourcePaths.length > 0)
    assert.ok(caller.currentEntryPoints.length > 0)
    assert.ok(caller.targetOwners.length > 0)
    assert.ok(caller.targetOwners.every(owner => TARGET_OWNERS.has(owner)))
    assert.ok(caller.surfaceIds.length > 0)
    assert.ok(caller.surfaceIds.every(id => surfaceById.has(id)))
    assert.ok(caller.behaviorBaselineIds.length > 0)
    assert.ok(caller.behaviorBaselineIds.every(id => baselineIds.has(id)))

    const source = (await Promise.all(caller.sourcePaths.map(path => (
      readFile(join(root, path), 'utf8')
    )))).join('\n')
    for (const entryPoint of caller.currentEntryPoints) {
      assert.ok(
        source.includes(entryPoint),
        `${caller.id} is missing current entry point ${entryPoint}`,
      )
    }
  }
})

test('persistence, event, and error observations are frozen independently', async () => {
  const inventory = await json(relative(root, inventoryPath))
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

test('every migration adapter and removal has one explicit deletion gate', async () => {
  const inventory = await json(relative(root, inventoryPath))
  assert.ok(inventory.temporaryAdapters.length > 0)
  assert.ok(inventory.removedCapabilities.length > 0)

  for (const adapter of inventory.temporaryAdapters) {
    assert.ok(Object.hasOwn(ADAPTER_REMOVAL_TASK_BY_PHASE, adapter.removeInPhase))
    assert.equal(
      adapter.removalTask,
      ADAPTER_REMOVAL_TASK_BY_PHASE[adapter.removeInPhase],
    )
    assert.ok(adapter.allowedCallers.length > 0)
  }
  for (const capability of inventory.removedCapabilities) {
    assert.equal(capability.removeInPhase, '6')
    assert.equal(capability.removalTask, 'winwincode-9c4.16.6.6')
    assert.ok(capability.reason.length > 0)
  }

  const deletionPaths = new Set(
    inventory.surfaces
      .filter(surface => surface.disposition === 'delete')
      .flatMap(surface => surface.sourcePaths),
  )
  assert.ok(deletionPaths.has('packages/native/src/index.ts'))
  assert.ok(deletionPaths.has('crates/native/src/lib.rs'))
})

test('inventory paths are repository-relative and stay inside their declared roots', async () => {
  const inventory = await json(relative(root, inventoryPath))
  for (const path of inventory.surfaces.flatMap(surface => surface.sourcePaths)) {
    assert.equal(path.includes('\\'), false)
    assert.equal(path.startsWith('/'), false)
    assert.equal(path.split('/').includes('..'), false)
    assert.ok(SOURCE_ROOTS.some(sourceRoot => (
      path === sourceRoot || path.startsWith(`${sourceRoot}/`)
    )))
  }
  assert.equal(dirname(inventoryPath), join(root, 'docs/decisions'))
})
