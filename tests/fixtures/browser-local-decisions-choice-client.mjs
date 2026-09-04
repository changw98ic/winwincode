import { mountLocalDecisionsPage } from '/module/local-decisions-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const inputRequestId = 'inp_00000000000000000000000001'
const duplicateChoices = [
  { id: 'ich_00000000000000000000000001', label: 'Continue', value: 'continue' },
  { id: 'ich_00000000000000000000000002', label: 'Continue', value: 'continue' },
]
const calls = []
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
      state: 'waiting_for_input',
      title: 'Duplicate choice fixture',
      updatedAt: '2030-01-01T00:00:00.000Z',
    },
    inputs: [{
      projection: {
        kind: 'input',
        inputRequestId,
        revision: 1,
        state: 'pending',
        binding: {
          productSessionId: 'psn_00000000000000000000000001',
          executionJobId: 'job_00000000000000000000000001',
          workerSessionId: 'wsn_00000000000000000000000001',
          sessionIdentity: {
            productSessionId: 'psn_00000000000000000000000001',
            workerSessionId: 'wsn_00000000000000000000000001',
            codexThreadId: 'cdx_00000000000000000000000001',
          },
        },
        mode: 'single_choice',
        prompt: 'Choose one identical public label.',
        options: duplicateChoices.map(option => ({ ...option })),
        allowEmpty: false,
        expiresAt: '2030-01-01T01:00:00.000Z',
      },
      expired: false,
    }],
    approvals: [],
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
  async provideInput(id, value) { calls.push({ id, value }) },
  async cancelInput() {},
  async decideApproval() {},
  async resolveAttention() {},
  cancelPending() {},
  reconnect() {},
  close() {},
}

const mounted = mountLocalDecisionsPage({ root, model })

globalThis.runLocalDecisionsChoiceScenario = async () => {
  const before = [...document.querySelectorAll('.wwc-local-input-option')]
  state = pageState()
  listener(state)
  const after = [...document.querySelectorAll('.wwc-local-input-option')]
  after[1].click()
  await Promise.resolve()
  const result = {
    labels: after.map(option => option.textContent),
    stableAcrossRefresh: after.every((option, index) => option === before[index]),
    submitted: calls.at(-1) ?? null,
  }
  mounted.close()
  return result
}
