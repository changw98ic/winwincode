import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm',
  'exec',
  'tsc',
  '-p',
  'apps/client/tsconfig.ui-components-tests.json',
  '--pretty',
  'false',
  '--incremental',
  'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `Keyed collection did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const { mountKeyedCollection } = await import(`${pathToFileURL(resolve(
  root,
  '.cache/ui-components-tests/components/keyed-collection.js',
)).href}?run=${String(Date.now())}`)

class FakeNode {
  constructor(id) {
    this.id = id
  }

  parentNode = null
  value = ''
  selectionStart = 0
  scrollTop = 0

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.childNodes.indexOf(this)
    if (index >= 0) this.parentNode.childNodes.splice(index, 1)
    this.parentNode = null
  }
}

class FakeParent extends FakeNode {
  constructor() { super('parent') }

  childNodes = []

  insertBefore(node, reference) {
    node.remove()
    const index = reference === null ? this.childNodes.length : this.childNodes.indexOf(reference)
    this.childNodes.splice(index < 0 ? this.childNodes.length : index, 0, node)
    node.parentNode = this
    return node
  }
}

test('keyed collection preserves node identity, draft state, focus target, order, and bounds', () => {
  const parent = new FakeParent()
  const removed = []
  let created = 0
  const collection = mountKeyedCollection({
    parent,
    key: item => item.id,
    create(item) {
      created += 1
      return new FakeNode(item.id)
    },
    update(node, item) { node.label = item.label },
    remove(node) { removed.push(node.id) },
  })

  collection.update([{ id: 'a', label: 'A' }, { id: 'b', label: 'B' }])
  const a = collection.node('a')
  const b = collection.node('b')
  b.value = 'dirty draft'
  b.selectionStart = 5
  b.scrollTop = 44
  const activeElement = b

  for (let index = 0; index < 500; index += 1) {
    collection.update([
      { id: 'a', label: `A${String(index)}` },
      { id: 'b', label: `B${String(index)}` },
    ])
  }
  assert.equal(collection.node('a'), a)
  assert.equal(collection.node('b'), activeElement)
  assert.equal(b.value, 'dirty draft')
  assert.equal(b.selectionStart, 5)
  assert.equal(b.scrollTop, 44)
  assert.equal(parent.childNodes.length, 2)
  assert.equal(created, 2)

  collection.update([{ id: 'b', label: 'B last' }, { id: 'a', label: 'A last' }])
  assert.deepEqual(parent.childNodes, [b, a])
  collection.update([{ id: 'b', label: 'B only' }])
  assert.deepEqual(removed, ['a'])
  assert.equal(a.parentNode, null)

  assert.throws(() => {
    collection.update([{ id: 'b', label: 'one' }, { id: 'b', label: 'two' }])
  }, /unique key/u)
  assert.deepEqual(parent.childNodes, [b])

  collection.close()
  assert.deepEqual(removed, ['a', 'b'])
  assert.equal(parent.childNodes.length, 0)
  assert.throws(() => { collection.update([]) }, /closed/u)
})
