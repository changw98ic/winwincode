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
    'apps/client/tsconfig.page-annotation-tests.json',
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
  `page annotation area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/page-annotation-tests')
async function cachedModule(name) {
  return import(`${pathToFileURL(resolve(cache, name)).href}?run=${String(Date.now())}`)
}

const viewModule = await cachedModule('page-annotation-view-model.js')
const pageModule = await cachedModule('page-annotation-page.js')
const {
  createPageAnnotationViewModel,
  resolveInjectability,
  surfaceKindFor,
} = viewModule
const { mountPageAnnotationPage } = pageModule

function flush() {
  return new Promise(resolvePromise => setTimeout(resolvePromise, 0))
}

const APP_ORIGIN = 'https://app.example.test'

test('RUN-09 cross-origin degrades to screenshot coordinates', () => {
  const resolved = resolveInjectability({
    appOrigin: APP_ORIGIN,
    pageUrl: 'https://other.example.test/page',
    access: 'authorized',
    injectable: true,
  })
  assert.equal(resolved.kind, 'cross-origin')
  assert.equal(surfaceKindFor(resolved.kind), 'screenshot-coordinate')
  assert.match(resolved.reason ?? '', /跨源/)
})

test('RUN-09 same-origin injectable supports element pick', () => {
  const resolved = resolveInjectability({
    appOrigin: APP_ORIGIN,
    pageUrl: `${APP_ORIGIN}/preview/page`,
    access: 'authorized',
    injectable: true,
  })
  assert.equal(resolved.kind, 'same-origin')
  assert.equal(surfaceKindFor(resolved.kind), 'injectable-element')
})

test('RUN-09 revoked or non-injectable never claims pickable', () => {
  assert.equal(
    resolveInjectability({
      appOrigin: APP_ORIGIN,
      pageUrl: `${APP_ORIGIN}/x`,
      access: 'revoked',
      injectable: true,
    }).kind,
    'revoked',
  )
  assert.equal(
    resolveInjectability({
      appOrigin: APP_ORIGIN,
      pageUrl: `${APP_ORIGIN}/x`,
      access: 'authorized',
      injectable: false,
    }).kind,
    'non-injectable',
  )
})

test('screenshot draft works without element pick', () => {
  const model = createPageAnnotationViewModel({ appOrigin: APP_ORIGIN })
  model.prepare({
    bindingKey: 'candidate-1',
    pageUrl: 'https://cdn.example.test/doc',
    pagePath: '/doc',
    access: 'authorized',
    injectable: true,
    viewport: { width: 1280, height: 800, devicePixelRatio: 1 },
  })
  assert.equal(model.state.status, 'ready')
  if (model.state.status !== 'ready') return
  assert.equal(model.state.surfaceKind, 'screenshot-coordinate')
  model.pickElement({
    tagName: 'a',
    role: 'link',
    accessibleName: 'x',
    locator: 'role=link',
    bounds: { x: 0, y: 0, width: 10, height: 10 },
  })
  if (model.state.status === 'ready') {
    assert.match(model.state.notice ?? '', /截图坐标/)
  }
  model.markScreenshotRegion({ x: 5, y: 5, width: 50, height: 40 })
  model.setComment('这里文案不清楚')
  model.addDraft()
  assert.equal(model.state.status, 'ready')
  if (model.state.status === 'ready') {
    assert.equal(model.state.drafts.length, 1)
    assert.equal(model.state.drafts[0].element, null)
    assert.equal(model.state.drafts[0].surfaceKind, 'screenshot-coordinate')
  }
  model.submit()
  assert.equal(model.state.status, 'submitted')
})

test('element pick draft keeps locator and bounds on injectable surface', () => {
  const model = createPageAnnotationViewModel({
    appOrigin: APP_ORIGIN,
    nextDraftId: () => 'ann_1',
  })
  model.prepare({
    bindingKey: 'candidate-1',
    pageUrl: `${APP_ORIGIN}/preview`,
    pagePath: '/preview',
    access: 'authorized',
    injectable: true,
    viewport: { width: 390, height: 844, devicePixelRatio: 2 },
  })
  model.pickElement({
    tagName: 'button',
    role: 'button',
    accessibleName: '提交',
    locator: 'role=button[name=提交]',
    bounds: { x: 10, y: 20, width: 80, height: 24 },
  })
  model.setComment('按钮文案应更明确')
  model.addDraft()
  assert.equal(model.state.status, 'ready')
  if (model.state.status === 'ready') {
    const draft = model.state.drafts[0]
    assert.equal(draft.id, 'ann_1')
    assert.equal(draft.element?.locator, 'role=button[name=提交]')
    assert.equal(draft.region?.width, 80)
  }
})

test('annotation drafts stay bound to one candidate identity', () => {
  const model = createPageAnnotationViewModel({ appOrigin: APP_ORIGIN })
  const input = {
    bindingKey: 'candidate-1',
    pageUrl: `${APP_ORIGIN}/preview`,
    pagePath: '/preview',
    access: 'authorized',
    injectable: true,
    viewport: { width: 390, height: 844, devicePixelRatio: 2 },
  }
  model.prepare(input)
  model.markScreenshotRegion({ x: 1, y: 1, width: 10, height: 10 })
  model.setComment('候选一批注')
  model.addDraft()
  assert.equal(model.state.status, 'ready')
  if (model.state.status !== 'ready') return
  assert.equal(model.state.drafts.length, 1)
  model.prepare({ ...input, pagePath: '/other' })
  assert.equal(model.state.status, 'ready')
  if (model.state.status !== 'ready') return
  assert.equal(model.state.drafts.length, 1)
  model.prepare({ ...input, bindingKey: 'candidate-2' })
  assert.equal(model.state.status, 'ready')
  if (model.state.status === 'ready') assert.equal(model.state.drafts.length, 0)
})

test('page element pick uses only the host bridge result', () => {
  const document = new TrackedDocument()
  const rootElement = document.createElement('div')
  const model = createPageAnnotationViewModel({ appOrigin: APP_ORIGIN })
  model.prepare({
    bindingKey: 'candidate-1',
    pageUrl: `${APP_ORIGIN}/preview`,
    pagePath: '/preview',
    access: 'authorized',
    injectable: true,
    viewport: { width: 390, height: 844, devicePixelRatio: 2 },
  })
  const page = mountPageAnnotationPage({
    root: rootElement,
    model,
    pickElement: () => ({
      tagName: 'button',
      role: 'button',
      accessibleName: '保存',
      locator: 'role=button[name=保存]',
      bounds: { x: 12, y: 20, width: 80, height: 32 },
    }),
  })
  findByClass(rootElement, 'wwc-page-annotation-pick').emit('click')
  findByClass(rootElement, 'wwc-page-annotation-comment').value = '保留这个按钮'
  findByClass(rootElement, 'wwc-page-annotation-add').emit('click')
  assert.equal(model.state.status, 'ready')
  if (model.state.status === 'ready') {
    assert.equal(model.state.drafts[0].element?.locator, 'role=button[name=保存]')
  }
  page.close()
})

test('page mounts degradation copy for cross-origin', async () => {
  const document = new TrackedDocument()
  const rootElement = document.createElement('div')
  const model = createPageAnnotationViewModel({ appOrigin: APP_ORIGIN })
  model.prepare({
    bindingKey: 'candidate-1',
    pageUrl: 'https://evil.example.test/',
    pagePath: '/',
    access: 'authorized',
    injectable: true,
    viewport: { width: 1280, height: 800, devicePixelRatio: 1 },
  })
  const page = mountPageAnnotationPage({
    root: rootElement,
    model,
  })
  await flush()
  const mode = findByClass(rootElement, 'wwc-page-annotation-mode')
  assert.match(mode.textContent, /降级为截图坐标/)
  const pick = findByClass(rootElement, 'wwc-page-annotation-pick')
  assert.equal(pick.disabled, true)
  const region = findByClass(rootElement, 'wwc-page-annotation-region')
  const coordinates = []
  const collectCoordinates = node => {
    if (node.className === 'wwc-page-annotation-coordinate') coordinates.push(node)
    for (const child of node.children) collectCoordinates(child)
  }
  collectCoordinates(rootElement)
  ;[coordinates[0].value, coordinates[1].value, coordinates[2].value, coordinates[3].value] = ['10', '10', '200', '120']
  region.emit('click')
  const comment = findByClass(rootElement, 'wwc-page-annotation-comment')
  comment.value = '此处需要说明'
  const add = findByClass(rootElement, 'wwc-page-annotation-add')
  add.emit('click')
  await flush()
  const items = []
  const walk = node => {
    if (node.className === 'wwc-page-annotation-item') items.push(node)
    for (const child of node.children) walk(child)
  }
  walk(rootElement)
  assert.equal(items.length, 1)
  assert.match(items[0].textContent, /screenshot-coordinate/)
  page.close()
})

test('page annotation transport restores persisted drafts and advances the catalog revision', async () => {
  const submitted = []
  const persisted = {
    id: 'ann_persisted',
    body: '恢复后的页面批注',
    candidateRef: 'git-candidate:sha256:abc',
    workRunId: 'wrn_1',
    target: {
      pagePath: '/checkout',
      viewport: { width: 390, height: 844, devicePixelRatio: 2 },
      element: null,
      region: { x: 4, y: 8, width: 120, height: 40 },
    },
    updatedAt: '2026-09-18T00:00:00.000Z',
  }
  const model = createPageAnnotationViewModel({
    appOrigin: APP_ORIGIN,
    transport: {
      async list() {
        return { items: [persisted], catalogRevision: 7 }
      },
      async submit(input) {
        submitted.push(input)
        return { annotation: persisted, catalogRevision: 8 }
      },
    },
  })
  model.prepare({
    bindingKey: 'candidate-1',
    pageUrl: `${APP_ORIGIN}/preview`,
    pagePath: '/checkout',
    access: 'authorized',
    injectable: false,
    viewport: { width: 390, height: 844, devicePixelRatio: 2 },
    deliveryId: 'dlv_1',
    candidateRef: persisted.candidateRef,
    workRunId: persisted.workRunId,
  })
  await flush()
  assert.equal(model.state.status, 'ready')
  if (model.state.status !== 'ready') return
  assert.equal(model.state.drafts.length, 1)
  model.submit()
  await flush()
  assert.equal(submitted.length, 1)
  assert.equal(submitted[0].expectedRevision, 7)
  assert.equal(model.state.status, 'submitted')
})
