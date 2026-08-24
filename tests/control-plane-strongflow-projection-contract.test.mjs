import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-strongflow-projection.rules.json',
)
const contractPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-strongflow-projection.md',
)

const REQUIRED_RULE_IDS = Object.freeze([
  'boundary.raw_transport_facts_cannot_construct',
  'boundary.websocket_uses_committed_cursor_only',
  'composition.delivery_projection_owned_by_delivery',
  'composition.generated_api_dto_only',
  'dependency.delivery_never_imports_api',
  'freshness.stale_foreign_or_raced_fails_closed',
  'gate.publication_adapter_required',
  'gate.runtime_adapter_required',
  'publication.current_approved_passing_set_only',
  'read.delivery_runtime_share_bounded_cursor',
  'read.replay_is_bounded_and_deterministic',
  'security.public_projection_is_redacted',
].sort())

const REQUIRED_RISK_IDS = Object.freeze([
  'delivery-runtime-torn-read',
  'generic-state-loader-escape',
  'publication-summary-underbound',
  'runtime-ledger-authority-missing',
  'transport-storage-bypass',
  'websocket-cursor-not-durable',
].sort())

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(relativePath) {
  assert.equal(relativePath.startsWith('/'), false, `${relativePath} must be repository-relative`)
  assert.equal(relativePath.split('/').includes('..'), false, `${relativePath} must not escape`)
  return join(root, relativePath)
}

function cargoPackages() {
  const result = spawnSync(
    'cargo',
    ['metadata', '--format-version', '1', '--locked', '--no-deps'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(result.status, 0, result.stderr)
  return new Map(JSON.parse(result.stdout).packages.map(package_ => [package_.name, package_]))
}

function dependencyNames(package_) {
  return new Set(package_.dependencies.map(dependency => dependency.name))
}

function escaped(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')
}

test('Control Plane StrongFlow rules freeze ownership, one read cut, and closed adapters', () => {
  const rules = json(rulesPath)
  assert.equal(
    rules.schemaVersion,
    'winwincode.control-plane-strongflow-projection-rules.v1',
  )
  assert.equal(rules.issueId, 'winwincode-9c4.16.2.5.6')
  assert.equal(rules.preflightIssueId, 'winwincode-9c4.16.2.5.6.1')
  assert.equal(rules.owner, 'winwincode-control-plane')
  assert.equal(rules.trigger, 'crates/winwincode-control-plane/src/strongflow_projection.rs')
  assert.deepEqual(rules.apiContract, {
    sourceOfTruth: 'schema/winwincode/v1/control-plane-http.schema.json',
    compatibility: 'breaking-canonical-revision',
    migration: 'regenerate-rust-and-typescript-together-no-alias',
    consumers: [
      'rust-control-plane',
      'typescript-generated-client',
      'strongflow-web',
      'installed-cli',
    ],
  })

  assert.deepEqual(rules.composition, {
    deliveryInput: 'winwincode-delivery-internal-projection',
    output: 'winwincode-api-generated-dto',
    rawDomainEntityOutput: 'forbidden',
    rawTransportFactInput: 'forbidden',
  })
  assert.deepEqual(rules.readCut.requiredCoordinates, [
    'deliveryId',
    'deliveryRevision',
    'runtimeProjectionCursor',
    'publicationRevision',
  ])
  assert.equal(rules.readCut.deliveryQuery, 'delivery.get')
  assert.equal(rules.readCut.runtimeQuery, 'runtime.projection.get')
  assert.equal(rules.readCut.mismatchPolicy, 'reject-and-reload')
  assert.equal(rules.readCut.limitPolicy, 'server-bounded-no-unbounded-ledger-read')

  assert.deepEqual(rules.publication.requiredCurrentFacts, [
    'delivery',
    'deliverySpecRevision',
    'candidate',
    'passingVerdict',
    'humanApproval',
    'publicationTarget',
  ])
  assert.equal(rules.publication.mismatchPolicy, 'omit-nothing-and-fail-closed')
  assert.equal(rules.productionGate.missingTrustedAdapterPolicy, 'trusted-facts-unavailable')
  assert.deepEqual(rules.implementationGate.transportAdapters, [
    {
      kind: 'http',
      path: 'crates/winwincode-control-plane/src/http.rs',
      requiredGeneratedTypes: ['QueryRequest', 'QueryResultResponse'],
      forbiddenTypes: [
        'ProductStateStorage',
        'StateChange',
        'StoredState',
        'RuntimeEventMessage',
        'ExecutionPortMessage',
      ],
    },
    {
      kind: 'websocket',
      path: 'crates/winwincode-control-plane/src/websocket.rs',
      requiredGeneratedTypes: [
        'ControlPlaneWebSocketEventFrame',
        'ControlPlaneWebSocketServerFrame',
      ],
      forbiddenTypes: [
        'ProductStateStorage',
        'StateChange',
        'StoredState',
        'RuntimeEventMessage',
        'ExecutionPortMessage',
      ],
    },
  ])

  const ruleIds = rules.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), REQUIRED_RULE_IDS)
  for (const rule of rules.rules) {
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    assert.ok(['black-box', 'static'].includes(rule.verification.kind), rule.id)
    for (const ref of rule.refs) assert.equal(existsSync(repositoryPath(ref)), true, ref)
    if (rule.verification.kind === 'black-box') {
      assert.match(rule.verification.testName, /^[a-z][a-z0-9_]+$/u)
    } else {
      assert.ok(rule.verification.gate.length > 0, rule.id)
    }
  }

  const riskIds = rules.p0Risks.map(risk => risk.id)
  assert.equal(new Set(riskIds).size, riskIds.length)
  assert.deepEqual([...riskIds].sort(), REQUIRED_RISK_IDS)
  for (const risk of rules.p0Risks) {
    assert.ok(risk.current.length > 0, risk.id)
    assert.ok(risk.consequence.length > 0, risk.id)
    assert.ok(risk.requiredClosure.length > 0, risk.id)
    assert.ok(risk.refs.length > 0, risk.id)
    for (const ref of risk.refs) assert.equal(existsSync(repositoryPath(ref)), true, ref)
  }
  assert.deepEqual(rules.implementationOrder.map(step => step.id), [
    'delivery-projection',
    'runtime-ledger-projection',
    'generated-api-cursor',
    'publication-authority',
    'control-plane-composition',
    'http-websocket-adapters',
  ])
})

