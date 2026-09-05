import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
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
    'apps/client/tsconfig.contextual-decision-view-model-tests.json',
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
  `The contextual decision view model did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const module = await import(`${pathToFileURL(resolve(
  root,
  '.cache/contextual-decision-view-model-tests/contextual-decision-view-model.js',
)).href}`)

const {
  boundedContextualDecisions,
  contextualDecisionCapability,
  contextualDecisionKindLabel,
  contextualDecisionPresentation,
  contextualDecisions,
  orderedContextualDecisions,
  DEFAULT_CONTEXTUAL_DECISION_LIMIT,
} = module

const productSessionId = 'psn_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const now = Date.parse('2026-09-04T12:00:00.000Z')

function binding(overrides = {}) {
  return {
    productSessionId,
    executionJobId: 'job_00000000000000000000000001',
    workerSessionId: 'wsn_00000000000000000000000001',
    sessionIdentity: {
      productSessionId,
      workerSessionId: 'wsn_00000000000000000000000001',
      codexThreadId: 'cdx_00000000000000000000000001',
      stageRunId,
    },
    ...overrides,
  }
}

function input(overrides = {}) {
  return {
    kind: 'input',
    inputRequestId: 'inp_00000000000000000000000001',
    revision: 3,
    state: 'pending',
    prompt: 'Select the next StageRun step.',
    binding: binding(),
    mode: 'single_choice',
    options: [
      { id: 'ich_00000000000000000000000001', value: 'candidate', label: 'Candidate workspace' },
      { id: 'ich_00000000000000000000000002', value: 'delivery', label: 'Delivery workspace' },
    ],
    allowEmpty: false,
    expiresAt: '2026-09-04T12:30:00.000Z',
    ...overrides,
  }
}

function approval(overrides = {}) {
  return {
    id: 'apr_00000000000000000000000001',
    requestedAt: '2026-09-04T11:00:00.000Z',
    expiresAt: '2026-09-04T12:10:00.000Z',
    revision: 5,
    state: 'pending',
    subject: 'Run the approved test command.',
    binding: binding(),
    ...overrides,
  }
}

function attention(overrides = {}) {
  return {
    projection: {
      id: 'atn_00000000000000000000000001',
      title: 'Verification blocked on the delivery criterion.',
      status: 'open',
      blocking: true,
      createdAt: '2026-09-04T11:30:00.000Z',
      stageRunId,
      type: 'verification_blocked',
      options: [],
      assignedTo: null,
      deliverySpecId: 'spec:1',
      resolutionSummary: null,
      resolvedAt: null,
      resolvedBy: null,
    },
    deliveryId,
    deliveryRevision: 9,
    ...overrides,
  }
}

function source(overrides = {}) {
  return {
    inputs: [],
    approvals: [],
    attention: [],
    nowMillis: now,
    ...overrides,
  }
}

test('the card projects the session inputs and approvals of one snapshot', () => {
  const view = contextualDecisions(source({
    inputs: [input()],
    approvals: [approval()],
  }))
  assert.equal(view.items.length, 2)
  assert.deepEqual(view.items.map(item => item.kind), ['approval', 'input'])
  assert.deepEqual(view.counts, {
    blocking: 1,
    pending: 1,
    expired: 0,
    bindingInvalid: 0,
  })
  const approvalItem = view.items[0]
  assert.equal(approvalItem.id, 'apr_00000000000000000000000001')
  assert.equal(approvalItem.requiresNote, true)
  assert.equal(approvalItem.stageRunId, stageRunId)
  assert.equal(approvalItem.productSessionId, productSessionId)
  const inputItem = view.items[1]
  assert.equal(inputItem.mode, 'single_choice')
  assert.equal(inputItem.options.length, 2)
  assert.equal(inputItem.options[0].value, 'candidate')
  assert.equal(inputItem.requiresNote, false)
})

test('the card projects Delivery Attention with its exact Delivery binding', () => {
  const view = contextualDecisions(source({ attention: [attention()] }))
  assert.equal(view.items.length, 1)
  const item = view.items[0]
  assert.equal(item.kind, 'attention')
  assert.equal(item.urgency, 'blocking')
  assert.equal(item.deliveryId, deliveryId)
  assert.equal(item.revision, 9)
  assert.equal(item.expiresAt, null)
})

test('an expired or answered decision stays visible but loses its urgency', () => {
  const view = contextualDecisions(source({
    inputs: [input({
      inputRequestId: 'inp_00000000000000000000000002',
      state: 'expired',
      expiresAt: '2026-09-04T11:00:00.000Z',
    })],
    approvals: [approval({ expiresAt: '2026-09-04T06:00:00.000Z' })],
  }))
  assert.deepEqual(view.items.map(item => item.urgency), ['expired', 'expired'])
  assert.deepEqual(view.counts, { blocking: 0, pending: 0, expired: 2, bindingInvalid: 0 })
})

test('a decision raised before the clock passes its own expiry is expired', () => {
  const view = contextualDecisions(source({
    approvals: [approval()],
    nowMillis: Date.parse('2026-09-04T12:11:00.000Z'),
  }))
  assert.equal(view.counts.expired, 1)
  assert.equal(view.items[0].urgency, 'expired')
})

test('an inconsistent binding fails closed instead of entering a card', () => {
  const view = contextualDecisions(source({
    approvals: [approval({
      binding: binding({
        workerSessionId: 'wsn_00000000000000000000000002',
      }),
    })],
  }))
  assert.equal(view.counts.bindingInvalid, 1)
  assert.equal(view.items[0].urgency, 'binding-invalid')
})

test('closed Attention does not present itself as an open decision', () => {
  const view = contextualDecisions(source({
    attention: [{
      ...attention(),
      projection: { ...attention().projection, status: 'resolved' },
    }],
  }))
  assert.equal(view.counts.bindingInvalid, 1)
  assert.equal(view.items[0].urgency, 'binding-invalid')
})

test('blocking decisions are ordered first, then soonest expiry, then identity', () => {
  const view = contextualDecisions(source({
    inputs: [input({
      inputRequestId: 'inp_00000000000000000000000009',
      expiresAt: '2026-09-04T13:00:00.000Z',
    })],
    approvals: [approval({
      id: 'apr_00000000000000000000000002',
      expiresAt: '2026-09-04T12:05:00.000Z',
    })],
    attention: [attention({
      projection: {
        ...attention().projection,
        id: 'atn_00000000000000000000000002',
        blocking: false,
      },
    })],
  }))
  assert.deepEqual(view.items.map(item => item.kind), ['approval', 'input', 'attention'])
  assert.equal(view.items[0].id, 'apr_00000000000000000000000002')
  assert.equal(view.items[0].urgency, 'blocking')
  // One stable order, so a refreshed snapshot never reshuffles the card rows.
  const reversed = orderedContextualDecisions([...view.items].reverse())
  assert.deepEqual(reversed.map(item => item.id), view.items.map(item => item.id))
})

test('the card stays bounded and reports what it did not render', () => {
  const decisions = contextualDecisions(source({
    approvals: Array.from({ length: 7 }, (_, index) => approval({
      id: `apr_${String(index + 1).padStart(26, '0')}`,
      expiresAt: `2026-09-04T13:0${String(index)}:00.000Z`,
    })),
  }))
  assert.equal(decisions.items.length, DEFAULT_CONTEXTUAL_DECISION_LIMIT)
  assert.equal(decisions.omitted, 7 - DEFAULT_CONTEXTUAL_DECISION_LIMIT)
  const bounded = boundedContextualDecisions(decisions.items, 2)
  assert.equal(bounded.items.length, 2)
  assert.equal(bounded.omitted, DEFAULT_CONTEXTUAL_DECISION_LIMIT - 2)
  assert.deepEqual(boundedContextualDecisions(decisions.items, 0), {
    items: [],
    omitted: DEFAULT_CONTEXTUAL_DECISION_LIMIT,
  })
})

test('the card presentation stays a plain projection with one capability flag', () => {
  const view = contextualDecisions(source({ approvals: [approval()] }))
  const ready = contextualDecisionPresentation(view)
  assert.equal(ready.statusText, '1 need a decision')
  assert.equal(ready.decisionsDisabled, false)
  assert.equal(contextualDecisionPresentation(view, { loading: true }).statusText, 'Loading decisions…')
  assert.equal(
    contextualDecisionPresentation(view, { loading: true }).decisionsDisabled,
    false,
    'loading alone must not disable a decision the page still owns',
  )
  const empty = contextualDecisionPresentation(contextualDecisions(source()))
  assert.equal(empty.statusText, 'No decision is waiting on you in this context')
})

test('busy, unavailable, and read-only pages disable decisions', () => {
  const view = contextualDecisions(source({ approvals: [approval()] }))
  for (const options of [
    { busy: true },
    { pageUnavailable: true },
    { readOnly: true },
  ]) {
    assert.equal(
      contextualDecisionPresentation(view, options).decisionsDisabled,
      true,
      JSON.stringify(options),
    )
  }
})

test('one row keeps its controls only while the decision is still decidable', () => {
  const view = contextualDecisions(source({
    inputs: [input()],
    approvals: [approval({ expiresAt: '2026-09-04T06:00:00.000Z' })],
  }))
  const presentation = contextualDecisionPresentation(view)
  const expired = view.items.find(item => item.kind === 'approval')
  const live = view.items.find(item => item.kind === 'input')
  assert.equal(contextualDecisionCapability(expired, presentation).disabled, true)
  assert.equal(
    contextualDecisionCapability(expired, presentation).stateLabel,
    'Expired · decision disabled',
  )
  assert.equal(contextualDecisionCapability(live, presentation).disabled, false)
  assert.equal(
    contextualDecisionCapability(live, presentation).stateLabel,
    'Needs a decision',
  )
  assert.equal(
    contextualDecisionKindLabel('attention'),
    'Business Attention',
  )
})
