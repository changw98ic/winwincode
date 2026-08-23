import assert from 'node:assert/strict'
import test from 'node:test'

import { DshModelPort } from '../packages/dsh-profile/dist/index.js'

function modelRequest(overrides = {}) {
  return {
    requestId: 'thread:1',
    provider: 'deepseek',
    sessionId: 'session-1',
    threadId: 'thread-1',
    turnId: 'turn-1',
    request: {
      model: 'deepseek-chat',
      instructions: 'Use the available tools when needed.',
      input: [{
        type: 'message',
        role: 'user',
        content: [{ type: 'input_text', text: 'hello' }],
      }],
      tools: [{
        type: 'function',
        name: 'inspect',
        description: 'Inspect a fixture.',
        strict: false,
        parameters: {
          type: 'object',
          properties: { path: { type: 'string' } },
          required: ['path'],
        },
      }],
      tool_choice: 'auto',
      parallel_tool_calls: true,
      reasoning: null,
      text: null,
      store: false,
      stream: true,
      include: [],
      ...overrides,
    },
  }
}

async function collect(iterable) {
  const values = []
  for await (const value of iterable) values.push(value)
  return values
}

function fixtureRuntime(streamFactory) {
  const prepared = []
  const generated = []
  return {
    prepared,
    generated,
    runtime: {
      async prepareCall(config, signal) {
        prepared.push({ config, signal })
        return {
          config: Object.freeze({ ...config }),
          stream(options) {
            generated.push(options)
            return streamFactory(options)
          },
        }
      },
    },
  }
}

async function* streamedAnswer() {
  yield { type: 'block-start', index: 0, blockType: 'reasoning' }
  yield { type: 'reasoning-delta', index: 0, text: 'checked' }
  // DSH adapters may defer block-end until the provider terminal frame.
  yield { type: 'block-start', index: 1, blockType: 'text' }
  yield { type: 'text-delta', index: 1, text: 'hel' }
  yield { type: 'text-delta', index: 1, text: 'lo' }
  yield { type: 'block-end', index: 0, block: { type: 'reasoning', text: 'checked' } }
  yield { type: 'block-end', index: 1, block: { type: 'text', text: 'hello' } }
  yield {
    type: 'usage',
    usage: {
      inputTokens: 10,
      outputTokens: 4,
      cacheReadTokens: 3,
      cacheWriteTokens: 2,
      reasoningTokens: 1,
    },
  }
  yield { type: 'finish', reason: { kind: 'stop' } }
}

for (const route of [
  { provider: 'deepseek', model: 'deepseek-chat' },
  { provider: 'anthropic', model: 'claude-sonnet-4-6' },
]) {
  test(`streams Codex output through the DSH ${route.provider} provider family`, async () => {
    const fixture = fixtureRuntime(streamedAnswer)
    const port = new DshModelPort(fixture.runtime)
    const request = modelRequest({ model: route.model })
    request.provider = route.provider
    const signal = new AbortController().signal
    const messages = await collect(port.stream(request, signal))

    assert.deepEqual(fixture.prepared.map(call => call.config), [{
      provider: route.provider,
      model: route.model,
    }])
    assert.equal(fixture.prepared[0].signal, signal)
    assert.equal(fixture.generated.length, 1)
    assert.equal(fixture.generated[0].provider, route.provider)
    assert.equal(fixture.generated[0].model, route.model)
    assert.equal(fixture.generated[0].system, 'Use the available tools when needed.')
    assert.equal(fixture.generated[0].tools[0].name, 'inspect')
    assert.ok(messages.some(message => (
      message.type === 'reasoning_summary_delta' && message.delta === 'checked'
    )))
    assert.deepEqual(
      messages.filter(message => message.type === 'output_text_delta').map(message => message.delta),
      ['hel', 'lo'],
    )
    assert.deepEqual(messages.at(-1), {
      type: 'completed',
      responseId: 'thread:1',
      tokenUsage: {
        input_tokens: 15,
        cached_input_tokens: 3,
        cache_write_input_tokens: 2,
        output_tokens: 4,
        reasoning_output_tokens: 1,
        total_tokens: 19,
      },
      endTurn: true,
    })
  })
}

