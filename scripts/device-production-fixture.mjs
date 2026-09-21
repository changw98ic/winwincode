// SPDX-License-Identifier: Apache-2.0

/**
 * Device-only production acceptance prerequisites.
 *
 * Production Server execution is fixed to Device Worker. These helpers keep
 * the acceptance runners on that path: model secrets belong on the Device,
 * session model routes must match Device-local Provider credentials, and the
 * Server must never receive centralized model environment variables.
 */

import assert from 'node:assert/strict'
import {
  createCipheriv,
  createECDH,
  createHash,
  hkdfSync,
  randomBytes,
  X509Certificate,
} from 'node:crypto'
import { spawn, spawnSync } from 'node:child_process'
import { chmodSync, openSync, readFileSync, writeFileSync } from 'node:fs'
import { createServer as createHttpsServer } from 'node:https'
import { createServer as createNetServer } from 'node:net'
import { join } from 'node:path'

/** Server environment keys that encode the removed Server-local model path. */
export const FORBIDDEN_SERVER_MODEL_ENVIRONMENT_KEYS = Object.freeze([
  'WWC_SERVER_MODEL_PROVIDER_ID',
  'WWC_SERVER_MODEL_ID',
  'WWC_SERVER_MODEL_ANTHROPIC_ENDPOINT',
  'WWC_SERVER_MODEL_API_KEY',
  'WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID',
])

/**
 * Ordered Device-only prerequisites that a production vertical must establish
 * before chat/StrongFlow/cancel/restart. Kept as data so unit tests can assert
 * the migration did not invent a parallel Server-local path.
 */
export const DEVICE_ONLY_PREREQUISITES = Object.freeze([
  'temporary-real-git-repository',
  'device-enroll-pair',
  'client-connect',
  'client-occupancy',
  'repository-binding',
  'device-local-provider',
  'worker-launch-grant',
  'launch-anchor',
  'chat-strongflow-cancel-restart',
])

/**
 * Device Provider credential reference id used by the Server session route
 * gate. Mirrors `device_providers::route_reference` in
 * crates/winwincode-server/src/device_providers.rs.
 */
export function deviceCredentialReferenceId({ clientNodeId, providerId }) {
  assert.equal(typeof clientNodeId, 'string')
  assert.equal(typeof providerId, 'string')
  assert.ok(clientNodeId.length > 0)
  assert.ok(providerId.length > 0)
  const digest = createHash('sha256')
    .update(`${clientNodeId}\n${providerId}`)
    .digest('hex')
    .toUpperCase()
  return `crd_0${digest.slice(0, 25)}`
}

/**
 * Session model route for Device-local execution. The credential reference
 * must be the Device-bound route id, not a Server-side credential.
 */
export function configuredDeviceModelRoute({
  clientNodeId,
  providerId,
  modelId,
}) {
  return {
    providerId,
    modelId,
    credentialReferenceId: deviceCredentialReferenceId({ clientNodeId, providerId }),
  }
}

/** Deterministic Device-local test Provider used by acceptance runners. */
export function deterministicDeviceProvider({
  providerId = 'winwincode-device-deterministic',
  modelId = 'device-deterministic-model',
} = {}) {
  return Object.freeze({
    kind: DEVICE_PROVIDER_KIND_DETERMINISTIC,
    fixture: true,
    providerId,
    modelId,
    displayName: 'WinWinCode Device deterministic Provider',
    endpointHint: 'https://127.0.0.1:<device-mock-model-port>/v1/messages',
    protocol: 'anthropic_messages',
    seedMode: 'deterministic-loopback',
  })
}

/** Provider identity kinds for isolated Device verticals. */
export const DEVICE_PROVIDER_KIND_DETERMINISTIC = 'device-local-deterministic'
export const DEVICE_PROVIDER_KIND_REAL = 'device-local-real'

/**
 * Normalizes a base or full Anthropic Messages endpoint to
 * `https://…/v1/messages`. Rejects non-HTTPS and credential-bearing URLs.
 */
export function normalizeAnthropicMessagesEndpoint(value) {
  assert.equal(typeof value, 'string')
  const trimmed = value.trim()
  assert.ok(trimmed.length > 0, 'Device Provider endpoint must be non-empty')
  const url = new URL(trimmed)
  assert.equal(url.protocol, 'https:', 'Device Provider endpoint must use https')
  assert.equal(url.username, '', 'Device Provider endpoint must not embed credentials')
  assert.equal(url.password, '', 'Device Provider endpoint must not embed credentials')
  assert.equal(url.search, '', 'Device Provider endpoint must not include a query string')
  assert.equal(url.hash, '', 'Device Provider endpoint must not include a fragment')
  const path = url.pathname.replace(/\/+$/u, '')
  if (path.endsWith('/v1/messages')) {
    return `https://${url.host}${path}`
  }
  return `https://${url.host}${path}/v1/messages`
}

/**
 * Builds one real Device-local Provider identity from secrets + model route.
 * Secrets stay Device-local; the returned object never carries API keys.
 */
export function realDeviceProvider({
  providerId,
  modelId,
  endpoint,
  displayName = null,
} = {}) {
  assert.equal(typeof providerId, 'string')
  assert.ok(providerId.length > 0, 'real Device Provider requires providerId')
  assert.equal(typeof modelId, 'string')
  assert.ok(modelId.length > 0, 'real Device Provider requires modelId')
  const normalizedEndpoint = normalizeAnthropicMessagesEndpoint(endpoint)
  return Object.freeze({
    kind: DEVICE_PROVIDER_KIND_REAL,
    fixture: false,
    providerId,
    modelId,
    displayName: displayName ?? `Device-local real Provider (${providerId})`,
    endpointHint: normalizedEndpoint,
    endpoint: normalizedEndpoint,
    protocol: 'anthropic_messages',
    seedMode: 'encrypted-web-to-device-apply-real-endpoint',
  })
}

