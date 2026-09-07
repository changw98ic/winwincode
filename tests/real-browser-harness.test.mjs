import assert from 'node:assert/strict'
import test from 'node:test'

import {
  BoundedEventBuffer,
  DevTools,
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
