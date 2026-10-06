#!/usr/bin/env node

// SPDX-License-Identifier: Apache-2.0

// OC-01 only: native Device transport verification using private authorization files.
// The fixture lease identifies a probe. It does not represent a product Worker run.
import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import { mkdirSync, readFileSync, statSync, lstatSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { encryptDeviceProvider } from '../../apps/client/src/device-provider-encryption.ts'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const privateDirectory = process.argv[2]
const modelId = process.argv[3] ?? 'glm-5.3-flash'
const attempt = process.argv[4] ?? 'initial'
if (!/^[a-z0-9-]{1,64}$/u.test(attempt)) throw new Error('invalid probe attempt')
const evidenceDirectory = join(privateDirectory, attempt)
const goBase = 'https://opencode.ai/inference/go/openai/v1'

function readPrivate(name, directory = privateDirectory) {
  const path = join(directory, name)
  const meta = lstatSync(path)
  if (!meta.isFile() || (meta.mode & 0o077) !== 0 || meta.size > 1024 * 1024) throw new Error('unsafe private input')
  return JSON.parse(readFileSync(path, 'utf8'))
}

function writePrivate(name, value) {
  writeFileSync(join(evidenceDirectory, name), `${JSON.stringify(value)}\n`, { mode: 0o600, flag: 'wx' })
}

function runNative(args) {
  const binary = join(process.env.CARGO_TARGET_DIR ?? join(root, 'target'), 'debug/examples/opencode_go_probe')
  const result = spawnSync(binary, args, { cwd: root, encoding: 'utf8', timeout: 120_000, maxBuffer: 1024 * 1024 })
  if (args[0] === 'execute') writeFileSync(`${args[3]}.stderr`, result.stderr ?? '', { mode: 0o600, flag: 'wx' })
  if (result.error !== undefined || result.status !== 0) throw new Error('native Device probe failed; private receipt retained')
  return result.stdout
}

function messages(name) {
  const chunks = readPrivate(name, evidenceDirectory)
  const decoded = chunks.filter(chunk => chunk.payload != null).map(chunk => JSON.parse(Buffer.from(chunk.payload.dataBase64, 'base64').toString('utf8')))
  if (decoded.some(message => message.type === 'error') || !decoded.some(message => message.type === 'completed')) throw new Error('incomplete native model response')
  return decoded
}

function modelOpen(sequence, request, conversationId) {
  const fixture = JSON.parse(readFileSync(join(root, 'tests/fixtures/contracts/execution-port.valid.json'), 'utf8'))
  const open = structuredClone(fixture.messages.find(message => message.kind === 'model.open'))
  const id = createHash('sha256').update(`${evidenceDirectory}:${sequence}`).digest('hex').toUpperCase().slice(0, 26)
  open.modelExchangeId = `mdl_${id}`
  open.requestId = `req_${id}`
  const bytes = Buffer.from(JSON.stringify({ requestId: open.requestId, provider: 'opencode-go-probe', sessionId: conversationId, threadId: conversationId, request }))
  open.request = { contentType: 'application/json', dataBase64: bytes.toString('base64'), payloadDigest: `sha256:${createHash('sha256').update(bytes).digest('hex')}` }
  return open
}

async function main() {
  if (privateDirectory === undefined || !lstatSync(privateDirectory).isDirectory() || (statSync(privateDirectory).mode & 0o077) !== 0) throw new Error('private authorization directory required')
  const account = readPrivate('account.json')
  const tokens = readPrivate('tokens.json')
  const remote = readPrivate('config.json').config.provider['opencode-go']
  const model = remote.models[modelId]
  const org = account.orgs[0]
  if (account.orgs.length !== 1 || remote.api !== goBase || remote.npm !== '@ai-sdk/openai-compatible'
      || model === undefined || model.provider !== undefined || !model.tool_call
      || remote.options.headers['x-opencode-org-id'] !== org.id
      || tokens.expires_at * 1000 < Date.now() + 120_000) throw new Error('unverified Go route or stale credentials')
  mkdirSync(evidenceDirectory, { mode: 0o700 })
  const directory = join(evidenceDirectory, 'native-device')
  mkdirSync(directory, { mode: 0o700 })
  const snapshot = JSON.parse(runNative(['snapshot', directory]))
  const conversationId = `wwc-opencode-go-probe-${createHash('sha256').update(evidenceDirectory).digest('hex').slice(0, 26)}`
  const config = { providerId: 'opencode-go-probe', displayName: 'OpenCode Go authorization probe', endpoint: `${goBase}/chat/completions`, protocol: 'openai_chat_completions', modelIds: [modelId], enabled: true }
  const envelope = await encryptDeviceProvider(snapshot, 'opencode-go-probe-save', { operation: 'save', config, apiKey: tokens.access_token,
    customHeaders: { 'x-opencode-org-id': org.id, 'x-opencode-session': conversationId, 'User-Agent': 'winwincode/0.1.0-alpha.2' } })
  writePrivate('native-config-envelope.json', envelope)
  runNative(['apply', directory, join(evidenceDirectory, 'native-config-envelope.json')])

  const source = 'fn subtract(a: i32, b: i32) -> i32 { a - b }\n'
  writeFileSync(join(evidenceDirectory, 'probe-source.rs'), source, { mode: 0o600, flag: 'wx' })
  const request = { model: modelId, instructions: 'You are a coding agent. Inspect the supplied Rust source using the advertised read_source tool. Then report the return value for subtract(41, 24).',
    input: [{ type: 'message', role: 'user', content: [{ type: 'input_text', text: 'Read probe-source.rs with the tool and verify subtract(41, 24). Do not assume its implementation.' }] }],
    tools: [{ type: 'function', name: 'read_source', description: 'Read the authorized probe-source.rs file. Takes no arguments.', parameters: { type: 'object', properties: {}, required: [], additionalProperties: false }, strict: true }],
    tool_choice: 'required', parallel_tool_calls: false, stream: true, store: false }
  writePrivate('native-model-open-1.json', modelOpen(1, request, conversationId))
  console.log('OC-01 native Device request 1 started; uncertain calls are not retried')
  runNative(['execute', directory, join(evidenceDirectory, 'native-model-open-1.json'), join(evidenceDirectory, 'native-model-result-1.json')])
  const first = messages('native-model-result-1.json')
  const calls = first.filter(message => message.type === 'output_item_done' && message.item.type === 'function_call').map(message => message.item)
  if (calls.length !== 1 || calls[0].name !== 'read_source' || calls[0].namespace !== undefined
      || Object.keys(JSON.parse(calls[0].arguments)).length !== 0) throw new Error('model did not produce the single authorized tool call')
  const actualSource = readFileSync(join(evidenceDirectory, 'probe-source.rs'), 'utf8')
  request.input.push(calls[0], { type: 'function_call_output', call_id: calls[0].call_id, output: actualSource })
  request.tool_choice = 'none'
  writePrivate('native-model-open-2.json', modelOpen(2, request, conversationId))
  console.log('OC-01 actual source read complete; native Device request 2 started')
  runNative(['execute', directory, join(evidenceDirectory, 'native-model-open-2.json'), join(evidenceDirectory, 'native-model-result-2.json')])
  const second = messages('native-model-result-2.json')
  const text = second.filter(message => message.type === 'output_text_delta').map(message => message.delta).join('')
  const models = [...new Set([...first, ...second].filter(message => message.type === 'server_model').map(message => message.model))]
  const completions = [...first, ...second].filter(message => message.type === 'completed').map(message => ({ responseId: message.responseId, usage: message.tokenUsage }))
  const evidence = { evidenceClass: 'real-oauth-native-device-provider-probe', productWorkerAcceptance: false, accountId: account.user.id,
    organizationId: org.id, modelId, observedModels: models, endpoint: config.endpoint, conversationId, tool: calls[0].name,
    toolCallId: calls[0].call_id, sourceSha256: createHash('sha256').update(actualSource).digest('hex'), answer: text, completions,
    passed: text.includes('17'), useBalance: { value: false, evidence: 'human-confirmed' } }
  writePrivate('native-probe-evidence.json', evidence)
  console.log(JSON.stringify(evidence))
  if (!evidence.passed) throw new Error('unexpected model answer')
}

try { await main() } catch {
  console.error('OpenCode Go probe failed. Private receipts retain the exact request state. Do not restart a sent request with a new identity.')
  process.exitCode = 1
}