/**
 * Selects the Device-local Provider identity for an isolated coding vertical.
 *
 * - No Device secrets → deterministic fixture (acceptance default).
 * - Secrets + modelRoute (providerId/modelId) → real Device-local identity.
 * - Never reads WWC_SERVER_MODEL_* and never puts secrets on Server env.
 *
 * Endpoint resolution order for the real path:
 * `deviceRoute.endpoint` → `deviceProviderEndpoint` → `ZHIPU_BASE_URL`.
 */
export function selectDeviceProvider({
  deviceProviderSecrets = [],
  deviceRoute = null,
  deviceProviderEndpoint = null,
  environment = process.env,
} = {}) {
  const secret = Array.isArray(deviceProviderSecrets)
    ? deviceProviderSecrets.find(value => typeof value === 'string' && value.length > 0) ?? null
    : null
  const providerId = typeof deviceRoute?.providerId === 'string'
    && deviceRoute.providerId.length > 0
    && !deviceRoute.providerId.startsWith('winwincode-device-deterministic')
    ? deviceRoute.providerId
    : null
  const modelId = typeof deviceRoute?.modelId === 'string'
    && deviceRoute.modelId.length > 0
    && deviceRoute.modelId !== 'device-deterministic-model'
    ? deviceRoute.modelId
    : null
  if (secret === null || providerId === null || modelId === null) {
    return deterministicDeviceProvider()
  }
  const rawEndpoint = (typeof deviceRoute?.endpoint === 'string' && deviceRoute.endpoint.length > 0)
    ? deviceRoute.endpoint
    : (typeof deviceProviderEndpoint === 'string' && deviceProviderEndpoint.length > 0)
      ? deviceProviderEndpoint
      : environment?.ZHIPU_BASE_URL
  assert.ok(
    typeof rawEndpoint === 'string' && rawEndpoint.length > 0,
    'Device-local real Provider requires an https endpoint '
    + '(deviceRoute.endpoint, deviceProviderEndpoint, or ZHIPU_BASE_URL)',
  )
  return realDeviceProvider({
    providerId,
    modelId,
    endpoint: rawEndpoint,
    displayName: typeof deviceRoute?.displayName === 'string' && deviceRoute.displayName.length > 0
      ? deviceRoute.displayName
      : null,
  })
}

export const DEVICE_PROVIDER_ENCRYPTION_CONTEXT = 'winwincode.device-provider.v1'
export const DETERMINISTIC_LOOPBACK_RESPONSE = 'WinWinCode deterministic loopback response'
export const DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER =
  'Apply the requested source change in the assigned checkout.'
export const DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER =
  'Use read-only commands against exactly workInput.candidateRef.'
export const DETERMINISTIC_PLANNER_PROTOCOL = 'winwincode.planner-solution.v1'
export const DETERMINISTIC_VERIFICATION_PROTOCOL = 'winwincode.independent-verification-result.v1'
export const DETERMINISTIC_VERIFICATION_CALL_ID = 'loopback-verification-command'
export const DETERMINISTIC_VERIFICATION_POLL_CALL_ID = 'loopback-verification-command-poll'
/** Long enough for contract verification methods (e.g. git rev-parse) to finish. */
export const DETERMINISTIC_VERIFICATION_YIELD_MS = 30_000

/**
 * Encrypts one Device Provider mutation with the Device public key.
 * The ciphertext is the production Web → Device apply payload; the API key
 * never appears in Server environment maps or public receipts.
 */
export function encryptDeviceProviderEnvelope(snapshot, requestId, mutation) {
  assert.equal(typeof snapshot?.clientNodeId, 'string')
  assert.equal(typeof snapshot?.encryptionPublicKey, 'string')
  assert.equal(typeof requestId, 'string')
  const expectedRevision = snapshot.revision
  const ephemeral = createECDH('prime256v1')
  ephemeral.generateKeys()
  const shared = ephemeral.computeSecret(Buffer.from(snapshot.encryptionPublicKey, 'base64'))
  const aad = `${DEVICE_PROVIDER_ENCRYPTION_CONTEXT}\n${snapshot.clientNodeId}\n${requestId}\n${expectedRevision}`
  const key = Buffer.from(hkdfSync(
    'sha256',
    shared,
    Buffer.from(DEVICE_PROVIDER_ENCRYPTION_CONTEXT),
    Buffer.from(aad),
    32,
  ))
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', key, nonce)
  cipher.setAAD(Buffer.from(aad))
  const plaintext = Buffer.from(JSON.stringify(mutation))
  const ciphertext = Buffer.concat([cipher.update(plaintext), cipher.final(), cipher.getAuthTag()])
  key.fill(0)
  shared.fill(0)
  plaintext.fill(0)
  return {
    clientNodeId: snapshot.clientNodeId,
    expectedRevision,
    nonce: nonce.toString('base64'),
    publicKey: ephemeral.getPublicKey().toString('base64'),
    ciphertext: ciphertext.toString('base64'),
    requestId,
  }
}

async function freeLoopbackPort() {
  const server = createNetServer()
  await new Promise((resolvePromise, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolvePromise)
  })
  const address = server.address()
  const port = typeof address === 'object' && address !== null ? address.port : 0
  await new Promise(resolvePromise => server.close(resolvePromise))
  assert.ok(port > 0, 'Device model fixture port must be positive')
  return port
}

function containsValueWithType(value, expected) {
  if (Array.isArray(value)) return value.some(item => containsValueWithType(item, expected))
  if (value === null || typeof value !== 'object') return false
  if (value.type === expected) return true
  return Object.values(value).some(item => containsValueWithType(item, expected))
}

