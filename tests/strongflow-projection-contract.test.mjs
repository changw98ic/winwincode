import assert from 'node:assert/strict'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { extname, join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(root, 'docs', 'contracts', 'strongflow-projection.rules.json')
const contractPath = join(root, 'docs', 'contracts', 'strongflow-projection.md')
const domainSchemaPath = join(root, 'schema', 'winwincode', 'v1', 'domain.schema.json')
const httpSchemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'control-plane-http.schema.json',
)
const eventSchemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'control-plane-events.schema.json',
)
const executionPortSchemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'execution-port.schema.json',
)

const REQUIRED_SECTION_IDS = Object.freeze([
  'agent-graph',
  'attention',
  'command-test-activity',
  'delivery-tasks',
  'evidence',
  'publication',
  'requirements',
  'solution',
  'stages',
  'usage',
  'verdict',
].sort())

const REQUIRED_RULE_IDS = Object.freeze([
  'authority.projection_is_read_only',
  'boundary.web_generated_contracts_only',
  'boundary.web_never_connects_to_worker',
  'boundary.websocket_never_writes_business_state',
  'ordering.deterministic_projection',
  'publication.current_delivery_candidate_verdict',
  'recovery.live_and_replay_equal',
  'recovery.websocket_reset_reloads_snapshot',
  'runtime.contiguous_session_sequence',
  'runtime.exact_session_binding',
  'runtime.reject_stale_lease_attempt',
  'runtime.reject_unbound_or_ambiguous_event',
  'security.frozen_diff_requires_finished_candidate',
  'security.live_diff_is_summary_only',
  'security.no_logs_tool_payloads_or_credentials',
].sort())

const REQUIRED_FINDING_IDS = Object.freeze([
  'codegen.generated_runtime_client_missing',
  'execution_port.codex_binding_not_guaranteed',
  'http.delivery_detail_projection_missing',
  'http.publication_join_missing',
  'http.structured_runtime_projection_missing',
  'http.solution_projection_missing',
  'websocket.structured_runtime_delta_missing',
].sort())

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(relativePath) {
  assert.equal(relativePath.startsWith('/'), false, `${relativePath} must be repository-relative`)
  assert.equal(relativePath.split('/').includes('..'), false, `${relativePath} must stay in the repository`)
  return join(root, relativePath)
}

function assertPublicSymbol(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.match(
    source,
    new RegExp(`export\\s+(?:const|type|class|interface|function)\\s+${mapping.name}\\b`, 'u'),
    `${mapping.path} does not export ${mapping.name}`,
  )
}

function assertTestCase(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.equal(
    source.includes(`test('${mapping.name}'`) || source.includes(`test("${mapping.name}"`),
    true,
    `${mapping.path} does not define test ${mapping.name}`,
  )
}

function rustFiles(directory) {
  const result = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) result.push(...rustFiles(path))
    else if (entry.isFile() && extname(entry.name) === '.rs') result.push(path)
  }
  return result
}

function oneOfValues(schema, definitionName, discriminator) {
  return schema.$defs[definitionName].oneOf.map(branch => {
    const definition = schema.$defs[branch.$ref.split('/').at(-1)]
    const object = definition.allOf?.find(entry => entry.type === 'object') ?? definition
    return object.properties[discriminator].const
  })
}

