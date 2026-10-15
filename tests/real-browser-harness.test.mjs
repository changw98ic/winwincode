import assert from 'node:assert/strict'
import childProcess, { spawn } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { syncBuiltinESMExports } from 'node:module'
import test from 'node:test'

import {
  BoundedEventBuffer,
  DevTools,
  closeServer,
  freePort,
  listen,
  staticClientServer,
} from './fixtures/real-browser-harness.mjs'
import { boundedRemoteObjectText } from './fixtures/bounded-browser-diagnostics.mjs'

class FakeSocket extends EventTarget {
  messages = []

  send(message) {
    this.messages.push(JSON.parse(message))
  }

  respond(id, result = {}) {
    this.dispatchEvent(new MessageEvent('message', {
      data: JSON.stringify({ id, result }),
    }))
  }

  close() {
    this.dispatchEvent(new Event('close'))
  }
}

test('static Client serves its fixture and configuration over loopback HTTP', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-http-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const publicRoot = join(directory, 'apps/client/dist/public')
  mkdirSync(publicRoot, { recursive: true })
  writeFileSync(join(publicRoot, 'index.html'), '<script src="./runtime-config.js"></script><script type="module" src="./assets/client.js"></script>')
  const configuration = { serverUrl: 'http://127.0.0.1:8443' }
  const server = staticClientServer({
    root: directory,
    fixturePath: 'tests/fixtures/browser-http.mjs',
    configuration: () => configuration,
  })
  t.after(() => closeServer(server))
  const origin = `http://127.0.0.1:${await listen(server)}`
  const entry = await fetch(origin)
  assert.equal(entry.status, 200)
  assert.match(await entry.text(), /src="\/fixture\/browser-http\.mjs"/u)
  const response = await fetch(`${origin}/fixture/server-url.json`)
  assert.equal(response.status, 200)
  assert.deepEqual(await response.json(), configuration)
})

test('failed browser startup releases the owned child before returning', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-startup-failure-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const chromePath = join(directory, 'chrome-fixture')
  writeFileSync(chromePath, '#!/bin/sh\nexec sleep 600\n', { mode: 0o700 })
  let pid
  const originalSpawn = childProcess.spawn
  t.mock.method(childProcess, 'spawn', (file, ...args) => {
    const child = originalSpawn(file, ...args)
    if (file === chromePath) pid = child.pid
    return child
  })
  syncBuiltinESMExports()
  t.after(() => { t.mock.restoreAll(); syncBuiltinESMExports() })
  await assert.rejects(DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
    startupTimeoutMillis: 1_000,
  }), /Chrome DevTools endpoint did not start/u)
  assert.ok(Number.isInteger(pid), 'the real OS spawn identifies its owned child before the timeout')
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' })
})

test('browser startup retains bounded diagnostics when the executable exits', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-startup-exit-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const chromePath = join(directory, 'chrome-fixture')
  writeFileSync(chromePath, `#!/bin/sh\nprintf '${'x'.repeat(9_000)}\nfixture browser startup failed\n' >&2\nexit 7\n`, { mode: 0o700 })
  await assert.rejects(DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  }), error => {
    assert.match(error.message, /Chrome exited before its DevTools endpoint started/u)
    assert.match(error.message, /fixture browser startup failed/u)
    assert.ok(error.message.length <= 8_300)
    return true
  })
})

test('bounded event buffers retain only the newest samples', () => {
  const events = new BoundedEventBuffer(3)
  for (let index = 0; index < 100_000; index += 1) events.push(index)

  assert.equal(events.length, 3)
  assert.equal(events.totalCount, 100_000)
  assert.deepEqual(events.values(), [99_997, 99_998, 99_999])
  assert.deepEqual([...events], events.values())
  assert.deepEqual(events.slice(-2), [99_998, 99_999])
})

