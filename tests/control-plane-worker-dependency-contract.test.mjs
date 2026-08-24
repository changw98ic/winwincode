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
  controlPlaneForbiddenPackagePatterns: ['codex-*', 'winwincode-codex'],
  workerRoots: ['winwincode-worker'],
  workerForbiddenPackages: [
    'winwincode-approval',
    'winwincode-audit',
    'winwincode-collaboration',
    'winwincode-control-plane',
    'winwincode-credential',
    'winwincode-delivery',
    'winwincode-github',
    'winwincode-identity',
    'winwincode-project',
    'winwincode-provider',
    'winwincode-publication',
    'winwincode-session',
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
})

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

test('target graph declares the accepted modules without claiming planned crates exist', () => {
  const graph = json(targetGraphPath)
  assert.equal(graph.schemaVersion, 1)
  assert.equal(graph.status, 'accepted-target')
  assert.equal(graph.decision, 'docs/decisions/0028-control-plane-worker-migration.md')
  assert.deepEqual(graph.verification, {
    plannedModulesMayBeAbsent: true,
    enforceCargoDependenciesWhenManifestExists: true,
    implementationCompletionSource: 'beads-and-release-gates',
  })

  const nodeIds = graph.nodes.map(node => node.id)
  const nodePaths = graph.nodes.map(node => node.path)
  assert.equal(duplicate(nodeIds), undefined)
  assert.equal(duplicate(nodePaths), undefined)
  assert.ok(graph.nodes.some(node => node.id === 'typescript-web'))
  assert.ok(graph.nodes.some(node => node.id === 'winwincode-control-plane'))
  assert.ok(graph.nodes.some(node => node.id === 'winwincode-worker'))
  assert.ok(graph.nodes.some(node => node.id === 'winwincode-codex'))

  for (const node of graph.nodes) {
    assert.match(node.id, /^[a-z][a-z0-9-]+$/u)
    assert.match(node.phase, /^(?:[1-6]|enterprise)$/u)
    assert.ok(['generated', 'rust-crate', 'schema', 'typescript-app'].includes(node.kind))
    assert.ok(node.responsibilities.length > 0)
    assert.equal(duplicate(node.allowedInternalDependencies), undefined)
    for (const dependency of node.allowedInternalDependencies) {
      assert.ok(nodeIds.includes(dependency), `${node.id} has unknown dependency ${dependency}`)
    }

    if (node.kind === 'rust-crate') {
      assert.equal(node.packageName, node.id)
      assert.equal(node.path, `crates/${node.id}`)
    } else {
      assert.equal(Object.hasOwn(node, 'packageName'), false)
    }
  }
})

test('planned dependency closure enforces the Control Plane, Worker, Web, and local seams', () => {
  const graph = json(targetGraphPath)
  assert.deepEqual(graph.guardrails, REQUIRED_GUARDRAILS)

  const dependenciesById = new Map(graph.nodes.map(node => (
    [node.id, node.allowedInternalDependencies]
  )))
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
      `Worker target reaches Control Plane business package ${dependency}`,
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
    owner: 'winwincode-provider',
    credentialOwner: 'winwincode-credential',
    workerInterface: 'execution-port-model-stream',
    longLivedCredentialConsumers: ['winwincode-provider'],
  })
})

test('every migration inventory surface has one matching phase and target node', () => {
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
    assert.match(phase.phase, /^[1-6]$/u)
    const expectedIds = inventory.surfaces
      .filter(surface => surface.phase === phase.phase)
      .map(surface => surface.id)
      .sort()
    assert.deepEqual([...phase.surfaceIds].sort(), expectedIds)
  }

  for (const surface of inventory.surfaces) {
    for (const targetPath of surface.targetModules) {
      assert.ok(
        graphPaths.has(targetPath),
        `${surface.id} target module is missing from target graph: ${targetPath}`,
      )
    }
  }
})

test('existing target Cargo manifests obey the declared dependency graph', () => {
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

  for (const node of graph.nodes.filter(node => node.kind === 'rust-crate')) {
    const manifestExists = existsSync(join(root, node.path, 'Cargo.toml'))
    const package_ = packageByName.get(node.packageName)
    if (!manifestExists) {
      assert.equal(package_, undefined)
      continue
    }
    assert.ok(package_, `${node.path}/Cargo.toml exists but is not a workspace package`)
    const dependencies = package_.dependencies.map(dependency => dependency.name)
    actualDependencies.set(node.id, dependencies)
    for (const dependency of dependencies) {
      if (dependency.startsWith('winwincode-')) {
        assert.ok(
          plannedRustPackages.has(dependency),
          `${node.id} depends on unplanned product package ${dependency}`,
        )
        assert.ok(
          node.allowedInternalDependencies.includes(dependency),
          `${node.id} has forbidden product dependency ${dependency}`,
        )
      }
    }
  }

  const actualControlPlaneClosure = transitiveDependencies(
    graph.nodes
      .filter(node => node.zone === 'control-plane' && actualDependencies.has(node.id))
      .map(node => node.id),
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
})

test('existing Web and local launcher sources cannot bypass their declared owners', () => {
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
  if (existsSync(join(root, local.path, 'Cargo.toml'))) {
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
    const productDependencies = package_.dependencies
      .map(dependency => dependency.name)
      .filter(name => name.startsWith('winwincode-'))
      .sort()
    assert.deepEqual(
      productDependencies,
      [...graph.guardrails.localLauncherAllowedProductDependencies].sort(),
    )
  }
})

test('dependency rules explain what is planned, what is checked now, and what fails later', () => {
  const text = readFileSync(dependencyRulesPath, 'utf8')
  for (const requiredStatement of [
    '目标声明，不是完成声明',
    'Control Plane 不得依赖 Codex Core',
    'Worker 不得依赖产品业务模块',
    'Web 只能访问 Control Plane',
    '本地启动器只负责组装',
    'Provider Gateway 是长期模型凭据的唯一使用者',
    '阶段 2 到阶段 6',
    '`cargo metadata`',
  ]) assert.ok(text.includes(requiredStatement), `missing rule: ${requiredStatement}`)
})