test('StrongFlow projection matrix covers every required field group and named Rust seam', () => {
  const matrix = json(rulesPath)
  assert.equal(matrix.schemaVersion, 'winwincode.strongflow-projection-rules.v1')
  assert.equal(matrix.issueId, 'winwincode-9c4.16.2.5')
  assert.equal(matrix.rustTarget.deliveryCrate, 'crates/winwincode-delivery')
  assert.equal(matrix.rustTarget.controlPlaneCrate, 'crates/winwincode-control-plane')

  const sectionIds = matrix.sections.map(section => section.id)
  assert.equal(new Set(sectionIds).size, sectionIds.length)
  assert.deepEqual([...sectionIds].sort(), REQUIRED_SECTION_IDS)

  for (const section of matrix.sections) {
    assert.ok(section.fields.length > 0, `${section.id} needs public fields`)
    assert.equal(new Set(section.fields).size, section.fields.length, `${section.id} repeats fields`)
    assert.ok([
      'approved-plan-review',
      'bound-runtime-events',
      'canonical-delivery',
      'canonical-publication',
    ].includes(section.source.kind), `${section.id} has an unknown source`)
    assert.ok(section.source.refs.length > 0, `${section.id} needs source references`)
    for (const path of section.source.refs) {
      assert.equal(existsSync(repositoryPath(path)), true, `${section.id}: ${path}`)
    }
    assert.ok(section.scope.length > 0, `${section.id} needs an exact scope rule`)
    assert.ok(section.transports.http.length > 0, `${section.id} needs a reload query`)
    assert.ok(section.transports.websocket.length > 0, `${section.id} needs an invalidation event`)
    for (const mapping of section.baseline.publicSymbols) assertPublicSymbol(mapping)
    for (const mapping of section.baseline.tests) assertTestCase(mapping)
    assert.match(section.rust.path, /^crates\/winwincode-[a-z-]+\/src\/[a-z_/]+\.rs$/u)
    assert.match(section.rust.testName, /^[a-z][a-z0-9_]+$/u)
  }
})

test('StrongFlow projection rules freeze binding, replay, redaction, and Web authority', () => {
  const matrix = json(rulesPath)
  const ruleIds = matrix.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), REQUIRED_RULE_IDS)

  assert.deepEqual(matrix.runtimeBinding.requiredIdentity, [
    'deliveryId',
    'stageRunId',
    'productSessionId',
    'workerSessionId',
    'codexThreadId',
    'executionJobId',
    'leaseId',
    'attempt',
    'fencingToken',
  ])
  assert.equal(matrix.runtimeBinding.unboundEventPolicy, 'reject-before-projection')
  assert.equal(matrix.runtimeBinding.ambiguousEventPolicy, 'reject-before-projection')
  assert.equal(matrix.runtimeBinding.staleLeasePolicy, 'reject-before-persist-and-projection')

  assert.deepEqual(matrix.dataPolicy.liveDiff.allowedFields, [
    'changedFileCount',
    'additions',
    'deletions',
    'detailsVisible',
    'sourceRef',
  ])
  assert.equal(matrix.dataPolicy.liveDiff.detailsVisible, false)
  for (const field of [
    'changedFiles',
    'filePath',
    'hunk',
    'hunkContent',
    'unifiedDiff',
  ]) assert.ok(matrix.dataPolicy.liveDiff.forbiddenFields.includes(field), field)
  for (const field of [
    'apiKey',
    'authorization',
    'credential',
    'providerRequest',
    'providerResponse',
    'rawRuntimeLog',
    'stderr',
    'stdout',
    'toolPayload',
  ]) assert.ok(matrix.dataPolicy.forbiddenAtEveryPublicSeam.includes(field), field)

  for (const rule of matrix.rules) {
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    for (const path of rule.refs) assert.equal(existsSync(repositoryPath(path)), true, path)
    for (const mapping of rule.baseline.tests) assertTestCase(mapping)
    assert.match(rule.rust.path, /^crates\/winwincode-[a-z-]+\/src\/[a-z_/]+\.rs$/u)
    assert.match(rule.rust.testName, /^[a-z][a-z0-9_]+$/u)
  }
})

