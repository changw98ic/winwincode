// SPDX-License-Identifier: Apache-2.0
import { appendFileSync, existsSync } from 'node:fs'
import { join } from 'node:path'
import { createInterface } from 'node:readline'

const directory = process.argv[2]
const calls = new Map()
const record = event => appendFileSync(join(directory, 'calls.jsonl'), `${JSON.stringify(event)}\n`)
const send = (id, result) => process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`)
const input = createInterface({ input: process.stdin })
input.on('line', line => {
  const request = JSON.parse(line)
  if (request.method === 'notifications/cancelled') {
    const call = calls.get(request.params.requestId)
    if (call) call.cancelled = true
    return
  }
  if (request.id === undefined) return
  if (request.method === 'initialize') {
    send(request.id, { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} },
      serverInfo: { name: 'cancellation-fixture', version: '1' } })
  } else if (request.method === 'tools/list') {
    send(request.id, { tools: [{ name: 'public_smoke', description: 'Wait for a fixture release',
      annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
      inputSchema: { type: 'object', required: ['tag', 'phase'], additionalProperties: false,
        properties: { tag: { enum: ['A', 'B'] }, phase: { enum: ['hold', 'after'] } } } }] })
  } else if (request.method === 'tools/call') {
    const { tag, phase } = request.params.arguments
    if (!['A', 'B'].includes(tag) || !['hold', 'after'].includes(phase)) throw new Error('invalid fixture call')
    const call = { tag, phase, cancelled: false }
    calls.set(request.id, call)
    record({ tag, phase, state: 'started' })
    const timer = setInterval(() => {
      if (!call.cancelled && phase === 'hold' && !existsSync(join(directory, `${tag}.release`))) return
      clearInterval(timer)
      calls.delete(request.id)
      record({ tag, phase, state: call.cancelled ? 'cancelled' : 'completed' })
      send(request.id, { content: [{ type: 'text', text: `${tag}:${phase}` }], isError: call.cancelled })
    }, 20)
  } else if (request.method === 'ping') {
    send(request.id, {})
  } else {
    process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id: request.id,
      error: { code: -32601, message: 'Method not found' } })}\n`)
  }
})
input.on('close', () => process.exit(0))