test('current crate dependencies preserve Delivery ownership and generated API mapping', () => {
  const rules = json(rulesPath)
  const packages = cargoPackages()
  const controlPlane = packages.get('winwincode-control-plane')
  const delivery = packages.get('winwincode-delivery')
  assert.ok(controlPlane)
  assert.ok(delivery)

  const controlPlaneDependencies = dependencyNames(controlPlane)
  for (const dependency of rules.implementationGate.controlPlaneRequiredDependencies) {
    assert.equal(controlPlaneDependencies.has(dependency), true, dependency)
  }
  const deliveryDependencies = dependencyNames(delivery)
  for (const dependency of rules.implementationGate.deliveryForbiddenDependencies) {
    assert.equal(deliveryDependencies.has(dependency), false, dependency)
  }
})

test('plain-language contract states the concrete read, publication, and adapter outcomes', () => {
  const contract = readFileSync(contractPath, 'utf8')
  for (const phrase of [
    'Delivery 模块先生成内部投影',
    '同一个有上限的读取截面',
    '不能把两次独立的最新读取拼在一起',
    '发布摘要必须同时匹配当前 Delivery、候选、通过结论、人工批准和目标',
    '缺少可信运行台账或发布 adapter 时，生产查询返回可信事实不可用',
    'HTTP 和 WebSocket 输入不能构造领域投影、运行事实或发布事实',
    'Delivery crate 不依赖 winwincode-api',
  ]) assert.equal(contract.includes(phrase), true, phrase)
})

