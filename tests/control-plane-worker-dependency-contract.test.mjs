import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  existsSync,
  readFileSync,
  readdirSync,
} from 'node:fs'
import { extname, join, relative, resolve, sep } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const targetGraphPath = join(
  root,
  'docs/decisions/0028-control-plane-worker-target-graph.json',
)
const dependencyRulesPath = join(
  root,
  'docs/decisions/0028-control-plane-worker-dependency-rules.md',
)
const inventoryPath = join(
  root,
  'docs/decisions/0028-control-plane-worker-migration.inventory.json',
)

const REQUIRED_GUARDRAILS = Object.freeze({
  controlPlaneRoots: ['winwincode-control-plane'],
  controlPlaneForbiddenPackagePatterns: [
    'codex-*',
    'winwincode-codex',
    'winwincode-kernel',
  ],
  workerRoots: ['winwincode-worker'],
  workerForbiddenPackages: [
    'winwincode-api',
    'winwincode-audit',
    'winwincode-control-plane',
    'winwincode-delivery',
    'winwincode-publication',
    'winwincode-session',
    'winwincode-storage',
  ],
  webAllowedBackends: ['control-plane-http', 'control-plane-websocket'],
  webForbiddenBackends: ['execution-port', 'execution-worker'],
  webNetworkOwner: 'typescript-generated-client',
  localLauncher: 'winwincode-local',
  localLauncherAllowedProductDependencies: [
    'winwincode-control-plane',
    'winwincode-observability',
    'winwincode-worker',
  ],
  serverEntrypoint: 'winwincode-server',
  serverAllowedProductDependencies: [
    'winwincode-api',
    'winwincode-codex',
    'winwincode-control-plane',
    'winwincode-domain',
    'winwincode-execution-port',
    'winwincode-local',
    'winwincode-storage',
    'winwincode-worker',
  ],
  helperExecutable: 'winwincode-kernel-helper',
})

const CANONICAL_PATHS = Object.freeze([
  'apps/client',
  'crates/helper',
  'crates/kernel',
  'crates/winwincode-server',
  'crates/winwincode-control-plane',
  'crates/winwincode-worker',
  'crates/winwincode-local',
])

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function duplicate(values) {
  return values.find((value, index) => values.indexOf(value) !== index)
}

function transitiveDependencies(rootIds, dependenciesById) {
  const visited = new Set()
  const pending = [...rootIds]
  while (pending.length > 0) {
    const id = pending.pop()
    if (visited.has(id)) continue
    visited.add(id)
    pending.push(...(dependenciesById.get(id) ?? []))
  }
  return visited
}

function matchesPackagePattern(packageName, pattern) {
  if (pattern.endsWith('*')) return packageName.startsWith(pattern.slice(0, -1))
  return packageName === pattern
}

function filesBelow(path) {
  if (!existsSync(path)) return []
  const files = []
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    if (entry.isDirectory() && ['dist', 'node_modules'].includes(entry.name)) continue
    const entryPath = join(path, entry.name)
    if (entry.isDirectory()) files.push(...filesBelow(entryPath))
    else if (entry.isFile()) files.push(entryPath)
  }
  return files
}

function productDependencies(package_) {
  return package_.dependencies
    .filter(dependency => dependency.kind !== 'dev')
    .map(dependency => dependency.name)
    .filter(name => name.startsWith('winwincode-'))
}

