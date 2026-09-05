import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.local-candidate-tests.json',
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
  `Local candidate area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/local-candidate-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-model, the card controls, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const viewModelModule = await cachedModule('local-candidate-view-model.js')
const actionsModule = await cachedModule('local-candidate-actions.js')

const {
  ControlPlaneClientError,
  controlPlaneCandidateActionFailure,
  createControlPlaneClientCandidates,
} = facade
const {
  candidateDisplayState,
  candidateDisplayStateText,
  candidateDisplayStateTone,
  candidateResultText,
  candidateResultTone,
  createLocalCandidateViewModel,
  localCandidateConflictSummaryText,
  localCandidatePortFromFacade,
  shortCommitText,
} = viewModelModule
const { mountLocalCandidateCard } = actionsModule

const clientId = '123456789012'
const fullCommit = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
const expectedHead = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

function candidate(overrides = {}) {
  return {
    localCandidateReceiptId: 'lcr_1',
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
    candidateCommit: fullCommit,
    localRefName: 'refs/winwincode/candidates/cand_1',
    state: 'retained',
    createdAt: '2026-09-04T00:00:00.000Z',
    revision: 3,
    branchName: null,
    history: [],
    ...overrides,
  }
}

function applyReceipt(overrides = {}) {
  return {
    localApplyReceiptId: 'lar_1',
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
    targetBranch: 'feature/conflict',
    expectedHead,
    strategy: 'cherry_pick',
    result: 'applied',
    resultingCommit: null,
    conflictArtifactRef: null,
    createdAt: '2026-09-04T00:00:01.000Z',
    revision: 4,
    ...overrides,
  }
}

function candidateError(code, kind = 'server') {
  return new ControlPlaneClientError({
    kind,
    code,
    message: 'candidate rejected',
    requestId: null,
    retryable: false,
  })
}

/** One deterministic candidate port: the list resolves, the actions wait. */
function portFake(initialCandidates) {
  let current = initialCandidates
  const calls = []
  const listCalls = []
  const pending = []
  function track(action, input) {
    calls.push({ action, ...input })
    return new Promise((resolvePromise, rejectPromise) => {
      pending.push({ action, resolve: resolvePromise, reject: rejectPromise })
    })
  }
  return {
    get candidates() { return current },
    set candidates(next) { current = next },
    calls,
    listCalls,
    pending,
    async listCandidates(input) {
      listCalls.push(input.clientId)
      return current
    },
    createBranch(input) { return track('branch', input) },
    apply(input) { return track('apply', input) },
    discard(input) { return track('discard', input) },
  }
}

async function candidateFixture({
  candidates = [candidate()],
  port = null,
  classify = null,
} = {}) {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const resolvedPort = port ?? portFake(candidates)
  const model = createLocalCandidateViewModel({
    port: resolvedPort,
    ...(classify === null ? {} : { classify }),
  })
  const cards = new Map()
  const objectUrls = []
  const revoked = []
  const unsubscribe = model.subscribe(state => {
    const seen = new Set()
    for (const entry of state.candidates) {
      seen.add(entry.candidateRef)
      if (!cards.has(entry.candidateRef)) {
        cards.set(entry.candidateRef, mountLocalCandidateCard({
          document,
          model,
          createObjectUrl: text => {
            objectUrls.push(text)
            return `blob:fake-${String(objectUrls.length)}`
          },
          revokeObjectUrl: url => { revoked.push(url) },
        }))
      }
      cards.get(entry.candidateRef).update(entry)
    }
    for (const [ref, card] of cards) {
      if (!seen.has(ref)) {
        card.close()
        cards.delete(ref)
      }
    }
  })
  await model.refresh(clientId)
  return { rootElement, document, model, port: resolvedPort, cards, objectUrls, revoked, unsubscribe }
}

function cardOf(fixture, index) {
  const entry = fixture.model.state.candidates[index]
  assert.notEqual(entry, undefined, `candidate ${index} is projected`)
  const card = fixture.cards.get(entry.candidateRef)
  assert.notEqual(card, undefined, `card ${index} is mounted`)
  return card
}

class PageElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
    this.listeners = new Map()
    this.dataset = {}
    this.className = ''
    this.disabled = false
    this.hidden = false
    this.tabIndex = 0
    this.title = ''
    this.download = ''
    this.href = ''
    this.id = ''
    this.htmlFor = ''
    this.name = ''
    this.required = false
    this.spellcheck = true
    this.autocomplete = ''
    this.type = ''
    this.value = ''
    this.maxLength = -1
    this.#textContent = ''
    this.parentNode = null
    this.checkValidity = () => true
  }
  #textContent = ''
  get childNodes() { return this.children }
  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }
  append(...children) {
    for (const child of children) child.parentNode = this
    this.children.push(...children)
  }
  replaceChildren(...children) {
    for (const child of this.children) child.parentNode = null
    for (const child of children) child.parentNode = this
    this.children = [...children]
  }
  insertBefore(node, current) {
    this.children = this.children.filter(child => child !== node)
    const index = current === null ? -1 : this.children.indexOf(current)
    if (index < 0) this.children.push(node)
    else this.children.splice(index, 0, node)
    node.parentNode = this
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  remove() {
    const parent = this.parentNode
    if (parent !== null) parent.children = parent.children.filter(child => child !== this)
    this.parentNode = null
  }
  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }
  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(entry => entry !== listener),
    )
  }
  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event)
    return !event.defaultPrevented
  }
  emit(type, event = {}) { this.dispatchEvent({ type, ...event }) }
  click() { this.dispatchEvent({ type: 'click' }) }
}

class PageDocument {
  createElement(tagName) { return new PageElement(this, tagName) }
}

function pageDescendants(node) {
  return [node, ...node.children.flatMap(child => pageDescendants(child))]
}

function hasClass(node, className) {
  return node.className.split(/\s+/u).includes(className)
}

function findOne(scope, className) {
  const node = pageDescendants(scope).find(entry => hasClass(entry, className))
  assert.notEqual(node, undefined, `${className} is mounted`)
  return node
}

function typeInto(input, value) {
  input.value = value
  input.emit('input')
}

function waitFor(predicate, label) {
  return (async () => {
    const deadline = Date.now() + 5_000
    while (Date.now() < deadline) {
      if (predicate()) return
      await new Promise(resolvePromise => setTimeout(resolvePromise, 10))
    }
    assert.fail(`timed out waiting for ${label}`)
  })()
}

test('the Server projection decides every card capability and badge', async () => {
  const { rootElement, model, cards } = await candidateFixture({
    candidates: [
      candidate({ candidateRef: 'cand_retained' }),
      candidate({
        candidateRef: 'cand_branch',
        state: 'branch_created',
        branchName: 'winwincode/cand-1',
      }),
      candidate({ candidateRef: 'cand_applied', state: 'applied' }),
      candidate({ candidateRef: 'cand_discarded', state: 'discarded' }),
      candidate({ candidateRef: 'cand_failed', state: 'failed' }),
      candidate({
        candidateRef: 'cand_conflict',
        history: [applyReceipt({
          candidateRef: 'cand_conflict',
          result: 'merge_conflict',
          resultingCommit: null,
          conflictArtifactRef: 'local-candidate/conflict-1',
        })],
      }),
    ],
  })
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.clientId, clientId)
  assert.equal(cards.size, 6)

  const badgeOf = card => findOne(card.root, 'wwc-candidate-card-state')
  const branchOf = card => findOne(card.root, 'wwc-candidate-card-branch-create')
  const applyOf = card => findOne(card.root, 'wwc-candidate-card-apply')
  const discardOf = card => findOne(card.root, 'wwc-candidate-card-discard')
  const downloadOf = card => findOne(card.root, 'wwc-candidate-card-conflict-download')

  const retained = cardOf({ model, cards }, 0)
  assert.equal(badgeOf(retained).textContent, 'Retained on the device')
  assert.equal(badgeOf(retained).dataset.tone, 'info')
  assert.equal(branchOf(retained).hidden, false)
  assert.equal(branchOf(retained).disabled, false)
  assert.equal(applyOf(retained).hidden, false)
  assert.equal(discardOf(retained).hidden, false)
  assert.equal(downloadOf(retained).hidden, true)

  const branched = cardOf({ model, cards }, 1)
  assert.equal(badgeOf(branched).textContent, 'Local branch created')
  assert.equal(branchOf(branched).hidden, true, 'an existing branch hides the creation entry')
  assert.equal(applyOf(branched).hidden, false)
  assert.equal(discardOf(branched).hidden, false)
  assert.equal(
    findOne(branched.root, 'wwc-candidate-card-branch').textContent,
    'Branch winwincode/cand-1',
  )

  const applied = cardOf({ model, cards }, 2)
  assert.equal(badgeOf(applied).textContent, 'Applied to the target branch')
  assert.equal(badgeOf(applied).dataset.tone, 'success')
  assert.equal(branchOf(applied).hidden, true)
  assert.equal(applyOf(applied).hidden, true)
  assert.equal(discardOf(applied).hidden, true, 'an applied candidate is settled')

  const discarded = cardOf({ model, cards }, 3)
  assert.equal(badgeOf(discarded).textContent, 'Discarded')
  assert.equal(branchOf(discarded).hidden, true)
  assert.equal(applyOf(discarded).hidden, true)
  assert.equal(discardOf(discarded).hidden, true)

  const failed = cardOf({ model, cards }, 4)
  assert.equal(badgeOf(failed).textContent, 'Retention failed')
  assert.equal(badgeOf(failed).dataset.tone, 'danger')
  assert.equal(branchOf(failed).hidden, true)
  assert.equal(applyOf(failed).hidden, true)
  assert.equal(discardOf(failed).hidden, false, 'a failed candidate can still be discarded')

  const conflict = cardOf({ model, cards }, 5)
  assert.equal(badgeOf(conflict).textContent, 'Apply conflict needs attention')
  assert.equal(badgeOf(conflict).dataset.tone, 'warning')
  assert.equal(applyOf(conflict).hidden, false, 'a conflict stays retryable')
  assert.equal(downloadOf(conflict).hidden, false)

  // The card identity carries the full commit in the title and the short
  // form in the copy, and the aria label names the candidate and its state.
  const refLine = findOne(retained.root, 'wwc-candidate-card-ref')
  assert.equal(refLine.textContent, 'Ref cand_retained')
  const commitLine = findOne(retained.root, 'wwc-candidate-card-commit')
  assert.equal(commitLine.textContent, `Commit ${shortCommitText(fullCommit)}`)
  assert.equal(commitLine.title, fullCommit)
  assert.equal(
    retained.root.getAttribute('aria-label'),
    'Candidate cand_retained: Retained on the device',
  )
  assert.equal(rootElement.tagName, 'DIV')
})

test('branch creation submits once and the refreshed snapshot settles the card', async () => {
  const fixture = await candidateFixture()
  const card = cardOf(fixture, 0)
  const branch = findOne(card.root, 'wwc-candidate-card-branch-create')

  branch.click()
  assert.deepEqual(fixture.port.calls, [{
    action: 'branch',
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
  }])
  assert.equal(branch.textContent, 'Creating…')
  assert.equal(branch.disabled, true)
  assert.equal(
    findOne(card.root, 'wwc-candidate-card-actions').getAttribute('aria-busy'),
    'true',
  )
  branch.click()
  branch.click()
  assert.equal(fixture.port.calls.length, 1, 'a second click during flight is never repeated')

  // The branch fact arrives through the next Server snapshot, never from the
  // port result: the refreshed list drives the card copy.
  fixture.port.candidates = [candidate({
    state: 'branch_created',
    branchName: 'winwincode/cand-1',
  })]
  fixture.port.pending[0].resolve({
    candidate: candidate({ state: 'branch_created', branchName: 'winwincode/cand-1' }),
    branchName: 'winwincode/cand-1',
  })
  await waitFor(
    () => findOne(card.root, 'wwc-candidate-card-state').textContent
      === 'Local branch created',
    'the refreshed snapshot reaches the card',
  )
  assert.ok(fixture.port.listCalls.length >= 2, 'the landed action re-read the list')
  assert.equal(findOne(card.root, 'wwc-candidate-card-branch-create').hidden, true,
    'a repeated creation request finds no creation entry anymore')
  assert.equal(
    findOne(card.root, 'wwc-candidate-card-branch').textContent,
    'Branch winwincode/cand-1',
  )
})

test('the dangerous apply names the target branch and expected HEAD first', async () => {
  const fixture = await candidateFixture()
  const card = cardOf(fixture, 0)
  const apply = findOne(card.root, 'wwc-candidate-card-apply')
  const confirm = findOne(card.root, 'wwc-candidate-card-confirm-apply')
  const confirmText = findOne(card.root, 'wwc-candidate-card-confirm-apply-text')
  const branchInput = findOne(card.root, 'wwc-candidate-card-apply-branch')
  const headInput = findOne(card.root, 'wwc-candidate-card-apply-head')
  const accept = findOne(card.root, 'wwc-candidate-card-apply-accept')

  apply.click()
  assert.equal(fixture.port.calls.length, 0, 'the dangerous apply waits for the explicit accept')
  assert.equal(confirm.hidden, false)
  assert.equal(accept.disabled, true, 'an incomplete draft cannot submit')
  assert.equal(
    confirmText.textContent,
    'Applying rewrites the target branch history. Apply cand_1 onto '
      + 'the exact target branch only while its HEAD is still the expected commit.',
  )

  typeInto(branchInput, 'feature/delivery')
  assert.equal(accept.disabled, true, 'the expected HEAD is still missing')
  typeInto(headInput, expectedHead)
  assert.equal(accept.disabled, false)
  assert.equal(
    confirmText.textContent,
    `Apply cand_1 onto branch feature/delivery only while its HEAD is still ${expectedHead}.`,
  )

  accept.click()
  assert.deepEqual(fixture.port.calls, [{
    action: 'apply',
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
    targetBranch: 'feature/delivery',
    expectedHead,
  }])
  assert.equal(apply.textContent, 'Applying…')
  accept.click()
  assert.equal(fixture.port.calls.length, 1, 'the submit is deduplicated while in flight')

  fixture.port.candidates = [candidate({ state: 'applied' })]
  fixture.port.pending[0].resolve(applyReceipt({
    result: 'applied',
    resultingCommit: 'cccccccccccccccccccccccccccccccccccccccc',
  }))
  await waitFor(
    () => findOne(card.root, 'wwc-candidate-card-state').textContent
      === 'Applied to the target branch',
    'the apply lands through the snapshot',
  )
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-apply').hidden, true)
  assert.equal(branchInput.value, '', 'the settled draft is cleared')
  assert.equal(headInput.value, '', 'the settled draft is cleared')
})

test('a failed apply keeps the armed confirmation and the typed draft', async () => {
  const fixture = await candidateFixture()
  const card = cardOf(fixture, 0)
  const confirm = findOne(card.root, 'wwc-candidate-card-confirm-apply')
  const branchInput = findOne(card.root, 'wwc-candidate-card-apply-branch')
  const headInput = findOne(card.root, 'wwc-candidate-card-apply-head')
  const accept = findOne(card.root, 'wwc-candidate-card-apply-accept')

  findOne(card.root, 'wwc-candidate-card-apply').click()
  typeInto(branchInput, 'feature/delivery')
  typeInto(headInput, expectedHead)
  accept.click()
  fixture.port.pending[0].reject(candidateError('RATE_LIMITED'))

  await waitFor(
    () => fixture.model.interaction('cand_1').kind === 'failed',
    'the rejection reaches the interaction',
  )
  assert.equal(
    findOne(card.root, 'wwc-candidate-card-error').textContent,
    'Too many attempts. Wait a moment, then try again.',
  )
  assert.equal(confirm.hidden, false, 'the armed confirmation survives the failure')
  assert.equal(branchInput.value, 'feature/delivery', 'the typed draft survives')
  assert.equal(headInput.value, expectedHead, 'the typed draft survives')

  accept.click()
  assert.equal(fixture.port.calls.length, 2, 'the same explicit accept retries')
  fixture.port.pending[1].resolve(applyReceipt())
  await waitFor(
    () => fixture.model.interaction('cand_1').kind === 'rest',
    'the retry settles',
  )
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-apply').hidden, true)
  assert.equal(findOne(card.root, 'wwc-candidate-card-error').hidden, true)
})

test('Keep drops the armed apply draft without submitting', async () => {
  const fixture = await candidateFixture()
  const card = cardOf(fixture, 0)
  findOne(card.root, 'wwc-candidate-card-apply').click()
  typeInto(findOne(card.root, 'wwc-candidate-card-apply-branch'), 'feature/delivery')
  typeInto(findOne(card.root, 'wwc-candidate-card-apply-head'), expectedHead)

  findOne(card.root, 'wwc-candidate-card-apply-keep').click()
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-apply').hidden, true)
  assert.equal(fixture.port.calls.length, 0)
  assert.equal(fixture.model.interaction('cand_1').kind, 'rest')
  assert.equal(findOne(card.root, 'wwc-candidate-card-apply-branch').value, '')
})

test('discard always asks first, and the confirmed discard settles through the list', async () => {
  const fixture = await candidateFixture()
  const card = cardOf(fixture, 0)
  const discard = findOne(card.root, 'wwc-candidate-card-discard')

  discard.click()
  assert.equal(fixture.port.calls.length, 0)
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-discard').hidden, false)
  assert.equal(
    findOne(card.root, 'wwc-candidate-card-confirm-discard-text').textContent,
    'Discarding removes the retained candidate ref on the device. '
      + 'The candidate can no longer be applied or recovered.',
  )

  findOne(card.root, 'wwc-candidate-card-discard-keep').click()
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-discard').hidden, true)
  assert.equal(fixture.model.interaction('cand_1').kind, 'rest')

  discard.click()
  findOne(card.root, 'wwc-candidate-card-discard-accept').click()
  assert.deepEqual(fixture.port.calls, [{
    action: 'discard',
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
  }])
  assert.equal(discard.textContent, 'Discarding…')

  fixture.port.candidates = [candidate({ state: 'discarded' })]
  fixture.port.pending[0].resolve(candidate({ state: 'discarded' }))
  await waitFor(
    () => findOne(card.root, 'wwc-candidate-card-state').textContent === 'Discarded',
    'the discard lands through the snapshot',
  )
  assert.equal(findOne(card.root, 'wwc-candidate-card-discard').hidden, true)
})

test('a conflict downloads the safe receipt summary and revokes in order', async () => {
  const conflictReceipt = applyReceipt({
    result: 'merge_conflict',
    resultingCommit: null,
    conflictArtifactRef: 'local-candidate/conflict-1',
  })
  const fixture = await candidateFixture({
    candidates: [candidate({ history: [conflictReceipt] })],
  })
  const card = cardOf(fixture, 0)
  const download = findOne(card.root, 'wwc-candidate-card-conflict-download')

  const rows = pageDescendants(card.root)
    .filter(entry => hasClass(entry, 'wwc-candidate-card-history-row'))
  assert.equal(rows.length, 1)
  assert.equal(
    rows[0].textContent,
    'Conflicts must be resolved before this apply can land. '
      + `(feature/conflict @ ${shortCommitText(expectedHead)})`,
  )
  assert.equal(rows[0].dataset.result, 'merge_conflict')
  assert.equal(rows[0].dataset.tone, 'warning')

  assert.equal(download.hidden, false)
  download.click()
  assert.equal(fixture.objectUrls.length, 1)
  assert.deepEqual(fixture.objectUrls[0].split('\n'), [
    'Candidate apply conflict summary',
    'Candidate ref: cand_1',
    `Candidate commit: ${shortCommitText(fullCommit)} (${fullCommit})`,
    'Repository binding: rep_1',
    'Target branch: feature/conflict',
    `Expected HEAD: ${expectedHead}`,
    'Strategy: cherry_pick',
    'Result: merge_conflict',
    'Resulting commit: none',
    'Conflict artifact: local-candidate/conflict-1',
    'Recorded at: 2026-09-04T00:00:01.000Z',
    'Resolve the conflicts on the device, then retry the apply with a fresh expected HEAD.',
  ])
  // The safe summary is receipt fields only: no worktree path, no diff body.
  assert.ok(!fixture.objectUrls[0].includes('/Users/'))
  assert.ok(!fixture.objectUrls[0].includes('\\'))
  assert.equal(fixture.revoked.length, 0)

  download.click()
  assert.equal(fixture.objectUrls.length, 2)
  assert.deepEqual(fixture.revoked, ['blob:fake-1'], 'the previous summary URL is revoked')

  card.close()
  assert.deepEqual(fixture.revoked, ['blob:fake-1', 'blob:fake-2'])
})

test('every apply result carries its own honest copy and tone', async () => {
  const table = [
    ['retained', 'Still retained locally.', 'info'],
    ['branch_created', 'Local branch created.', 'info'],
    ['applied', 'Applied to the target branch.', 'success'],
    ['base_stale', 'The target branch moved ahead. Refresh the expected HEAD and retry.', 'warning'],
    ['working_tree_dirty', 'The target worktree has uncommitted changes. Settle them first.', 'warning'],
    ['merge_conflict', 'Conflicts must be resolved before this apply can land.', 'warning'],
    ['candidate_missing', 'The candidate ref is gone from the device.', 'danger'],
    ['permission_denied', 'You lack permission for the target repository.', 'danger'],
    ['discarded', 'The candidate was discarded.', 'neutral'],
    ['failed', 'The apply failed. Check the device and try again.', 'danger'],
  ]
  for (const [result, copy, tone] of table) {
    assert.equal(candidateResultText(result), copy, result)
    assert.equal(candidateResultTone(result), tone, result)
  }

  // A retryable failure result renders a history row but no download entry:
  // only a live merge conflict offers the safe summary.
  const fixture = await candidateFixture({
    candidates: [candidate({
      history: [applyReceipt({ result: 'base_stale', resultingCommit: null })],
    })],
  })
  const card = cardOf(fixture, 0)
  const rows = pageDescendants(card.root)
    .filter(entry => hasClass(entry, 'wwc-candidate-card-history-row'))
  assert.equal(rows.length, 1)
  assert.equal(
    rows[0].textContent,
    'The target branch moved ahead. Refresh the expected HEAD and retry. '
      + `(feature/conflict @ ${shortCommitText(expectedHead)})`,
  )
  assert.equal(findOne(card.root, 'wwc-candidate-card-conflict-download').hidden, true)
  assert.equal(findOne(card.root, 'wwc-candidate-card-state').textContent, 'Retained on the device')
})

test('a stale projection drops the armed draft without submitting', async () => {
  const fixture = await candidateFixture({
    candidates: [candidate()],
  })
  const card = cardOf(fixture, 0)
  findOne(card.root, 'wwc-candidate-card-apply').click()
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-apply').hidden, false)

  // The Server applies the candidate elsewhere while the draft is armed.
  fixture.port.candidates = [candidate({ state: 'applied' })]
  await fixture.model.refresh(clientId)
  await waitFor(
    () => fixture.model.interaction('cand_1').kind === 'rest',
    'the stale draft is dropped',
  )
  assert.equal(findOne(card.root, 'wwc-candidate-card-confirm-apply').hidden, true)
  assert.equal(findOne(card.root, 'wwc-candidate-card-apply').hidden, true)
  assert.equal(fixture.port.calls.length, 0, 'a stale draft never reaches the port')
})

test('the presentation mapping keeps the Server state names honest', () => {
  assert.equal(candidateDisplayState(candidate({ state: 'retained' })), 'retained')
  assert.equal(candidateDisplayState(candidate({ state: 'branch_created' })), 'branch_created')
  assert.equal(candidateDisplayState(candidate({ state: 'applied' })), 'applied')
  assert.equal(candidateDisplayState(candidate({ state: 'discarded' })), 'discarded')
  assert.equal(candidateDisplayState(candidate({ state: 'failed' })), 'failed')
  const conflicted = candidate({
    history: [applyReceipt({ result: 'merge_conflict' })],
  })
  assert.equal(candidateDisplayState(conflicted), 'conflict')
  assert.equal(candidateDisplayStateText('conflict'), 'Apply conflict needs attention')
  assert.equal(candidateDisplayStateTone('conflict'), 'warning')

  const summary = localCandidateConflictSummaryText(candidate({ history: [] }))
  assert.equal(summary, null, 'a candidate without a conflict has no summary')
})

test('the classifier maps the stable wire codes and stays honest elsewhere', () => {
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('INVALID_REQUEST')),
    'invalid-request',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('RESOURCE_NOT_FOUND')),
    'candidate-not-found',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('CLIENT_NOT_FOUND')),
    'client-not-found',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('CLIENT_OFFLINE')),
    'client-offline',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('PERMISSION_DENIED')),
    'permission-denied',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('ACCESS_DENIED')),
    'permission-denied',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('PERMISSION_DENIED', 'authorization')),
    'permission-denied',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('WRONG_STATE')),
    'wrong-state',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('RATE_LIMITED')),
    'rate-limited',
  )
  assert.equal(
    controlPlaneCandidateActionFailure(candidateError('SOMETHING_ELSE')),
    'unavailable',
    'an unknown wire code stays honestly unavailable',
  )
  assert.equal(controlPlaneCandidateActionFailure(new Error('boom')), 'unavailable')
})

test('a port-less composition stays honest and never submits', async () => {
  const model = createLocalCandidateViewModel({ port: null })
  const unsubscribe = model.subscribe(() => {})
  await model.refresh(clientId)
  assert.equal(model.state.status, 'unavailable')
  assert.equal(model.state.candidates.length, 0, 'no port reads no projection')

  model.requestBranch('cand_1')
  model.requestApply('cand_1')
  model.requestDiscard('cand_1')
  assert.equal(model.interaction('cand_1').kind, 'rest', 'an empty projection never arms')
  model.confirmApply('cand_1', { targetBranch: 'feature/x', expectedHead })
  model.confirmDiscard('cand_1')
  assert.equal(model.interaction('cand_1').kind, 'rest')
  model.close()
  unsubscribe()
})

test('an unknown rejection shows the honest unavailable copy', async () => {
  const fixture = await candidateFixture()
  const card = cardOf(fixture, 0)
  findOne(card.root, 'wwc-candidate-card-apply').click()
  typeInto(findOne(card.root, 'wwc-candidate-card-apply-branch'), 'feature/delivery')
  typeInto(findOne(card.root, 'wwc-candidate-card-apply-head'), expectedHead)
  findOne(card.root, 'wwc-candidate-card-apply-accept').click()
  fixture.port.pending[0].reject(new Error('boom'))

  await waitFor(
    () => fixture.model.interaction('cand_1').kind === 'failed',
    'the rejection reaches the interaction',
  )
  assert.equal(
    findOne(card.root, 'wwc-candidate-card-error').textContent,
    'The request did not go through. Check the connection and try again.',
  )
  assert.equal(
    findOne(card.root, 'wwc-candidate-card-error').getAttribute('role'),
    'alert',
  )
})

test('the facade speaks the receipt shapes over the provisional wire', async () => {
  const schemaVersion = 'winwincode/v1'
  const requests = []
  function response(status, payload = '') {
    return {
      ok: status >= 200 && status < 300,
      status,
      async text() {
        return typeof payload === 'string' ? payload : JSON.stringify(payload)
      },
    }
  }
  const transport = {
    async fetch(input, init) {
      requests.push({ url: String(input), method: init.method, body: init.body ?? null })
      if (String(input).endsWith('/candidates') && init.method === 'GET') {
        return response(200, { schemaVersion, candidates: [candidate()] })
      }
      if (init.method !== 'POST') return response(405)
      if (String(input).endsWith('/candidates/branch')) {
        return response(201, {
          schemaVersion,
          candidate: candidate({ state: 'branch_created', branchName: 'winwincode/cand-1' }),
          branchName: 'winwincode/cand-1',
        })
      }
      if (String(input).endsWith('/candidates/apply')) {
        return response(201, { schemaVersion, receipt: applyReceipt() })
      }
      if (String(input).endsWith('/candidates/discard')) {
        return response(200, { schemaVersion, candidate: candidate({ state: 'discarded' }) })
      }
      return response(404)
    },
  }
  const candidates = createControlPlaneClientCandidates({
    client: { serverUrl: 'https://control.example' },
    transport,
  })

  const listed = await candidates.listDeviceCandidates({ clientId: clientId })
  assert.equal(listed.length, 1)
  assert.deepEqual(listed[0], candidate())
  assert.equal(requests[0].url, `https://control.example/api/v1/clients/${clientId}/candidates`)
  assert.equal(requests[0].method, 'GET')

  const branch = await candidates.createCandidateBranch({
    clientId: '1234 5678 9012',
    candidateRef: ' cand_1 ',
    repositoryBindingId: 'rep_1',
  })
  assert.equal(branch.branchName, 'winwincode/cand-1')
  assert.deepEqual(JSON.parse(requests[1].body), {
    schemaVersion,
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
  })
  assert.equal(requests[1].url, 'https://control.example/api/v1/clients/candidates/branch')

  const receipt = await candidates.applyCandidate({
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
    targetBranch: 'feature/delivery',
    expectedHead,
  })
  assert.deepEqual(receipt, applyReceipt())
  assert.deepEqual(JSON.parse(requests[2].body), {
    schemaVersion,
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
    targetBranch: 'feature/delivery',
    expectedHead,
  })

  const discarded = await candidates.discardCandidate({
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
  })
  assert.equal(discarded.state, 'discarded')
  assert.equal(requests[3].url, 'https://control.example/api/v1/clients/candidates/discard')

  // The facade owns the input bounds, so a broken draft never reaches the wire.
  await assert.rejects(
    candidates.applyCandidate({
      clientId,
      candidateRef: 'cand_1',
      repositoryBindingId: 'rep_1',
      targetBranch: 'feature/delivery',
      expectedHead: 'not-a-sha',
    }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.code, 'CLIENT_CANDIDATE_EXPECTED_HEAD_INVALID')
      return true
    },
  )
  await assert.rejects(
    candidates.listDeviceCandidates({ clientId: '12' }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.kind, 'protocol')
      return true
    },
  )
  assert.equal(requests.length, 4, 'a rejected input never opened a request')

  // A rejection surfaces through the one error identity and the classifier.
  const failing = createControlPlaneClientCandidates({
    client: { serverUrl: 'https://control.example' },
    transport: {
      async fetch() {
        return response(409, {
          schemaVersion,
          requestId: 'req_1',
          error: { code: 'WRONG_STATE', message: 'superseded', retryable: false, details: {} },
        })
      },
    },
  })
  await assert.rejects(
    failing.discardCandidate({
      clientId,
      candidateRef: 'cand_1',
      repositoryBindingId: 'rep_1',
    }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.code, 'WRONG_STATE')
      assert.equal(controlPlaneCandidateActionFailure(error), 'wrong-state')
      return true
    },
  )

  // A drifted result code fails closed as a protocol error.
  const drifted = createControlPlaneClientCandidates({
    client: { serverUrl: 'https://control.example' },
    transport: {
      async fetch() {
        return response(201, {
          schemaVersion,
          receipt: applyReceipt({ result: 'teleported' }),
        })
      },
    },
  })
  await assert.rejects(
    drifted.applyCandidate({
      clientId,
      candidateRef: 'cand_1',
      repositoryBindingId: 'rep_1',
      targetBranch: 'feature/delivery',
      expectedHead,
    }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.code, 'INVALID_CLIENT_CANDIDATES_RESPONSE')
      return true
    },
  )
})

