import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { createServer } from 'node:https'
import { createServer as createNetServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join, normalize, resolve } from 'node:path'
import test from 'node:test'

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
  const fixture = resolve(root, 'tests/fixtures/browser-local-controls-client.mjs')
  const productionIndex = readFileSync(resolve(root, 'apps/client/public/index.html'), 'utf8')
    .replace(/\s*<link rel="stylesheet"[^>]*>/u, '')
    .replace(/\s*<script src="\.\/runtime-config\.js"><\/script>/u, '')
    .replace(
      /<script type="module" src="\.\/assets\/client\.js"><\/script>/u,
      '<script type="module" src="/fixture/browser-local-controls-client.mjs"></script>',
    )
  return createServer(
    { key: readFileSync(cert.key), cert: readFileSync(cert.cert) },
    (request, response) => {
      const path = new URL(request.url, 'https://client.localhost').pathname
      if (path === '/') {
        response.writeHead(200, {
          'Content-Type': 'text/html; charset=utf-8',
          'Content-Security-Policy': "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src https: wss:; object-src 'none'; base-uri 'none'; form-action 'none'",
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
      const source = path === '/fixture/browser-local-controls-client.mjs'
        ? fixture
        : normalize(join(moduleRoot, path.replace(/^\/module\//u, '')))
      if (
        (path.startsWith('/module/') && source.startsWith(`${moduleRoot}/`))
        || path === '/fixture/browser-local-controls-client.mjs'
      ) {
        response.writeHead(200, { 'Content-Type': 'text/javascript; charset=utf-8' })
        response.end(readFileSync(source))
        return
      }
      response.writeHead(404).end()
    },
  )
}

async function freePort() {
  const server = createNetServer()
  const port = await listen(server)
  await new Promise(resolvePromise => server.close(resolvePromise))
  return port
}

function startFixture({ cert, clientOrigin, controlPort, directory, proof, errors }) {
  const controlUrl = `https://control.localhost:${String(controlPort)}`
  const child = spawn(
    resolve(root, 'target/debug/examples/browser_local_controls_fixture'),
    [],
    {
      cwd: root,
      env: {
        ...process.env,
        WWC_SERVER_BIND: `127.0.0.1:${String(controlPort)}`,
        WWC_SERVER_PUBLIC_URL: controlUrl,
        WWC_SERVER_DATA_DIRECTORY: join(directory, 'server-data'),
        WWC_SERVER_ALLOWED_ORIGINS: clientOrigin,
        WWC_SERVER_BOOTSTRAP_PROOF: proof,
        WWC_SERVER_TLS_CERTIFICATE: cert.cert,
        WWC_SERVER_TLS_PRIVATE_KEY: cert.key,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  )
  child.stdout.setEncoding('utf8')
  child.stdout.on('data', chunk => { errors.push(chunk) })
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', chunk => { errors.push(chunk) })
  return { child, controlUrl }
}

async function waitForServer(controlUrl, child, errors) {
  const deadline = Date.now() + 30_000
  const port = new URL(controlUrl).port
  while (Date.now() < deadline) {
    assert.equal(child.exitCode, null, `fixture Server exited:\n${errors.join('')}`)
    const response = spawnSync(
      'curl',
      ['-ksS', '--noproxy', '*', `https://127.0.0.1:${port}/health`],
      { encoding: 'utf8' },
    )
    if (response.status === 0 && JSON.parse(response.stdout).status === 'ready') return
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error(`fixture Server did not become healthy:\n${errors.join('')}`)
}

async function stopChild(child, signalName) {
  if (child.exitCode !== null || child.signalCode !== null) return
  child.kill(signalName)
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
      if (await evaluate(
        devtools,
        sessionId,
        'typeof globalThis.runLocalControlsFixture === "function"',
      )) return
    } catch {}
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error('local-controls browser fixture did not load')
}

function filesUnder(directory) {
  if (!existsSync(directory)) return []
  return readdirSync(directory).flatMap(name => {
    const path = join(directory, name)
    return statSync(path).isDirectory() ? filesUnder(path) : [path]
  })
}

function publicEventBytes(directory) {
  return Buffer.concat(filesUnder(directory).map(path => readFileSync(path)))
}

test('real browser preserves local settings, decisions, resume cursors, and revoked access', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome or Chromium is required for the real-browser local-controls gate')
    return
  }
  if (process.env.WWC_BROWSER_REUSE_BUILD !== '1') {
    command('corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
    command('cargo', [
      'build', '-p', 'winwincode-server', '--example', 'browser_local_controls_fixture',
      '--locked', '--offline',
    ])
  }
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-local-controls-'))
  const cert = certificate(directory)
  let controlUrl = ''
  const clientServer = staticClientServer(cert, () => ({ serverUrl: controlUrl }))
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  const controlPort = await freePort()
  const proof = `proof-${randomBytes(24).toString('base64url')}`
  const firstLocator = `vault://browser/${randomBytes(20).toString('base64url')}`
  const rotatedLocator = `vault://browser/${randomBytes(20).toString('base64url')}`
  const errors = []
  let standalone = null
  let chrome = null
  let devtools
  t.after(async () => {
    devtools?.close()
    await Promise.allSettled([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      ...(standalone === null ? [] : [stopChild(standalone, 'SIGINT')]),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
  })

  ;({ child: standalone, controlUrl } = startFixture({
    cert,
    clientOrigin,
    controlPort,
    directory,
    proof,
    errors,
  }))
  await waitForServer(controlUrl, standalone, errors)

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

  const first = await evaluate(
    devtools,
    sessionId,
    `globalThis.runLocalControlsFixture(${JSON.stringify(proof)}, ${JSON.stringify(firstLocator)}, ${JSON.stringify(rotatedLocator)})`,
  )
  assert.equal(first.submittedProof, '')
  assert.equal(first.clearedProof, '')
  assert.equal(first.browserSecretFound, false)
  assert.deepEqual(first.credentialRevisions, [1, 2, 3])
  assert.equal(first.credentialState, 'revoked')
  assert.equal(first.settingsConcurrency, 3)
  assert.equal(first.settingsRevision, 1)
  assert.deepEqual(first.approval, { id: 'apr_00000000000000000000000001', state: 'approved' })
  assert.equal(first.afterReconnect, first.beforeReconnect)
  assert.equal(first.uniqueEventCount, first.eventCount)
  assert.deepEqual(first.eventSequences, first.eventSequences.toSorted((left, right) => left - right))

  await stopChild(standalone, 'SIGINT')
  ;({ child: standalone, controlUrl } = startFixture({
    cert,
    clientOrigin,
    controlPort,
    directory,
    proof,
    errors,
  }))
  await waitForServer(controlUrl, standalone, errors)
  const restarted = await evaluate(
    devtools,
    sessionId,
    'globalThis.runLocalControlsAfterRestart()',
  )
  assert.equal(restarted.approvalState, 'approved')
  assert.equal(restarted.cancellationState, 'cancelled')
  assert.equal(restarted.credentialRevision, 3)
  assert.equal(restarted.credentialState, 'revoked')
  assert.equal(restarted.settingsConcurrency, 3)
  assert.equal(restarted.settingsRevision, 1)
  assert.equal(restarted.uniqueEventCount, restarted.eventCount)
  assert.deepEqual(
    restarted.eventSequences,
    restarted.eventSequences.toSorted((left, right) => left - right),
  )

  standalone.kill('SIGUSR1')
  const revoked = await evaluate(
    devtools,
    sessionId,
    'globalThis.runLocalControlsAfterPermissionRevocation()',
  )
  assert.deepEqual(revoked, {
    queryError: { code: 'AUTHENTICATION_REQUIRED', kind: 'authentication' },
    revocation: {
      authorizationEpoch: 2,
      subscriptionId: 'sub_00000000000000000000000052',
      type: 'transport.authorization-revoked.v1',
    },
    webSocketClosed: true,
  })

  const publicBytes = publicEventBytes(join(directory, 'server-data', 'event-hub'))
  for (const secret of [proof, firstLocator, rotatedLocator, 'QlJPV1NFUl9QUklWQVRFX0FDVElPTg==']) {
    assert.equal(publicBytes.includes(Buffer.from(secret)), false, `public event data leaked ${secret}`)
    assert.equal(errors.join('').includes(secret), false, `Server logs leaked ${secret}`)
  }
})