function findTextValue(value, needle) {
  if (typeof value === 'string') return value.includes(needle) ? value : null
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = findTextValue(item, needle)
      if (found !== null) return found
    }
    return null
  }
  if (value === null || typeof value !== 'object') return null
  for (const child of Object.values(value)) {
    const found = findTextValue(child, needle)
    if (found !== null) return found
  }
  return null
}

function findToolOutput(value, callId) {
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = findToolOutput(item, callId)
      if (found !== null) return found
    }
    return null
  }
  if (value === null || typeof value !== 'object') return null
  const kind = value.type
  const id = value.call_id ?? value.tool_use_id
  if (
    (kind === 'function_call_output' || kind === 'custom_tool_call_output' || kind === 'tool_result')
    && id === callId
  ) return value
  for (const child of Object.values(value)) {
    const found = findToolOutput(child, callId)
    if (found !== null) return found
  }
  return null
}

function toolOutputText(output) {
  if (typeof output?.output === 'string') return output.output
  if (typeof output?.content === 'string') return output.content
  if (Array.isArray(output?.content)) {
    return output.content
      .filter(item => typeof item?.text === 'string')
      .map(item => item.text)
      .join('\n')
  }
  if (Array.isArray(output?.output)) {
    return output.output
      .filter(item => typeof item?.text === 'string')
      .map(item => item.text)
      .join('\n')
  }
  return null
}

export function parseProcessExitCode(text) {
  if (typeof text !== 'string') return null
  for (const line of text.split(/\r?\n/u)) {
    const match = line.match(/^(?:Exit code: |Process exited with code )(-?\d+)\s*$/u)
    if (match) {
      const value = Number(match[1])
      return Number.isSafeInteger(value) ? value : null
    }
  }
  return null
}

/**
 * Extracts StrongFlow workInput fields the Device deterministic Provider must
 * honor for reviewer/verifier roles: contract criterion ids and the approved
 * verification method. Top-level `criterionIds` / `verificationCommand` remain
 * accepted for older runners; WorkRun dispatch uses nested WorkContract shape.
 */
export function workInputFromRequest(request) {
  const text = findTextValue(request, 'StrongFlow workInput (canonical JSON):')
  if (text === null) return null
  const start = text.indexOf('{')
  if (start < 0) return null
  // WorkInput is embedded after the marker; take the first balanced JSON object.
  let depth = 0
  let end = -1
  for (let index = start; index < text.length; index += 1) {
    const character = text[index]
    if (character === '{') depth += 1
    else if (character === '}') {
      depth -= 1
      if (depth === 0) {
        end = index + 1
        break
      }
    }
  }
  if (end < 0) return null
  try {
    const parsed = JSON.parse(text.slice(start, end))
    const criterionIds = []
    const pushCriterionIds = values => {
      if (!Array.isArray(values)) return
      for (const value of values) {
        if (typeof value === 'string' && value.length > 0 && !criterionIds.includes(value)) {
          criterionIds.push(value)
        }
      }
    }
    pushCriterionIds(parsed.criterionIds)
    pushCriterionIds(parsed.criterion_ids)
    pushCriterionIds(parsed.workItem?.criterionIds)
    pushCriterionIds(parsed.work_item?.criterion_ids)
    const contractCriteria = parsed.workContract?.criteria
      ?? parsed.work_contract?.criteria
      ?? []
    if (Array.isArray(contractCriteria)) {
      for (const criterion of contractCriteria) {
        const id = criterion?.id
        if (typeof id === 'string' && id.length > 0 && !criterionIds.includes(id)) {
          criterionIds.push(id)
        }
      }
    }
    const verificationCommands = []
    const pushVerificationCommand = value => {
      if (typeof value === 'string' && value.length > 0 && !verificationCommands.includes(value)) {
        verificationCommands.push(value)
      }
    }
    if (Array.isArray(contractCriteria)) {
      for (const criterion of contractCriteria) {
        pushVerificationCommand(criterion?.verificationMethod)
        pushVerificationCommand(criterion?.verification_method)
      }
    }
    const walk = value => {
      if (Array.isArray(value)) {
        for (const item of value) walk(item)
        return
      }
      if (value === null || typeof value !== 'object') return
      pushVerificationCommand(value.verificationCommand)
      pushVerificationCommand(value.verification_command)
      pushVerificationCommand(value.verificationMethod)
      pushVerificationCommand(value.verification_method)
      for (const child of Object.values(value)) walk(child)
    }
    walk(parsed)
    return {
      deliverySpecId: parsed.deliverySpecId ?? parsed.delivery_spec_id ?? null,
      deliverySpecRevision: Number(parsed.deliverySpecRevision ?? parsed.delivery_spec_revision ?? 0),
      candidateRef: parsed.candidateRef ?? parsed.candidate_ref ?? null,
      criterionIds,
      verificationCommand: verificationCommands[0] ?? 'git rev-parse --verify HEAD',
    }
  } catch {
    return null
  }
}

export function resolveVerificationObservation(providerRequest) {
  const primary = findToolOutput(providerRequest, DETERMINISTIC_VERIFICATION_CALL_ID)
  const poll = findToolOutput(providerRequest, DETERMINISTIC_VERIFICATION_POLL_CALL_ID)
  const primaryExit = primary === null ? null : parseProcessExitCode(toolOutputText(primary))
  const pollExit = poll === null ? null : parseProcessExitCode(toolOutputText(poll))
  if (pollExit !== null) {
    return { exitCode: pollExit, evidenceSourceId: DETERMINISTIC_VERIFICATION_POLL_CALL_ID, hasToolOutput: true }
  }
  if (primaryExit !== null) {
    return { exitCode: primaryExit, evidenceSourceId: DETERMINISTIC_VERIFICATION_CALL_ID, hasToolOutput: true }
  }
  return {
    exitCode: null,
    evidenceSourceId: primary !== null || poll !== null
      ? (poll !== null ? DETERMINISTIC_VERIFICATION_POLL_CALL_ID : DETERMINISTIC_VERIFICATION_CALL_ID)
      : null,
    hasToolOutput: primary !== null || poll !== null,
  }
}

