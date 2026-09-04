import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:https'
import { createServer as createNetServer } from 'node:net'
import { normalize, resolve } from 'node:path'

// Every real-browser test rebuilds the client into the shared
// `apps/client/dist` tree, and a rebuild empties that tree before writing it
// again.  A test that serves that tree while another test rebuilds it can
// therefore observe a missing file for the length of one build.  Waiting a
// bounded time for the file to come back reads the completed artifact instead
// of failing inside the request; an asset that never returns still throws.
const SHARED_CLIENT_READ_TIMEOUT_MILLIS = 60_000
const SHARED_CLIENT_READ_POLL_MILLIS = 25

function waitSync(millis) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, millis)
}

function readSharedClientFile(path) {
  const deadline = Date.now() + SHARED_CLIENT_READ_TIMEOUT_MILLIS
  for (;;) {
    try {
      return readFileSync(path)
    } catch (error) {
      if (error.code !== 'ENOENT' || Date.now() >= deadline) throw error
      waitSync(SHARED_CLIENT_READ_POLL_MILLIS)
    }
  }
}

export function chromeBinary() {
  const candidates = [
    process.env.CHROME_BIN,
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    spawnSync('sh', ['-c', 'command -v google-chrome || command -v chromium'], {
      encoding: 'utf8',
    }).stdout.trim(),
  ]
  return candidates.find(candidate => candidate !== undefined && existsSync(candidate)) ?? null
}

export function command(root, name, args) {
  const result = spawnSync(name, args, { cwd: root, encoding: 'utf8' })
  assert.equal(result.status, 0, `${name} failed:\n${result.stdout}${result.stderr}`)
}

export function certificate(root, directory) {
  const configuration = resolve(directory, 'openssl.cnf')
  const key = resolve(directory, 'fixture-key.pem')
  const cert = resolve(directory, 'fixture-cert.pem')
  writeFileSync(configuration, `[req]
distinguished_name = dn
x509_extensions = extensions
prompt = no
[dn]
CN = control.localhost
[extensions]
subjectAltName = @names
[names]
DNS.1 = control.localhost
DNS.2 = client.localhost
`)
  command(root, 'openssl', [
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-config', configuration, '-keyout', key, '-out', cert,
  ])
  return { key, cert }
}

export async function listen(server, port = 0) {
  await new Promise((resolvePromise, reject) => {
    server.once('error', reject)
    server.listen(port, '127.0.0.1', resolvePromise)
  })
  return server.address().port
}

export async function freePort() {
  const server = createNetServer()
  const port = await listen(server)
  await new Promise(resolvePromise => server.close(resolvePromise))
  return port
}

export async function closeServer(server) {
  server.closeAllConnections?.()
  await new Promise(resolvePromise => server.close(resolvePromise))
}

export async function stopChild(child, signal) {
  const exited = new Promise(resolvePromise => {
    if (child.exitCode !== null || child.signalCode !== null) resolvePromise()
    else child.once('exit', resolvePromise)
  })
  if (child.exitCode !== null || child.signalCode !== null) return
  signalChild(child, signal)
  const graceful = await Promise.race([
    exited.then(() => true),
    new Promise(resolvePromise => setTimeout(() => resolvePromise(false), 5_000)),
  ])
  if (graceful) return
  signalChild(child, 'SIGKILL')
  const forced = await Promise.race([
    exited.then(() => true),
    new Promise(resolvePromise => setTimeout(() => resolvePromise(false), 5_000)),
  ])
  assert.equal(forced, true, `child ${String(child.pid)} did not exit after SIGKILL`)
}

function signalChild(child, signal) {
  if (child.pid === undefined) return
  if (process.platform !== 'win32') {
    try {
      process.kill(-child.pid, signal)
      return
    } catch {}
  }
  child.kill(signal)
}

