import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { DeliveryRequestReplayFixture } from './fixtures/delivery-request-replay-fixture.mjs'

test('review: cleanup must not delete a caller-provided root', async () => {
  const externalRoot = await mkdtemp(join(tmpdir(), 'q0d-review-external-root-'))
  const sentinel = join(externalRoot, 'caller-sentinel.txt')
  await writeFile(sentinel, 'caller data that must survive fixture cleanup\n')
  const kit = await DeliveryRequestReplayFixture.create({ root: externalRoot })
  try {
    await kit.cleanup()
    assert.equal(existsSync(sentinel), true, 'cleanup deleted a caller-provided root')
    assert.equal(existsSync(externalRoot), true, 'cleanup deleted a caller-provided root')
  } finally {
    await rm(externalRoot, { recursive: true, force: true })
  }
})

test('review: cleanup removes an owned root once, even when called concurrently', async () => {
  const before = new Set(await readdir(tmpdir()))
  const kit = await DeliveryRequestReplayFixture.create()
  const prefix = 'winwincode-delivery-replay-'
  try {
    assert.equal(kit.root.startsWith(join(tmpdir(), prefix)), true)
    assert.equal(existsSync(kit.root), true)
    await Promise.all([kit.cleanup(), kit.cleanup(), kit.cleanup()])
    assert.equal(existsSync(kit.root), false)
    const leaked = (await readdir(tmpdir())).filter(name => name.startsWith(prefix) && !before.has(name))
    assert.deepEqual(leaked, [])
  } catch (error) {
    await rm(kit.root, { recursive: true, force: true })
    throw error
  }
})

test('review: failed initialization must not leak the temporary root', async () => {
  const prefix = 'winwincode-delivery-replay-'
  const before = new Set(await readdir(tmpdir()))
  const originalPath = process.env.PATH
  let creationError = null
  try {
    process.env.PATH = '/nonexistent-q0d-review-empty-path'
    try {
      await DeliveryRequestReplayFixture.create()
    } catch (error) {
      creationError = error
    }
  } finally {
    process.env.PATH = originalPath
  }
  assert.notEqual(creationError, null, 'expected initialization to fail without git on PATH')
  const leaked = (await readdir(tmpdir())).filter(name => name.startsWith(prefix) && !before.has(name))
  try {
    assert.deepEqual(leaked, [], 'initialization failure leaked its temporary root')
  } finally {
    for (const name of leaked) {
      await rm(join(tmpdir(), name), { recursive: true, force: true })
    }
  }
})