function deterministicPlannerProduct(criterionIds) {
  return JSON.stringify({
    schemaVersion: 1,
    protocol: DETERMINISTIC_PLANNER_PROTOCOL,
    solution: {
      id: 'solution:deterministic-device',
      summary: 'Apply the approved Delivery plan.',
      approach: ['Apply the approved scope and run the exact checks.'],
      components: [{
        id: 'component:deterministic-device',
        label: 'Approved Delivery',
        responsibility: 'Represent the approved source change.',
        kind: 'component',
        trustBoundary: 'repository',
        unresolved: false,
        repositoryPathPrefixes: ['.'],
      }],
      connections: [{
        id: 'connection:deterministic-device',
        from: 'platform:device-provider',
        to: 'component:deterministic-device',
        label: 'plans',
      }],
    },
    architectureDiagram: {
      id: 'diagram:deterministic-architecture',
      kind: 'system_architecture',
      title: 'Approved Delivery architecture',
      nodes: [{
        id: 'diagram:deterministic-architecture:stage',
        label: 'Delivery stage',
        description: 'Applies the approved source change.',
        kind: 'stage',
        trustBoundary: null,
        unresolved: false,
      }],
      edges: [],
    },
    processDiagram: {
      id: 'diagram:deterministic-process',
      kind: 'process_flow',
      title: 'Approved Delivery process',
      nodes: [{
        id: 'diagram:deterministic-process:stage',
        label: 'Plan and verify',
        description: 'Plans and verifies the approved change.',
        kind: 'stage',
        trustBoundary: null,
        unresolved: false,
      }],
      edges: [],
    },
    risks: ['The exact check may expose a regression.'],
    unresolvedItems: [],
    workItemProposals: [{
      id: 'wit_01J00000000000000000000001',
      title: 'Apply approved Delivery plan',
      goal: 'Apply the approved source change and run its checks.',
      criterionIds,
      dependsOn: [],
    }],
  })
}

export function deterministicVerificationProduct({
  passed,
  workInput,
  evidenceSourceId = DETERMINISTIC_VERIFICATION_CALL_ID,
}) {
  const criterionIds = workInput?.criterionIds?.length
    ? workInput.criterionIds
    : ['api-terminal-criterion']
  const explanation = passed
    ? 'The observed verification command completed successfully.'
    : 'The observed verification command exited with a non-zero code.'
  return JSON.stringify({
    protocol: DETERMINISTIC_VERIFICATION_PROTOCOL,
    delivery_spec_id: workInput?.deliverySpecId ?? 'dlv_01J00000000000000000000001',
    delivery_spec_revision: workInput?.deliverySpecRevision ?? 0,
    candidate_ref: workInput?.candidateRef ?? 'candidate:deterministic',
    findings: criterionIds.map(criterionId => ({
      finding_id: `finding:deterministic:${criterionId}`,
      criterion_id: criterionId,
      verdict: passed ? 'pass' : 'fail',
      explanation,
      evidence_sources: [{ source_id: evidenceSourceId }],
    })),
  })
}

/**
 * Starts a Device-local deterministic anthropic-messages HTTPS Provider.
 * Trust is the explicit fixture certificate DER only (never system CAs).
 */
