// SPDX-License-Identifier: Apache-2.0

// Real HTTPS Provider fixture for the sealed Reviewer Fusion path. Member
// responses are untrusted hypotheses. They contain no claimed verified evidence.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { createServer } from 'node:https'

export function fusionMemberProvider({ certificatePath, privateKeyPath, apiKey, responsesByModel,
  gateModels = [] }) {
  assert.ok(typeof apiKey === 'string' && apiKey.length >= 16)
  assert.ok(Object.keys(responsesByModel).length > 0)
  const held = new Map()
  const records = []
  let active = 0
  let peak = 0
  const gated = new Set(gateModels)
  const server = createServer({ cert: readFileSync(certificatePath), key: readFileSync(privateKeyPath) }, (incoming, outgoing) => {
    if (incoming.method !== 'POST' || incoming.url !== '/v1/messages'
      || incoming.headers.authorization !== `Bearer ${apiKey}`
      || incoming.headers['anthropic-version'] !== '2023-06-01') {
      outgoing.writeHead(401).end()
      return
    }
    const parts = []
    let bytes = 0
    incoming.on('data', part => {
      bytes += part.length
      if (bytes > 8 * 1024 * 1024) incoming.destroy()
      else parts.push(part)
    })
    incoming.on('end', () => {
      const body = Buffer.concat(parts)
      let model
      try { model = JSON.parse(body.toString('utf8')).model } catch { outgoing.writeHead(400).end(); return }
      if (typeof model !== 'string' || !Object.hasOwn(responsesByModel, model)) { outgoing.writeHead(400).end(); return }
      active += 1
      peak = Math.max(peak, active)
      const text = JSON.stringify(responsesByModel[model])
      const record = { ordinal: records.length + 1, model, active: true, completed: false,
        requestSha256: createHash('sha256').update(body).digest('hex'),
        responseTextSha256: createHash('sha256').update(text).digest('hex') }
      records.push(record)
      outgoing.on('close', () => {
        if (record.active) { active -= 1; record.active = false }
      })
      const finish = () => {
        if (outgoing.destroyed || outgoing.writableEnded) return
        outgoing.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' })
        const event = (type, value) => outgoing.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...value })}\n\n`)
        event('message_start', { message: { id: `fusion-fixture-${record.ordinal}`, type: 'message', role: 'assistant',
          model, content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } })
        event('content_block_start', { index: 0, content_block: { type: 'text', text: '' } })
        event('content_block_delta', { index: 0, delta: { type: 'text_delta', text } })
        event('content_block_stop', { index: 0 })
        event('message_delta', { delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 5 } })
        event('message_stop', {})
        record.completed = true
        outgoing.end()
      }
      if (gated.has(model)) {
        if (!held.has(model)) held.set(model, [])
        held.get(model).push(finish)
      } else finish()
    })
  })
  return {
    records,
    get activeRequests() { return active },
    get peakActiveRequests() { return peak },
    async listen() {
      await new Promise((resolveReady, rejectReady) => {
        server.once('error', rejectReady)
        server.listen(0, '127.0.0.1', resolveReady)
      })
      return { endpoint: `https://127.0.0.1:${server.address().port}/v1/messages`, protocol: 'anthropic_messages' }
    },
    release(model) {
      gated.delete(model)
      for (const finish of held.get(model) ?? []) finish()
      held.delete(model)
    },
    async close() {
      for (const model of [...held.keys()]) this.release(model)
      server.closeAllConnections()
      await new Promise(resolveClosed => server.close(resolveClosed))
    },
  }
}
