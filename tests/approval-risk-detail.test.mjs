import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.approval-risk-tests.json',
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
  `Approval risk detail did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const contractsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/approval-risk-tests/generated/contracts.js',
)).href}`)
const riskModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/approval-risk-tests/approval-risk-detail.js',
)).href}`)

const {
  ApprovalEffectiveDecisionScope,
  ApprovalProjectionCategory,
  ApprovalSanitizedDetailUnavailableReason,
} = contractsModule
const {
  APPROVAL_TEXT_LIMIT,
  approvalDecisionScope,
  approvalExpiry,
  approvalImpact,
  approvalImpactStatements,
  approvalRiskDetail,
  approvalRiskLevel,
  approvalWithheldLabel,
  boundApprovalText,
} = riskModule

const productSessionId = 'psn_00000000000000000000000001'
const workerSessionId = 'wss_00000000000000000000000001'
const executionJobId = 'job_00000000000000000000000001'
const codexThreadId = 'cdx_00000000000000000000000001'
const stageRunId = 'str_00000000000000000000000001'
const now = Date.parse('2026-09-03T12:00:00.000Z')
const futureExpiry = '2026-09-03T12:30:00.000Z'
const pastExpiry = '2026-09-03T11:30:00.000Z'

function binding(overrides = {}) {
  return {
    productSessionId,
    executionJobId,
    workerSessionId,
    sessionIdentity: {
      productSessionId,
      workerSessionId,
      codexThreadId,
      stageRunId,
    },
    ...overrides,
  }
}

function projection(overrides = {}) {
  return {
    binding: binding(),
    category: ApprovalProjectionCategory.Shell,
    effectiveDecisionScope: ApprovalEffectiveDecisionScope.Once,
    expiresAt: futureExpiry,
    id: 'apr_00000000000000000000000001',
    requestedAt: '2026-09-03T11:55:00.000Z',
    revision: 7,
    sanitizedDetail: {
      kind: 'unavailable',
      reason: ApprovalSanitizedDetailUnavailableReason.EncodedPayloadRedacted,
    },
    state: 'pending',
    subject: 'rm -rf ./build && git commit -m "rebuild"',
    ...overrides,
  }
}

const FIELD_KEYS = Object.freeze([
  'command',
  'cwd',
  'fileImpact',
  'networkTargets',
  'mcpTarget',
  'requestedReason',
])

test('the sealed Approval projection offers no structured payload path to the page', () => {
  const generated = readFileSync(
    resolve(root, 'apps/client/src/generated/contracts.ts'),
    'utf8',
  )
  const start = generated.indexOf('export type ApprovalSanitizedDetailProjection = {')
  assert.notEqual(start, -1)
  const declaration = generated.slice(start, generated.indexOf('}\n', start))
  assert.match(declaration, /readonly "kind": "unavailable"/u)
  assert.match(declaration, /readonly "reason": ApprovalSanitizedDetailUnavailableReason/u)
  for (const forbidden of ['details', 'payload', 'command', 'cwd', 'argv', 'env']) {
    assert.equal(declaration.includes(`"${forbidden}"`), false, forbidden)
  }
  assert.deepEqual(
    Object.values(ApprovalSanitizedDetailUnavailableReason).sort(),
    ['encoded_payload_redacted', 'producer_unavailable', 'source_not_recorded'].sort(),
  )
  assert.deepEqual(
    Object.values(ApprovalEffectiveDecisionScope),
    ['once'],
    'approval.decide accepts exactly one descriptive decision scope',
  )
})

test('every rendered approval text is bounded and cannot smuggle hidden content', () => {
  assert.equal(typeof APPROVAL_TEXT_LIMIT, 'number')
  assert.ok(APPROVAL_TEXT_LIMIT > 0)

  assert.deepEqual(boundApprovalText('rm -rf ./build'), {
    text: 'rm -rf ./build',
    truncated: false,
  })
  assert.deepEqual(boundApprovalText('  a\tb\nc\r\n  d  '), {
    text: 'a b c d',
    truncated: false,
  })
  // Directional and zero-width controls must never reach the page: they hide
  // the real command from the operator who has to approve it.  Removing them
  // leaves the logical text the producer actually sent.
  assert.deepEqual(boundApprovalText('git​ commit'), {
    text: 'git commit',
    truncated: false,
  })
  assert.deepEqual(boundApprovalText('‪/usr‬/bin/env'), {
    text: '/usr/bin/env',
    truncated: false,
  })
  assert.deepEqual(boundApprovalText('a‼b'), { text: 'a‼b', truncated: false })
  const long = 'x'.repeat(APPROVAL_TEXT_LIMIT * 4)
  const bounded = boundApprovalText(long)
  assert.equal(bounded.truncated, true)
  assert.equal(bounded.text.length, APPROVAL_TEXT_LIMIT)
  assert.equal(bounded.text.endsWith('…'), true)
  assert.deepEqual(boundApprovalText('   '), { text: '', truncated: false })
})

test('the risk detail bounds the subject and never carries the raw producer payload', () => {
  const secret = 'SK-LIVE-0000000000000000000000000000000000000001'
  const rawSubject = `deploy ${'y'.repeat(APPROVAL_TEXT_LIMIT * 3)}`
  const planted = {
    ...projection({ subject: rawSubject }),
    // Not part of the contract: a hostile or legacy producer may still send it.
    details: { encodedPayload: secret },
  }
  const detail = approvalRiskDetail(planted, { nowMillis: () => now })
  const rendered = JSON.stringify(detail)
  assert.equal(detail.subject, boundApprovalText(rawSubject).text)
  assert.equal(rendered.includes(rawSubject), false)
  assert.equal(rendered.includes(secret), false)
  assert.equal(rendered.includes('encodedPayload'), false)
})

test('shell approvals expose a safely truncated command summary and an execution impact', () => {
  const detail = approvalRiskDetail(projection(), { nowMillis: () => now })
  assert.equal(detail.impact, 'shell')
  assert.equal(detail.impactLabel, 'Shell execution')
  const command = detail.fieldByKey.command
  assert.equal(command.availability, 'available')
  assert.equal(command.text, 'rm -rf ./build && git commit -m "rebuild"')
  assert.equal(command.withheldReason, null)
  assert.equal(command.withheldLabel, null)
  assert.ok(command.note === null || command.note.length > 0)
  assert.deepEqual(detail.impactStatements, [
    'Runs a shell command inside the delivery workspace.',
  ])
  assert.equal(detail.risk.level, 'high')
  assert.equal(detail.risk.label, 'High risk')
  assert.ok(detail.risk.rationale.length > 0)
  assert.deepEqual(
    approvalRiskLevel(ApprovalProjectionCategory.Shell).level,
    'high',
  )
})

test('non-shell categories withhold the command and state their own impact', () => {
  const cases = [
    {
      category: ApprovalProjectionCategory.FilesystemWrite,
      impact: 'filesystem_write',
      impactLabel: 'Filesystem write',
      statement: 'Writes files inside the delivery workspace.',
      level: 'moderate',
    },
    {
      category: ApprovalProjectionCategory.Network,
      impact: 'network',
      impactLabel: 'Network access',
      statement: 'Performs outbound network access.',
      level: 'elevated',
    },
    {
      category: ApprovalProjectionCategory.Mcp,
      impact: 'mcp',
      impactLabel: 'MCP tool call',
      statement: 'Calls an MCP tool through a connected server.',
      level: 'elevated',
    },
  ]
  for (const item of cases) {
    const detail = approvalRiskDetail(projection({ category: item.category }), {
      nowMillis: () => now,
    })
    assert.equal(detail.impact, item.impact, item.impact)
    assert.equal(detail.impactLabel, item.impactLabel, item.impact)
    assert.deepEqual(detail.impactStatements, [item.statement], item.impact)
    assert.equal(detail.risk.level, item.level, item.impact)
    assert.equal(approvalImpact(item.category), item.impact, item.impact)
    assert.equal(detail.fieldByKey.command.availability, 'withheld', item.impact)
    assert.equal(detail.fieldByKey.command.text, null, item.impact)
  }
  assert.deepEqual(
    approvalImpactStatements(ApprovalProjectionCategory.Unavailable),
    [],
    'an unclassified action states no impact',
  )
})

test('fields the secret-safe projection does not carry are withheld with a reason', () => {
  const detail = approvalRiskDetail(projection(), { nowMillis: () => now })
  assert.deepEqual(detail.fields.map(field => field.key), FIELD_KEYS)
  for (const key of ['cwd', 'fileImpact', 'networkTargets', 'mcpTarget', 'requestedReason']) {
    const field = detail.fieldByKey[key]
    assert.equal(field.availability, 'withheld', key)
    assert.equal(field.text, null, key)
    assert.equal(field.note, null, key)
    assert.equal(field.withheldReason, 'encoded_payload_redacted', key)
    assert.ok(field.withheldLabel.length > 0, key)
  }
})

test('a producer that recorded no detail degrades with its own reason', () => {
  const detail = approvalRiskDetail(projection({
    category: ApprovalProjectionCategory.Network,
    sanitizedDetail: {
      kind: 'unavailable',
      reason: ApprovalSanitizedDetailUnavailableReason.ProducerUnavailable,
    },
  }), { nowMillis: () => now })
  assert.equal(detail.fieldByKey.networkTargets.withheldReason, 'producer_unavailable')
  assert.equal(detail.fieldByKey.requestedReason.withheldReason, 'producer_unavailable')
  for (const reason of [
    'producer_unavailable',
    'source_not_recorded',
    'not_in_secret_safe_projection',
  ]) {
    assert.ok(approvalWithheldLabel(reason).length > 0, reason)
  }
})

test('an unclassified action degrades instead of inventing a risk', () => {
  const detail = approvalRiskDetail(projection({
    category: ApprovalProjectionCategory.Unavailable,
    sanitizedDetail: {
      kind: 'unavailable',
      reason: ApprovalSanitizedDetailUnavailableReason.SourceNotRecorded,
    },
  }), { nowMillis: () => now })
  assert.equal(detail.impact, 'unknown')
  assert.equal(detail.impactLabel, 'Unclassified action')
  assert.equal(detail.risk.level, 'unknown')
  assert.equal(detail.risk.label, 'Risk unknown')
  assert.ok(detail.risk.rationale.length > 0)
  assert.equal(detail.fieldByKey.command.availability, 'withheld')
  assert.equal(detail.fieldByKey.command.withheldReason, 'source_not_recorded')
  for (const field of detail.fields) assert.equal(field.availability, 'withheld')
})

test('the decision scope is descriptive, never selectable, and bounded to one request', () => {
  assert.deepEqual(approvalRiskDetail(projection(), { nowMillis: () => now }).decisionScope, {
    scope: 'once',
    label: 'Approve once',
    detail: 'This decision covers this single request only and never extends to the Worker session.',
    selectable: false,
  })
  assert.equal(approvalDecisionScope(ApprovalEffectiveDecisionScope.Once).selectable, false)
  assert.equal(approvalDecisionScope(undefined).selectable, false)
  assert.equal(approvalDecisionScope(undefined).scope, 'unknown')
})

test('a projection persisted before later fields degrades instead of throwing', () => {
  const stale = {
    id: 'apr_00000000000000000000000002',
    revision: 1,
    state: 'pending',
    requestedAt: '2026-08-27T02:59:00.000Z',
    expiresAt: '2026-08-27T04:00:00.000Z',
    subject: 'Allow the projected repository action',
    binding: binding(),
  }
  const detail = approvalRiskDetail(stale, { nowMillis: () => now })
  assert.equal(detail.impact, 'unknown')
  assert.equal(detail.impactLabel, 'Unclassified action')
  assert.equal(detail.risk.level, 'unknown')
  assert.equal(detail.state, 'pending', 'a reported state is preserved verbatim')
  assert.equal(
    approvalRiskDetail({ ...stale, state: undefined }, { nowMillis: () => now }).state,
    'unknown',
    'a missing state degrades to unknown instead of looking actionable',
  )
  assert.equal(detail.subject, stale.subject)
  assert.equal(detail.decisionScope.scope, 'unknown')
  assert.equal(detail.decisionScope.selectable, false)
  assert.equal(detail.fieldByKey.command.availability, 'withheld')
  for (const field of detail.fields) {
    assert.equal(field.availability, 'withheld', field.key)
    assert.equal(field.withheldReason, 'not_in_secret_safe_projection', field.key)
  }
  assert.equal(approvalRiskDetail(undefined, { nowMillis: () => now }).risk.level, 'unknown')
})

test('expiry is explicit before a decision and fails closed on an unreadable deadline', () => {
  const future = approvalExpiry(futureExpiry, now)
  assert.equal(future.expired, false)
  assert.equal(future.millisRemaining, 30 * 60 * 1000)
  assert.match(future.label, /Expires/u)
  assert.match(future.label, /30m/u)

  const past = approvalExpiry(pastExpiry, now)
  assert.equal(past.expired, true)
  assert.equal(past.millisRemaining, 0)
  assert.match(past.label, /Expired/u)

  const unreadable = approvalExpiry('not-a-timestamp', now)
  assert.equal(unreadable.expired, true)
  assert.equal(unreadable.millisRemaining, null)
  assert.match(unreadable.label, /unknown/u)

  assert.equal(
    approvalRiskDetail(projection({ expiresAt: pastExpiry }), { nowMillis: () => now })
      .expiry.expired,
    true,
  )
  assert.equal(
    approvalRiskDetail(projection({ expiresAt: futureExpiry }), {
      expired: true,
      nowMillis: () => now,
    }).expiry.expired,
    true,
    'the card decision state wins over the clock',
  )
})

test('the execution target names where the action runs', () => {
  const detail = approvalRiskDetail(projection(), { nowMillis: () => now })
  assert.deepEqual(detail.executionTarget, {
    productSessionId,
    workerSessionId,
    executionJobId,
    stageRunId,
    label: 'ProductSession, StageRun, ExecutionJob, and WorkerSession-bound',
  })
  const withoutStage = approvalRiskDetail(projection({
    binding: binding({
      sessionIdentity: {
        productSessionId,
        workerSessionId,
        codexThreadId,
      },
    }),
  }), { nowMillis: () => now })
  assert.equal(withoutStage.executionTarget.stageRunId, null)
  assert.equal(
    withoutStage.executionTarget.label,
    'ProductSession, ExecutionJob, and WorkerSession-bound',
  )
})

test('the risk detail is a frozen, stable snapshot for one projection', () => {
  const detail = approvalRiskDetail(projection(), { nowMillis: () => now })
  assert.equal(Object.isFrozen(detail), true)
  assert.equal(Object.isFrozen(detail.fields), true)
  assert.deepEqual(approvalRiskDetail(projection(), { nowMillis: () => now }), detail)
  assert.equal(detail.approvalId, 'apr_00000000000000000000000001')
  assert.equal(detail.state, 'pending')
  assert.equal(detail.revision, 7)
  assert.equal(detail.expiry.expiresAt, futureExpiry)
})