export function startDeterministicDeviceModelServer({
  certificatePath,
  privateKeyPath,
  chatContent = 'WinWinCode Device deterministic Provider completed the Chat workflow.',
}) {
  const errors = []
  const requests = []
  const server = createHttpsServer({
    key: readFileSync(privateKeyPath),
    cert: readFileSync(certificatePath),
  }, (request, response) => {
    const chunks = []
    let size = 0
    request.on('data', chunk => {
      size += chunk.length
      if (size <= 512 * 1024) chunks.push(chunk)
    })
    request.on('end', () => {
      try {
        const bodyText = Buffer.concat(chunks).toString('utf8')
        assert.ok(size <= 512 * 1024, 'Device Provider request exceeds fixture bound')
        const providerRequest = JSON.parse(bodyText)
        requests.push({
          path: request.url ?? '',
          verification: bodyText.includes(DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER),
          executor: bodyText.includes(DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER),
          planner: bodyText.includes(DETERMINISTIC_PLANNER_PROTOCOL),
          hasToolOutput: /function_call_output|custom_tool_call_output|tool_result/u.test(bodyText),
        })
        const shellTool = providerRequest.tools?.find(tool => (
          /(?:^|__)shell_command$|(?:^|__)exec_command$/u.test(tool.name)
        )) ?? null
        const workInput = workInputFromRequest(providerRequest)
        const isExecutor = bodyText.includes(DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER)
        const isVerification = bodyText.includes(DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER)
        const isPlanner = bodyText.includes(DETERMINISTIC_PLANNER_PROTOCOL)
          && workInput?.criterionIds?.length
        const executorOutput = containsValueWithType(providerRequest, 'function_call_output')
          || containsValueWithType(providerRequest, 'custom_tool_call_output')
          || (bodyText.includes('loopback-executor-change')
            && /function_call_output|custom_tool_call_output|tool_result/u.test(bodyText))
        const verificationObservation = resolveVerificationObservation(providerRequest)
        const verificationExit = verificationObservation.exitCode
        const verificationEvidenceSourceId = verificationObservation.evidenceSourceId
          ?? DETERMINISTIC_VERIFICATION_CALL_ID
        const verificationCommand = workInput?.verificationCommand
          ?? 'git rev-parse --verify HEAD'

        let text = chatContent
        let useTool = false
        let toolCallId = 'loopback-chat'
        let toolCommand = 'true'
        let stopReason = 'end_turn'
        let toolYieldMs = 1000

        if (isExecutor && !executorOutput) {
          useTool = true
          stopReason = 'tool_use'
          toolCallId = 'loopback-executor-change'
          toolCommand = shellTool?.input_schema?.properties?.cmd
            ? "printf '%s\\n' 'status: complete' > TASK.md; printf '%s\\n' 'deterministic Device candidate' > .winwincode-api-candidate; git status --porcelain=v1 --untracked-files=all"
            : "printf '%s\\n' 'status: complete' > TASK.md; printf '%s\\n' 'deterministic Device candidate' > .winwincode-api-candidate; git status --porcelain=v1 --untracked-files=all"
          text = ''
        } else if (isExecutor) {
          text = 'The requested stage action completed.'
        } else if (isVerification && !verificationObservation.hasToolOutput && shellTool) {
          useTool = true
          stopReason = 'tool_use'
          toolCallId = DETERMINISTIC_VERIFICATION_CALL_ID
          toolCommand = verificationCommand
          toolYieldMs = DETERMINISTIC_VERIFICATION_YIELD_MS
          text = ''
        } else if (isVerification && verificationExit === null && shellTool
          && verificationEvidenceSourceId === DETERMINISTIC_VERIFICATION_CALL_ID) {
          // Tool output arrived without a sealed exit code (process still
          // running). Do not invent a fail verdict — poll once under a
          // distinct evidence source so Core can finish and seal command
          // evidence before the verification product is emitted.
          useTool = true
          stopReason = 'tool_use'
          toolCallId = DETERMINISTIC_VERIFICATION_POLL_CALL_ID
          toolCommand = verificationCommand
          toolYieldMs = DETERMINISTIC_VERIFICATION_YIELD_MS
          text = ''
        } else if (isVerification && verificationExit !== null) {
          text = deterministicVerificationProduct({
            passed: verificationExit === 0,
            workInput,
            evidenceSourceId: verificationEvidenceSourceId,
          })
        } else if (isVerification) {
          // Still no sealed exit after the poll. Do not invent pass or fail:
          // Worker bind_verification_evidence requires a sealed command
          // outcome for the cited source and must fail closed without one.
          text = 'Verification command did not yield a sealed exit code; no independent result emitted.'
        } else if (isPlanner) {
          text = deterministicPlannerProduct(workInput.criterionIds)
        }

        const events = [
          ['message_start', {
            type: 'message_start',
            message: {
              id: `msg-device-${Date.now()}`,
              type: 'message',
              role: 'assistant',
              model: providerRequest.model ?? 'device-deterministic-model',
              content: [],
              usage: { input_tokens: 1, output_tokens: 0 },
            },
          }],
        ]
        if (useTool) {
          assert.ok(shellTool !== null, 'Device Provider fixture requires an exposed shell tool')
          events.push(['content_block_start', {
            type: 'content_block_start',
            index: 0,
            content_block: {
              type: 'tool_use',
              id: toolCallId,
              name: shellTool.name,
              input: {},
            },
          }])
          const input = shellTool.input_schema?.properties?.cmd
            ? { cmd: toolCommand, workdir: '.', yield_time_ms: toolYieldMs }
            : { command: toolCommand, workdir: '.' }
          events.push(['content_block_delta', {
            type: 'content_block_delta',
            index: 0,
            delta: { type: 'input_json_delta', partial_json: JSON.stringify(input) },
          }])
          events.push(['content_block_stop', { type: 'content_block_stop', index: 0 }])
        } else {
          events.push(['content_block_start', {
            type: 'content_block_start',
            index: 0,
            content_block: { type: 'text', text: '' },
          }])
          events.push(['content_block_delta', {
            type: 'content_block_delta',
            index: 0,
            delta: { type: 'text_delta', text },
          }])
          events.push(['content_block_stop', { type: 'content_block_stop', index: 0 }])
        }
        events.push(['message_delta', {
          type: 'message_delta',
          delta: { stop_reason: stopReason, stop_sequence: null },
          usage: { output_tokens: 1 },
        }])
        events.push(['message_stop', { type: 'message_stop' }])
        const payload = events
          .map(([name, value]) => `event: ${name}\ndata: ${JSON.stringify(value)}\n\n`)
          .join('')
        response.writeHead(200, {
          'content-type': 'text/event-stream',
          'cache-control': 'no-store',
          connection: 'close',
        })
        response.end(payload)
      } catch (error) {
        errors.push(String(error instanceof Error ? error.message : error))
        response.writeHead(500, { 'content-type': 'text/plain', connection: 'close' })
        response.end('device provider fixture failure\n')
      }
    })
  })
  return {
    errors,
    requests,
    server,
    async listen() {
      const port = await freeLoopbackPort()
      await new Promise((resolvePromise, reject) => {
        server.once('error', reject)
        server.listen(port, '127.0.0.1', resolvePromise)
      })
      this.port = port
      this.endpoint = `https://127.0.0.1:${String(port)}/v1/messages`
      return this
    },
    close() {
      server.closeAllConnections?.()
      return new Promise(resolvePromise => server.close(() => resolvePromise()))
    },
  }
}

/**
 * Seeds one Device-local Provider through the production encrypted Web → Device
 * HTTP apply path. Secrets are sealed to the Device store; Server only relays
 * ciphertext and public receipts.
 */
