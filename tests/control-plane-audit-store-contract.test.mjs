import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-audit-store.rules.json',
)
const documentationPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-audit-store.md',
)

function read(path) {
  return readFileSync(path, 'utf8')
}

function rules() {
  return JSON.parse(read(rulesPath))
}

function run(command) {
  const [program, ...commandArguments] = command
  const result = spawnSync(program, commandArguments, {
    cwd: root,
    encoding: 'utf8',
    env: process.env,
    timeout: 300_000,
  })
  assert.equal(
    result.status,
    0,
    [result.stdout, result.stderr, result.error?.stack].filter(Boolean).join('\n'),
  )
  return `${result.stdout}\n${result.stderr}`
}

function structFields(source, name) {
  const body = source.match(new RegExp(`pub struct ${name} \\{([\\s\\S]*?)\\n\\}`, 'u'))?.[1]
  assert.ok(body, `missing public ${name} structure`)
  return [...body.matchAll(/^\s+([a-z_]+):/gmu)].map(match => match[1])
}

test('phase 3.4 freezes one implemented typed Audit Event and store', () => {
  const contract = rules()
  assert.deepEqual(Object.keys(contract), [
    'schemaVersion',
    'status',
    'issueId',
    'decision',
    'documentation',
    'implementationCompletionSource',
    'authorityChain',
    'event',
    'modelInvocation',
    'storage',
    'readAuthority',
    'retention',
    'integrationBoundary',
    'rustGate',
    'compatibility',
  ])
  assert.deepEqual(
    {
      schemaVersion: contract.schemaVersion,
      status: contract.status,
      issueId: contract.issueId,
      decision: contract.decision,
      documentation: contract.documentation,
      implementationCompletionSource: contract.implementationCompletionSource,
    },
    {
      schemaVersion: 'winwincode.control-plane-audit-store-rules.v1',
      status: 'implemented-enforced',
      issueId: 'winwincode-9c4.16.3.4',
      decision: 'docs/decisions/0028-control-plane-worker-migration.md',
      documentation: 'docs/contracts/control-plane-audit-store.md',
      implementationCompletionSource: 'rust-black-box-tests-and-beads',
    },
  )
  assert.deepEqual(contract.authorityChain, [
    'authenticated-actor-and-policy-authorized-scope',
    'closed-typed-audit-event',
    'canonical-json-payload-digest',
    'organization-local-sequence-and-chain-digest',
    'sqlite-immutable-event-header',
    'scope-filtered-read-or-retention-tombstone',
  ])
  assert.deepEqual(contract.compatibility, {
    legacyAuditShapeRetained: false,
    secondAuditStorePathRetained: false,
    rawSensitiveTextFallbackRetained: false,
  })
})

test('Audit Event identity, actors, actions, state, and results are closed', () => {
  const event = rules().event
  assert.equal(event.identity, 'aud_ plus 26 Crockford characters')
  assert.deepEqual(event.actors, ['user', 'service_account', 'system'])
  assert.deepEqual(event.scopeLevels, [
    'organization',
    'workspace',
    'project',
    'repository',
  ])
  assert.deepEqual(event.actionCategories, [
    'command',
    'approval',
    'policy',
    'worker_lease',
    'model_invocation',
    'delivery_state',
    'publication',
  ])
  assert.deepEqual(event.stateKinds, ['changed', 'unchanged'])
  assert.deepEqual(event.outcomes, ['succeeded', 'rejected', 'failed'])
  assert.deepEqual(event.origins, ['local-component', 'source-ip'])
  assert.deepEqual(event.subjectReferences, [
    'deliveryId',
    'productSessionId',
    'leaseId',
    'publicationId',
  ])
  assert.equal(event.successfulStateChangeRequiresDistinctDigests, true)
  assert.equal(event.rejectedOrFailedStateChangeAllowed, false)
  assert.equal(event.exactEventIdReplay, 'original-record-without-new-sequence')
  assert.equal(event.changedEventIdReuse, 'request-conflict')
  assert.equal(event.arbitraryPayloadFieldAllowed, false)
})

test('the Rust event shape has no raw payload, prompt, response, or credential slot', () => {
  const eventSource = read(
    join(root, 'crates', 'winwincode-audit', 'src', 'event.rs'),
  )
  assert.deepEqual(structFields(eventSource, 'AuditEvent'), [
    'event_id',
    'occurred_at_millis',
    'actor',
    'scope',
    'request_id',
    'action',
    'state',
    'origin',
    'subject',
    'outcome',
    'result_code',
    'retention',
  ])
  assert.deepEqual(structFields(eventSource, 'AuditModelInvocation'), [
    'provider_id',
    'model_id',
    'input_digest',
    'output_digest',
    'input_tokens',
    'output_tokens',
  ])
  assert.deepEqual(rules().modelInvocation, {
    retainedFields: [
      'providerId',
      'modelId',
      'inputSha256',
      'outputSha256',
      'inputTokens',
      'outputTokens',
    ],
    rawPromptAllowed: false,
    rawResponseAllowed: false,
    credentialAllowed: false,
    providerDiagnosticAllowed: false,
  })
})

