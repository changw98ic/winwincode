import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

import { findByClass, TrackedDocument } from './fixtures/ui601-keyed-dom.mjs'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  ['pnpm', 'exec', 'tsc', '-p', 'apps/client/tsconfig.decision-clarification-tests.json', '--pretty', 'false', '--incremental', 'false'],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(compiler.status, 0, `Decision clarification area did not compile:\n${compiler.stdout}${compiler.stderr}`)

const cache = resolve(root, '.cache/decision-clarification-tests')
const viewModule = await import(pathToFileURL(resolve(cache, 'decision-clarification-view-model.js')).href)
const pageModule = await import(pathToFileURL(resolve(cache, 'decision-clarification-page.js')).href)
const { clarificationUpdateCommand, createClarificationViewModel } = viewModule
const { mountDecisionClarificationPage } = pageModule

const deliveryId = 'dlv_00000000000000000000000001'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function values(overrides = {}) {
  return {
    title: 'Community task',
    goal: 'Ship the task',
    scope: 'apps/client\ncrates/server',
    outOfScope: 'cloud',
    constraints: 'no fallback',
    acceptanceCriteria: JSON.stringify([{
      id: 'criterion-one',
      title: 'Focused test passes',
      required: true,
      verificationMethod: 'node --test',
    }]),
    ...overrides,
  }
}

function snapshot(overrides = {}) {
  return {
    deliveryId,
    deliveryRevision: 4,
    deliverySpecRevision: 2,
    actorId: actor.id,
    productSessionId: 'psn_00000000000000000000000001',
    candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
    baseRevision: 'main',
    publicationTarget: null,
    values: values(),
    ...overrides,
  }
}

function modelFor(port, ids = ['req_00000000000000000000000001']) {
  let request = 0
  let criterion = 0
  return createClarificationViewModel({
    port,
    nextRequestId: () => ids[Math.min(request++, ids.length - 1)],
    nextCriterionId: () => `crt_0000000000000000000000000${++criterion}`,
  })
}

test('builds the canonical delivery.update_spec command and preserves server-owned bindings', () => {
  const source = snapshot()
  const command = clarificationUpdateCommand({
    source,
    expectedRevision: 4,
    requestId: 'req_00000000000000000000000001',
    values: values({
      title: '  Edited title  ',
      scope: 'apps/client\napps/client\n packages/ui ',
      acceptanceCriteria: JSON.stringify([{
        id: 'criterion-one', title: '  Browser check  ', required: true,
        verificationMethod: 'must not enter command',
      }]),
    }),
  }, actor, scope)

  assert.equal(command.command, 'delivery.update_spec')
  assert.equal(command.expectedRevision, 4)
  assert.equal(command.actor, actor)
  assert.equal(command.scope, scope)
  assert.deepEqual(command.payload.spec.scope, ['apps/client', 'packages/ui'])
  assert.equal(command.payload.spec.repositoryId, scope.repositoryId)
  assert.equal(command.payload.spec.sourceProductSessionId, source.productSessionId)
  assert.deepEqual(command.payload.spec.acceptanceCriteria, [{
    id: 'criterion-one', title: 'Browser check', required: true,
  }])
})

test('autosaves page edits in one draft and prevents duplicate in-flight submission', async () => {
  let finishSave
  const saves = []
  const port = {
    async load() { return snapshot() },
    save(input) {
      saves.push(input)
      return new Promise(resolvePromise => { finishSave = resolvePromise })
    },
  }
  const model = modelFor(port)
  await model.start()
  model.edit('goal', 'Edited in browser')
  model.addCriterion()
  model.updateCriterion('crt_00000000000000000000000001', { title: 'New acceptance', required: true })

  const first = model.submit()
  const duplicate = model.submit()
  assert.equal(saves.length, 1)
  assert.equal(saves[0].expectedRevision, 4)
  assert.equal(saves[0].values.goal, 'Edited in browser')
  assert.equal(JSON.parse(saves[0].values.acceptanceCriteria).length, 2)

  finishSave(snapshot({
    deliveryRevision: 5,
    deliverySpecRevision: 3,
    values: saves[0].values,
  }))
  await Promise.all([first, duplicate])
  assert.equal(model.state.dirty, false)
  assert.equal(model.state.lastRequestId, 'req_00000000000000000000000001')
  assert.match(model.state.notice, /规范修订 3/)
})

test('revision conflicts retain the browser draft until the user resolves them', async () => {
  let loads = 0
  const port = {
    async load() {
      loads += 1
      return loads === 1
        ? snapshot()
        : snapshot({ deliveryRevision: 5, values: values({ goal: 'Edited by another window' }) })
    },
    async save() { throw { code: 'REVISION_CONFLICT' } },
  }
  const model = modelFor(port)
  await model.start()
  model.edit('goal', 'My browser draft')
  await model.submit()

  assert.equal(model.state.values.goal, 'My browser draft')
  assert.equal(model.state.conflicts.length, 1)
  assert.match(model.state.notice, /其他窗口/)
  model.resolveConflicts('use-server')
  assert.equal(model.state.values.goal, 'Edited by another window')
  assert.equal(model.state.conflicts.length, 0)
})

test('offline and unresolved values never submit an authorized revision', async () => {
  let saves = 0
  const port = {
    async load() { return snapshot() },
    async save() { saves += 1; return snapshot() },
  }
  const model = modelFor(port)
  await model.start()
  model.edit('goal', 'Changed')
  model.setOffline(true)
  await model.submit()
  assert.equal(saves, 0)
  assert.match(model.state.notice, /离线/)

  model.setOffline(false)
  model.markLater('goal')
  await model.submit()
  assert.equal(saves, 0)
  assert.match(model.state.notice, /稍后补充/)
})

test('page uses labeled native controls and exposes real revision impact', async () => {
  const document = new TrackedDocument()
  const rootElement = document.createElement('div')
  const model = modelFor({
    async load() { return snapshot() },
    async save(input) { return snapshot({ deliveryRevision: 5, values: input.values }) },
  })
  await model.start()
  const page = mountDecisionClarificationPage({ root: rootElement, model, backHref: '#/home' })

  assert.equal(findByClass(rootElement, 'wwc-decision-clarification-heading').textContent, '编辑需求与验收')
  assert.match(findByClass(rootElement, 'wwc-decision-clarification-binding').textContent, /交付修订 4/)
  assert.match(findByClass(rootElement, 'wwc-decision-clarification-impact-warning').textContent, /保留为历史记录/)
  const goal = document.elements.find(element => element.id === 'wwc-decision-clarification-goal')
  assert.equal(goal.tagName, 'TEXTAREA')
  assert.equal(goal.required, true)
  goal.value = 'Keyboard-editable goal'
  goal.emit('input')
  assert.equal(model.state.values.goal, 'Keyboard-editable goal')
  assert.equal(findByClass(rootElement, 'wwc-decision-clarification-submit').disabled, false)

  page.close()
  assert.equal(rootElement.childNodes.length, 0)
})