test('translates DSH function calls into Codex response items', async () => {
  const fixture = fixtureRuntime(async function* toolCall() {
    yield { type: 'block-start', index: 0, blockType: 'tool-call' }
    yield {
      type: 'tool-call-delta',
      index: 0,
      id: 'call-1',
      name: 'inspect',
      argumentsDelta: '{"path":',
    }
    yield {
      type: 'tool-call-delta',
      index: 0,
      id: 'call-1',
      argumentsDelta: '"sample"}',
    }
    yield {
      type: 'block-end',
      index: 0,
      block: {
        type: 'tool-call',
        id: 'call-1',
        name: 'inspect',
        arguments: '{"path":"sample"}',
      },
    }
    yield { type: 'finish', reason: { kind: 'tool-calls' } }
  })
  const messages = await collect(new DshModelPort(fixture.runtime).stream(
    modelRequest(),
    new AbortController().signal,
  ))
  const done = messages.find(message => message.type === 'output_item_done')
  assert.deepEqual(done.item, {
    type: 'function_call',
    id: 'fc_thread-1-0',
    name: 'inspect',
    arguments: '{"path":"sample"}',
    call_id: 'call-1',
  })
  assert.equal(messages.at(-1).endTurn, false)
})

test('replays reasoning and parallel Codex tool calls as one DSH assistant message', async () => {
  const fixture = fixtureRuntime(streamedAnswer)
  const request = modelRequest({
    input: [{
      type: 'message',
      role: 'user',
      content: [{ type: 'input_text', text: 'inspect both paths' }],
    }, {
      type: 'reasoning',
      id: 'reasoning-1',
      summary: [{ type: 'summary_text', text: 'Inspect both paths together.' }],
      content: [],
    }, {
      type: 'function_call',
      id: 'function-call-1',
      call_id: 'call-1',
      name: 'inspect',
      arguments: '{"path":"first"}',
    }, {
      type: 'function_call',
      id: 'function-call-2',
      call_id: 'call-2',
      name: 'inspect',
      arguments: '{"path":"second"}',
    }, {
      type: 'function_call_output',
      call_id: 'call-1',
      output: 'first result',
    }, {
      type: 'function_call_output',
      call_id: 'call-2',
      output: 'second result',
    }],
  })

  await collect(new DshModelPort(fixture.runtime).stream(
    request,
    new AbortController().signal,
  ))

  const assistantMessages = fixture.generated[0].messages
    .filter(message => message.role === 'assistant')
  assert.equal(assistantMessages.length, 1)
  assert.deepEqual(
    assistantMessages[0].content.map(block => block.type),
    ['reasoning', 'tool-call', 'tool-call'],
  )
  assert.deepEqual(
    assistantMessages[0].content.filter(block => block.type === 'tool-call')
      .map(block => block.id),
    ['call-1', 'call-2'],
  )
})

test('wraps Codex custom tools in DSH JSON-schema functions and restores freeform input', async () => {
  const fixture = fixtureRuntime(async function* customCall(options) {
    const tool = options.tools[0]
    yield { type: 'block-start', index: 0, blockType: 'tool-call' }
    yield {
      type: 'tool-call-delta',
      index: 0,
      id: 'call-patch',
      name: tool.name,
      argumentsDelta: '{"input":"*** Begin Patch\\n*** End Patch"}',
    }
    yield {
      type: 'block-end',
      index: 0,
      block: {
        type: 'tool-call',
        id: 'call-patch',
        name: tool.name,
        arguments: '{"input":"*** Begin Patch\\n*** End Patch"}',
      },
    }
    yield { type: 'finish', reason: { kind: 'tool-calls' } }
  })
  const request = modelRequest()
  request.request.tools = [{
    type: 'custom',
    name: 'apply_patch',
    description: 'Apply a patch.',
    format: { type: 'grammar', syntax: 'lark', definition: 'start: /.+/' },
  }]
  const messages = await collect(new DshModelPort(fixture.runtime).stream(
    request,
    new AbortController().signal,
  ))
  assert.deepEqual(fixture.generated[0].tools[0].parameters.required, ['input'])
  assert.deepEqual(messages.find(message => message.type === 'output_item_done').item, {
    type: 'custom_tool_call',
    id: 'fc_thread-1-0',
    call_id: 'call-patch',
    name: 'apply_patch',
    input: '*** Begin Patch\n*** End Patch',
  })
})