test('phase-one public contracts expose safe transport anchors and keep known detail gaps explicit', () => {
  const matrix = json(rulesPath)
  const domain = json(domainSchemaPath)
  const http = json(httpSchemaPath)
  const events = json(eventSchemaPath)
  const executionPort = json(executionPortSchemaPath)

  assert.ok(oneOfValues(http, 'QueryRequest', 'query').includes('delivery.get'))
  assert.ok(oneOfValues(http, 'QueryRequest', 'query').includes('runtime.projection.get'))
  assert.ok(oneOfValues(http, 'QueryRequest', 'query').includes('publication.list'))
  assert.ok(oneOfValues(events, 'ControlPlaneWebSocketEventPayload', 'type')
    .includes('runtime-projection.appended.v1'))
  assert.ok(oneOfValues(events, 'ControlPlaneWebSocketEventPayload', 'type')
    .includes('delivery.changed.v1'))

  const runtimeItem = domain.$defs.RuntimeProjectionItem
  for (const field of [
    'productSessionId',
    'workerSessionId',
    'codexThreadId',
    'leaseId',
    'deliveryId',
    'stageRunId',
    'projectionSequence',
    'projectionKind',
    'summary',
    'occurredAt',
  ]) assert.ok(runtimeItem.properties[field], field)
  assert.equal(runtimeItem.properties.unifiedDiff, undefined)
  assert.equal(runtimeItem.properties.toolPayload, undefined)

  const findingIds = matrix.contractFindings.map(finding => finding.id)
  assert.equal(new Set(findingIds).size, findingIds.length)
  assert.deepEqual([...findingIds].sort(), REQUIRED_FINDING_IDS)
  for (const finding of matrix.contractFindings) {
    assert.equal(finding.status, 'open')
    assert.ok(finding.current.length > 0, finding.id)
    assert.ok(finding.target.length > 0, finding.id)
    assert.ok(finding.action.length > 0, finding.id)
    for (const path of finding.refs) assert.equal(existsSync(repositoryPath(path)), true, path)
  }

  const deliveryFields = domain.$defs.DeliveryProjection.properties
  assert.equal(deliveryFields.requirements, undefined)
  assert.equal(deliveryFields.solution, undefined)
  assert.equal(deliveryFields.stages, undefined)
  assert.equal(deliveryFields.tasks, undefined)
  assert.equal(deliveryFields.attention, undefined)
  assert.equal(deliveryFields.evidence, undefined)
  assert.equal(deliveryFields.verdict, undefined)
  assert.equal(deliveryFields.publication, undefined)

  for (const field of ['plan', 'agents', 'activities', 'usage', 'evidence']) {
    assert.equal(runtimeItem.properties[field], undefined, field)
  }
  assert.equal(executionPort.$defs.RuntimeEventMessage.properties.codexThreadId, undefined)
})

test('plain-language contract states reload, live Diff, and generated-Web boundaries', () => {
  const contract = readFileSync(contractPath, 'utf8')
  for (const phrase of [
    '未绑定的事件不进入投影',
    '刷新和重启后得到同一份结果',
    '执行中只显示 Diff 数量摘要',
    'Web 只使用生成的 HTTP 和 WebSocket 客户端',
    'Web 不连接 Execution Worker',
    'WebSocket 只通知读取方刷新或追加只读内容',
  ]) assert.equal(contract.includes(phrase), true, phrase)
})

test('Rust projection modules implement every named rule once their seam appears', () => {
  const matrix = json(rulesPath)
  const checks = [
    ...matrix.sections.map(section => section.rust),
    ...matrix.rules.map(rule => rule.rust),
  ]
  const byTrigger = Map.groupBy(checks, check => check.trigger)

  for (const [trigger, triggerChecks] of byTrigger) {
    const triggerPath = repositoryPath(trigger)
    if (!existsSync(triggerPath)) continue

    const crate = trigger.includes('/winwincode-control-plane/')
      ? matrix.rustTarget.controlPlaneCrate
      : matrix.rustTarget.deliveryCrate
    const crateRoot = repositoryPath(crate)
    const allRust = rustFiles(crateRoot).map(path => readFileSync(path, 'utf8')).join('\n')
    for (const check of triggerChecks) {
      assert.equal(existsSync(repositoryPath(check.path)), true, `${check.path} is missing`)
      assert.match(
        allRust,
        new RegExp(`\\bfn\\s+${check.testName}\\s*\\(`, 'u'),
        `${check.testName} is missing`,
      )
    }
  }
})