test('target graph declares the accepted single path and existing directories', () => {
  const graph = json(targetGraphPath)
  assert.equal(graph.schemaVersion, 1)
  assert.equal(graph.status, 'accepted-target')
  assert.equal(graph.decision, 'docs/decisions/0028-control-plane-worker-migration.md')
  assert.deepEqual(graph.verification, {
    plannedModulesMayBeAbsent: false,
    enforceCargoDependenciesWhenManifestExists: true,
    implementationCompletionSource: 'beads-and-release-gates',
  })

  const nodeIds = graph.nodes.map(node => node.id)
  const nodePaths = graph.nodes.map(node => node.path)
  assert.equal(duplicate(nodeIds), undefined)
  assert.equal(duplicate(nodePaths), undefined)
  for (const path of CANONICAL_PATHS) assert.ok(existsSync(join(root, path)), path)
  for (const id of [
    'typescript-web',
    'winwincode-server',
    'winwincode-control-plane',
    'winwincode-worker',
    'winwincode-local',
    'winwincode-kernel-helper',
  ]) assert.ok(nodeIds.includes(id), id)

  for (const node of graph.nodes) {
    assert.match(node.id, /^[a-z][a-z0-9-]+$/u)
    assert.match(node.phase, /^(?:[1-6]|enterprise)$/u)
    assert.ok(['generated', 'rust-crate', 'schema', 'typescript-app', 'typescript-package'].includes(node.kind))
    assert.ok(node.responsibilities.length > 0)
    assert.equal(duplicate(node.allowedInternalDependencies), undefined)
    assert.ok(existsSync(join(root, node.path)), `${node.id} path is missing`)
    for (const dependency of node.allowedInternalDependencies) {
      assert.ok(nodeIds.includes(dependency), `${node.id} has unknown dependency ${dependency}`)
    }

    if (node.kind === 'rust-crate') {
      assert.equal(node.packageName, node.id)
      const expectedPath = node.id === 'winwincode-kernel'
        ? 'crates/kernel'
        : node.id === 'winwincode-kernel-helper'
          ? 'crates/helper'
          : `crates/${node.id}`
      assert.equal(node.path, expectedPath)
    } else if (['typescript-app', 'typescript-package'].includes(node.kind)) {
      assert.match(node.packageName, /^@winwincode\/[a-z][a-z0-9-]+$/u)
      const manifest = json(join(root, node.path, 'package.json'))
      assert.equal(manifest.name, node.packageName)
    } else {
      assert.equal(Object.hasOwn(node, 'packageName'), false)
    }
  }

  for (const forbidden of [
    'apps/host',
    'apps/web',
    'packages/dsh-profile',
    'packages/native',
    'crates/native',
    'dsh-profile',
    'napi-kernel-bridge',
  ]) {
    assert.equal(JSON.stringify(graph).includes(forbidden), false, forbidden)
  }
})

test('dependency graph enforces the Control Plane, Server, Worker, Client, Local and Helper seams', () => {
  const graph = json(targetGraphPath)
  assert.deepEqual(graph.guardrails, REQUIRED_GUARDRAILS)

  const dependenciesById = new Map(graph.nodes.map(node => (
    [node.id, node.allowedInternalDependencies]
  )))
  const controlPlane = graph.nodes.find(node => node.id === 'winwincode-control-plane')
  assert.ok(controlPlane)
  assert.ok(controlPlane.allowedInternalDependencies.includes('winwincode-repository-context'))
  const codexAdapter = graph.nodes.find(node => node.id === 'winwincode-codex')
  assert.deepEqual(codexAdapter.allowedInternalDependencies, [
    'winwincode-domain',
    'winwincode-execution-port',
    'winwincode-kernel',
  ])
  const worker = graph.nodes.find(node => node.id === 'winwincode-worker')
  assert.deepEqual(worker.allowedInternalDependencies, [
    'winwincode-codex',
    'winwincode-domain',
    'winwincode-execution-port',
  ])
  const server = graph.nodes.find(node => node.id === 'winwincode-server')
  assert.deepEqual(server.allowedInternalDependencies, [
    'winwincode-api',
    'winwincode-codex',
    'winwincode-control-plane',
    'winwincode-domain',
    'winwincode-execution-port',
    'winwincode-local',
    'winwincode-storage',
    'winwincode-worker',
  ])
  const helper = graph.nodes.find(node => node.id === 'winwincode-kernel-helper')
  assert.deepEqual(helper.allowedInternalDependencies, [])

  const controlPlaneClosure = transitiveDependencies(
    graph.guardrails.controlPlaneRoots,
    dependenciesById,
  )
  for (const dependency of controlPlaneClosure) {
    assert.equal(
      graph.guardrails.controlPlaneForbiddenPackagePatterns.some(pattern => (
        matchesPackagePattern(dependency, pattern)
      )),
      false,
      `Control Plane target reaches forbidden package ${dependency}`,
    )
  }

  const workerClosure = transitiveDependencies(
    graph.guardrails.workerRoots,
    dependenciesById,
  )
  for (const dependency of graph.guardrails.workerForbiddenPackages) {
    assert.equal(
      workerClosure.has(dependency),
      false,
      `Worker target reaches forbidden product package ${dependency}`,
    )
  }

  const web = graph.nodes.find(node => node.id === 'typescript-web')
  assert.deepEqual(web.allowedBackends, graph.guardrails.webAllowedBackends)
  assert.deepEqual(web.allowedInternalDependencies, ['typescript-generated-client'])

  const local = graph.nodes.find(node => node.id === graph.guardrails.localLauncher)
  assert.deepEqual(
    local.allowedInternalDependencies,
    graph.guardrails.localLauncherAllowedProductDependencies,
  )
  assert.deepEqual(local.responsibilities, [
    'load-process-configuration',
    'compose-control-plane',
    'compose-local-worker',
    'start-and-stop-process',
  ])

  assert.deepEqual(graph.providerGateway, {
    owner: 'winwincode-control-plane',
    credentialOwner: 'winwincode-control-plane',
    workerInterface: 'execution-port-model-stream',
    longLivedCredentialConsumers: ['winwincode-control-plane'],
  })
})