export async function seedDeviceLocalProvider({
  api,
  publicClientId,
  providerId,
  modelId,
  endpoint,
  apiKey,
  displayName = 'WinWinCode Device deterministic Provider',
  timeoutMillis = 60_000,
}) {
  assert.equal(typeof apiKey, 'string')
  assert.ok(apiKey.length > 0)
  assert.ok(typeof publicClientId === 'string' && publicClientId.length > 0,
    'Device public client id is required to seed Provider')
  const providerPath = `/api/v1/clients/${encodeURIComponent(publicClientId)}/providers`
  const current = await waitFor(async () => {
    const response = await api.request(providerPath)
    return response.status === 200
      && response.json?.snapshot?.clientNodeId
      && response.json?.snapshot?.encryptionPublicKey
      ? response
      : false
  }, 'Device Provider public snapshot', timeoutMillis)
  const snapshot = current.json.snapshot
  const requestId = `provider_save_${Date.now()}_${randomBytes(6).toString('hex')}`
  const encrypted = encryptDeviceProviderEnvelope(snapshot, requestId, {
    operation: 'save',
    config: {
      providerId,
      displayName,
      endpoint,
      protocol: 'anthropic_messages',
      modelIds: [modelId],
      enabled: true,
    },
    apiKey,
  })
  const applied = await api.request(providerPath, { method: 'POST', body: encrypted })
  assert.equal(applied.status, 202, `Device Provider apply failed: ${applied.text}`)
  const saved = await waitFor(async () => {
    const response = await api.request(`${providerPath}/receipts/${encodeURIComponent(requestId)}`)
    if (response.json?.receipt && response.json.receipt.outcome !== 'saved') {
      throw new Error(`Device provider rejected: ${JSON.stringify(response.json.receipt)}`)
    }
    return response.status === 200
      && response.json?.receipt?.outcome === 'saved'
      && response.json?.snapshot?.providers?.some(item => (
        item.config.providerId === providerId && item.credentialConfigured
      ))
      ? response.json
      : false
  }, 'Device Provider encrypted apply receipt', timeoutMillis)
  return {
    providerId,
    modelId,
    endpoint,
    credentialReferenceId: deviceCredentialReferenceId({
      clientNodeId: snapshot.clientNodeId,
      providerId,
    }),
    clientNodeId: snapshot.clientNodeId,
    receiptOutcome: saved.receipt?.outcome ?? null,
  }
}

/**
 * Rejects Server environments that still encode the removed Server-local
 * model execution path, including real GLM secrets.
 */
export function assertServerEnvironmentIsDeviceOnly(serverEnvironment = {}) {
  for (const key of FORBIDDEN_SERVER_MODEL_ENVIRONMENT_KEYS) {
    assert.equal(
      serverEnvironment[key],
      undefined,
      `${key} must not be set on the Server; Device owns Provider configuration`,
    )
  }
}

/**
 * Asserts known model secrets never appear in Server-facing environment maps.
 * Real GLM/modeled secrets belong only on the Device config path.
 */
export function assertDeviceSecretsNeverOnServer(serverEnvironment = {}, secrets = []) {
  for (const [key, value] of Object.entries(serverEnvironment)) {
    if (value === undefined || value === null) continue
    const text = String(value)
    for (const secret of secrets) {
      if (typeof secret !== 'string' || secret.length === 0) continue
      assert.equal(
        text.includes(secret),
        false,
        `Server environment ${key} must not contain Device Provider secrets`,
      )
    }
  }
}

export function checkedWwc(wwc, args, environment = process.env) {
  const result = spawnSync(wwc, args, {
    encoding: 'utf8',
    env: environment,
    stdio: 'pipe',
  })
  assert.equal(result.status, 0, `${args.join(' ')} failed: ${result.stderr || result.stdout}`)
  return result.stdout.trim().length === 0 ? null : JSON.parse(result.stdout)
}

