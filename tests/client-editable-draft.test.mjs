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
    'apps/client/tsconfig.strongflow-page-tests.json',
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
  `Editable draft state did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const { createEditableDraft } = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/editable-draft.js',
)).href}`)

const snapshot = (revision, values, scope = 'scope-a') => ({ scope, revision, values })

test('editable draft merges clean fields and reports conflicting dirty fields', () => {
  const draft = createEditableDraft()
  draft.synchronize(snapshot(1, { provider: 'server-a', model: 'model-a' }))
  draft.edit('provider', 'browser-draft')

  draft.synchronize(snapshot(2, { provider: 'server-b', model: 'model-b' }))

  assert.deepEqual(draft.state.values, {
    provider: 'browser-draft',
    model: 'model-b',
  })
  assert.deepEqual(draft.state.dirtyFields, ['provider'])
  assert.deepEqual(draft.state.conflicts, [{
    field: 'provider',
    baseValue: 'server-a',
    serverValue: 'server-b',
    draftValue: 'browser-draft',
  }])

  draft.resolveConflicts('keep-draft')
  assert.deepEqual(draft.state.conflicts, [])
  assert.equal(draft.state.baseRevision, 2)
  assert.equal(draft.state.values.provider, 'browser-draft')

  draft.resolveConflicts('use-server')
  assert.deepEqual(draft.state.values, { provider: 'server-b', model: 'model-b' })
  assert.deepEqual(draft.state.dirtyFields, [])
})

test('a clean server refresh becomes the baseline for edits started afterwards', () => {
  const fieldSensitive = createEditableDraft()
  fieldSensitive.synchronize(snapshot(1, { provider: 'server-a', model: 'model-a' }))
  fieldSensitive.synchronize(snapshot(2, { provider: 'server-a', model: 'model-b' }))
  fieldSensitive.edit('model', 'browser-model')
  fieldSensitive.synchronize(snapshot(2, { provider: 'server-a', model: 'model-b' }))

  assert.equal(fieldSensitive.state.baseRevision, 2)
  assert.deepEqual(fieldSensitive.state.conflicts, [])

  const revisionSensitive = createEditableDraft({ revisionSensitive: true })
  revisionSensitive.synchronize(snapshot(1, { comments: '' }))
  revisionSensitive.synchronize(snapshot(2, { comments: '' }))
  revisionSensitive.edit('comments', 'browser review')
  revisionSensitive.synchronize(snapshot(2, { comments: '' }))

  assert.equal(revisionSensitive.state.baseRevision, 2)
  assert.deepEqual(revisionSensitive.state.conflicts, [])
})

test('editable draft captures one immutable submission and retains it on failure', () => {
  const draft = createEditableDraft()
  draft.synchronize(snapshot(4, { comments: '', changes: '' }))
  draft.edit('comments', 'exact submitted review')
  const submitted = draft.beginSubmission()

  assert.deepEqual(submitted, {
    scope: 'scope-a',
    revision: 4,
    values: { comments: 'exact submitted review', changes: '' },
  })
  assert.throws(() => { submitted.values.comments = 'mutated' }, TypeError)
  draft.synchronize(snapshot(5, { comments: '', changes: '' }))
  assert.equal(draft.state.values.comments, 'exact submitted review')
  assert.equal(draft.state.submission, submitted)

  draft.finishSubmission('failure')
  assert.equal(draft.state.submission, null)
  assert.equal(draft.state.values.comments, 'exact submitted review')
})

test('editable draft clears on confirmed success, deletion, and scope change', () => {
  const draft = createEditableDraft()
  draft.synchronize(snapshot(1, { resolution: '' }))
  draft.edit('resolution', 'browser decision')
  draft.beginSubmission()
  draft.finishSubmission('success')
  assert.deepEqual(draft.state.dirtyFields, [])
  assert.equal(draft.state.values.resolution, '')

  draft.edit('resolution', 'wrong entity draft')
  draft.synchronize(snapshot(1, { resolution: '' }, 'scope-b'))
  assert.equal(draft.state.values.resolution, '')
  assert.deepEqual(draft.state.dirtyFields, [])

  draft.edit('resolution', 'deleted entity draft')
  draft.synchronize(null)
  assert.equal(draft.state.scope, null)
  assert.deepEqual(draft.state.values, {})
})

test('cancel ends submission but retains ordinary browser edits', () => {
  const draft = createEditableDraft()
  draft.synchronize(snapshot(3, { comments: '' }))
  draft.edit('comments', 'retry after cancel')
  draft.beginSubmission()
  draft.finishSubmission('cancelled')

  assert.equal(draft.state.submission, null)
  assert.equal(draft.state.values.comments, 'retry after cancel')
  assert.deepEqual(draft.state.dirtyFields, ['comments'])
})

test('revision-sensitive drafts require acknowledgement without exposing redacted values', () => {
  const draft = createEditableDraft({
    revisionSensitive: true,
    redactFields: ['secret'],
  })
  draft.synchronize(snapshot(7, { secret: '' }))
  draft.edit('secret', 'LOCAL_SECRET')
  draft.synchronize(snapshot(8, { secret: '' }))

  assert.equal(draft.state.revisionConflict, true)
  assert.deepEqual(draft.state.conflicts, [{
    field: 'secret',
    baseValue: '[redacted]',
    serverValue: '[redacted]',
    draftValue: '[redacted]',
  }])
  assert.equal(JSON.stringify(draft.state.conflicts).includes('LOCAL_SECRET'), false)
})
