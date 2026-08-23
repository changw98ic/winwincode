import assert from 'node:assert/strict'
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { DELIVERY_SCHEMA_VERSION } from '../packages/contracts/dist/index.js'
import {
  DeliveryStore,
  DeliveryStoreError,
} from '../packages/strongflow/dist/index.js'

const REQUEST_A = 'a'.repeat(64)
const REQUEST_B = 'b'.repeat(64)

function snapshot(revision = 1, status = 'draft') {
  const deliveryId = 'delivery-store-main'
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision,
    status,
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-store-spec-v1',
      deliveryId,
      revision: 1,
      title: 'Durable delivery',
      goal: 'Persist one canonical Delivery without copying runtime state.',
      scope: ['Delivery state'],
      outOfScope: ['Codex runtime events'],
      constraints: ['Append only'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'criterion-durable',
        description: 'Restart reconstructs the same delivery.',
        verificationMethod: 'Reopen the store and compare the projection.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/repository',
      },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
      createdAtMillis: 1_800_000_000_000,
    },
    tasks: [],
    stageRuns: [],
    sessionBindings: [],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: 1_800_000_000_000,
    updatedAtMillis: 1_800_000_000_000 + revision,
  }
}

async function fixture(t, name) {
  const root = await mkdtemp(join(tmpdir(), `winwincode-delivery-store-${name}-`))
  t.after(() => rm(root, { recursive: true, force: true }))
  return root
}

function expectStoreError(code) {
  return error => error instanceof DeliveryStoreError && error.code === code
}

test('DeliveryStore creates, reopens, and replays one exact snapshot chain', async t => {
  const home = await fixture(t, 'replay')
  const store = await DeliveryStore.create({
    home,
    requestId: 'create-delivery',
    requestDigest: REQUEST_A,
    snapshot: snapshot(),
  })
  const appended = await store.append({
    requestId: 'update-spec',
    requestDigest: REQUEST_B,
    operation: 'delivery.spec.updated',
    expectedRevision: 1,
    snapshot: snapshot(2, 'ready'),
  })
  assert.equal(appended.replayed, false)
  assert.equal(appended.snapshot.revision, 2)

  const reopened = await DeliveryStore.open(home, 'delivery-store-main')
  const stored = await reopened.read()
  assert.equal(stored.records.length, 2)
  assert.equal(stored.records[0].previousDigest, null)
  assert.equal(stored.records[1].previousDigest, stored.records[0].digest)
  assert.deepEqual(stored.snapshot, appended.snapshot)
  assert.ok(Object.isFrozen(stored.snapshot))
})

test('DeliveryStore replays one request and rejects conflicting reuse or revision', async t => {
  const home = await fixture(t, 'idempotency')
  const store = await DeliveryStore.create({
    home,
    requestId: 'create-delivery',
    requestDigest: REQUEST_A,
    snapshot: snapshot(),
  })
  const mutation = {
    requestId: 'update-spec',
    requestDigest: REQUEST_B,
    operation: 'delivery.spec.updated',
    expectedRevision: 1,
    snapshot: snapshot(2, 'ready'),
  }
  const first = await store.append(mutation)
  const replay = await store.append(mutation)
  assert.equal(first.replayed, false)
  assert.equal(replay.replayed, true)
  assert.deepEqual(replay.snapshot, first.snapshot)
  await assert.rejects(
    store.append({ ...mutation, requestDigest: REQUEST_A }),
    expectStoreError('REQUEST_CONFLICT'),
  )
  await assert.rejects(
    store.append({
      requestId: 'stale-update',
      requestDigest: REQUEST_A,
      operation: 'delivery.spec.updated',
      expectedRevision: 1,
      snapshot: snapshot(3, 'ready'),
    }),
    expectStoreError('REVISION_CONFLICT'),
  )
})

test('competing DeliveryStore instances publish one revision', async t => {
  const home = await fixture(t, 'concurrency')
  await DeliveryStore.create({
    home,
    requestId: 'create-delivery',
    requestDigest: REQUEST_A,
    snapshot: snapshot(),
  })
  const left = await DeliveryStore.open(home, 'delivery-store-main')
  const right = await DeliveryStore.open(home, 'delivery-store-main')
  const results = await Promise.allSettled([
    left.append({
      requestId: 'left-update',
      requestDigest: REQUEST_A,
      operation: 'delivery.spec.updated',
      expectedRevision: 1,
      snapshot: snapshot(2, 'ready'),
    }),
    right.append({
      requestId: 'right-update',
      requestDigest: REQUEST_B,
      operation: 'delivery.spec.updated',
      expectedRevision: 1,
      snapshot: snapshot(2, 'ready'),
    }),
  ])
  assert.equal(results.filter(result => result.status === 'fulfilled').length, 1)
  const rejected = results.find(result => result.status === 'rejected')
  assert.ok(rejected)
  assert.ok(expectStoreError('REVISION_CONFLICT')(rejected.reason))
  assert.equal((await left.read()).records.length, 2)
})

test('DeliveryStore ignores pending files and rejects a changed durable record', async t => {
  const home = await fixture(t, 'corruption')
  const store = await DeliveryStore.create({
    home,
    requestId: 'create-delivery',
    requestDigest: REQUEST_A,
    snapshot: snapshot(),
  })
  await writeFile(join(store.recordsDirectory, '.pending-2-acde.json'), '{}\n')
  assert.equal((await store.read()).snapshot.revision, 1)

  const [recordName] = (await readdir(store.recordsDirectory)).filter(name => name === '1.json')
  assert.ok(recordName)
  const recordPath = join(store.recordsDirectory, recordName)
  const record = JSON.parse(await readFile(recordPath, 'utf8'))
  record.snapshot.status = 'ready'
  await writeFile(recordPath, `${JSON.stringify(record)}\n`)
  await assert.rejects(store.read(), expectStoreError('STORE_CORRUPT'))
})