test('inventory surfaces map one-to-one to graph phases and current paths', () => {
  const graph = json(targetGraphPath)
  const inventory = json(inventoryPath)
  const graphPaths = new Set(graph.nodes.map(node => node.path))
  const mappedSurfaceIds = graph.migrationPhases.flatMap(phase => phase.surfaceIds)
  assert.equal(duplicate(mappedSurfaceIds), undefined)
  assert.deepEqual(
    [...mappedSurfaceIds].sort(),
    inventory.surfaces.map(surface => surface.id).sort(),
  )

  for (const phase of graph.migrationPhases) {
    assert.match(phase.phase, /^(?:[1-6]|enterprise)$/u)
    const expectedIds = inventory.surfaces
      .filter(surface => surface.phase === phase.phase)
      .map(surface => surface.id)
      .sort()
    assert.deepEqual([...phase.surfaceIds].sort(), expectedIds)
  }

  for (const rootPath of inventory.sourceRoots) {
    assert.ok(existsSync(join(root, rootPath)), rootPath)
  }
  for (const surface of inventory.surfaces) {
    assert.ok(surface.sourcePaths.length > 0, surface.id)
    for (const sourcePath of surface.sourcePaths) {
      assert.ok(existsSync(join(root, sourcePath)), `${surface.id}: ${sourcePath}`)
    }
    for (const targetPath of surface.targetModules) {
      assert.ok(graphPaths.has(targetPath), `${surface.id}: ${targetPath}`)
    }
  }

  assert.deepEqual(inventory.upstreamPackages, [])
  assert.deepEqual(inventory.temporaryAdapters, [])
  assert.deepEqual(inventory.removedCapabilities, [])
  assert.equal(JSON.stringify(inventory).includes('apps/host'), false)
  assert.equal(JSON.stringify(inventory).includes('packages/dsh-profile'), false)
  assert.equal(JSON.stringify(inventory).includes('packages/native'), false)
  assert.equal(JSON.stringify(inventory).includes('crates/native'), false)
})

