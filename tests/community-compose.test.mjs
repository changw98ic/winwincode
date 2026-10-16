import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

async function unusedPort() {
  const server = createServer()
  await new Promise(resolvePromise => server.listen(0, '127.0.0.1', resolvePromise))
  const port = server.address().port
  await new Promise(resolvePromise => server.close(resolvePromise))
  return port
}

test('Compose serves Server and Web beside the host Device', async () => {
  const compose = await readFile(resolve(root, 'compose.yaml'), 'utf8')
  assert.match(compose, /^\s{2}web:\s*$/mu)
  assert.match(compose, /^\s{2}backend:\s*$/mu)
  assert.doesNotMatch(compose, /^\s{2}(?:client|device|worker):\s*$/mu)
  assert.match(compose, /127\.0\.0\.1:\$\{WWC_WEB_PORT:-8080\}:8080/u)
  assert.match(compose, /WWC_SERVER_BIND: 0\.0\.0\.0:8443/u)
  assert.match(compose, /127\.0\.0\.1:\$\{WWC_SERVER_PORT:-8443\}:8443/u)
  assert.match(compose, /WWC_SERVER_AUTH_MODE: local-open/u)
  assert.match(compose, /WWC_SERVER_URL: http:\/\/127\.0\.0\.1:\$\{WWC_SERVER_PORT:-8443\}/u)
  assert.match(compose, /\$\{WWC_SERVER_REPOSITORY_ROOT:-\.\}:\/workspace\/repository/u)
  assert.doesNotMatch(compose, /secrets:|TLS_|CREDENTIAL|WORKER_MODE|repository-data/u)
  const web = await readFile(resolve(root, 'deploy/Dockerfile.web'), 'utf8')
  assert.match(web, /corepack pnpm build:ts/u)
  assert.match(web, /WWC_CLIENT_DIST=\/app\/public/u)
})

test('static Web server supplies a usable local origin and deployment override', async t => {
  for (const configured of [undefined, 'http://127.0.0.1:9443']) {
    await t.test(configured ?? 'default', async subtest => {
      const port = await unusedPort()
      const env = { ...process.env, PORT: String(port), HOST: '127.0.0.1' }
      delete env.WWC_SERVER_URL
      if (configured !== undefined) env.WWC_SERVER_URL = configured
      const child = spawn(process.execPath, ['deploy/serve-client.mjs'], {
        cwd: root, env, stdio: ['ignore', 'pipe', 'pipe'],
      })
      subtest.after(() => { child.kill('SIGTERM') })
      await new Promise((resolvePromise, reject) => {
        child.stdout.once('data', resolvePromise)
        child.once('error', reject)
        child.once('exit', code => reject(new Error(`Web server exited: ${code}`)))
      })
      const response = await fetch(`http://127.0.0.1:${port}/runtime-config.js`)
      assert.equal(response.status, 200)
      assert.match(response.headers.get('Content-Security-Policy'), /connect-src/u)
      assert.ok((await response.text()).includes(configured ?? 'http://127.0.0.1:8443'))
      assert.equal((await fetch(`http://127.0.0.1:${port}/health`)).status, 200)
    })
  }
})