test('the facade adapter maps the candidate facade and rejects incomplete ones', async () => {
  const calls = []
  const port = localCandidatePortFromFacade({
    listDeviceCandidates: async input => {
      calls.push(['list', input.clientId])
      return [candidate()]
    },
    createCandidateBranch: async input => {
      calls.push(['branch', input.candidateRef])
      return { candidate: candidate(), branchName: 'winwincode/cand-1' }
    },
    applyCandidate: async input => {
      calls.push(['apply', input.expectedHead])
      return applyReceipt()
    },
    discardCandidate: async input => {
      calls.push(['discard', input.candidateRef])
      return candidate({ state: 'discarded' })
    },
  })
  assert.notEqual(port, null)
  assert.deepEqual(await port.listCandidates({ clientId }), [candidate()])
  await port.createBranch({ clientId, candidateRef: 'cand_1', repositoryBindingId: 'rep_1' })
  await port.apply({
    clientId,
    candidateRef: 'cand_1',
    repositoryBindingId: 'rep_1',
    targetBranch: 'feature/delivery',
    expectedHead,
  })
  await port.discard({ clientId, candidateRef: 'cand_1', repositoryBindingId: 'rep_1' })
  assert.deepEqual(calls, [
    ['list', clientId],
    ['branch', 'cand_1'],
    ['apply', expectedHead],
    ['discard', 'cand_1'],
  ])

  assert.equal(localCandidatePortFromFacade({ listDeviceCandidates: async () => [] }), null)
  const partial = localCandidatePortFromFacade({
    listDeviceCandidates: async () => [],
    createCandidateBranch: async () => ({}),
    applyCandidate: async () => ({}),
  })
  assert.equal(partial, null, 'a facade missing the discard seam composes nothing')

  // The frozen decorator reuses an injected implementation verbatim, so a
  // fake facade keeps its single seam through the wrapper.
  const injectedCalls = []
  const base = {
    serverUrl: 'https://control.example',
    async listDeviceCandidates(input) {
      injectedCalls.push(input.clientId)
      return [candidate()]
    },
    close() {},
  }
  const wrapped = createControlPlaneClientCandidates({ client: base })
  await wrapped.listDeviceCandidates({ clientId: '1234 5678 9012' })
  assert.deepEqual(injectedCalls, [clientId], 'the injected seam receives the normalized input')
})