export function staticClientServer({ root, certificateFiles, fixturePath, configuration }) {
  const moduleRoot = resolve(root, 'apps/client/dist/module')
  const publicRoot = resolve(root, 'apps/client/dist/public')
  const fixture = resolve(root, fixturePath)
  const productionIndex = readSharedClientFile(resolve(publicRoot, 'index.html'))
    .toString('utf8')
    .replace(/\s*<script src="\.\/runtime-config\.js"><\/script>/u, '')
    .replace(
      /<script type="module" src="\.\/assets\/client\.js"><\/script>/u,
      `<script type="module" src="/fixture/${fixturePath.split('/').at(-1)}"></script>`,
    )
  return createServer({
    key: readFileSync(certificateFiles.key),
    cert: readFileSync(certificateFiles.cert),
  }, (request, response) => {
    const path = new URL(request.url, 'https://client.localhost').pathname
    if (path === '/') {
      response.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8',
        'Content-Security-Policy': "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src https: wss:; object-src 'none'; base-uri 'none'; form-action 'none'",
      })
      response.end(productionIndex)
      return
    }
    if (path === '/fixture/server-url.json') {
      response.writeHead(200, {
        'Content-Type': 'application/json',
        'Cache-Control': 'no-store',
      })
      response.end(JSON.stringify(configuration()))
      return
    }
    if (path === '/favicon.ico') {
      response.writeHead(204).end()
      return
    }
    const fixtureRequest = path === `/fixture/${fixturePath.split('/').at(-1)}`
    const moduleRequest = path.startsWith('/module/')
    const publicRequest = path.startsWith('/assets/')
    const source = fixtureRequest
      ? fixture
      : moduleRequest
        ? normalize(resolve(moduleRoot, path.replace(/^\/module\//u, '')))
        : normalize(resolve(publicRoot, path.replace(/^\//u, '')))
    if (
      (moduleRequest && source.startsWith(`${moduleRoot}/`))
      || (publicRequest && source.startsWith(`${publicRoot}/`))
      || fixtureRequest
    ) {
      response.writeHead(200, {
        'Content-Type': publicRequest && path.endsWith('.css')
          ? 'text/css; charset=utf-8'
          : 'text/javascript; charset=utf-8',
      })
      response.end(readSharedClientFile(source))
      return
    }
    response.writeHead(404).end()
  })
}

export function startStandaloneServer({
  root,
  certificateFiles,
  clientOrigin,
  controlPort,
  directory,
  proof,
  errors,
  actionSigningKeyHex,
  checkoutRevision,
  helperExecutable,
  helperReleaseManifest,
  repositoryRoot,
  serverBinary,
  sourceRoot,
}) {
  const controlUrl = `https://control.localhost:${String(controlPort)}`
  const child = spawn(serverBinary, [], {
    cwd: root,
    detached: process.platform !== 'win32',
    env: {
      ...process.env,
      WWC_SERVER_BIND: `127.0.0.1:${String(controlPort)}`,
      WWC_SERVER_PUBLIC_URL: controlUrl,
      WWC_SERVER_DATA_DIRECTORY: resolve(directory, 'server-data'),
      WWC_SERVER_ALLOWED_ORIGINS: clientOrigin,
      WWC_SERVER_BOOTSTRAP_PROOF: proof,
      WWC_SERVER_AUTH_SUBJECT: 'usr_01J00000000000000000000000',
      WWC_SERVER_REPOSITORY_ROOT: repositoryRoot,
      WWC_SERVER_SOURCE_ROOT: sourceRoot,
      WWC_SERVER_CHECKOUT_REVISION: checkoutRevision,
      WWC_SERVER_HELPER_EXECUTABLE: helperExecutable,
      WWC_SERVER_HELPER_RELEASE_MANIFEST: helperReleaseManifest,
      WWC_SERVER_ACTION_SIGNING_KEY_HEX: actionSigningKeyHex,
      WWC_SERVER_EXECUTION_ENVELOPE_DIGEST: `sha256:${'a'.repeat(64)}`,
      WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID: 'crd_01J00000000000000000000001',
      WWC_SERVER_ORGANIZATION_ID: 'org_01J00000000000000000000000',
      WWC_SERVER_WORKSPACE_ID: 'wsp_01J00000000000000000000000',
      WWC_SERVER_PROJECT_ID: 'prj_01J00000000000000000000000',
      WWC_SERVER_REPOSITORY_ID: 'rep_01J00000000000000000000000',
      GITHUB_REPOSITORY: 'winwincode/browser-fixture',
      GITHUB_CREDENTIAL_REFERENCE_ID: 'crd_01J00000000000000000000000',
      GITHUB_API_BASE_URL: 'https://api.github.example',
      SECRET_DIRECTORY: resolve(directory, 'publication-secrets'),
      PUBLICATION_REQUESTERS: 'usr_01J00000000000000000000000',
      PUBLICATION_APPROVERS: 'usr_01J00000000000000000000000',
      PUBLICATION_APPROVAL_MAX_AGE_MILLIS: '86400000',
      WWC_SERVER_TLS_CERTIFICATE: certificateFiles.cert,
      WWC_SERVER_TLS_PRIVATE_KEY: certificateFiles.key,
      GIT_CONFIG_NOSYSTEM: '1',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  child.stdout.setEncoding('utf8')
  child.stdout.on('data', chunk => { errors.push(chunk) })
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', chunk => { errors.push(chunk) })
  return { child, controlUrl }
}

export async function waitForServer(controlUrl, child, errors) {
  const deadline = Date.now() + 30_000
  const port = new URL(controlUrl).port
  while (Date.now() < deadline) {
    assert.equal(child.exitCode, null, `standalone Server exited:\n${errors.join('')}`)
    const response = spawnSync(
      'curl',
      ['-ksS', '--noproxy', '*', `https://127.0.0.1:${port}/health`],
      { encoding: 'utf8' },
    )
    if (response.status === 0 && JSON.parse(response.stdout).status === 'ready') return
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error(`standalone Server did not become healthy:\n${errors.join('')}`)
}

async function devtoolsUrl(port) {
  const deadline = Date.now() + 20_000
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${String(port)}/json/version`)
      if (response.ok) return (await response.json()).webSocketDebuggerUrl
    } catch {}
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error('Chrome DevTools endpoint did not start')
}

export class DevTools {
  constructor(socket) {
    this.socket = socket
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data)
      if (message.id === undefined) {
        for (const listener of this.listeners.get(message.method) ?? []) listener(message.params)
        return
      }
      const pending = this.pending.get(message.id)
      if (pending === undefined) return
      this.pending.delete(message.id)
      if (message.error === undefined) pending.resolve(message.result)
      else pending.reject(new Error(message.error.message))
    })
  }

  nextId = 1
  pending = new Map()
  listeners = new Map()

  static async launch({ chromePath, directory, debugPort }) {
    const chrome = spawn(chromePath, [
      '--headless=new',
      '--disable-gpu',
      '--ignore-certificate-errors',
      '--no-first-run',
      '--no-default-browser-check',
      `--remote-debugging-port=${String(debugPort)}`,
      `--user-data-dir=${resolve(directory, 'chrome-profile')}`,
      'about:blank',
    ], { detached: process.platform !== 'win32', stdio: 'ignore' })
    const socket = new WebSocket(await devtoolsUrl(debugPort))
    await new Promise((resolvePromise, reject) => {
      socket.addEventListener('open', resolvePromise, { once: true })
      socket.addEventListener('error', reject, { once: true })
    })
    return { chrome, devtools: new DevTools(socket) }
  }

  send(method, params = {}, sessionId = undefined) {
    const id = this.nextId
    this.nextId += 1
    return new Promise((resolvePromise, reject) => {
      this.pending.set(id, { resolve: resolvePromise, reject })
      this.socket.send(JSON.stringify({
        id,
        method,
        params,
        ...(sessionId === undefined ? {} : { sessionId }),
      }))
    })
  }

  on(method, listener) {
    const listeners = this.listeners.get(method) ?? []
    listeners.push(listener)
    this.listeners.set(method, listeners)
  }

  close() {
    this.socket.close()
  }
}

export async function evaluate(devtools, sessionId, expression) {
  const result = await devtools.send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  }, sessionId)
  if (result.exceptionDetails !== undefined) {
    throw new Error(result.exceptionDetails.exception?.description ?? 'browser evaluation failed')
  }
  return result.result.value
}

export async function waitForGlobal(devtools, sessionId, name) {
  const deadline = Date.now() + 20_000
  while (Date.now() < deadline) {
    try {
      if (await evaluate(devtools, sessionId, `typeof globalThis.${name} === "function"`)) return
    } catch {}
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error(`browser fixture ${name} did not load`)
}