test('pnpm workspace members and internal dependencies exactly match the target graph', () => {
  const graph = json(targetGraphPath)
  const result = spawnSync(
    'corepack',
    ['pnpm', 'list', '-r', '--depth', '-1', '--json'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(result.status, 0, result.stderr)
  const workspacePackages = JSON.parse(result.stdout)
    .filter(package_ => package_.path !== root)
  const targetPackages = graph.nodes
    .filter(node => ['typescript-app', 'typescript-package'].includes(node.kind))
  assert.deepEqual(
    workspacePackages.map(package_ => relative(root, package_.path)).sort(),
    targetPackages.map(node => node.path).sort(),
  )
  const nodeByPackageName = new Map(targetPackages.map(node => [node.packageName, node]))
  const workspaceNodeIds = new Set(targetPackages.map(node => node.id))
  for (const node of targetPackages) {
    const manifest = json(join(root, node.path, 'package.json'))
    const dependencies = Object.keys(manifest.dependencies ?? {})
      .filter(name => name.startsWith('@winwincode/'))
    assert.deepEqual(
      dependencies.map(name => nodeByPackageName.get(name)?.id ?? name).sort(),
      node.allowedInternalDependencies.filter(id => workspaceNodeIds.has(id)).sort(),
      `${node.id} workspace package dependency list`,
    )
  }
})

test('existing target Cargo manifests obey the declared production dependency graph', () => {
  const graph = json(targetGraphPath)
  const result = spawnSync(
    'cargo',
    ['metadata', '--format-version', '1', '--locked', '--no-deps'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(result.status, 0, result.stderr)

  const metadata = JSON.parse(result.stdout)
  const packageByName = new Map(metadata.packages.map(package_ => [package_.name, package_]))
  const plannedRustPackages = new Set(
    graph.nodes
      .filter(node => node.kind === 'rust-crate')
      .map(node => node.packageName),
  )
  const actualDependencies = new Map()
  const graphRustPackages = graph.nodes
    .filter(node => node.kind === 'rust-crate')
    .map(node => node.packageName)
    .sort()
  const workspaceRustPackages = metadata.packages
    .filter(package_ => package_.manifest_path.startsWith(`${root}/crates/`))
    .map(package_ => package_.name)
    .sort()
  assert.deepEqual(graphRustPackages, workspaceRustPackages)

  for (const node of graph.nodes.filter(node => node.kind === 'rust-crate')) {
    const manifestExists = existsSync(join(root, node.path, 'Cargo.toml'))
    const package_ = packageByName.get(node.packageName)
    assert.equal(manifestExists, true, `${node.path}/Cargo.toml is missing`)
    assert.ok(package_, `${node.path}/Cargo.toml exists but is not a workspace package`)
    const dependencies = productDependencies(package_)
    actualDependencies.set(node.id, dependencies)
    assert.deepEqual(
      [...dependencies].sort(),
      [...node.allowedInternalDependencies].filter(name => name.startsWith('winwincode-')).sort(),
      `${node.id} production dependency list`,
    )
    for (const dependency of dependencies) {
      assert.ok(
        plannedRustPackages.has(dependency),
        `${node.id} depends on unplanned product package ${dependency}`,
      )
    }
  }

  const actualControlPlaneClosure = transitiveDependencies(
    graph.guardrails.controlPlaneRoots.filter(id => actualDependencies.has(id)),
    actualDependencies,
  )
  for (const dependency of actualControlPlaneClosure) {
    assert.equal(
      graph.guardrails.controlPlaneForbiddenPackagePatterns.some(pattern => (
        matchesPackagePattern(dependency, pattern)
      )),
      false,
      `Control Plane manifest reaches forbidden package ${dependency}`,
    )
  }

  const actualWorkerClosure = transitiveDependencies(
    graph.guardrails.workerRoots.filter(id => actualDependencies.has(id)),
    actualDependencies,
  )
  for (const dependency of graph.guardrails.workerForbiddenPackages) {
    assert.equal(actualWorkerClosure.has(dependency), false)
  }

  const actualServerDependencies = actualDependencies.get(graph.guardrails.serverEntrypoint)
  assert.deepEqual(
    [...actualServerDependencies].sort(),
    [...graph.guardrails.serverAllowedProductDependencies].sort(),
  )
})

test('Client and Local sources cannot bypass their declared owners', () => {
  const graph = json(targetGraphPath)
  const web = graph.nodes.find(node => node.id === 'typescript-web')
  const webRoot = join(root, web.path)
  if (existsSync(webRoot)) {
    const sourceFiles = filesBelow(webRoot).filter(path => (
      ['.js', '.jsx', '.mjs', '.ts', '.tsx'].includes(extname(path))
    ))
    for (const path of sourceFiles) {
      const source = readFileSync(path, 'utf8')
      assert.doesNotMatch(source, /(?:execution-port|winwincode-worker)/u)
      if (!relative(webRoot, path).split(sep).includes('generated')) {
        assert.doesNotMatch(source, /\bfetch\s*\(/u, `${relative(root, path)} bypasses the generated client`)
        assert.doesNotMatch(source, /\bnew\s+WebSocket\s*\(/u, `${relative(root, path)} bypasses the generated client`)
      }
    }
  }

  const local = graph.nodes.find(node => node.id === graph.guardrails.localLauncher)
  const metadata = spawnSync(
    'cargo',
    ['metadata', '--format-version', '1', '--locked', '--no-deps'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(metadata.status, 0, metadata.stderr)
  const package_ = JSON.parse(metadata.stdout).packages.find(entry => (
    entry.name === local.packageName
  ))
  assert.ok(package_)
  assert.deepEqual(
    productDependencies(package_).sort(),
    [...graph.guardrails.localLauncherAllowedProductDependencies].sort(),
  )
})

test('dependency rules document the current single path and exact checks', () => {
  const text = readFileSync(dependencyRulesPath, 'utf8')
  for (const requiredStatement of [
    '已接受的单一路径合同',
    'Control Plane 不得到达 Codex 执行模块',
    'Worker 只持有执行闭包',
    'Client 只能访问 Server',
    'Local 只负责组装',
    'Provider Gateway 和 Credential',
    '`winwincode-kernel-helper`',
    '`cargo metadata --locked`',
    '不为旧入口或临时适配器留下允许边',
  ]) assert.ok(text.includes(requiredStatement), `missing rule: ${requiredStatement}`)
})
