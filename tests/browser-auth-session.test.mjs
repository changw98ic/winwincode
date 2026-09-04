import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { createServer } from 'node:https'
import { createServer as createNetServer } from 'node:net'
import { join, normalize, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import {
  serverTargetDirectory,
  writeHelperReleaseManifest,
} from '../scripts/run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')

function chromeBinary() {
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

function command(name, args) {
  const result = spawnSync(name, args, { cwd: root, encoding: 'utf8' })
  assert.equal(result.status, 0, `${name} failed:\n${result.stdout}${result.stderr}`)
}

function certificate(directory) {
  const configuration = join(directory, 'openssl.cnf')
  const key = join(directory, 'fixture-key.pem')
  const cert = join(directory, 'fixture-cert.pem')
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
  command('openssl', [
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-config', configuration, '-keyout', key, '-out', cert,
  ])
  return { key, cert }
}

async function listen(server, port = 0) {
  await new Promise((resolvePromise, reject) => {
    server.once('error', reject)
    server.listen(port, '127.0.0.1', resolvePromise)
  })
  return server.address().port
}

function staticClientServer(cert, controlConfiguration) {
  const moduleRoot = resolve(root, 'apps/client/dist/module')
  const fixture = resolve(root, 'tests/fixtures/browser-auth-client.mjs')
  const productionIndex = readFileSync(resolve(root, 'apps/client/public/index.html'), 'utf8')
    .replace(/\s*<link rel="stylesheet"[^>]*>/u, '')
    .replace(/\s*<script src="\.\/runtime-config\.js"><\/script>/u, '')
    .replace(
      /<script type="module" src="\.\/assets\/client\.js"><\/script>/u,
      '<script type="module" src="/fixture/browser-auth-client.mjs"></script>',
    )
  return createServer({ key: readFileSync(cert.key), cert: readFileSync(cert.cert) }, (request, response) => {
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
      response.end(JSON.stringify(controlConfiguration()))
      return
    }
    const source = path === '/fixture/browser-auth-client.mjs'
      ? fixture
      : normalize(join(moduleRoot, path.replace(/^\/module\//u, '')))
    if (
      (path.startsWith('/module/') && source.startsWith(`${moduleRoot}/`))
      || path === '/fixture/browser-auth-client.mjs'
    ) {
      response.writeHead(200, { 'Content-Type': 'text/javascript; charset=utf-8' })
      response.end(readFileSync(source))
      return
    }
    response.writeHead(404).end()
  })
}

async function waitForServer(controlUrl, child, errors) {
  const deadline = Date.now() + 30_000
  const port = new URL(controlUrl).port
  while (Date.now() < deadline) {
    assert.equal(child.exitCode, null, `standalone Server exited:\n${errors.join('')}`)
    const response = spawnSync(
      'curl',
      ['-ksS', '--noproxy', '*', `https://127.0.0.1:${port}/health`],
      { encoding: 'utf8' },
    )
    if (response.status === 0) {
      const health = JSON.parse(response.stdout)
      if (health.status === 'ready') return health
    }
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error(`standalone Server did not become healthy:\n${errors.join('')}`)
}

async function expectStartupFailure(child, errors, expectedMessage) {
  const exited = await Promise.race([
    new Promise(resolvePromise => child.once('exit', () => resolvePromise(true))),
    new Promise(resolvePromise => setTimeout(() => resolvePromise(false), 5_000)),
  ])
  if (!exited) await stopChild(child, 'SIGKILL')
  assert.equal(exited, true, 'standalone Server accepted an incomplete production configuration')
  assert.equal(child.exitCode, 1)
  assert.match(errors.join(''), expectedMessage)
}

function startStandaloneServer({
  cert,
  checkoutRevision,
  clientOrigin,
  controlPort,
  directory,
  proof,
  errors,
  helperExecutable,
  helperReleaseManifest,
  serverBinary,
}) {
  const controlUrl = `https://control.localhost:${String(controlPort)}`
  const child = spawn(serverBinary, [], {
    cwd: root,
    env: {
      ...process.env,
      WWC_SERVER_BIND: `127.0.0.1:${String(controlPort)}`,
      WWC_SERVER_PUBLIC_URL: controlUrl,
      WWC_SERVER_DATA_DIRECTORY: join(directory, 'server-data'),
      WWC_SERVER_ALLOWED_ORIGINS: clientOrigin,
      WWC_SERVER_BOOTSTRAP_PROOF: proof,
      WWC_SERVER_AUTH_SUBJECT: 'usr_01J00000000000000000000000',
      WWC_SERVER_REPOSITORY_ROOT: root,
      WWC_SERVER_CHECKOUT_REVISION: checkoutRevision,
      WWC_SERVER_HELPER_EXECUTABLE: helperExecutable,
      WWC_SERVER_HELPER_RELEASE_MANIFEST: helperReleaseManifest,
      WWC_SERVER_ORGANIZATION_ID: 'org_01J00000000000000000000000',
      WWC_SERVER_WORKSPACE_ID: 'wsp_01J00000000000000000000000',
      WWC_SERVER_PROJECT_ID: 'prj_01J00000000000000000000000',
      WWC_SERVER_REPOSITORY_ID: 'rep_01J00000000000000000000000',
      GITHUB_REPOSITORY: 'winwincode/browser-fixture',
      GITHUB_CREDENTIAL_REFERENCE_ID: 'crd_01J00000000000000000000000',
      GITHUB_API_BASE_URL: 'https://api.github.example',
      SECRET_DIRECTORY: join(directory, 'publication-secrets'),
      PUBLICATION_REQUESTERS: 'usr_01J00000000000000000000000',
      PUBLICATION_APPROVERS: 'usr_01J00000000000000000000000',
      PUBLICATION_APPROVAL_MAX_AGE_MILLIS: '86400000',
      WWC_SERVER_TLS_CERTIFICATE: cert.cert,
      WWC_SERVER_TLS_PRIVATE_KEY: cert.key,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  child.stdout.setEncoding('utf8')
  child.stdout.on('data', chunk => { errors.push(chunk) })
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', chunk => { errors.push(chunk) })
  return { child, controlUrl }
}

async function freePort() {
  const server = createNetServer()
  const port = await listen(server)
  await new Promise(resolvePromise => server.close(resolvePromise))
  return port
}

async function stopChild(child, signal) {
  if (child.exitCode !== null || child.signalCode !== null) return
  child.kill(signal)
  await Promise.race([
    new Promise(resolvePromise => child.once('exit', resolvePromise)),
    new Promise(resolvePromise => setTimeout(resolvePromise, 5_000)),
  ])
}

async function closeServer(server) {
  server.closeAllConnections?.()
  await new Promise(resolvePromise => server.close(resolvePromise))
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

class DevTools {
  constructor(socket) {
    this.socket = socket
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data)
      if (message.id === undefined) return
      const pending = this.pending.get(message.id)
      if (pending === undefined) return
      this.pending.delete(message.id)
      if (message.error === undefined) pending.resolve(message.result)
      else pending.reject(new Error(message.error.message))
    })
  }

  nextId = 1
  pending = new Map()

  static async connect(url) {
    const socket = new WebSocket(url)
    await new Promise((resolvePromise, reject) => {
      socket.addEventListener('open', resolvePromise, { once: true })
      socket.addEventListener('error', reject, { once: true })
    })
    return new DevTools(socket)
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

  close() {
    this.socket.close()
  }
}

async function evaluate(devtools, sessionId, expression) {
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

async function waitForFixture(devtools, sessionId) {
  const deadline = Date.now() + 20_000
  while (Date.now() < deadline) {
    try {
      if (await evaluate(devtools, sessionId, 'typeof globalThis.runAuthBrowserFixture === "function"')) {
        return
      }
    } catch {}
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error('browser fixture module did not load')
}

test('static Client and standalone TLS Server run real cross-origin workflows and restart independently', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome or Chromium is required for the real-browser auth gate')
    return
  }
  command('corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  command('cargo', ['build', '-p', 'winwincode-server', '--bin', 'winwincode-server', '--locked', '--offline'])
  command('cargo', ['build', '--release', '-p', 'winwincode-kernel-helper', '--locked', '--offline'])
  const targetDirectory = serverTargetDirectory(root)
  const serverBinary = resolve(targetDirectory, 'debug/winwincode-server')
  const helperExecutable = resolve(targetDirectory, 'debug/winwincode-kernel-helper')
  const releaseHelper = resolve(targetDirectory, 'release/winwincode-kernel-helper')
  assert.equal(existsSync(serverBinary), true, `Server binary is missing: ${serverBinary}`)
  assert.equal(existsSync(releaseHelper), true, `release helper is missing: ${releaseHelper}`)
  copyFileSync(releaseHelper, helperExecutable)
  chmodSync(helperExecutable, 0o755)
  const helperReleaseManifest = writeHelperReleaseManifest(root, helperExecutable)
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-auth-'))
  const cert = certificate(directory)
  let controlUrl = ''
  const baselineResult = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' })
  assert.equal(baselineResult.status, 0, baselineResult.stderr)
  const repositoryBaseline = baselineResult.stdout.trim()
  assert.match(repositoryBaseline, /^[0-9a-f]{40}$/u)
  let clientServer = staticClientServer(cert, () => ({
    repositoryBaseline,
    serverUrl: controlUrl,
  }))
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  const controlPort = await freePort()
  const proof = randomBytes(32).toString('base64url')
  const errors = []
  let standalone = null
  let chrome = null
  let devtools
  t.after(async () => {
    devtools?.close()
    await Promise.allSettled([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      ...(standalone === null ? [] : [stopChild(standalone, 'SIGINT')]),
      ...(clientServer === null ? [] : [closeServer(clientServer)]),
    ])
    rmSync(directory, { recursive: true, force: true })
  })
  const missingRevisionErrors = []
  const missingRevision = startStandaloneServer({
    cert,
    checkoutRevision: undefined,
    clientOrigin,
    controlPort,
    directory,
    proof,
    errors: missingRevisionErrors,
    helperExecutable,
    helperReleaseManifest,
    serverBinary,
  }).child
  await expectStartupFailure(
    missingRevision,
    missingRevisionErrors,
    /WWC_SERVER_CHECKOUT_REVISION is required/u,
  )
  ;({ child: standalone, controlUrl } = startStandaloneServer({
    cert,
    checkoutRevision: repositoryBaseline,
    clientOrigin,
    controlPort,
    directory,
    proof,
    errors,
    helperExecutable,
    helperReleaseManifest,
    serverBinary,
  }))
  const firstHealth = await waitForServer(controlUrl, standalone, errors)
  const debugPort = await freePort()
  chrome = spawn(chromePath, [
    '--headless=new',
    '--disable-gpu',
    '--ignore-certificate-errors',
    '--no-first-run',
    '--no-default-browser-check',
    `--remote-debugging-port=${String(debugPort)}`,
    `--user-data-dir=${join(directory, 'chrome-profile')}`,
    'about:blank',
  ], { stdio: 'ignore' })
  devtools = await DevTools.connect(await devtoolsUrl(debugPort))
  const { targetId } = await devtools.send('Target.createTarget', { url: clientOrigin })
  const { sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Network.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await waitForFixture(devtools, sessionId)
  const result = await evaluate(
    devtools,
    sessionId,
    `globalThis.runAuthBrowserFixture(${JSON.stringify(proof)})`,
  )
  assert.deepEqual(result, {
    failedInputValue: '',
    inputAfterFailedLogin: '',
    submittedInputValue: '',
    inputAfterSuccessfulLogin: '',
    approvalCount: 0,
    approvalDecisionCode: 'RESOURCE_NOT_FOUND',
    chatCommand: 'chat.submit',
    chatMessage: 'Cross-origin browser message',
    contentSecurityPolicy: "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src https: wss:; object-src 'none'; base-uri 'none'; form-action 'none'",
    corsResponseType: 'cors',
    deliveryAdvanceCommand: 'delivery.advance',
    deliveryCreateCommand: 'delivery.create',
    deliveryCount: 0,
    deliveryDetailRevision: 2,
    deliveryRevision: 2,
    eventSequences: [1, 2, 3],
    eventTypes: [
      'product-session.changed.v1',
      'product-session.changed.v1',
      'product-session.message.appended.v1',
    ],
    firstCursorSequence: 1,
    publicationCount: 0,
    publicationPublishCode: 'TRUSTED_FACTS_UNAVAILABLE',
    proofFound: false,
    redirectedResources: 0,
    sessionCommand: 'session.create',
    sessionActor: {
      kind: 'user',
      id: 'usr_01J00000000000000000000000',
    },
    sessionScope: {
      kind: 'repository',
      organizationId: 'org_01J00000000000000000000000',
      workspaceId: 'wsp_01J00000000000000000000000',
      projectId: 'prj_01J00000000000000000000000',
      repositoryId: 'rep_01J00000000000000000000000',
    },
    settingsConcurrency: 2,
    settingsPreviousConcurrency: 1,
    settingsRevision: 1,
    versionCode: 'INVALID_REQUEST',
    versionReason: 'CLIENT_UPGRADE_REQUIRED',
    versionSupportedSchema: 'winwincode/v1',
    versionStatus: 426,
    workerCount: 1,
    cookieVisibleToScript: '',
  })
  const cookies = await devtools.send('Network.getAllCookies', {}, sessionId)
  const sessionCookie = cookies.cookies.find(cookie => cookie.name === 'wwc_session')
  assert.ok(sessionCookie)
  assert.equal(sessionCookie.domain, 'control.localhost')
  assert.equal(sessionCookie.path, '/')
  assert.equal(sessionCookie.httpOnly, true)
  assert.equal(sessionCookie.secure, true)
  assert.equal(sessionCookie.sameSite, 'None')
  assert.ok(sessionCookie.expires > Date.now() / 1000)
  assert.ok(sessionCookie.value.length >= 43)
  assert.notEqual(sessionCookie.value, proof)

  await stopChild(standalone, 'SIGINT')
  ;({ child: standalone, controlUrl } = startStandaloneServer({
    cert,
    checkoutRevision: repositoryBaseline,
    clientOrigin,
    controlPort,
    directory,
    proof,
    errors,
    helperExecutable,
    helperReleaseManifest,
    serverBinary,
  }))
  const restartedHealth = await waitForServer(controlUrl, standalone, errors)
  assert.equal(restartedHealth.serverVersion, firstHealth.serverVersion)
  assert.deepEqual(
    await evaluate(devtools, sessionId, 'globalThis.runPostServerRestartFixture()'),
    {
      command: 'session.cancel',
      eventSequences: [1, 2, 3, 4],
      eventTypes: [
        'product-session.changed.v1',
        'product-session.changed.v1',
        'product-session.message.appended.v1',
        'product-session.changed.v1',
      ],
      settings: { concurrency: 2, revision: 1 },
    },
  )

  await closeServer(clientServer)
  clientServer = null
  assert.deepEqual(
    await evaluate(devtools, sessionId, 'globalThis.runWhileClientServerStoppedFixture()'),
    { concurrency: 2, revision: 1 },
  )
  clientServer = staticClientServer(cert, () => ({
    repositoryBaseline,
    serverUrl: controlUrl,
  }))
  await listen(clientServer, clientPort)
  await evaluate(devtools, sessionId, 'delete globalThis.runAuthBrowserFixture')
  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForFixture(devtools, sessionId)
  assert.deepEqual(
    await evaluate(devtools, sessionId, 'globalThis.runExistingSessionFixture()'),
    {
      settings: { concurrency: 2, revision: 1 },
      actor: {
        kind: 'user',
        id: 'usr_01J00000000000000000000000',
      },
      scope: {
        kind: 'repository',
        organizationId: 'org_01J00000000000000000000000',
        workspaceId: 'wsp_01J00000000000000000000000',
        projectId: 'prj_01J00000000000000000000000',
        repositoryId: 'rep_01J00000000000000000000000',
      },
    },
  )
  assert.deepEqual(
    await evaluate(devtools, sessionId, 'globalThis.finishAuthBrowserFixture()'),
    { revokedKind: 'authentication', webSocketClosed: true },
  )
  assert.equal(errors.join('').includes(proof), false)
  const clearedCookies = await devtools.send('Network.getAllCookies', {}, sessionId)
  assert.equal(clearedCookies.cookies.some(cookie => cookie.name === 'wwc_session'), false)
})
