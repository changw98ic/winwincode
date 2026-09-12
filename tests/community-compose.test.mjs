import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import test from 'node:test'
import { promisify } from 'node:util'

const root = resolve(import.meta.dirname, '..')
const execFileAsync = promisify(execFile)

test('Community compose contains only Web and Backend with persistent state', async () => {
  const compose = await readFile(resolve(root, 'compose.yaml'), 'utf8')
  assert.match(compose, /^\s{2}backend:\s*$/mu)
  assert.match(compose, /^\s{2}web:\s*$/mu)
  assert.doesNotMatch(compose, /^\s{2}(?:client|device|worker):\s*$/mu)
  assert.match(compose, /^\s{2}backend-data:\s*$/mu)
  assert.match(compose, /^\s{2}model-secrets:\s*$/mu)
  assert.match(compose, /^\s{2}repository-data:\s*$/mu)
  assert.match(compose, /SECRET_DIRECTORY: \/var\/lib\/winwincode-secrets/u)
  assert.doesNotMatch(compose, /SECRET_DIRECTORY: \/var\/lib\/winwincode\//u)
  assert.match(compose, /WWC_SERVER_WORKER_MODE: remote/u)
  assert.match(compose, /condition: service_healthy/u)
})

test('container contracts keep runtime configuration and health checks explicit', async () => {
  const [server, web, runtime] = await Promise.all([
    readFile(resolve(root, 'deploy/Dockerfile.server'), 'utf8'),
    readFile(resolve(root, 'deploy/Dockerfile.web'), 'utf8'),
    readFile(resolve(root, 'deploy/serve-client.mjs'), 'utf8'),
  ])
  assert.match(server, /ENTRYPOINT \["\/usr\/local\/bin\/winwincode-server"\]/u)
  assert.match(web, /USER node/u)
  assert.match(runtime, /WWC_SERVER_URL/u)
  assert.match(runtime, /Content-Security-Policy/u)
  assert.match(runtime, /pathname === '\/health'/u)
})

test('remote-only Server image excludes local Codex and Worker implementations', async () => {
  const { stdout } = await execFileAsync(
    'cargo',
    [
      'tree',
      '--locked',
      '-p',
      'winwincode-server',
      '--no-default-features',
      '-e',
      'normal',
    ],
    { cwd: root, maxBuffer: 16 * 1024 * 1024 },
  )
  assert.doesNotMatch(
    stdout,
    /(?:^| )(?:codex-core|winwincode-(?:codex|local|worker)) v/mu,
  )
})