test('remote browser diagnostics do not follow prototypes or exceed their traversal budget', async () => {
  const calls = []
  const devtools = {
    async send(_method, { objectId }) {
      calls.push(objectId)
      return {
        result: Array.from({ length: 20 }, (_, index) => ({
          name: `property-${String(index)}`,
          value: {
            description: `object-${String(index)}`,
            objectId: `${objectId}.${String(index)}`,
            type: 'object',
          },
        })),
        internalProperties: [{
          name: '[[Prototype]]',
          value: { description: 'must-not-be-visited', objectId: 'prototype', type: 'object' },
        }],
      }
    },
  }

  const text = await boundedRemoteObjectText(
    devtools,
    'session',
    { description: 'root', objectId: 'root', type: 'object' },
    { maxChars: 120, maxDepth: 2, maxObjects: 3, maxProperties: 5, timeoutMillis: 50 },
  )

  assert.ok(text.length <= 120)
  assert.ok(calls.length <= 3)
  assert.equal(calls.includes('prototype'), false)
  assert.equal(text.includes('must-not-be-visited'), false)
})

test('DevTools removes completed and timed-out commands from its pending set', async () => {
  const socket = new FakeSocket()
  const devtools = new DevTools(socket, { commandTimeoutMillis: 10 })

  const completed = devtools.send('Runtime.enable')
  socket.respond(socket.messages[0].id, { enabled: true })
  assert.deepEqual(await completed, { enabled: true })
  assert.equal(devtools.pending.size, 0)

  await assert.rejects(
    devtools.send('Runtime.neverResponds'),
    /Chrome DevTools Runtime\.neverResponds command timed out/u,
  )
  assert.equal(devtools.pending.size, 0)
  devtools.close()
})

test('DevTools rejects every pending command when the socket closes', async () => {
  const socket = new FakeSocket()
  const devtools = new DevTools(socket)
  const first = devtools.send('Runtime.enable')
  const second = devtools.send('Network.enable')

  socket.close()
  await assert.rejects(first, /Chrome DevTools connection closed/u)
  await assert.rejects(second, /Chrome DevTools connection closed/u)
  assert.equal(devtools.pending.size, 0)
})


test('browser startup failure releases stderr inherited by an owned descendant', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-browser-descendant-'))
  const pidPath = join(directory, 'descendant.pid')
  const chromePath = join(directory, 'chrome-fixture')
  writeFileSync(chromePath, `#!${process.execPath}
const { spawn } = require('node:child_process')
const { writeFileSync } = require('node:fs')
const descendant = spawn('/bin/sleep', ['600'], { stdio: ['ignore', 'ignore', 'inherit'] })
descendant.once('spawn', () => {
  writeFileSync(${JSON.stringify(pidPath)}, String(descendant.pid))
  process.exit(7)
})
`, { mode: 0o700 })
  const child = spawn(process.execPath, ['--input-type=module', '-e', `
    import { DevTools } from ${JSON.stringify(new URL('./fixtures/real-browser-harness.mjs', import.meta.url).href)}
    try {
      await DevTools.launch(${JSON.stringify({ chromePath, directory, debugPort: await freePort(), startupTimeoutMillis: 1000 })})
      throw new Error('expected startup failure')
    } catch (error) {
      if (!error.message.includes('Chrome exited before')) throw error
    }
  `], { stdio: ['ignore', 'ignore', 'pipe'] })
  let diagnostics = ''
  child.stderr.on('data', bytes => { diagnostics += bytes.toString('utf8') })
  t.after(() => {
    child.kill('SIGKILL')
    if (existsSync(pidPath)) {
      try { process.kill(Number(readFileSync(pidPath, 'utf8')), 'SIGKILL') } catch (error) {
        if (error.code !== 'ESRCH') throw error
      }
    }
    rmSync(directory, { recursive: true, force: true })
  })
  let timer
  const exited = await Promise.race([
    new Promise(resolvePromise => child.once('exit', code => resolvePromise({ code }))),
    new Promise(resolvePromise => { timer = setTimeout(() => resolvePromise(null), 3000) }),
  ])
  clearTimeout(timer)
  assert.deepEqual(exited, { code: 0 }, diagnostics || 'diagnostic pipe keeps the subprocess alive')
})