export async function waitFor(check, label, timeoutMillis = 30_000, pollMillis = 200) {
  const deadline = Date.now() + timeoutMillis
  for (;;) {
    const value = await check()
    if (value) return value
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`)
    await new Promise(resolvePromise => setTimeout(resolvePromise, pollMillis))
  }
}

export function stopProcessGroup(child) {
  if (child === null || child.exitCode !== null || child.signalCode !== null) return
  try {
    process.kill(-child.pid, 'SIGTERM')
  } catch {
    child.kill('SIGTERM')
  }
}

/**
 * Writes the Device TLS trust root derived from the fixture certificate.
 * Explicit test trust root: Device-side Provider HTTPS uses this file only.
 */
export function writeDeviceTrustRoot({ certificatePath, destination }) {
  const der = new X509Certificate(readFileSync(certificatePath)).raw
  writeFileSync(destination, der)
  return destination
}

/**
 * Establishes Device-only production prerequisites against a running Server.
 *
 * Steps: enroll/pair Device Client → connect → occupy → bind repository →
 * optional encrypted/local Device Provider → launch anchor via `/api/v1/sessions`.
 *
 * This function never restores Server-local model execution. When
 * `deviceProvider` secrets are provided they are returned for Device-local
 * configuration only and must not be copied into Server environment.
 */
export async function establishDeviceOnlyExecutionPath({
  api,
  wwc,
  directory,
  repository,
  schemaVersion = 'winwincode/v1',
  deviceData = join(directory, 'device-data'),
  certificatePath = join(directory, 'fixture-cert.pem'),
  privateKeyPath = join(directory, 'fixture-key.pem'),
  helperReleaseManifest,
  helperExecutable,
  modelRouteSource,
  timeoutMillis = 60_000,
  deviceProvider = null,
  deviceSecret = null,
  seedDeviceProvider = true,
  repositoryScope = null,
  logName = 'device-process.log',
}) {
  assert.equal(typeof wwc, 'string')
  assert.equal(typeof api?.request, 'function')
  const steps = []
  const report = { steps, complete: false }
  const tlsRoot = writeDeviceTrustRoot({
    certificatePath,
    destination: join(directory, 'device-provider-trust-root.der'),
  })
  const providerKind = deviceProvider?.kind ?? DEVICE_PROVIDER_KIND_DETERMINISTIC
  const useRealDeviceProvider = providerKind === DEVICE_PROVIDER_KIND_REAL
  const deviceEnvironment = {
    ...process.env,
    WWC_DEVICE_TLS_ROOT_DER_FILE: tlsRoot,
    // Fixture TLS roots pin the Device Provider HTTPS client to the acceptance
    // mock. Real Device-local Providers (e.g. Zhipu) must use WebPki trust.
    ...(useRealDeviceProvider
      ? {}
      : { WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE: tlsRoot }),
    WWC_WORKER_TLS_ROOT_DER_FILE: tlsRoot,
    ...(helperReleaseManifest === undefined ? {} : {
      WWC_WORKER_HELPER_RELEASE_MANIFEST: helperReleaseManifest,
    }),
    ...(helperExecutable === undefined ? {} : {
      WWC_WORKER_HELPER_EXECUTABLE: helperExecutable,
    }),
    WWC_WORKER_ACTION_SIGNING_KEY_HEX: '1f'.repeat(32),
    WWC_WORKER_EXECUTION_ENVELOPE_DIGEST: `sha256:${'a'.repeat(64)}`,
    WWC_DEBUG_REMOTE_WORKER: '1',
    GIT_CONFIG_NOSYSTEM: '1',
    ...(process.env.WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX === undefined
      ? {}
      : {
        WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX:
          process.env.WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX,
      }),
  }
  if (useRealDeviceProvider) {
    // Explicitly drop fixture Provider TLS pin if the parent environment had one.
    delete deviceEnvironment.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE
  }
  if (deviceProvider !== null && typeof deviceProvider.providerId === 'string') {
    // Selected Device Provider identity wins over any pre-pairing route
    // placeholder so Worker/Codex launch against the seeded Provider.
    deviceEnvironment.WWC_WORKER_MODEL_PROVIDER_ID = deviceProvider.providerId
    deviceEnvironment.WWC_WORKER_MODEL_ID = deviceProvider.modelId
  } else if (modelRouteSource !== undefined && modelRouteSource !== null) {
    deviceEnvironment.WWC_WORKER_MODEL_PROVIDER_ID = modelRouteSource.providerId
    deviceEnvironment.WWC_WORKER_MODEL_ID = modelRouteSource.modelId
  }
  if (deviceSecret !== null && deviceSecret !== undefined) {
    // Secret stays on the Device process environment only.
    deviceEnvironment.WWC_DEVICE_PROVIDER_API_KEY = deviceSecret
  }

  const deviceLog = openSync(join(directory, logName), 'a', 0o600)
  const device = spawn(wwc, [
    'device', 'serve',
    '--data-dir', deviceData,
    '--server-url', api.baseUrl,
    '--server-name', 'Device production Server',
    '--device-name', 'Device production Client',
  ], {
    cwd: directory,
    detached: true,
    env: deviceEnvironment,
    stdio: ['ignore', deviceLog, deviceLog],
  })

  try {
    const status = await waitFor(() => {
      if (device.exitCode !== null) {
        throw new Error(`Device Client exited during enrollment (${device.exitCode})`)
      }
      try {
        const value = checkedWwc(wwc, ['device', 'status', '--data-dir', deviceData, '--json'], deviceEnvironment)
        return value?.device?.enrolled ? value.device : false
      } catch {
        return false
      }
    }, 'Device Client enrollment', timeoutMillis)
    report.publicClientId = status.publicClientId
    steps.push('device.enroll-pair')

    const refreshed = checkedWwc(wwc, [
      'device', 'refresh-code', '--data-dir', deviceData, '--json',
    ], deviceEnvironment)
    await new Promise(resolvePromise => setTimeout(resolvePromise, 250))
    const connected = await api.request('/api/v1/clients/connections', {
      method: 'POST',
      body: {
        schemaVersion,
        clientId: status.publicClientId,
        connectionCode: refreshed.connectCode,
      },
    })
    assert.equal(connected.status, 201, `Client connect failed: ${connected.text}`)
    steps.push('client-connect')

    const occupied = await api.request('/api/v1/clients/occupancy', {
      method: 'POST',
      body: { schemaVersion, clientId: status.publicClientId },
    })
    assert.equal(occupied.status, 201, `Client occupancy failed: ${occupied.text}`)
    steps.push('client-occupancy')

    const registered = checkedWwc(wwc, [
      'repo', 'add', repository, '--data-dir', deviceData, '--json',
    ], deviceEnvironment)
    const repositoryBindingId = registered.repository.repositoryBindingId
    await waitFor(async () => {
      const response = await api.request(`/api/v1/repositories?clientId=${status.publicClientId}`)
      return response.status === 200
        && response.json?.repositories?.some(item => item.repositoryBindingId === repositoryBindingId)
    }, 'Repository binding projection', timeoutMillis)
    report.repositoryBindingId = repositoryBindingId
    steps.push('repository-binding')

    let modelServer = null
    let providerApiKey = deviceSecret
    let seededProvider = null
    if (deviceProvider !== null && seedDeviceProvider && useRealDeviceProvider) {
      // Real Device-local Provider: encrypted Web→Device apply to the external
      // endpoint. Secrets never leave Device config; Server never sees them.
      assert.equal(
        typeof deviceSecret,
        'string',
        'Device-local real Provider requires a Device-local secret on the Device process only',
      )
      assert.ok(deviceSecret.length > 0, 'Device-local real Provider secret must be non-empty')
      assert.equal(
        typeof deviceProvider.endpoint,
        'string',
        'Device-local real Provider requires an explicit endpoint',
      )
      providerApiKey = deviceSecret
      seededProvider = await seedDeviceLocalProvider({
        api,
        publicClientId: status.publicClientId,
        providerId: deviceProvider.providerId,
        modelId: deviceProvider.modelId,
        endpoint: deviceProvider.endpoint,
        apiKey: providerApiKey,
        displayName: deviceProvider.displayName
          ?? `Device-local real Provider (${deviceProvider.providerId})`,
        timeoutMillis,
      })
      report.deviceProvider = {
        providerId: seededProvider.providerId,
        modelId: seededProvider.modelId,
        endpoint: seededProvider.endpoint,
        credentialReferenceId: seededProvider.credentialReferenceId,
        receiptOutcome: seededProvider.receiptOutcome,
        configured: 'encrypted-web-to-device-apply',
        kind: DEVICE_PROVIDER_KIND_REAL,
        seedMode: deviceProvider.seedMode ?? 'encrypted-web-to-device-apply-real-endpoint',
        secretPlacement: 'device-local-only',
      }
      steps.push('device-local-real-provider-seeded')
    } else if (deviceProvider !== null && seedDeviceProvider) {
      // Production path: Device-local HTTPS Provider + encrypted Web→Device apply.
      modelServer = await startDeterministicDeviceModelServer({
        certificatePath,
        privateKeyPath,
      }).listen()
      providerApiKey = deviceSecret ?? `device-local-${randomBytes(24).toString('hex')}`
      seededProvider = await seedDeviceLocalProvider({
        api,
        publicClientId: status.publicClientId,
        providerId: deviceProvider.providerId,
        modelId: deviceProvider.modelId,
        endpoint: modelServer.endpoint,
        apiKey: providerApiKey,
        displayName: deviceProvider.displayName
          ?? 'WinWinCode Device deterministic Provider',
        timeoutMillis,
      })
      report.deviceProvider = {
        providerId: seededProvider.providerId,
        modelId: seededProvider.modelId,
        endpoint: seededProvider.endpoint,
        credentialReferenceId: seededProvider.credentialReferenceId,
        receiptOutcome: seededProvider.receiptOutcome,
        configured: 'encrypted-web-to-device-apply',
        kind: DEVICE_PROVIDER_KIND_DETERMINISTIC,
        seedMode: 'deterministic-loopback',
        secretPlacement: 'device-local-only',
      }
      steps.push('device-local-model-server')
      steps.push('device-local-provider-seeded')
    } else if (deviceProvider !== null) {
      report.deviceProvider = {
        providerId: deviceProvider.providerId,
        modelId: deviceProvider.modelId,
        // Encrypted Web→Device apply is the production path. Acceptance runners
        // may provision Device-local store state when a live encrypted apply
        // endpoint is unavailable; secrets never leave Device config.
        configured: 'device-local-unseeded',
        kind: providerKind,
      }
      steps.push('device-local-provider')
    }

    const modelRoute = modelRouteSource && seededProvider === null
      ? modelRouteSource
      : configuredDeviceModelRoute({
        // Server route_reference binds the Device Provider credential to the
        // Device node id from the public Provider snapshot, not the public
        // client id used by occupancy HTTP paths.
        clientNodeId: seededProvider?.clientNodeId
          ?? status.publicClientId,
        providerId: deviceProvider?.providerId ?? seededProvider?.providerId
          ?? 'winwincode-device-deterministic',
        modelId: deviceProvider?.modelId ?? seededProvider?.modelId
          ?? 'device-deterministic-model',
      })
    report.modelRoute = modelRoute

    return {
      ...report,
      api,
      device,
      deviceData,
      deviceEnvironment,
      modelRoute,
      modelServer,
      providerApiKey,
      seededProvider,
      publicClientId: status.publicClientId,
      repositoryBindingId,
      schemaVersion,
      wwc,
      /**
       * Launches one Device WorkerSession as the durable launch anchor.
       * Chat/cancel need `productSessionId` so Server creates a
       * WorkerLaunchGrant bound to that ProductSession; StrongFlow passes
       * `workRunId` for the Controller-dispatched execution job.
       */
      async launchAnchor({ workRunId = null, productSessionId = null } = {}) {
        const body = {
          schemaVersion,
          clientId: status.publicClientId,
          repositoryBindingId,
          ...(workRunId === null ? {} : { workRunId }),
        }
        if (productSessionId !== null) {
          assert.ok(repositoryScope !== null,
            'productSession launch requires repositoryScope on establishDeviceOnlyExecutionPath')
          body.productSession = {
            id: productSessionId,
            scope: {
              kind: 'repository',
              organizationId: repositoryScope.organizationId,
              workspaceId: repositoryScope.workspaceId,
              projectId: repositoryScope.projectId,
              repositoryId: repositoryScope.repositoryId,
            },
          }
        }
        const launched = await api.request('/api/v1/sessions', {
          method: 'POST',
          timeoutMillis: 30_000,
          body,
        })
        assert.equal(launched.status, 201, `Worker launch failed: ${launched.text}`)
        steps.push(productSessionId === null
          ? 'launch-anchor'
          : `launch-anchor:${productSessionId}`)
        report.workerSessionId = launched.json.workerSessionId
        report.workerId = launched.json.workerId
        report.workerInstanceId = launched.json.workerInstanceId
        return launched.json
      },
      stop() {
        if (modelServer !== null) {
          try {
            modelServer.close()
          } catch {
            // Model fixture shutdown is best-effort.
          }
        }
        stopProcessGroup(device)
      },
    }
  } catch (error) {
    stopProcessGroup(device)
    throw error
  }
}

/**
 * Builds a Device-only Server environment for production acceptance.
 * Centralized model startup assumptions are removed on purpose.
 */
export function deviceOnlyServerEnvironment(overrides = {}) {
  assertServerEnvironmentIsDeviceOnly(overrides)
  return { ...overrides }
}

/**
 * Captures Device-side Provider secret material for Device configuration only.
 * The returned object is never spread into Server environment maps.
 */
export function deviceProviderSecretBundle({
  providerId,
  modelId,
  apiKey,
  endpoint,
} = {}) {
  return Object.freeze({
    providerId: providerId ?? null,
    modelId: modelId ?? null,
    apiKey: apiKey ?? null,
    endpoint: endpoint ?? null,
  })
}

export function chmodDeviceCredential(path) {
  chmodSync(path, 0o600)
  return path
}