test('implementation trigger activates public-seam and black-box behavior gates', () => {
  const rules = json(rulesPath)
  const triggerPath = repositoryPath(rules.trigger)
  for (const adapter of rules.implementationGate.transportAdapters) {
    const adapterPath = repositoryPath(adapter.path)
    if (!existsSync(adapterPath)) continue
    assert.equal(existsSync(triggerPath), true, `${adapter.kind} adapter preceded projection owner`)
    const source = readFileSync(adapterPath, 'utf8')
    assert.match(source, /\bStrongFlowProjectionQueryPort\b/u, adapter.path)
    assert.match(source, /winwincode_api::generated/u, adapter.path)
    for (const typeName of adapter.requiredGeneratedTypes) {
      assert.match(source, new RegExp(`\\b${escaped(typeName)}\\b`, 'u'), typeName)
    }
    for (const typeName of adapter.forbiddenTypes) {
      assert.doesNotMatch(source, new RegExp(`\\b${escaped(typeName)}\\b`, 'u'), typeName)
    }
  }
  if (!existsSync(triggerPath)) {
    assert.equal(rules.implementationGate.absentTriggerStatus, 'planned-not-implemented')
    return
  }

  const moduleSource = readFileSync(triggerPath, 'utf8')
  const librarySource = readFileSync(
    repositoryPath(rules.implementationGate.libraryPath),
    'utf8',
  )
  assert.match(librarySource, /\bpub\s+mod\s+strongflow_projection\s*;/u)

  for (const symbol of rules.implementationGate.publicSeam.exports) {
    assert.match(
      moduleSource,
      new RegExp(`\\bpub\\s+${escaped(symbol.kind)}\\s+${escaped(symbol.name)}\\b`, 'u'),
      `${symbol.kind} ${symbol.name} is not public`,
    )
  }
  for (const method of rules.implementationGate.publicSeam.methods) {
    assert.match(moduleSource, new RegExp(`\\bfn\\s+${escaped(method)}\\s*\\(`, 'u'), method)
  }
  assert.match(moduleSource, /winwincode_api::generated/u)
  assert.match(moduleSource, /winwincode_delivery::projection/u)
  for (const typeName of rules.implementationGate.publicSeam.generatedTypes) {
    assert.match(moduleSource, new RegExp(`\\b${escaped(typeName)}\\b`, 'u'), typeName)
  }

  for (const typeName of rules.implementationGate.forbiddenRawInputTypes) {
    assert.doesNotMatch(
      moduleSource,
      new RegExp(`\\b${escaped(typeName)}\\b`, 'u'),
      `projection module consumes raw ${typeName}`,
    )
  }
  for (const field of rules.implementationGate.forbiddenPublicFields) {
    const identifier = escaped(field)
    assert.doesNotMatch(moduleSource, new RegExp(`\\bpub\\s+${identifier}\\s*:`, 'u'), field)
    assert.doesNotMatch(
      moduleSource,
      new RegExp(`serde\\s*\\(\\s*rename\\s*=\\s*["']${identifier}["']`, 'u'),
      field,
    )
  }
  assert.doesNotMatch(moduleSource, /\bpub\s+fn\s+(?:from_raw|from_snapshot|from_worker|new_unchecked)\b/u)

  const blackBox = rules.implementationGate.blackBoxTest
  const blackBoxPath = repositoryPath(blackBox.path)
  assert.equal(existsSync(blackBoxPath), true, blackBox.path)
  const blackBoxSource = readFileSync(blackBoxPath, 'utf8')
  for (const fragment of blackBox.requiredSourceFragments) {
    assert.equal(blackBoxSource.includes(fragment), true, fragment)
  }

  const expectedNames = rules.rules
    .filter(rule => rule.verification.kind === 'black-box')
    .map(rule => rule.verification.testName)
    .sort()
  assert.deepEqual([...new Set(expectedNames)].sort(), [...blackBox.requiredTestNames].sort())

  const result = spawnSync(
    'cargo',
    [
      'test',
      '-p',
      'winwincode-control-plane',
      '--test',
      'strongflow_projection',
      '--locked',
      '--',
      '--test-threads=1',
    ],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`)
  const output = `${result.stdout}\n${result.stderr}`
  for (const testName of expectedNames) {
    assert.match(output, new RegExp(`test ${escaped(testName)} \\.\\.\\. ok`, 'u'), testName)
  }
})
