import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, readdirSync, rmSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const [bindingPath, helperExecutable, requestedIterations = '1'] = process.argv.slice(2)
if (bindingPath === undefined || helperExecutable === undefined) {
  throw new Error('binding and helper paths are required')
}
const iterations = Number(requestedIterations)
if (!Number.isSafeInteger(iterations) || iterations < 1 || iterations > 32) {
  throw new TypeError('native close iterations must be an integer from 1 to 32')
}

const require = createRequire(import.meta.url)
const binding = require(bindingPath)
for (let iteration = 0; iteration < iterations; iteration += 1) {
  const root = mkdtempSync(join(tmpdir(), 'winwincode-native-close-'))
  const home = join(root, 'home')
  const cwd = join(root, 'workspace')
  mkdirSync(cwd)

  const kernel = new binding.NativeKernel(
    { home, helperExecutable },
    () => new ReadableStream(),
    () => undefined,
  )
  try {
    const session = await kernel.createSession({
      cwd,
      provider: 'fixture',
      model: 'fixture-model',
    })
    await kernel.closeSession(session.sessionId)
    await kernel.shutdown()
    assert.deepEqual(
      readdirSync(home).filter(name => name.endsWith('-shm') || name.endsWith('-wal')),
      [],
      'native shutdown must close every SQLite pool before its home can be removed',
    )
  } finally {
    rmSync(root, { force: true, recursive: true })
  }
  globalThis.gc?.()
}

process.stdout.write(`${JSON.stringify({ iterations, status: 'clean' })}\n`)
