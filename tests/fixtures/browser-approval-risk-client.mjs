import { mountLocalDecisionsPage } from '/module/local-decisions-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const longTail = 'y'.repeat(600)
const rawCommand = `deploy ​${'x'.repeat(600)} --token SK-LIVE-0000000000000000000000000000000000000001\nrm -rf ${longTail}`
const expiryFuture = '2030-01-01T01:00:00.000Z'
const expiryPast = '2020-01-01T01:00:00.000Z'

function binding() {
  return {
    productSessionId: 'psn_00000000000000000000000001',
    executionJobId: 'job_00000000000000000000000001',
    workerSessionId: 'wss_00000000000000000000000001',
    sessionIdentity: {
      productSessionId: 'psn_00000000000000000000000001',
      workerSessionId: 'wss_00000000000000000000000001',
      codexThreadId: 'cdx_00000000000000000000000001',
    },
  }
}

function approval(id, overrides = {}) {
  return {
    binding: binding(),
    category: 'shell',
    effectiveDecisionScope: 'once',
    expiresAt: expiryFuture,
    id,
    requestedAt: '2030-01-01T00:00:00.000Z',
    revision: 3,
    sanitizedDetail: {
      kind: 'unavailable',
      reason: 'encoded_payload_redacted',
    },
    state: 'pending',
    subject: 'git status --porcelain',
    ...overrides,
  }
}

const approvals = [
  approval('apr_00000000000000000000000001', { subject: rawCommand }),
  approval('apr_00000000000000000000000002', {
    category: 'mcp',
    subject: 'Call the internal knowledge tool',
    sanitizedDetail: { kind: 'unavailable', reason: 'producer_unavailable' },
  }),
  approval('apr_00000000000000000000000003', {
    category: 'unavailable',
    subject: 'Unclassified producer request',
    sanitizedDetail: { kind: 'unavailable', reason: 'source_not_recorded' },
    expiresAt: expiryPast,
  }),
]

let listener = () => {}
let state = pageState()

function pageState() {
  return {
    status: 'ready',
    realtime: 'subscribed',
    session: {
      id: 'psn_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
      revision: 1,
      state: 'waiting_for_approval',
      title: 'Approval risk fixture',
      updatedAt: '2030-01-01T00:00:00.000Z',
    },
    inputs: [],
    approvals: approvals.map(projection => ({
      projection,
      expired: projection.id.endsWith('0003'),
    })),
    attention: [],
    interaction: { status: 'idle', operation: null, targetId: null, error: null },
    error: null,
  }
}

const model = {
  get state() { return state },
  subscribe(next) {
    listener = next
    next(state)
    return () => { listener = () => {} }
  },
  async start() {},
  async refresh() {},
  async provideInput() {},
  async cancelInput() {},
  async decideApproval() {},
  async resolveAttention() {},
  cancelPending() {},
  reconnect() {},
  close() {},
}

const mounted = mountLocalDecisionsPage({ root, model })

function card(index) {
  return document.querySelectorAll('.wwc-local-approval')[index]
}

globalThis.runApprovalRiskScenario = () => {
  const blocks = [...document.querySelectorAll("[data-wwc-component='approval-risk']")]
  const shell = card(0)
  const shellBlock = blocks.find(block => shell.contains(block))
  const shellCommand = shellBlock.querySelector('.wwc-approval-risk-command-text')
  const shellText = shellBlock.textContent
  const scope = shellBlock.querySelector('.wwc-approval-risk-scope')
  const expiry = shellBlock.querySelector('.wwc-approval-risk-expiry')
  const target = shellBlock.querySelector('.wwc-approval-risk-target')
  const order = [...shell.querySelectorAll(':scope > *')].map(node => node.className)

  const fields = key => [...document.querySelectorAll(
    `[data-wwc-component='approval-risk'] [data-field-key='${key}'] dd`,
  )].map(node => node.textContent)

  const unclassified = blocks.find(block => card(2).contains(block))
  const decisions = [...document.querySelectorAll('.wwc-local-approval-approve')]

  const result = {
    riskBlockCount: blocks.length,
    shell: {
      level: shellBlock.querySelector('.wwc-approval-risk-level').textContent,
      impact: shellBlock.querySelector('.wwc-approval-risk-impact').textContent,
      commandText: shellCommand === null ? null : shellCommand.textContent,
      commandLength: shellCommand === null ? 0 : shellCommand.textContent.length,
      subjectLength: shell.querySelector('.wwc-local-approval-subject').textContent.length,
      leaksRawCommand: shellText.includes('x'.repeat(200)) || shellText.includes(longTail),
      leaksToken: shellText.includes('SK-LIVE-'),
      scope: scope === null ? null : scope.textContent,
      expiry: expiry === null ? null : expiry.textContent,
      target: target === null ? null : target.textContent,
      commandBeforeDecisionForm: order.indexOf('wwc-approval-risk')
        < order.indexOf('wwc-local-approval-form'),
    },
    withheld: {
      cwd: fields('cwd'),
      fileImpact: fields('fileImpact'),
      networkTargets: fields('networkTargets'),
      mcpTarget: fields('mcpTarget'),
      requestedReason: fields('requestedReason'),
    },
    fieldKeyCount: document.querySelectorAll(
      "[data-wwc-component='approval-risk'] [data-field-key]",
    ).length,
    mcpImpact: blocks.find(block => card(1).contains(block))
      .querySelector('.wwc-approval-risk-impact').textContent,
    unclassified: {
      level: unclassified.querySelector('.wwc-approval-risk-level').textContent,
      impact: unclassified.querySelector('.wwc-approval-risk-impact').textContent,
      expiry: unclassified.querySelector('.wwc-approval-risk-expiry').textContent,
      cardState: card(2).dataset.state,
      approveDisabled: decisions[2].disabled,
    },
    decisions: decisions.map(node => ({
      disabled: node.disabled,
      label: node.textContent,
    })),
  }
  mounted.close()
  return result
}