test('rejects unsupported capabilities before DSH preparation or provider I/O', async () => {
  const fixture = fixtureRuntime(streamedAnswer)
  const request = modelRequest({
    text: {
      format: {
        type: 'json_schema',
        strict: true,
        name: 'result',
        schema: { type: 'object' },
      },
    },
  })
  const messages = await collect(new DshModelPort(fixture.runtime).stream(
    request,
    new AbortController().signal,
  ))
  assert.equal(fixture.prepared.length, 0)
  assert.deepEqual(messages, [{
    type: 'error',
    error: {
      code: 'UNSUPPORTED_OPTION',
      message: 'structured output schemas are not supported by DSH',
    },
  }])
})

test('retains safe DSH error facts while redacting provider diagnostics and cancellation', async () => {
  const secret = 'TOKEN-structured-provider-secret'
  const fixture = fixtureRuntime(async function* cancelled(options) {
    await new Promise(resolvePromise => options.signal.addEventListener(
      'abort',
      resolvePromise,
      { once: true },
    ))
    yield {
      type: 'finish',
      reason: {
        kind: 'aborted',
        failure: {
          code: 'RATE_LIMIT',
          message: `retry later with ${secret}`,
          status: 429,
          providerRetryAfterMs: 750,
          requestId: 'provider-request-1',
        },
      },
    }
  })
  const controller = new AbortController()
  const iterator = new DshModelPort(fixture.runtime)
    .stream(modelRequest(), controller.signal)[Symbol.asyncIterator]()
  assert.equal((await iterator.next()).value.type, 'created')
  assert.equal((await iterator.next()).value.type, 'server_model')
  const terminal = iterator.next()
  controller.abort()
  assert.deepEqual((await terminal).value, {
    type: 'error',
    error: {
      code: 'RATE_LIMIT',
      message: 'DSH model request failed with code RATE_LIMIT',
      status: 429,
      providerRetryAfterMillis: 750,
      providerRequestId: 'sha256:8ecc55c1261eef75e54d383e6d41733fce87238da9837cd4ea112e5f89a549fc',
    },
  })
  assert.equal(JSON.stringify((await terminal).value).includes(secret), false)
})

test('rejects credential-bearing prepared config before adapter dispatch', async () => {
  const secret = 'TOKEN-prepared-config-secret'
  let streamCalls = 0
  const runtime = {
    async prepareCall(config) {
      return {
        config: { ...config, apiKey: secret },
        stream() {
          streamCalls += 1
          return streamedAnswer()
        },
      }
    },
  }
  const messages = await collect(new DshModelPort(runtime).stream(
    modelRequest(),
    new AbortController().signal,
  ))
  assert.equal(streamCalls, 0)
  assert.deepEqual(messages, [{
    type: 'error',
    error: {
      code: 'MODEL_PORT_PREPARED_CONFIG_INVALID',
      message: 'DSH prepared a model call with an invalid or credential-bearing configuration',
    },
  }])
  assert.equal(JSON.stringify(messages).includes(secret), false)
})

test('does not copy DSH credentials or unknown thrown text into kernel messages', async () => {
  const credential = 'TOKEN-fixture-secret'
  const runtime = {
    credential,
    async prepareCall() {
      throw new Error(`provider leaked ${credential}`)
    },
  }
  const messages = await collect(new DshModelPort(runtime).stream(
    modelRequest(),
    new AbortController().signal,
  ))
  assert.deepEqual(messages, [{
    type: 'error',
    error: {
      code: 'MODEL_RUNTIME_FAILED',
      message: 'DSH model runtime failed without a structured failure',
    },
  }])
  assert.equal(JSON.stringify(messages).includes(credential), false)
})