test('ordering, exact scope filtering, integrity, and retention rules are closed', () => {
  const contract = rules()
  assert.deepEqual(contract.storage, {
    adapter: 'sqlite',
    database: 'audit.sqlite3',
    journalMode: 'WAL',
    synchronous: 'FULL',
    chainScope: 'organization',
    chainAlgorithm: 'sha256-framed-v1',
    sequenceStartsAt: 1,
    eventHeadersMutable: false,
    eventHeadersDeletable: false,
    retentionTombstonesMutable: false,
    retentionTombstonesDeletable: false,
    integrityChecks: [
      'gapless-sequence',
      'previous-event-digest',
      'header-event-digest',
      'canonical-payload-digest',
      'payload-header-identity',
      'retention-tombstone',
      'organization-chain-head',
    ],
  })
  assert.deepEqual(contract.readAuthority, {
    authorityOwner: 'policy-layer',
    storeTreatsScopeAsAuthenticationProof: false,
    filters: ['organizationId', 'workspaceId', 'projectId', 'repositoryId'],
    maximumPageSize: 200,
    cursor: 'organization-local-sequence',
    crossOrganizationPayloadAllowed: false,
  })
  assert.deepEqual(contract.retention, {
    modes: ['until-millis', 'indefinite'],
    beforeDeadlineDeletionAllowed: false,
    indefinitePayloadDeletionAllowed: false,
    expiredPayloadBytesDeleted: true,
    immutableHeaderRetained: true,
    payloadDigestRetained: true,
    immutableTombstoneRetained: true,
    exactReplayAfterPrune: 'original-header-without-payload',
  })

  const storeSource = read(
    join(root, 'crates', 'winwincode-audit', 'src', 'store.rs'),
  )
  for (const requiredSource of [
    'winwincode.audit-chain.v1',
    'TransactionBehavior::Immediate',
    'audit_events_no_update',
    'audit_events_no_delete',
    'audit_payload_tombstones_no_update',
    'audit_payload_tombstones_no_delete',
    'record_from_header',
    'verify_organization',
  ]) {
    assert.match(storeSource, new RegExp(requiredSource.replaceAll('.', '\\.'), 'u'))
  }
})

test('phase 3.4 does not claim policy or product command cutover', () => {
  assert.deepEqual(rules().integrationBoundary, {
    typedEventAndStoreImplemented: true,
    httpAuditQueryAdded: false,
    policyAuthorizationImplementedHere: false,
    productCommandCutoverTasks: [
      'winwincode-9c4.16.3.5',
      'winwincode-9c4.16.3.6',
      'winwincode-9c4.16.3.7',
    ],
    secondProductCommandPathAdded: false,
  })

  const workspace = read(join(root, 'Cargo.toml'))
  const manifest = read(join(root, 'crates', 'winwincode-audit', 'Cargo.toml'))
  assert.match(workspace, /"crates\/winwincode-audit"/u)
  assert.match(workspace, /winwincode-audit = \{ path = "crates\/winwincode-audit" \}/u)
  assert.match(manifest, /name = "winwincode-audit"/u)
  assert.match(manifest, /winwincode-domain\.workspace = true/u)
  assert.doesNotMatch(manifest, /winwincode-control-plane/u)
})

test('the contract gate executes every Audit Event and store black-box test', () => {
  const gate = rules().rustGate
  const output = run(gate.command)
  for (const requiredTest of gate.requiredTests) {
    assert.match(
      output,
      new RegExp(`test ${requiredTest} \\.\\.\\. ok`, 'u'),
      `${gate.package} did not execute ${requiredTest}`,
    )
  }
})

test('the documentation states the same path and the later integration boundary', () => {
  const documentation = read(documentationPath)
  for (const statement of [
    'closed typed AuditEvent',
    'organization 内连续 sequence + previous digest',
    '拒绝和失败只能记录',
    '相同 ID 改动任何事件内容时返回 `RequestConflict`',
    'Policy 层先完成认证和授权',
    '单页上限固定为 200 条',
    '清理事务先写入不可变 tombstone',
    '直接删除 payload 而没有匹配的 tombstone 会被判为损坏',
    '原始 prompt、原始回答',
    '阶段 3.4 交付的是唯一 typed `AuditEvent` 和 `AuditStore`',
    '后续任务必须在每次成功状态变化、拒绝和失败时调用本 seam',
  ]) {
    assert.ok(
      documentation.includes(statement),
      `documentation is missing: ${statement}`,
    )
  }
})
