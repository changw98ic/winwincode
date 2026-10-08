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
import { execFileSync, spawn, spawnSync } from 'node:child_process'
import { chmodSync, existsSync, openSync, readFileSync, realpathSync, writeFileSync } from 'node:fs'
import { createServer as createHttpsServer } from 'node:https'
import { createServer as createNetServer } from 'node:net'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'

/** Runtime children receive platform settings, never the orchestrator's credentials. */
export function runtimeChildEnvironment(environment = process.env) {
  const keys = [
    'PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'TMP', 'TEMP',
    'LANG', 'LC_ALL', 'LC_CTYPE', 'TZ', 'SYSTEMROOT', 'WINDIR', 'COMSPEC', 'PATHEXT',
  ]
  return Object.fromEntries(keys
    .filter(key => environment[key] !== undefined)
    .map(key => [key, environment[key]]))
}

/** Interpret the task driver's launch response without changing public launch authority. */
export function deviceTaskLaunchResult({ response, directory, deliveryId, workRunId,
  publicClientId, holderUserId, repositoryBindingId }) {
  if (response.status === 201) return response.json
  // The scheduler can finish a role while the task driver drains its unused
  // source anchor. An expired grant is never launch authority here: recover
  // only the original, accepted and completed execution as an observation.
  if (response.status === 400 && response.json?.error?.code === 'INVALID_REQUEST'
    && [directory, deliveryId, workRunId, publicClientId, holderUserId, repositoryBindingId]
      .every(value => typeof value === 'string' && value.length > 0)) {
    const db = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
    try {
      const rows = db.prepare(`SELECT j.job_id, j.payload_digest, j.dispatch_payload,
        f.product_session_id, f.worker_session_id, f.worker_id, f.worker_instance_id
        FROM scheduler_execution_jobs j
        JOIN device_execution_current_facts f ON f.job_id = j.job_id
          AND f.work_run_id = j.work_run_id AND f.product_session_id = j.product_session_id
        JOIN client_nodes c ON c.client_node_id = f.client_node_id
        JOIN worker_launch_grants g ON g.worker_launch_grant_id = f.worker_launch_grant_id
          AND g.client_node_id = f.client_node_id AND g.client_instance_id = f.client_instance_id
          AND g.holder_user_id = f.holder_user_id AND g.repository_binding_id = f.repository_binding_id
          AND g.occupancy_lease_id = f.occupancy_lease_id
          AND g.occupancy_fencing_token = f.occupancy_fencing_token
          AND g.worker_session_id = f.worker_session_id AND g.worker_id = f.worker_id
          AND g.worker_instance_id = f.worker_instance_id
          AND g.product_session_id = j.product_session_id AND g.work_run_id = j.work_run_id
          AND g.state = 'consumed' AND g.consumed_at IS NOT NULL
        JOIN execution_leases l ON l.job_id = j.job_id AND l.payload_digest = j.payload_digest
          AND l.worker_id = f.worker_id AND l.worker_instance_id = f.worker_instance_id
          AND l.attempt = j.attempt
        JOIN execution_lease_terminals t ON t.lease_id = l.lease_id AND t.job_id = l.job_id
          AND t.worker_id = l.worker_id AND t.worker_instance_id = l.worker_instance_id
          AND t.attempt = l.attempt AND t.fencing_token = l.fencing_token AND t.outcome = 'completed'
        WHERE j.delivery_id = ? AND j.work_run_id = ? AND j.state = 'completed'
          AND c.public_client_id = ? AND f.holder_user_id = ? AND f.repository_binding_id = ?`)
        .all(deliveryId, workRunId, publicClientId, holderUserId, repositoryBindingId)
      if (rows.length === 1) {
        const row = rows[0], payload = JSON.parse(Buffer.from(row.dispatch_payload).toString('utf8'))
        if (payload.jobId === row.job_id && payload.payloadDigest === row.payload_digest
          && payload.scope?.kind === 'work-run' && payload.scope.workRunId === workRunId
          && payload.scope.productSessionId === row.product_session_id) {
          return { workRunId, productSessionId: row.product_session_id,
            workerSessionId: row.worker_session_id, workerId: row.worker_id,
            workerInstanceId: row.worker_instance_id, recoveredCompleted: true }
        }
      }
    } finally { db.close() }
  }
  throw Object.assign(new Error(`Worker launch failed: ${response.text}`), {
    code: 'DEVICE_LAUNCH_FAILED',
  })
}

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
  repeatToolMarker = null,
} = {}) {
  return Object.freeze({
    providerId,
    modelId,
    displayName: 'WinWinCode Device deterministic Provider',
    endpointHint: 'https://127.0.0.1:<device-mock-model-port>/v1/messages',
    protocol: 'anthropic_messages',
    ...(repeatToolMarker === null ? {} : { repeatToolMarker }),
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
  return encryptDeviceConfigurationEnvelope(DEVICE_PROVIDER_ENCRYPTION_CONTEXT, snapshot, requestId, mutation)
}

// API and installed-product acceptance run without Client build artifacts.
// Match the Web client envelope using only Node's standard crypto primitives.
function encryptDeviceConfigurationEnvelope(context, snapshot, requestId, mutation) {
  assert.equal(typeof snapshot?.clientNodeId, 'string')
  assert.equal(typeof snapshot?.encryptionPublicKey, 'string')
  assert.equal(typeof requestId, 'string')
  const expectedRevision = snapshot.revision
  const ephemeral = createECDH('prime256v1')
  ephemeral.generateKeys()
  const shared = ephemeral.computeSecret(Buffer.from(snapshot.encryptionPublicKey, 'base64'))
  const aad = `${context}\n${snapshot.clientNodeId}\n${requestId}\n${expectedRevision}`
  const key = Buffer.from(hkdfSync(
    'sha256',
    shared,
    Buffer.from(context),
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

export function observeToolProcess(providerRequest, callId) {
  const originalOutput = findToolOutput(providerRequest, callId)
  let output = originalOutput
  let evidenceSourceId = codeModeCommandSource(originalOutput)
  let pollIndex = 1
  let nextCallId = `${callId}-poll`
  for (;;) {
    const poll = findToolOutput(providerRequest, nextCallId)
    if (poll === null) break
    output = poll
    evidenceSourceId ??= codeModeCommandSource(poll)
    pollIndex += 1
    nextCallId = `${callId}-poll-${pollIndex}`
  }
  const text = toolOutputText(output)
  const exitCode = parseProcessExitCode(text)
  const session = text?.match(/^Process running with session ID (\d+)\s*$/mu)
  const sessionId = session ? Number(session[1]) : null
  const cell = text?.match(/^Script running with cell ID (\S+)\s*$/mu)
  return {
    exitCode,
    cellId: cell?.[1] ?? null,
    sessionId: Number.isSafeInteger(sessionId) ? sessionId : null,
    nextCallId,
    evidenceSourceId: output === null ? null : evidenceSourceId ?? callId,
    hasToolOutput: output !== null,
  }
}

export function resolveVerificationObservation(providerRequest) {
  return observeToolProcess(providerRequest, DETERMINISTIC_VERIFICATION_CALL_ID)
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
  repeatToolMarker = null,
  nativeCellScripts = {},
}) {
  assert.ok(repeatToolMarker === null || (typeof repeatToolMarker === 'string' && repeatToolMarker.length > 0))
  for (const [marker, source] of Object.entries(nativeCellScripts)) {
    assert.match(marker, /^native-cell-[AB]$/u)
    assert.equal(typeof source, 'string')
    assert.ok(source.length <= 6000)
  }
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
        const repeatTool = repeatToolMarker !== null && bodyText.includes(repeatToolMarker)
        const nativeCell = Object.keys(nativeCellScripts).find(marker => bodyText.includes(marker)) ?? null
        requests.push({
          path: request.url ?? '',
          verification: bodyText.includes(DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER),
          executor: bodyText.includes(DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER),
          planner: bodyText.includes(DETERMINISTIC_PLANNER_PROTOCOL),
          hasToolOutput: /function_call_output|custom_tool_call_output|tool_result/u.test(bodyText),
          repeatTool,
        })
        const executionTool = providerRequest.tools?.find(tool => /(?:^|__)exec$/u.test(tool.name)) ?? null
        const waitTool = providerRequest.tools?.find(tool => /(?:^|__)wait$/u.test(tool.name)) ?? null
        const workInput = workInputFromRequest(providerRequest)
        const isExecutor = bodyText.includes(DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER)
        const isVerification = bodyText.includes(DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER)
        const isPlanner = bodyText.includes(DETERMINISTIC_PLANNER_PROTOCOL)
          && workInput?.criterionIds?.length
        const executorObservation = observeToolProcess(providerRequest, 'loopback-executor-change')
        const executorOutput = executorObservation.hasToolOutput
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
        let toolInput = null
        let toolSource = null
        let selectedTool = executionTool
        let waitInput = null

        const observation = isExecutor ? executorObservation : verificationObservation
        if (nativeCell !== null) {
          assert.deepEqual(providerRequest.tools.map(tool => tool.name).sort(), ['exec', 'wait'])
          const native = observeToolProcess(providerRequest, nativeCell)
          toolCallId = native.hasToolOutput ? native.nextCallId : nativeCell
          if (native.cellId !== null) {
            selectedTool = waitTool
            waitInput = { cell_id: native.cellId, yield_time_ms: 1000 }
          } else if (!native.hasToolOutput) {
            toolSource = nativeCellScripts[nativeCell]
          }
          useTool = native.cellId !== null || !native.hasToolOutput
          stopReason = useTool ? 'tool_use' : 'end_turn'
          text = useTool ? '' : `${nativeCell} completed`
        } else if ((isExecutor || isVerification) && observation.cellId !== null) {
          assert.ok(waitTool, 'running cell requires the Code Mode wait tool')
          selectedTool = waitTool
          useTool = true
          stopReason = 'tool_use'
          toolCallId = observation.nextCallId
          waitInput = { cell_id: observation.cellId, yield_time_ms: 1000 }
          text = ''
        } else if ((isExecutor || isVerification) && observation.sessionId !== null) {
          assert.ok(executionTool, 'running command requires the Code Mode exec tool')
          useTool = true
          stopReason = 'tool_use'
          toolCallId = observation.nextCallId
          toolInput = { session_id: observation.sessionId, chars: '', yield_time_ms: 1000 }
          text = ''
        } else if (isExecutor && !executorOutput) {
          useTool = true
          stopReason = 'tool_use'
          toolCallId = 'loopback-executor-change'
          toolCommand = "printf '%s\\n' 'status: complete' > TASK.md; printf '%s\\n' 'deterministic Device candidate' > .winwincode-api-candidate; git status --porcelain=v1 --untracked-files=all"
          text = ''
        } else if (isExecutor) {
          text = executorObservation.exitCode === 0
            ? 'The requested stage action completed.'
            : 'The stage command did not complete successfully.'
        } else if (isVerification && !verificationObservation.hasToolOutput && executionTool) {
          useTool = true
          stopReason = 'tool_use'
          toolCallId = DETERMINISTIC_VERIFICATION_CALL_ID
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
        } else if (repeatTool) {
          useTool = true
          stopReason = 'tool_use'
          toolCallId = `loopback-repeat-${requests.length}`
          toolCommand = 'git rev-parse --verify HEAD'
          text = ''
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
          assert.ok(selectedTool !== null, 'Device Provider fixture requires a Code Mode tool')
          events.push(['content_block_start', {
            type: 'content_block_start',
            index: 0,
            content_block: {
              type: 'tool_use',
              id: toolCallId,
              name: selectedTool.name,
              input: {},
            },
          }])
          const input = waitInput ?? { input: toolSource ?? deviceFixtureCodeModeCommand(toolInput ?? {
            cmd: toolCommand, workdir: '.', yield_time_ms: toolYieldMs,
          }) }
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

// Revision-bound settings mutations share one queue per Device. Sessions still
// execute concurrently; only this short configuration transaction is serialized.
const deviceExtensionMutations = new WeakMap()

async function mutateDeviceExtension(api, publicClientId, mutation, expected, timeoutMillis) {
  let clients = deviceExtensionMutations.get(api)
  if (!clients) { clients = new Map(); deviceExtensionMutations.set(api, clients) }
  const previous = clients.get(publicClientId) ?? Promise.resolve()
  const result = previous.catch(() => {}).then(async () => {
    const base = `/api/v1/clients/${encodeURIComponent(publicClientId)}/extensions`
    const current = await api.request(base)
    assert.equal(current.status, 200)
    assert.equal(current.json?.online, true, 'Device must be online to configure public smoke')
    const requestId = `extension_${randomBytes(16).toString('hex')}`
    const envelope = encryptDeviceConfigurationEnvelope(
      'winwincode.device-extensions.v1', current.json.snapshot, requestId, mutation,
    )
    const applied = await api.request(base, { method: 'POST', body: envelope })
    assert.equal(applied.status, 202, 'Device extension apply was rejected')
    const completed = await waitFor(async () => {
      const response = await api.request(`${base}/receipts/${requestId}`)
      if (!response.json?.receipt) return false
      assert.equal(response.status, 200)
      assert.equal(response.json.receipt.outcome, expected, 'Device public smoke configuration failed')
      return response.json
    }, `Device public smoke ${expected}`, timeoutMillis)
    return completed
  })
  clients.set(publicClientId, result)
  return result
}

export async function installDevicePublicSmoke({ api, publicClientId, configuration,
  id = 'benchmark_public_smoke', timeoutMillis = 60_000 }) {
  assert.match(id, /^benchmark_public_smoke(?:_psn_[0-9A-HJKMNP-TV-Z]{26})?$/u)
  const receipts = []
  for (const mutation of [
    { operation: 'save_mcp', id, configuration: JSON.stringify(configuration), enabled: true },
    { operation: 'test_mcp', id },
  ]) {
    const expected = mutation.operation === 'save_mcp' ? 'saved' : 'tested'
    const completed = await mutateDeviceExtension(api, publicClientId, mutation, expected, timeoutMillis)
    receipts.push(completed.receipt)
    if (expected === 'tested') {
      const server = completed.snapshot?.mcpServers?.find(server => server.id === id)
      assert.equal(server?.enabled, true)
      assert.equal(server?.connectionStatus, 'ready')
      assert.deepEqual(server?.toolNames, ['public_smoke'])
    }
  }
  return { id, toolNames: ['public_smoke'], receipts }
}

export async function removeDevicePublicSmoke({ api, publicClientId, id, timeoutMillis = 60_000 }) {
  assert.match(id, /^benchmark_public_smoke_psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
  return (await mutateDeviceExtension(api, publicClientId,
    { operation: 'delete', kind: 'mcp', id }, 'deleted', timeoutMillis)).receipt
}

/** Resolve only approvals belonging to the currently observed Device WorkRuns. */
export async function resolveDeviceTaskApprovals({ api, runs, publicSmokeId,
  automaticTaskActions = false, onDecision }) {
  assert.equal(typeof automaticTaskActions, 'boolean')
  const currentRun = approval => runs.find(run => {
    const binding = approval.binding
    const identity = binding?.sessionIdentity
    return ['leased', 'running'].includes(run.state)
      && run.productSessionId && run.codexThreadId
      && identity?.workRunId === run.id
      && identity.codexThreadId === run.codexThreadId
      && identity.productSessionId === run.productSessionId
      && identity.workerSessionId === run.workerSessionId
      && binding.productSessionId === run.productSessionId
      && binding.workerSessionId === run.workerSessionId
      && binding.executionJobId === run.executionJobId
  })
  const pending = await api.query('approval.list', { states: ['pending'] })
  assert.equal(pending.page?.hasMore, false, 'Device task approval list must be complete')
  assert.ok(Array.isArray(pending.result?.items), 'Device task approval list is invalid')
  for (const item of pending.result.items) {
    if (!currentRun(item)) continue
    const approval = (await api.query('approval.get', { approvalId: item.id })).result
    if (approval.state !== 'pending' || !approval.decisionEnabled || !currentRun(approval)) continue
    const detail = approval.sanitizedDetail
    const publicSmoke = typeof publicSmokeId === 'string'
      && /^benchmark_public_smoke(?:_psn_[0-9A-HJKMNP-TV-Z]{26})?$/u.test(publicSmokeId)
      && approval.category === 'mcp' && approval.effectiveDecisionScope === 'once'
      && detail?.kind === 'available' && detail.operation === 'execute'
      && detail.reasonCode === 'mcp_permission' && detail.targetCount === 1
      && detail.targetSummaries?.length === 1
      && detail.targetSummaries[0] === `server:${publicSmokeId}`
    // This opt-in represents the operator's authorization for this task's
    // role Sessions. The server still validates the exact binding, revision,
    // expiry and one-use decision, and Worker action enforcement still runs.
    const taskAction = automaticTaskActions && approval.effectiveDecisionScope === 'once'
      && detail?.kind === 'available' && detail.targetCount > 0
      && Array.isArray(detail.targetSummaries) && detail.targetSummaries.length > 0
      && ((approval.category === 'shell' && detail.operation === 'execute'
        && ['sandbox_escalation', 'network_access'].includes(detail.reasonCode)
        && detail.workingDirectory === 'workspace')
      || (approval.category === 'network' && detail.operation === 'execute'
        && detail.reasonCode === 'network_access' && detail.workingDirectory === 'workspace')
      || (approval.category === 'filesystem_write' && detail.operation === 'modify'
        && detail.reasonCode === 'filesystem_write'))
    const allow = publicSmoke || taskAction
    const decision = allow ? 'approve' : 'reject'
    let result
    try {
      result = await api.command('approval.decide', approval.revision, {
        approvalId: approval.id, binding: approval.binding, decision,
        reason: taskAction
          ? 'The operator authorized automatic task actions for this exact active Session and WorkRun.'
          : allow ? 'Run the configured public_smoke in the frozen offline sandbox.'
          : 'Continue within the configured workspace and public_smoke tool; escalation is not authorized.',
      })
    } catch (error) {
      if (['REVISION_CONFLICT', 'WRONG_STATE'].includes(error?.code)) continue
      throw error
    }
    assert.equal(result.outcome, 'completed', 'Device approval decision did not complete')
    await onDecision({ approval, decision, result })
  }
}

/** Seed a Device-local Provider through the encrypted Web → Device HTTP path. */
export async function seedDeviceLocalProvider({
  api,
  publicClientId,
  providerId,
  modelId,
  endpoint,
  apiKey,
  protocol = 'anthropic_messages',
  customHeaders,
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
      protocol,
      modelIds: [modelId],
      enabled: true,
    },
    apiKey,
    ...(customHeaders === undefined ? {} : { customHeaders }),
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

export function deviceConnectCodePublished(database, connectCodeId) {
  return database.prepare(`
    SELECT 1 FROM client_outbox
    WHERE kind = 'client.connect_code.published' AND published = 1
      AND json_extract(CAST(payload AS TEXT), '$.payload.connectCodeId') = ?
  `).get(connectCodeId) !== undefined
}

export function deviceHelloAcknowledged(database, previousInstanceId) {
  return database.prepare(`SELECT 1 FROM client_outbox
    WHERE kind = 'client.hello' AND published = 1 AND client_instance_id =
      (SELECT current_instance_id FROM device_identity LIMIT 1)
      AND (? IS NULL OR client_instance_id <> ?)`).get(previousInstanceId, previousInstanceId) !== undefined
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

function processSnapshot() {
  const result = spawnSync('ps', ['-axo', 'pid=,ppid=,pgid=,stat=,lstart='], {
    encoding: 'utf8', timeout: 1000, maxBuffer: 8 * 1024 * 1024,
    env: { ...process.env, LC_ALL: 'C' },
  })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error('cannot inspect fixture process ownership')
  const rows = new Map()
  for (const line of result.stdout.split('\n')) {
    const match = /^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(.+?)\s*$/u.exec(line)
    if (!match) continue
    const row = {
      pid: Number(match[1]), parent: Number(match[2]), group: Number(match[3]),
      state: match[4], start: match[5].replace(/\s+/gu, ' '),
    }
    if (process.platform === 'linux') {
      try {
        const stat = readFileSync(`/proc/${row.pid}/stat`, 'utf8')
        row.start = `linux-${stat.slice(stat.lastIndexOf(')') + 1).trim().split(/\s+/u)[19]}`
      } catch (error) {
        if (error.code === 'ENOENT' || error.code === 'ESRCH') continue
        throw error
      }
    }
    rows.set(row.pid, row)
  }
  return rows
}

function ownedProcessAlive(witness, current) {
  const row = current.get(witness.pid)
  return row !== undefined && row.start === witness.start && row.group === witness.group
    && !row.state.startsWith('Z')
}

function registeredFixtureWorkers(deviceData, snapshot) {
  if (!deviceData) return []
  const path = join(deviceData, 'device-client.sqlite3')
  if (!existsSync(path)) return []
  const database = new DatabaseSync(path, { readOnly: true })
  try {
    database.exec('PRAGMA busy_timeout = 1000')
    if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'worker_process_registry'").get()) return []
    return database.prepare(`SELECT pid, process_start_identity FROM worker_process_registry
      WHERE state = 'running'`).all().flatMap(record => {
      const row = snapshot.get(record.pid)
      if (!row || row.state.startsWith('Z')) return []
      const identity = process.platform === 'linux' ? row.start : `darwin-ps-${row.start}`
      return record.process_start_identity === identity ? [row] : []
    })
  } finally { database.close() }
}

// The source anchor establishes Device authority for Delivery creation but
// receives no execution job. After dispatch, drain it through the product and
// interrupt its witnessed process; role Workers shut down after work_drained.
export async function stopUnusedDeviceTaskAnchor({ api, directory, deviceData, launched, productSessionId }) {
  const server = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
  try {
    server.exec('PRAGMA busy_timeout = 5000')
    assert.equal(server.prepare('SELECT count(*) AS n FROM scheduler_execution_jobs WHERE product_session_id = ?')
      .get(productSessionId).n, 0, 'source anchor must have no execution jobs')
    assert.equal(server.prepare(`SELECT count(*) AS n FROM execution_leases l
      WHERE worker_id = ? AND worker_instance_id = ?
      AND NOT EXISTS (SELECT 1 FROM execution_lease_terminals t WHERE t.lease_id = l.lease_id)`)
      .get(launched.workerId, launched.workerInstanceId).n, 0, 'source anchor must have no active lease')
  } finally { server.close() }
  const worker = await getDeviceWorker(api, launched.workerId)
  assert.ok(worker, 'source anchor must be registered before it can drain')
  if (worker.state === 'enabled') {
    const drained = await api.command('worker.drain', worker.revision, {
      workerId: worker.id, reason: 'Source authority anchor has no job; Delivery roles own execution.',
    })
    assert.equal(drained.outcome, 'completed')
  }
  const device = new DatabaseSync(join(deviceData, 'device-client.sqlite3'), { readOnly: true })
  try {
    const record = device.prepare('SELECT * FROM worker_process_registry WHERE worker_session_id = ?')
      .get(launched.workerSessionId)
    assert.equal(record.worker_id, launched.workerId)
    assert.equal(record.worker_instance_id, launched.workerInstanceId)
    const row = registeredFixtureWorkers(deviceData, processSnapshot()).find(row => row.pid === record.pid)
    assert.ok(row, 'source anchor process must match its Device registry identity')
    process.kill(row.pid, 'SIGINT')
    await waitFor(() => !ownedProcessAlive(row, processSnapshot()), 'unused task anchor graceful exit', 30_000)
    await waitFor(() => device.prepare('SELECT state FROM worker_process_registry WHERE worker_session_id = ?')
      .get(launched.workerSessionId).state !== 'running', 'Device observed unused task anchor exit', 30_000)
    return { workerSessionId: launched.workerSessionId, workerId: launched.workerId, state: 'drained' }
  } finally { device.close() }
}

// An exact read has its own protocol page shape. Historical drained Workers
// remain durable; registration and drain must not depend on their list position.
export async function getDeviceWorker(api, workerId) {
  try {
    const response = await api.query('worker.get', { workerId }, { cursor: null, limit: 1 })
    assert.equal(response.result.id, workerId, 'Worker lookup must return the requested identity')
    return response.result
  } catch (error) {
    if (error.code === 'RESOURCE_NOT_FOUND') return null
    throw error
  }
}

export function registerOrReuseDeviceRepository({ wwc, repository, deviceData, deviceEnvironment }) {
  const bindings = checkedWwc(wwc, ['repo', 'list', '--data-dir', deviceData, '--json'], deviceEnvironment)
  const existing = bindings.repositories.find(binding => binding.canonicalPath === realpathSync(repository))
  if (existing) {
    const head = execFileSync('git', ['-C', repository, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
    assert.equal(existing.headCommit, head, 'retained Device repository HEAD differs from its binding')
    assert.equal(existing.dirtyState, 'clean', 'retained Device repository binding is dirty')
    return existing
  }
  return checkedWwc(wwc, ['repo', 'add', repository, '--data-dir', deviceData, '--json'], deviceEnvironment).repository
}

/** Stop the owned tree, including Workers which the Device puts in separate groups. */
export async function stopProcessGroup(child, { graceMillis = 1000, deviceData } = {}) {
  const parentExited = () => child === null || child.exitCode !== null || child.signalCode !== null
  if (parentExited() && !deviceData) return
  const snapshot = processSnapshot()
  const root = parentExited() ? undefined : snapshot.get(child.pid)
  // Capture boot identities before TERM reparents descendants to init/launchd.
  const owned = new Map(registeredFixtureWorkers(deviceData, snapshot).map(row => [row.pid, row]))
  if (root?.parent === process.pid) owned.set(root.pid, root)
  if (owned.size === 0) {
    assert.equal(parentExited(), true, 'cannot confirm ownership of a live fixture parent')
    return
  }
  for (;;) {
    const before = owned.size
    for (const row of snapshot.values()) {
      if (owned.has(row.parent)) owned.set(row.pid, row)
    }
    if (before === owned.size) break
  }
  const targets = new Map()
  for (const row of [...owned.values()].reverse()) {
    // A shared parent group is not ours: signal only its witnessed descendants.
    const target = owned.has(row.group) ? -row.group : row.pid
    const witnesses = targets.get(target) ?? []
    witnesses.push(row)
    targets.set(target, witnesses)
  }
  const signalOwned = signal => {
    for (const [target, witnesses] of targets) {
      if (!witnesses.some(row => ownedProcessAlive(row, processSnapshot()))) continue
      try { process.kill(target, signal) } catch (error) {
        if (error.code !== 'ESRCH') throw error
      }
    }
  }
  const stopped = () => {
    const current = processSnapshot()
    return [...owned.values()].every(row => !ownedProcessAlive(row, current))
      && parentExited()
  }
  signalOwned('SIGTERM')
  const deadline = Date.now() + graceMillis
  while (!stopped() && Date.now() < deadline) {
    await new Promise(resolvePromise => setTimeout(resolvePromise, 25))
  }
  // A reaped Device is not proof its detached Workers or their tools have exited.
  if (!stopped()) {
    signalOwned('SIGKILL')
    await waitFor(stopped, 'fixture process tree shutdown', 1000, 25)
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
  agentEnvironment = process.env,
}) {
  assert.equal(typeof wwc, 'string')
  assert.equal(typeof api?.request, 'function')
  const steps = []
  const report = { steps, complete: false }
  const tlsRoot = writeDeviceTrustRoot({
    certificatePath,
    destination: join(directory, 'device-provider-trust-root.der'),
  })
  const deviceEnvironment = {
    ...runtimeChildEnvironment(),
    ...(agentEnvironment.PYTHONDONTWRITEBYTECODE === '1' ? { PYTHONDONTWRITEBYTECODE: '1' } : {}),
    WWC_WORKER_MODEL_REASONING_EFFORT: agentEnvironment.WWC_WORKER_MODEL_REASONING_EFFORT,
    WWC_WORKER_FUSION: agentEnvironment.WWC_WORKER_FUSION,
    WWC_WORKER_JEV_JUDGE: agentEnvironment.WWC_WORKER_JEV_JUDGE,
    WWC_WORKER_JEV_CONTEXT: agentEnvironment.WWC_WORKER_JEV_CONTEXT,
    WWC_DEVICE_JEV_SETTINGS_FILE: agentEnvironment.WWC_DEVICE_JEV_SETTINGS_FILE,
    WWC_DEVICE_PROVIDER_HTTPS_PROXY: agentEnvironment.WWC_DEVICE_PROVIDER_HTTPS_PROXY,
    WWC_BENCHMARK_SEALED_TOOLS: agentEnvironment.WWC_BENCHMARK_SEALED_TOOLS,
    WWC_DEVICE_TLS_ROOT_DER_FILE: tlsRoot,
    WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE: deviceProvider?.endpoint
      ? process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE
      : tlsRoot,
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
  if (modelRouteSource !== undefined && modelRouteSource !== null) {
    deviceEnvironment.WWC_WORKER_MODEL_PROVIDER_ID = modelRouteSource.providerId
    deviceEnvironment.WWC_WORKER_MODEL_ID = modelRouteSource.modelId
  }
  const priorStatus = existsSync(join(deviceData, 'device-client.sqlite3'))
    ? checkedWwc(wwc, ['device', 'status', '--data-dir', deviceData, '--json'], deviceEnvironment)
    : null
  const directoryView = priorStatus?.device?.enrolled ? await api.request('/api/v1/clients') : null
  if (directoryView) assert.equal(directoryView.status, 200, 'retained connection authorization must be readable')
  const retainedGrant = directoryView?.json.clients.some(client => client.clientId === priorStatus.device.publicClientId)
  let previousInstanceId = null
  if (priorStatus) {
    const previous = new DatabaseSync(join(deviceData, 'device-client.sqlite3'), { readOnly: true })
    try { previousInstanceId = previous.prepare('SELECT current_instance_id FROM device_identity LIMIT 1').get().current_instance_id }
    finally { previous.close() }
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

    const deviceDatabase = new DatabaseSync(join(deviceData, 'device-client.sqlite3'), { readOnly: true })
    try {
      // The daemon owns instance takeover. A reconnect command must not be
      // queued for an instance which has not announced itself to the Server.
      await waitFor(() => {
        if (device.exitCode !== null) throw new Error(`Device Client exited before hello acknowledgement (${device.exitCode})`)
        return deviceHelloAcknowledged(deviceDatabase, previousInstanceId)
      }, 'Device hello acknowledgement', Math.min(timeoutMillis, 60_000))
      if (!retainedGrant) {
        const refreshed = checkedWwc(wwc, [
          'device', 'refresh-code', '--data-dir', deviceData, '--json',
        ], deviceEnvironment)
        await waitFor(() => {
          if (device.exitCode !== null) throw new Error(`Device Client exited before connect code publication (${device.exitCode})`)
          return deviceConnectCodePublished(deviceDatabase, refreshed.code.connectCodeId)
        }, 'Device connect code publication acknowledgement', Math.min(timeoutMillis, 60_000))
        const connected = await api.request('/api/v1/clients/connections', {
          method: 'POST', body: { schemaVersion, clientId: status.publicClientId, connectionCode: refreshed.connectCode },
        })
        assert.equal(connected.status, 201, `Client connect failed: ${connected.text}`)
      }
    } finally { deviceDatabase.close() }
    steps.push('client-connect')

    const occupancy = await waitFor(async () => {
      const value = await api.request(`/api/v1/clients/${status.publicClientId}/occupancy`)
      assert.equal(value.status, 200, `Client occupancy lookup failed: ${value.text}`)
      return value.json.occupancy === 'recovery_pending' ? false : value
    }, 'Device occupancy recovery', Math.min(timeoutMillis, 60_000))
    assert.equal(occupancy.status, 200, `Client occupancy lookup failed: ${occupancy.text}`)
    if (occupancy.json.occupancy === 'available') {
      const occupied = await api.request('/api/v1/clients/occupancy', {
        method: 'POST', body: { schemaVersion, clientId: status.publicClientId },
      })
      assert.equal(occupied.status, 201, `Client occupancy failed: ${occupied.text}`)
    } else {
      assert.equal(occupancy.json.occupancy, 'occupied', 'retained Device occupancy is not usable')
      assert.equal(occupancy.json.holderUserId, api.actor.id, 'retained Device occupancy belongs to another user')
    }
    steps.push('client-occupancy')

    const registered = registerOrReuseDeviceRepository({ wwc, repository, deviceData, deviceEnvironment })
    const repositoryBindingId = registered.repositoryBindingId
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
    if (deviceProvider !== null && seedDeviceProvider) {
      // Production path: Device-local HTTPS Provider + encrypted Web→Device apply.
      if (deviceProvider.endpoint) {
        assert.ok(deviceSecret, 'External Provider requires a Device-local credential')
      } else {
        modelServer = await startDeterministicDeviceModelServer({
          certificatePath,
          privateKeyPath,
          repeatToolMarker: deviceProvider.repeatToolMarker,
          nativeCellScripts: deviceProvider.nativeCellScripts,
        }).listen()
      }
      providerApiKey = deviceSecret ?? `device-local-${randomBytes(24).toString('hex')}`
      seededProvider = await seedDeviceLocalProvider({
        api,
        publicClientId: status.publicClientId,
        providerId: deviceProvider.providerId,
        modelId: deviceProvider.modelId,
        endpoint: deviceProvider.endpoint ?? modelServer.endpoint,
        apiKey: providerApiKey,
        protocol: deviceProvider.protocol,
        customHeaders: deviceProvider.customHeaders,
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
        secretPlacement: 'device-local-only',
      }
      if (modelServer !== null) steps.push('device-local-model-server')
      steps.push('device-local-provider-seeded')
    } else if (deviceProvider !== null) {
      report.deviceProvider = {
        providerId: deviceProvider.providerId,
        modelId: deviceProvider.modelId,
        // Encrypted Web→Device apply is the production path. Acceptance runners
        // may provision Device-local store state when a live encrypted apply
        // endpoint is unavailable; secrets never leave Device config.
        configured: 'device-local-unseeded',
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
    const launchAnchor = async ({ workRunId = null, productSessionId = null, deliveryId = null, repositoryBindingId: selectedBindingId = repositoryBindingId } = {}, owner = null) => {
      if (owner !== null) {
        assert.ok(productSessionId === null || productSessionId === owner,
          'launch anchor must belong to its ProductSession')
        productSessionId = owner
      }
      const body = {
        schemaVersion,
        clientId: status.publicClientId,
        repositoryBindingId: selectedBindingId,
        ...(workRunId === null ? {} : { workRunId }),
      }
      if (workRunId === null && productSessionId !== null) {
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
      const launched = await waitFor(async () => {
        const response = await api.request('/api/v1/sessions', {
          method: 'POST', timeoutMillis: 30_000, body,
        })
        // Capacity rejection issues no grant and starts no Provider. Retry
        // the original launch after the Device observes a drained predecessor.
        return response.json?.error?.code === 'CAPACITY_EXHAUSTED' ? false : response
      }, 'available Device WorkerSession slot', timeoutMillis)
      const anchor = deviceTaskLaunchResult({ response: launched, directory,
        workRunId, deliveryId, publicClientId: status.publicClientId,
        holderUserId: api.actor.id, repositoryBindingId: selectedBindingId })
      if (owner !== null) {
        if (workRunId === null) assert.equal(anchor.productSessionId, owner,
          'Server launch authority must match the task ProductSession')
        else assert.equal(anchor.workRunId, workRunId,
          'Server launch authority must match the Controller WorkRun')
      }
      steps.push(anchor.recoveredCompleted === true ? 'recover-completed-anchor'
        : workRunId !== null || productSessionId === null
          ? 'launch-anchor' : `launch-anchor:${productSessionId}`)
      report.workerSessionId = anchor.workerSessionId
      report.workerId = anchor.workerId
      report.workerInstanceId = anchor.workerInstanceId
      return anchor
    }

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
      launchAnchor,
      /** Include this task's Controller role Sessions in its stop observation. */
      forProductSession(productSessionId, selectedBindingId = repositoryBindingId) {
        assert.match(productSessionId, /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
        return Object.freeze({
          launchAnchor: options => launchAnchor({ ...options, repositoryBindingId: selectedBindingId }, productSessionId),
        })
      },
      async stop() {
        if (modelServer !== null) {
          try {
            modelServer.close()
          } catch {
            // Model fixture shutdown is best-effort.
          }
        }
        await stopProcessGroup(device, { deviceData })
      },
    }
  } catch (error) {
    try { await stopProcessGroup(device, { deviceData }) } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], 'Device setup and cleanup failed')
    }
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

/** Executes each fixture command through the product's model-facing tool surface. */
export function deviceFixtureCodeModeCommand(input) {
  const name = input.session_id === undefined ? 'exec_command' : 'write_stdin'
  return `const tool = ALL_TOOLS.find(item => item.name.endsWith(${JSON.stringify(name)}));
if (!tool) throw new Error('Required command tool is unavailable');
const result = await tools[tool.name](${JSON.stringify(input)});
text(result.output);
if (Number.isInteger(result.session_id)) text('Process running with session ID ' + result.session_id);
if (Number.isInteger(result.exit_code)) text('Exit code: ' + result.exit_code);`
}

function codeModeCommandSource(output) {
  const text = toolOutputText(output)
  const packet = text?.match(/<core_tool_receipts>(.*?)<\/core_tool_receipts>/su)?.[1]
  if (packet === undefined) return null
  const receipts = JSON.parse(packet).receipts
  return receipts.find(receipt => receipt.tool.endsWith('exec_command'))?.source_id ?? null
}
