import { mkdtempSync, mkdirSync, rmSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const [bindingPath, helperExecutable] = process.argv.slice(2)
if (bindingPath === undefined || helperExecutable === undefined) {
  throw new Error('binding and helper paths are required')
}

const require = createRequire(import.meta.url)
const binding = require(bindingPath)
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
} finally {
  rmSync(root, { force: true, recursive: true })
}
