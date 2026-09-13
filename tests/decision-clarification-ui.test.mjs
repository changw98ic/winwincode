import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

import { findByClass, TrackedDocument } from './fixtures/ui601-keyed-dom.mjs'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.decision-clarification-tests.json',
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
  `Decision clarification area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/decision-clarification-tests')
async function cachedModule(name) {
  return import(`${pathToFileURL(resolve(cache, name)).href}?run=${String(Date.now())}`)
}

const viewModule = await cachedModule('decision-clarification-view-model.js')
const pageModule = await cachedModule('decision-clarification-page.js')
const {
  createClarificationViewModel,
  validateClarificationComponent,
  estimateDisplayText,
  missingRequiredFields,
  containsIllegalMarkup,
} = viewModule
const { mountDecisionClarificationPage } = pageModule

function flush() {
  return new Promise(resolvePromise => setTimeout(resolvePromise, 0))
}

function formComponent(overrides = {}) {
  return {
    kind: 'form',
    id: 'c_scope',
    title: '范围澄清',
    fields: [
      {
        id: 'f_paths',
        label: '允许路径',
        required: true,
        allowUnknown: true,
        allowLater: true,
        maxLength: 200,
      },
    ],
    ...overrides,
  }
}

test('DEC-01 rejects unknown kinds and illegal markup', () => {
  assert.equal(containsIllegalMarkup('<script>x</script>'), true)
  assert.equal(containsIllegalMarkup('普通文本'), false)
  assert.equal(
    validateClarificationComponent({ kind: 'iframe', id: 'x', title: 'x' }),
    'unknown-component-kind',
  )
  assert.equal(
    validateClarificationComponent(formComponent({ title: '<img onerror=alert(1)>' })),
    'illegal-html',
  )
  assert.equal(
    validateClarificationComponent({ kind: 'action', id: 'a', title: '运行' }),
    'unknown-action',
  )
  assert.equal(validateClarificationComponent(formComponent()), null)
})

test('DEC-02 missing required fields block only high-risk submit', () => {
  const model = createClarificationViewModel({
    highRiskComponentIds: ['c_scope'],
    nextRequestId: () => 'req_1',
  })
  model.load([formComponent()])
  assert.equal(model.state.status, 'editing')
  model.submit({
    userId: 'u1',
    productSessionId: 'psn_1',
    deliveryRevision: 1,
    candidateRef: null,
  })
  assert.equal(model.state.status, 'editing')
  if (model.state.status === 'editing') {
    assert.equal(model.state.blockedHighRisk, true)
    assert.match(model.state.notice ?? '', /必填/)
  }
  model.markUnknown('f_paths')
  model.submit({
    userId: 'u1',
    productSessionId: 'psn_1',
    deliveryRevision: 1,
    candidateRef: null,
  })
  assert.equal(model.state.status, 'submitted')
})

test('DEC-03 estimates are labeled and never bare facts', () => {
  assert.match(
    estimateDisplayText({ value: '3d', label: 'unverified-model-estimate' }),
    /模型估算，未验证/,
  )
  assert.match(
    estimateDisplayText({ value: '2h', label: 'measured' }),
    /已测量/,
  )
})

test('DEC-07 offline draft cannot submit; later/unknown answer required fields', () => {
  const model = createClarificationViewModel({
    highRiskComponentIds: ['c_scope'],
    nextRequestId: () => 'req_1',
  })
  model.load([formComponent()])
  model.setOffline(true)
  model.markLater('f_paths')
  model.submit({
    userId: 'u1',
    productSessionId: 'psn_1',
    deliveryRevision: 1,
    candidateRef: null,
  })
  assert.equal(model.state.status, 'editing')
  if (model.state.status === 'editing') {
    assert.match(model.state.notice ?? '', /离线/)
  }
})

test('page renders comparison, criteria, estimate labels, and progressive entry', async () => {
  const document = new TrackedDocument()
  const rootElement = document.createElement('div')
  const model = createClarificationViewModel({ nextRequestId: () => 'req_1' })
  model.load([
    {
      kind: 'comparison',
      id: 'c_cmp',
      title: '方案 A vs B',
      rows: [
        { id: 'r1', label: '范围', left: '仅 Community', right: '三仓' },
      ],
      estimate: { value: '3d', label: 'unverified-model-estimate' },
    },
    {
      kind: 'form',
      id: 'c_acc',
      title: '验收条件',
      criteria: [
        {
          id: 'ac1',
          title: '非法组件被拒绝',
          verificationMethod: '单元测试',
          required: true,
        },
      ],
      fields: [],
    },
  ])
  const page = mountDecisionClarificationPage({ root: rootElement, model })
  await flush()

  const estimate = findByClass(rootElement, 'wwc-decision-clarification-estimate')
  assert.ok(estimate)
  assert.match(estimate.textContent, /模型估算，未验证/)
  const comparison = findByClass(rootElement, 'wwc-decision-clarification-comparison-row')
  assert.ok(comparison)
  assert.match(comparison.textContent, /仅 Community/)
  const criterion = findByClass(rootElement, 'wwc-decision-clarification-criterion')
  assert.match(criterion.textContent, /必须/)
  const advanced = findByClass(rootElement, 'wwc-decision-clarification-advanced')
  assert.ok(advanced)
  page.close()
  assert.equal(rootElement.childNodes.length, 0)
})

test('missingRequiredFields ignores later/unknown answers', () => {
  const component = formComponent()
  assert.deepEqual(
    missingRequiredFields(component, [
      { fieldId: 'f_paths', text: null, choiceIds: [], unknown: false, later: true },
    ]),
    [],
  )
  assert.deepEqual(
    missingRequiredFields(component, [
      { fieldId: 'f_paths', text: '  ', choiceIds: [], unknown: false, later: false },
    ]),
    ['f_paths'],
  )
})
