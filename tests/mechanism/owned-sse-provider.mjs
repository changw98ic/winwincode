// SPDX-License-Identifier: Apache-2.0
// Synthetic loopback Provider used only by the explicit mechanism audit.
import { createHash } from 'node:crypto'
import { createServer } from 'node:https'

/** An owned TLS Provider with an explicit, observable response release gate. */
export async function startOwnedSseProvider({ key, cert, protocol = 'anthropic_messages',
  deltaCount = 2523, deltaBytes = 120, finalText = null,
  requestLimitBytes = 16 * 1024 * 1024 } = {}) {
  const text = (finalText ?? 'x'.repeat(deltaCount * deltaBytes))
    .padEnd(deltaCount * deltaBytes, ' ')
  const textSlice = index => text.slice(Math.floor(index * text.length / deltaCount),
    Math.floor((index + 1) * text.length / deltaCount))
  const origin = performance.now()
  const events = []
  const requests = []
  const pending = new Set()
  let released = false
  let closed = false
  const event = (stage, facts = {}) => events.push({ stage, atUtc: new Date().toISOString(),
    elapsedMillis: performance.now() - origin, ...facts })
  const write = (response, value) => response.write(`data: ${JSON.stringify(value)}\n\n`)
  const finish = (response, model, index) => {
    if (closed || response.destroyed) return
    event('complete_response_write_start', { requestIndex: index, deltaCount })
    if (protocol === 'anthropic_messages') {
      write(response, { type: 'message_start', message: { id: `msg_offline_${index}`, type: 'message',
        role: 'assistant', model, content: [], stop_reason: null, stop_sequence: null,
        usage: { input_tokens: 10, output_tokens: 0 } } })
      write(response, { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } })
      for (let n = 0; n < deltaCount; n += 1) write(response, { type: 'content_block_delta', index: 0,
        delta: { type: 'text_delta', text: textSlice(n) } })
      write(response, { type: 'content_block_stop', index: 0 })
      write(response, { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null },
        usage: { output_tokens: deltaCount } })
      write(response, { type: 'message_stop' })
    } else if (protocol === 'openai_responses') {
      const responseId = `resp_offline_${index}`
      const itemId = `message_offline_${index}`
      write(response, { type: 'response.created', response: { id: responseId, model } })
      write(response, { type: 'response.output_item.added', item: { type: 'message',
        id: itemId, role: 'assistant', content: [], phase: 'final_answer' } })
      for (let n = 0; n < deltaCount; n += 1) write(response,
        { type: 'response.output_text.delta', item_id: itemId, output_index: 0,
          content_index: 0, delta: textSlice(n) })
      write(response, { type: 'response.output_item.done', item: { type: 'message',
        id: itemId, role: 'assistant', phase: 'final_answer',
        content: [{ type: 'output_text', text }] } })
      write(response, { type: 'response.completed', response: { id: responseId, model,
        status: 'completed', error: null, end_turn: true,
        usage: { input_tokens: 10, output_tokens: deltaCount, total_tokens: deltaCount + 10 } } })
    } else if (protocol === 'openai_chat_completions') {
      for (let n = 0; n < deltaCount; n += 1) write(response, { id: `chatcmpl_offline_${index}`,
        object: 'chat.completion.chunk', model, choices: [{ index: 0,
          delta: { ...(n === 0 ? { role: 'assistant' } : {}), content: textSlice(n) }, finish_reason: null }] })
      write(response, { id: `chatcmpl_offline_${index}`, object: 'chat.completion.chunk', model,
        choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
        usage: { prompt_tokens: 10, completion_tokens: deltaCount, total_tokens: deltaCount + 10 } })
      response.write('data: [DONE]\n\n')
    } else {
      response.destroy(new Error('unsupported owned fixture protocol'))
      return
    }
    response.end(() => event('http_response_finished', { requestIndex: index }))
  }
  const server = createServer({ key, cert }, async (request, response) => {
    let bytes = 0
    const parts = []
    try {
      for await (const part of request) {
        bytes += part.length
        if (bytes > requestLimitBytes) throw new Error('owned fixture request exceeds bound')
        parts.push(part)
      }
      const body = Buffer.concat(parts)
      const parsed = JSON.parse(body)
      const index = requests.length + 1
      const requestedFormat = parsed.text?.format
      requests.push({ index, method: request.method, path: request.url, bytes,
        idempotencyKey: request.headers['idempotency-key'] ?? null,
        threadIdSha256: typeof request.headers['thread-id'] === 'string' ? createHash('sha256').update(request.headers['thread-id']).digest('hex') : null,
        requestSha256: createHash('sha256').update(body).digest('hex'), model: parsed.model,
        requestedTextFormatType: requestedFormat?.type ?? null,
        requestedTextSchemaSha256: requestedFormat?.schema === undefined ? null
          : createHash('sha256').update(JSON.stringify(requestedFormat.schema)).digest('hex') })
      response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' })
      response.flushHeaders()
      event('request_held', { requestIndex: index })
      const holder = { response, model: parsed.model, index }
      pending.add(holder)
      response.once('close', () => pending.delete(holder))
      if (released) { pending.delete(holder); finish(response, holder.model, index) }
    } catch (error) {
      event('owned_fixture_request_error', { errorType: error.name })
      response.destroy()
    }
  })
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve) })
  const port = server.address().port
  return { endpoint: `https://127.0.0.1:${port}${protocol === 'anthropic_messages' ? '/v1/messages'
    : protocol === 'openai_responses' ? '/v1/responses' : '/v1/chat/completions'}`,
    requests, events,
    release() {
      released = true
      event('gate_released')
      for (const holder of [...pending]) { pending.delete(holder); finish(holder.response, holder.model, holder.index) }
    },
    async close() {
      closed = true
      event('owned_fixture_closed')
      for (const holder of pending) holder.response.destroy()
      pending.clear()
      server.closeAllConnections()
      await new Promise(resolve => server.close(resolve))
    },
  }
}
