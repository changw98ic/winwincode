#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync, spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { appendFileSync, chmodSync, closeSync, existsSync, globSync, lstatSync, mkdirSync, openSync, readFileSync, readdirSync, realpathSync, renameSync, unlinkSync, writeFileSync } from 'node:fs'
import { request as httpsRequest } from 'node:https'
import { request as httpRequest } from 'node:http'
import { hostname } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { setTimeout as delay } from 'node:timers/promises'
import { pathToFileURL } from 'node:url'

import { executeRequest, httpFailure, NetworkError, retryAfter } from '../packages/network-request/src/index.mjs'

import { apiProductionSourceDigest, assertDeviceRuntimeWorker, createCertificate, prepareControlledRepository,
  driveDelivery, verifyApiProductionSourceSeal, waitForDeviceWorkerRegistered,
  workItemCreatePayload } from './run-api-production-vertical.mjs'
import { apiProductionSourceSealSummary } from './release-artifact-contract.mjs'
import { captureDeviceWorkerProcess, configuredDeviceModelRoute, runtimeChildEnvironment,
  seedDeviceLocalProvider, startDeterministicDeviceModelServer, stopDeviceWorkerProcess,
  stopProcessGroup, waitFor, writeDeviceTrustRoot } from './device-production-fixture.mjs'

const ROOT = resolve(import.meta.dirname, '..')
const SCHEMA = 'winwincode/v1'
const REPORT_SCHEMA = 'winwincode.remote-device-fault-acceptance.v1'
const BIND_RESPONSE_DROP_MARKER = 'winwincode-server: test-support dropped committed device binding response'
const FIXTURE_COMPLETED_COMMAND = 'printf acceptance-candidate > .winwincode-api-candidate && git status --porcelain=v1'
const ACCEPTANCE_SOURCE_PATHS = [
  'scripts/run-remote-device-fault-acceptance.mjs', 'scripts/device-production-fixture.mjs',
  'scripts/run-api-production-vertical.mjs', 'scripts/release-artifact-contract.mjs',
  'scripts/release-source-contract.mjs', 'scripts/product-build-contract.mjs',
  'scripts/device-agent-environment.mjs', 'scripts/device-provider-credentials.mjs',
  'packages/network-request/src/index.mjs', 'packages/network-request/src/policy.generated.mjs',
]
const DEFAULT_SCOPE = { kind: 'repository', organizationId: 'org_01J00000000000000000000000',
  workspaceId: 'wsp_01J00000000000000000000000', projectId: 'prj_01J00000000000000000000000',
  repositoryId: 'rep_01J00000000000000000000000' }
export const REMOTE_FAULT_SCENARIOS = Object.freeze([
  'two-distinct-hosts', 'server-tls-and-auth-boundary', 'device-binding',
  'binding-receipt-crash-recovery', 'binding-response-loss', 'device-restart',
  'task-tool-execution', 'worker-crash-process-closure', 'task-cancel-process-closure', 'network-reconnect-replay',
  'apply-commit-crash-recovery', 'apply-current-epoch-reconciliation',
  'baseline-stale', 'application-outcome-unknown-blocked',
  'offline-lease-expiry-process-closure', 'offline-revoke-reconnect-failure',
  'online-revoke-process-closure',
])

const digest = bytes => createHash('sha256').update(bytes).digest('hex')
const shellQuote = value => `'${String(value).replaceAll("'", "'\\''")}'`
const identifier = prefix => `${prefix}_0${randomBytes(13).toString('hex').slice(0, 25).toUpperCase()}`

function acceptanceSourceDigests(root) {
  return Object.fromEntries(ACCEPTANCE_SOURCE_PATHS.map(path => [path, digest(readFileSync(join(root, path)))]))
}

export function emptyRemoteFaultReport() {
  return { schemaVersion: REPORT_SCHEMA, status: 'incomplete', modelEvidence: 'fixture',
    topology: 'unverified', startedAt: new Date().toISOString(),
    scenarios: REMOTE_FAULT_SCENARIOS.map(id => ({ id, status: 'pending', evidence: null,
      reason: '尚未执行真实两机验收' })) }
}

export function finalizeRemoteFaultReport(report) {
  report.finishedAt = new Date().toISOString()
  report.status = report.failure || report.cleanupDevice === 'failed' || report.cleanupNetwork === 'failed'
    || report.scenarios.some(item => item.status === 'failed') ? 'failed'
    : report.scenarios.every(item => item.status === 'passed') ? 'passed' : 'incomplete'
  return report
}

export function validateRemoteFaultConfiguration(configuration) {
  assert.equal(configuration.schemaVersion, 1)
  const { server, device } = configuration
  const endpoint = new URL(server.url)
  assert.equal(endpoint.protocol, 'https:', 'remote Server requires HTTPS')
  assert.equal(endpoint.origin, server.url, 'Server URL must be an exact origin')
  assert.equal(endpoint.username + endpoint.password + endpoint.search + endpoint.hash, '')
  assert.equal(new URL(server.origin ?? server.url).origin, server.origin ?? server.url)
  assert.ok(['direct', 'loopback-proxy'].includes(server.tlsMode))
  assert.ok(typeof server.username === 'string' && server.username.length > 0)
  assert.match(server.passwordEnvironment, /^[A-Z][A-Z0-9_]+$/u)
  assert.equal(resolve(server.database), server.database)
  assert.equal(resolve(server.binary), server.binary)
  assert.ok(Number.isSafeInteger(server.pid) && server.pid > 0, 'the actual local Server PID is required')
  if (server.logFile !== undefined) assert.equal(resolve(server.logFile), server.logFile)
  assert.ok(typeof device.sshHost === 'string' && /^[A-Za-z0-9_@.:[\]-]+$/u.test(device.sshHost)
    && !device.sshHost.startsWith('-'), 'Device requires one explicit SSH destination')
  assert.ok(!['localhost', '127.0.0.1', '::1', '[::1]'].includes(device.sshHost.split('@').at(-1)),
    'loopback SSH cannot establish two-machine evidence')
  for (const key of ['checkout', 'binaryDirectory', 'directory']) assert.equal(resolve(device[key]), device[key])
  assert.ok(device.directory !== '/' && !device.directory.startsWith(`${device.checkout}/`),
    'Device acceptance data must be outside the checkout')
  if (device.port !== undefined) assert.ok(Number.isInteger(device.port) && device.port > 0 && device.port <= 65535)
  if (device.toolTty !== undefined) assert.equal(typeof device.toolTty, 'boolean')
  if (server.tlsMode === 'loopback-proxy') {
    const backend = new URL(server.backendUrl)
    assert.equal(backend.protocol, 'http:')
    assert.ok(['127.0.0.1', 'localhost', '[::1]'].includes(backend.hostname))
    assert.equal(backend.origin, server.backendUrl)
  }
  if (configuration.networkFault !== undefined) {
    for (const key of ['disconnect', 'reconnect']) {
      assert.ok(Array.isArray(configuration.networkFault[key]) && configuration.networkFault[key].length > 0
        && configuration.networkFault[key].every(value => typeof value === 'string' && value.length > 0))
    }
    assert.equal(configuration.networkFault.preservesSsh, true, 'network fault commands must preserve SSH access')
  }
  return configuration
}

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 })
  const temporary = `${path}.${process.pid}.tmp`
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 })
  renameSync(temporary, path)
}

function readPrivateJson(path) {
  const stat = lstatSync(path)
  assert.ok(stat.isFile() && !stat.isSymbolicLink() && (stat.mode & 0o077) === 0,
    'acceptance configuration must be an owner-only regular file')
  return JSON.parse(readFileSync(path, 'utf8'))
}

function machineIdentity() {
  let identity
  for (const path of ['/etc/machine-id', '/var/lib/dbus/machine-id']) {
    if (existsSync(path)) { identity = readFileSync(path, 'utf8').trim(); break }
  }
  if (!identity && process.platform === 'darwin') {
    const output = execFileSync('ioreg', ['-rd1', '-c', 'IOPlatformExpertDevice'], { encoding: 'utf8' })
    identity = output.match(/"IOPlatformUUID"\s*=\s*"([^"]+)"/u)?.[1]
  }
  assert.ok(identity, 'cannot establish the OS host identity')
  return { hostIdSha256: digest(identity), hostname: hostname(), platform: process.platform, architecture: process.arch }
}

function serverProcessIdentity(server) {
  process.kill(server.pid, 0)
  const binary = realpathSync(server.binary)
  if (process.platform === 'linux') assert.equal(realpathSync(`/proc/${server.pid}/exe`), binary)
  else {
    const paths = execFileSync('lsof', ['-a', '-p', String(server.pid), '-d', 'txt', '-Fn'], { encoding: 'utf8' })
      .split('\n').filter(line => line.startsWith('n')).map(line => line.slice(1))
    assert.ok(paths.includes(binary), 'the configured binary must be loaded by the actual Server process')
  }
  const verified = verifyApiProductionSourceSeal({ root: ROOT, serverBinary: binary,
    helperExecutable: join(dirname(binary), 'winwincode-kernel-helper') })
  return { pid: server.pid, binarySha256: digest(readFileSync(binary)), processBinaryVerified: true,
    sourceSeal: apiProductionSourceSealSummary(verified.seal) }
}

function readonly(database, callback) {
  const connection = new DatabaseSync(database, { readOnly: true })
  try { connection.exec('PRAGMA busy_timeout = 5000'); return callback(connection) }
  finally { connection.close() }
}

function command(command_, arguments_, { input, env = process.env, timeoutMillis = 120_000, diagnosticPath } = {}) {
  return new Promise((resolvePromise, reject) => {
    if (diagnosticPath) mkdirSync(dirname(diagnosticPath), { recursive: true, mode: 0o700 })
    const diagnostic = diagnosticPath ? openSync(diagnosticPath, 'a', 0o600) : null
    const child = spawn(command_, arguments_, { env, stdio: ['pipe', 'pipe', 'pipe'] })
    const output = []; let bytes = 0
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMillis)
    child.stdout.on('data', chunk => { bytes += chunk.length; if (bytes > 16 * 1024 * 1024) child.kill('SIGKILL'); else output.push(chunk) })
    child.stderr.on('data', chunk => { if (diagnostic !== null) appendFileSync(diagnostic, chunk) })
    child.on('error', reject)
    child.on('close', (code, signal) => {
      clearTimeout(timer)
      if (diagnostic !== null) closeSync(diagnostic)
      resolvePromise({ code, signal, stdout: Buffer.concat(output).toString('utf8'), diagnosticPath })
    })
    child.stdin.on('error', () => {})
    child.stdin.end(input)
  })
}

export function remoteOperationCommand(device) {
  const arguments_ = ['-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes']
  if (device.port !== undefined) arguments_.push('-p', String(device.port))
  arguments_.push(device.sshHost, `exec ${shellQuote(device.node ?? 'node')} ${shellQuote(join(device.checkout,
    'scripts/run-remote-device-fault-acceptance.mjs'))} --device-operation`)
  return arguments_
}

async function remote(configuration, operation, values = {}) {
  const diagnosticPath = join(configuration.diagnosticDirectory, `ssh-${operation}-${randomBytes(6).toString('hex')}.log`)
  const result = await command('ssh', remoteOperationCommand(configuration.device), {
    input: JSON.stringify({ operation, device: configuration.device, runId: configuration.runId, ...values }),
    timeoutMillis: configuration.timeoutMillis ?? 120_000,
    diagnosticPath,
  })
  let response
  try { response = JSON.parse(result.stdout) } catch {}
  if (result.code !== 0 || response?.ok !== true) {
    throw Object.assign(new Error(`SSH Device operation ${operation} failed (${result.signal ?? result.code}); private diagnostic retained`), {
      code: response?.code ?? 'SSH_DEVICE_OPERATION_FAILED', diagnosticPath,
      remoteDiagnosticPath: response?.diagnosticPath ?? null,
    })
  }
  assert.equal(response.operation, operation)
  assert.equal(response.ok, true, `Device operation ${operation} rejected`)
  return response.evidence
}

async function secureAcceptanceRequestOnce(url, { method = 'GET', body, cookie, origin, ca,
  headers = {}, allowLoopbackHttp = false, timeoutMillis = 15_000 } = {}) {
  const endpoint = new URL(url)
  assert.ok(endpoint.protocol === 'https:' || (allowLoopbackHttp && endpoint.protocol === 'http:'
    && ['127.0.0.1', 'localhost', '[::1]'].includes(endpoint.hostname)))
  const bytes = body === undefined ? null : Buffer.from(JSON.stringify(body))
  return new Promise((resolvePromise, reject) => {
    let tls = null
    const request = (endpoint.protocol === 'https:' ? httpsRequest : httpRequest)(endpoint, {
      method, ca, rejectUnauthorized: true, timeout: timeoutMillis,
      headers: { Accept: 'application/json', Connection: 'close', ...headers,
        ...(origin ? { Origin: origin } : {}), ...(cookie ? { Cookie: cookie } : {}),
        ...(bytes ? { 'Content-Type': 'application/json', 'Content-Length': bytes.length } : {}) },
    }, response => {
      const chunks = []; let size = 0
      response.on('data', chunk => { size += chunk.length; if (size > 8 * 1024 * 1024) response.destroy(); else chunks.push(chunk) })
      response.on('error', reject)
      response.on('end', () => {
        const text = Buffer.concat(chunks).toString('utf8')
        let json = null
        try { json = text ? JSON.parse(text) : null } catch {}
        resolvePromise({ status: response.statusCode, json, headers: response.headers, tls })
      })
    })
    request.on('socket', socket => socket.once('secureConnect', () => {
      const certificate = socket.getPeerCertificate().raw
      if (!certificate) { request.destroy(new Error('verified peer certificate evidence is unavailable')); return }
      tls = { authorized: socket.authorized === true,
        certificateSha256: digest(certificate), protocol: socket.getProtocol() }
    }))
    request.on('timeout', () => request.destroy(Object.assign(new Error('acceptance request timed out'), { code: 'ETIMEDOUT' })))
    request.on('error', reject)
    request.end(bytes)
  })
}

export async function secureAcceptanceRequest(url, options = {}) {
  const endpoint = new URL(url)
  assert.ok(endpoint.protocol === 'https:' || (options.allowLoopbackHttp && endpoint.protocol === 'http:'
    && ['127.0.0.1', 'localhost', '[::1]'].includes(endpoint.hostname)))
  const deadline = performance.now() + (options.timeoutMillis ?? 15_000)
  try {
    return await executeRequest(async () => {
      const response = await secureAcceptanceRequestOnce(url, { ...options,
        timeoutMillis: Math.max(1, deadline - performance.now()) })
      if (response.status < 200 || response.status > 299) {
        throw new NetworkError(httpFailure(response.status, retryAfter(response.headers['retry-after'])), response)
      }
      return response
    }, { replay: (options.method ?? 'GET') === 'GET' ? 'replay_exact' : 'reconcile_first', deadline })
  } catch (error) {
    if (error instanceof NetworkError && error.response !== undefined) return error.response
    throw error
  }
}

class RemoteApi {
  constructor(server, password) {
    this.server = server; this.password = password; this.scope = { ...DEFAULT_SCOPE, ...server.scope }
    this.ca = server.caFile ? readFileSync(server.caFile) : undefined
  }
  async request(path, options = {}) {
    return secureAcceptanceRequest(`${this.server.url}${path}`, { ...options, cookie: this.cookie,
      origin: this.server.origin ?? this.server.url, ca: this.ca })
  }
  requestId() { return identifier('req') }
  async login() {
    const response = await this.request('/api/v1/auth/session', { method: 'POST', body: {
      schemaVersion: SCHEMA, username: this.server.username, password: this.password,
    } })
    assert.equal(response.status, 201, 'Owner must authenticate through the configured TLS endpoint')
    this.cookie = response.headers['set-cookie']?.[0]?.split(';')[0]
    assert.ok(this.cookie); this.actor = response.json.actor
  }
  async command(name, expectedRevision, payload, requestId = identifier('req')) {
    const response = await this.request('/api/v1/commands', { method: 'POST', body: {
      schemaVersion: SCHEMA, requestId, actor: this.actor, scope: this.scope,
      command: name, expectedRevision, payload,
    } })
    assert.equal(response.status, 200, `command ${name} rejected`)
    assert.equal(response.json.requestId, requestId)
    return response.json
  }
  async query(name, parameters) {
    const requestId = identifier('req')
    const response = await this.request('/api/v1/queries', { method: 'POST', body: {
      schemaVersion: SCHEMA, requestId, actor: this.actor, scope: this.scope,
      query: name, parameters, page: { cursor: null, limit: 200 },
    } })
    assert.equal(response.status, 200, `query ${name} rejected`)
    assert.equal(response.json.requestId, requestId)
    return response.json.result
  }
}

async function startRemoteTask(api, binding, repository, modelRoute, marker) {
  const productSessionId = identifier('psn')
  await api.command('session.create', 0, { productSessionId, projectId: api.scope.projectId,
    repositoryId: api.scope.repositoryId, title: 'Remote fault acceptance fixture', modelRoute })
  const response = await api.request('/api/v1/sessions', { method: 'POST', body: {
    schemaVersion: SCHEMA, clientId: binding.publicClientId, repositoryBindingId: repository.repositoryBindingId,
    productSession: { id: productSessionId, scope: api.scope },
  } })
  assert.equal(response.status, 201, 'remote Device must launch the Worker')
  const requestId = identifier('req')
  await api.command('chat.submit', 1, { productSessionId, message: marker }, requestId)
  return { productSessionId, requestId, launched: response.json }
}

async function completedTaskRuntime(api, task) {
  const messages = await api.query('session.messages.list', { productSessionId: task.productSessionId })
  if (!messages.items?.some(item => item.role === 'assistant' && item.state === 'completed')) return false
  const runtime = (await api.query('runtime.projection.get', { kind: 'product-session', productSessionId: task.productSessionId }))
    .sessions?.find(item => item.productSessionId === task.productSessionId)
  if (!runtime) return false
  const deviceProcessBinding = assertDeviceRuntimeWorker({ serverDatabase: api.server.database,
    runtime, worker: task.launched })
  return deviceProcessBinding ? { ...runtime, deviceProcessBinding } : false
}

async function createRemoteDeliveryCandidate({ api, binding, repository, modelRoute, sourceProductSessionId,
  baseline, deviceCall }) {
  const deliveryId = identifier('dlv')
  const client = { requestId: () => api.requestId(), command: (...args) => api.command(...args),
    query: async (...args) => ({ result: await api.query(...args) }) }
  const created = await api.command('delivery.create', 0, { deliveryId, spec: {
    title: 'Remote fault acceptance candidate fixture', goal: 'Apply the approved fixture source change.',
    acceptanceCriteria: [{ id: 'fixture-candidate', required: true,
      title: 'The approved source change produces a verified frozen candidate.' }],
    baseRevision: baseline, constraints: ['use the exact fixture repository baseline'], outOfScope: [],
    publicationTarget: null, repositoryId: api.scope.repositoryId, scope: ['fixture candidate'],
    sourceProductSessionId, verificationCommand: 'git rev-parse --verify HEAD',
  } })
  const aggregate = await api.query('workrun.get', { deliveryId, workItemId: null, atCursor: null })
  const payload = workItemCreatePayload(aggregate, created.currentRevision)
  if (payload) await api.command('workitems.create', created.currentRevision, payload)
  const current = await api.query('delivery.get', { deliveryId })
  const started = await api.command('workrun.start', current.deliveryRevision, { deliveryId, dispatchProfile: 'executor' })
  const launches = new Map()
  const launch = async workRunId => {
    if (!workRunId || launches.has(workRunId)) return
    const response = await api.request('/api/v1/sessions', { method: 'POST', body: {
      schemaVersion: SCHEMA, clientId: binding.publicClientId,
      repositoryBindingId: repository.repositoryBindingId, workRunId,
    } })
    assert.equal(response.status, 201, 'the real Device launches the Controller-owned WorkRun')
    launches.set(workRunId, response.json)
    await waitForDeviceWorkerRegistered(client, response.json, 60_000)
  }
  await launch(started.result.activeWorkRunId ?? started.result.workRunId)
  const terminal = await driveDelivery(client, 180_000, modelRoute, Date.now, {
    deliveryId, expectDeviceWorkRun: true,
    onActiveWorkRuns: async runs => { for (const run of runs) await launch(run.id) },
    pendingDeviceWorkRunIds: () => readonly(api.server.database, database => database.prepare(`SELECT work_run_id
      FROM scheduler_execution_jobs WHERE delivery_id=? AND state='queued'`).all(deliveryId).map(row => row.work_run_id)),
    onPendingDeviceWorkRuns: async ids => { for (const id of ids) await launch(id) },
  })
  assert.equal(terminal.detail.status, 'done')
  assert.equal(terminal.detail.verdict?.status, 'pass')
  return { deliveryId, status: terminal.detail.status, verdictStatus: terminal.detail.verdict.status,
    workerSessions: [...launches.values()].map(item => item.workerSessionId), modelEvidence: 'fixture' }
}

function processTable() {
  const output = execFileSync('ps', ['-axo', 'pid=,ppid=,pgid=,stat=,lstart=,comm='], {
    encoding: 'utf8', env: { ...process.env, LC_ALL: 'C' },
  })
  return output.split('\n').flatMap(line => {
    const match = /^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(\S+\s+\S+\s+\d+\s+\S+\s+\d+)\s+(.+)$/u.exec(line)
    if (!match) return []
    const pid = Number(match[1])
    let start = match[5].replace(/\s+/gu, ' ')
    if (process.platform === 'linux') {
      try { const stat = readFileSync(`/proc/${pid}/stat`, 'utf8'); start = `linux-${stat.slice(stat.lastIndexOf(')') + 1).trim().split(/\s+/u)[19]}` }
      catch { return [] }
    }
    return [{ pid, parent: Number(match[2]), group: Number(match[3]), state: match[4], start,
      executable: basename(match[6]) }]
  })
}

export function observeProcessWitnesses(witnesses) {
  const identities = new Set(witnesses.map(item => `${item.pid}:${item.start}`))
  return { alive: processTable().filter(item => !item.state.startsWith('Z') && identities.has(`${item.pid}:${item.start}`)) }
}

function descendantProcesses(workers) {
  const rows = processTable()
  const roots = workers.filter(worker => {
    const row = rows.find(candidate => candidate.pid === worker.pid)
    const identity = row && (process.platform === 'linux' ? row.start : `darwin-ps-${row.start}`)
    return worker.state === 'running' && identity === worker.processStartIdentity
  }).map(worker => worker.pid)
  return processOwnershipWitnesses(rows, roots).filter(row => !row.state.startsWith('Z'))
}

function processOwnershipWitnesses(rows, roots) {
  const owned = new Set(roots)
  for (;;) {
    const count = owned.size
    for (const row of rows) if (owned.has(row.parent)) owned.add(row.pid)
    const groups = new Set(rows.filter(row => owned.has(row.pid) && Number.isSafeInteger(row.group)
      && row.group > 0 && owned.has(row.group)).map(row => row.group))
    for (const row of rows) if (groups.has(row.group)) owned.add(row.pid)
    if (count === owned.size) break
  }
  return rows.filter(row => owned.has(row.pid))
}

export function taskProcessWitnesses(snapshot, launched) {
  const worker = snapshot.workers.find(item => item.workerSessionId === launched.workerSessionId)
  assert.ok(worker, 'the launched Worker must be present in the Device registry')
  return processOwnershipWitnesses(snapshot.processes, [worker.pid]).filter(row => row.pid !== worker.pid)
}

function toolProcessWitnesses(snapshot, launched) {
  const witnesses = taskProcessWitnesses(snapshot, launched)
  assert.ok(witnesses.some(item => item.executable.includes('winwincode-kernel-helper')))
  assert.ok(witnesses.filter(item => /node/u.test(item.executable)).length >= 2)
  return witnesses
}

export function fixtureCommandCell(command_, { tty = false } = {}) {
  return `const execute=ALL_TOOLS.find(tool=>tool.name.endsWith('exec_command'));const poll=ALL_TOOLS.find(tool=>tool.name.endsWith('write_stdin'));if(!execute||!poll)throw new Error('command tools unavailable');let result=await tools[execute.name](${JSON.stringify({ cmd: command_, tty, yield_time_ms: 1000 })});while(result.exit_code===undefined&&Number.isInteger(result.session_id)){result=await tools[poll.name]({session_id:result.session_id,chars:'',yield_time_ms:1000});}if(result.exit_code!==0)throw new Error('fixture command did not complete successfully');text(result);`
}

export function readFixtureCommandEvidence({ deviceData, workerSessionId, jobId, command: command_ }) {
  const worker = readonly(join(deviceData, 'device-client.sqlite3'), database => database.prepare(`SELECT data_directory
    FROM worker_process_registry WHERE worker_session_id=?`).get(workerSessionId))
  assert.ok(worker, 'the actual tool must belong to one Device-managed WorkerSession')
  const runs = globSync('**/worker-codex.sqlite3', { cwd: worker.data_directory }).flatMap(path =>
    readonly(join(worker.data_directory, path), database => {
      if (!database.prepare("SELECT 1 FROM sqlite_master WHERE name='codex_run'").get()) return []
      return database.prepare('SELECT record_json FROM codex_run').all().map(row =>
        JSON.parse(Buffer.from(row.record_json).toString('utf8'))).filter(run => run.job.jobId === jobId)
    }))
  if (runs.length === 0) return null
  assert.equal(runs.length, 1, 'one original Core run must own the actual job')
  const kernelSessionId = runs[0].kernelSessionId
  const rows = globSync('**/state_5.sqlite', { cwd: deviceData }).flatMap(path => readonly(join(deviceData, path), database => {
    if (!database.prepare("SELECT 1 FROM sqlite_master WHERE name='tool_requests'").get()) return []
    return database.prepare(`SELECT r.sequence,r.request_json,a.attempt_id,a.effective_input,a.execution,a.disposition,
      a.execution_result,a.accepted_result FROM tool_requests r JOIN tool_attempts a ON a.request_sequence=r.sequence
      WHERE r.thread_id=? ORDER BY r.sequence`).all(kernelSessionId).map(row => ({ ...row, path: join(deviceData, path),
      request: JSON.parse(row.request_json) })).filter(row => row.request.tool_name.endsWith('exec_command')
        || row.request.tool_name.endsWith('write_stdin')).map(row => ({ ...row,
      input: row.effective_input ? JSON.parse(JSON.parse(row.effective_input).input) : null }))
  }))
  const commands = rows.filter(row => row.request.tool_name.endsWith('exec_command') && row.input?.cmd === command_)
  if (commands.length === 0) return null
  assert.equal(commands.length, 1, 'the exact original command must execute once')
  const original = commands[0]
  if (!original.execution_result || !original.accepted_result) return null
  const initial = JSON.parse(original.execution_result).code_mode_result
  const acceptedInitial = JSON.parse(original.accepted_result).code_mode_result
  assert.equal(initial?.session_id, acceptedInitial?.session_id,
    'both original durable receipts must identify the same actual command session')
  const receipts = [original, ...rows.filter(row => row.sequence > original.sequence
    && row.request.turn_id === original.request.turn_id && row.request.tool_name.endsWith('write_stdin')
    && Number.isInteger(initial?.session_id) && row.input?.session_id === initial.session_id)]
  const last = receipts.at(-1)
  if (!last.execution_result || !last.accepted_result) return null
  const exitCodes = [last.execution_result, last.accepted_result].map(value => JSON.parse(value).code_mode_result?.exit_code)
  if (exitCodes.some(value => value === undefined)) return null
  assert.deepEqual(exitCodes, [0, 0], 'the original actual command must exit successfully in both durable receipts')
  for (const receipt of receipts) {
    assert.equal(receipt.execution, 'completed')
    assert.equal(receipt.disposition, 'accepted')
  }
  return { workerSessionId, jobId, kernelSessionId, commandSha256: digest(command_), exitCodes,
    receipts: receipts.map(row => ({ database: row.path, requestSequence: row.sequence, attemptId: row.attempt_id,
      execution: row.execution, disposition: row.disposition, executionResultSha256: digest(row.execution_result),
      acceptedResultSha256: digest(row.accepted_result) })) }
}

function deviceSnapshot(device) {
  return readonly(join(device.directory, 'device-data/device-client.sqlite3'), database => {
    const binding = database.prepare(`SELECT request_id AS requestId,client_node_id AS clientNodeId,
      public_client_id AS publicClientId,bound_at AS boundAt,failure_reason AS failureReason,
      pairing_token IS NULL AS tokenCleared FROM remote_device_binding`).get()
    const workers = database.prepare(`SELECT worker_session_id AS workerSessionId,worker_id AS workerId,
      worker_instance_id AS workerInstanceId,pid,process_start_identity AS processStartIdentity,state,exit_code AS exitCode
      FROM worker_process_registry`).all()
    const mirror = database.prepare('SELECT occupancy_lease_id AS leaseId,fencing_token AS epoch FROM occupancy_mirror').get() ?? null
    const identity = database.prepare('SELECT current_instance_id AS instanceId FROM device_identity').get()
    const outbox = database.prepare(`SELECT message_id AS messageId,envelope_sequence AS sequence,
      kind,payload,published FROM client_outbox`).all().map(({ payload, ...row }) => ({
        ...row, payloadDigest: `sha256:${digest(payload)}`,
      }))
    const applications = database.prepare(`SELECT application_id AS applicationId,approval_id AS approvalId,
      candidate_ref AS candidateRef,base_commit AS baseCommit,original_epoch AS originalEpoch,
      finished_receipt_json AS finishedReceiptJson,recovery_outcome AS recoveryOutcome FROM candidate_applications`).all()
    const agentPath = join(device.directory, 'agent-state.json')
    const agent = existsSync(agentPath) ? JSON.parse(readFileSync(agentPath, 'utf8')) : null
    return { binding, identity, workers, processes: descendantProcesses(workers), mirror, outbox, applications, agent }
  })
}

function assertDeviceDirectory(payload) {
  const marker = join(payload.device.directory, 'acceptance-owner.json')
  assert.equal(JSON.parse(readFileSync(marker, 'utf8')).runId, payload.runId,
    'Device operations require this acceptance run to own the data directory')
}

async function controlAgent(payload, operation, environment = {}) {
  const statePath = join(payload.device.directory, 'agent-state.json')
  const previous = JSON.parse(readFileSync(statePath, 'utf8'))
  const sequence = previous.sequence + 1
  writeJson(join(payload.device.directory, 'agent-control.json'), { sequence, operation, environment })
  return waitFor(() => {
    const state = JSON.parse(readFileSync(statePath, 'utf8'))
    return state.sequence === sequence ? state : false
  }, `Device agent ${operation}`, 60_000)
}

async function deviceOperation(payload) {
  const { device, operation } = payload
  if (operation === 'identity') {
    const verified = verifyApiProductionSourceSeal({ root: device.checkout,
      serverBinary: join(device.binaryDirectory, 'winwincode-server'),
      helperExecutable: join(device.binaryDirectory, 'winwincode-kernel-helper') })
    return { ...machineIdentity(), sourceCommit: execFileSync('git', ['-C', device.checkout, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
      sourceSha256: apiProductionSourceDigest(device.checkout),
      sourceSeal: apiProductionSourceSealSummary(verified.seal),
      acceptanceSources: acceptanceSourceDigests(device.checkout),
      binaries: Object.fromEntries(['wwc', 'winwincode-server', 'winwincode-worker', 'winwincode-kernel-helper']
        .map(name => [name, digest(readFileSync(join(device.binaryDirectory, name)))])) }
  }
  if (operation === 'prepare') {
    assert.equal(existsSync(device.directory), false, 'Device acceptance directory must be new')
    mkdirSync(device.directory, { recursive: true, mode: 0o700 })
    writeJson(join(device.directory, 'acceptance-owner.json'), { runId: payload.runId })
    if (payload.caPem) writeFileSync(join(device.directory, 'server-ca.pem'), payload.caPem, { mode: 0o600 })
    const repository = prepareControlledRepository({ fixtureDirectory: device.directory })
    return { repository: repository.repository, baseline: repository.revision }
  }
  assertDeviceDirectory(payload)
  const wwc = join(device.binaryDirectory, 'wwc')
  const dataDirectory = join(device.directory, 'device-data')
  if (operation === 'bind') {
    const args = ['device', 'bind', '--data-dir', dataDirectory, '--json']
    if (payload.serverUrl) args.push('--server-url', payload.serverUrl)
    if (payload.caPem) args.push('--ca-file', join(device.directory, 'server-ca.pem'))
    const result = await command(wwc, args, { env: { ...runtimeChildEnvironment(),
      ...(payload.pairingToken ? { WWC_DEVICE_PAIRING_TOKEN: payload.pairingToken } : {}),
      ...(payload.crash ? { WWC_DEVICE_TEST_EXIT_BEFORE_BIND_RECEIPT_SAVE: '1' } : {}),
    }, diagnosticPath: join(device.directory, 'diagnostics', `bind-${Date.now()}.log`) })
    return { exitCode: result.code, receipt: result.code === 0 ? JSON.parse(result.stdout) : null,
      binding: deviceSnapshot(device).binding, diagnosticPath: result.diagnosticPath }
  }
  if (operation === 'start') {
    const configuration = { device, runId: payload.runId, expiresAt: Date.now() + 30 * 60_000 }
    writeJson(join(device.directory, 'agent-configuration.json'), configuration)
    const log = openSync(join(device.directory, 'agent.log'), 'a', 0o600)
    const child = spawn(process.execPath, [join(device.checkout, 'scripts/run-remote-device-fault-acceptance.mjs'),
      '--device-agent', join(device.directory, 'agent-configuration.json')], { detached: true, stdio: ['ignore', log, log] })
    child.unref()
    return waitFor(() => existsSync(join(device.directory, 'agent-state.json'))
      && JSON.parse(readFileSync(join(device.directory, 'agent-state.json'), 'utf8')), 'Device fixture agent', 30_000)
  }
  if (operation === 'restart' || operation === 'stop') return controlAgent(payload, operation, payload.environment)
  if (operation === 'snapshot') return deviceSnapshot(device)
  if (operation === 'observe-processes') return observeProcessWitnesses(payload.witnesses)
  if (operation === 'tool-completion-evidence') return readFixtureCommandEvidence({ deviceData: dataDirectory,
    workerSessionId: payload.workerSessionId, jobId: payload.jobId, command: FIXTURE_COMPLETED_COMMAND })
  if (operation === 'registered-repository') {
    const repository = join(device.directory, 'source-repositories', 'rep_01J00000000000000000000000')
    const result = await command(wwc, ['repo', 'add', repository, '--data-dir', dataDirectory, '--json'])
    assert.equal(result.code, 0, 'Device repository registration failed')
    return JSON.parse(result.stdout).repository
  }
  if (operation === 'stop-worker') {
    const worker = captureDeviceWorkerProcess({ deviceData: dataDirectory, launched: payload.launched })
    assert.ok(worker)
    const before = deviceSnapshot(device)
    await stopDeviceWorkerProcess({ deviceData: dataDirectory, worker })
    return { worker, before, after: deviceSnapshot(device) }
  }
  if (operation === 'network') {
    assert.ok(Array.isArray(payload.command) && payload.command.length > 0)
    const result = await command(payload.command[0], payload.command.slice(1))
    assert.equal(result.code, 0, 'configured Device network fault command failed')
    return { exitCode: result.code, changedAt: new Date().toISOString() }
  }
  if (operation === 'probe-server') {
    try {
      const response = await secureAcceptanceRequest(`${payload.serverUrl}/health`, {
        ca: payload.caPem ? Buffer.from(payload.caPem) : undefined, timeoutMillis: 2_000,
      })
      return { reachable: true, status: response.status, tls: response.tls }
    } catch (error) { return { reachable: false, code: error.code ?? 'NETWORK_FAILURE' } }
  }
  if (operation === 'repository-state') {
    const repository = join(device.directory, 'source-repositories', 'rep_01J00000000000000000000000')
    const git = args => execFileSync('git', ['-C', repository, ...args], { encoding: 'utf8' }).trim()
    const commits = git(['log', '--all', '--format=%H', '--grep=WinWinCode-Application:']).split('\n').filter(Boolean)
    return { head: git(['rev-parse', 'HEAD']), status: git(['status', '--porcelain=v1']),
      branches: Object.fromEntries(git(['for-each-ref', '--format=%(refname:short) %(objectname)', 'refs/heads/'])
        .split('\n').filter(Boolean).map(line => line.split(' '))),
      applicationCommits: commits.map(commit => ({ commit,
        applicationIds: [...git(['show', '-s', '--format=%B', commit])
          .matchAll(/^WinWinCode-Application: (.+)$/gmu)].map(match => match[1]),
      })) }
  }
  if (operation === 'prepare-application-branches') {
    const repository = join(device.directory, 'source-repositories', 'rep_01J00000000000000000000000')
    assert.match(payload.baseCommit, /^[a-f0-9]{40}$/u)
    for (const branch of ['wwc-acceptance-applied', 'wwc-acceptance-stale', 'wwc-acceptance-unknown']) {
      execFileSync('git', ['-C', repository, 'branch', branch, payload.baseCommit])
    }
    const tree = execFileSync('git', ['-C', repository, 'rev-parse', `${payload.baseCommit}^{tree}`], { encoding: 'utf8' }).trim()
    const moved = execFileSync('git', ['-C', repository, '-c', 'user.name=Acceptance fixture', '-c',
      'user.email=acceptance@winwincode.invalid', 'commit-tree', tree, '-p', payload.baseCommit,
      '-m', 'Fixture baseline moved'], { encoding: 'utf8',
      env: { ...runtimeChildEnvironment(), GIT_CONFIG_NOSYSTEM: '1' },
    }).trim()
    execFileSync('git', ['-C', repository, 'branch', '-f', 'wwc-acceptance-stale', moved])
    return { baseline: payload.baseCommit, movedHead: moved }
  }
  if (operation === 'make-unknown-outcome') {
    const repository = join(device.directory, 'source-repositories', 'rep_01J00000000000000000000000')
    writeFileSync(join(repository, 'acceptance-unknown-outcome.txt'), 'fixture: interrupted application requires a decision\n',
      { flag: 'wx', mode: 0o600 })
    return { fixtureDirtyPath: 'acceptance-unknown-outcome.txt' }
  }
  if (operation === 'clear-unknown-fixture') {
    const path = join(device.directory, 'source-repositories', 'rep_01J00000000000000000000000',
      'acceptance-unknown-outcome.txt')
    assert.equal(readFileSync(path, 'utf8'), 'fixture: interrupted application requires a decision\n')
    unlinkSync(path)
    return { removedFixtureDirtyPath: 'acceptance-unknown-outcome.txt' }
  }
  if (operation === 'application-crash-evidence') {
    assert.ok(['after_started', 'after_repository_commit'].includes(payload.point))
    assert.match(payload.applicationId, /^[A-Za-z0-9_-]+$/u)
    const marker = `WWC_TEST_APPLICATION_CRASH_POINT reached point=${payload.point} application_id=${payload.applicationId}`
    return { markerObserved: readFileSync(join(device.directory, 'device.log'), 'utf8').includes(marker),
      point: payload.point, applicationId: payload.applicationId }
  }
  if (operation === 'worker-expiry-diagnostic') {
    assert.match(payload.workerSessionId, /^wsn_[0-9A-HJKMNP-TV-Z]{26}$/u)
    const directory = join(dataDirectory, 'worker-logs', payload.workerSessionId)
    if (!existsSync(directory)) return { authorityExpired: false, logs: [] }
    const logs = readdirSync(directory).filter(name => /^stderr(?:\.\d+)?\.log$/u.test(name)).map(name => {
      const bytes = readFileSync(join(directory, name))
      const text = bytes.toString('utf8')
      const matches = [...text.matchAll(/network AuthorityExpired at ([A-Za-z]+); acceptance=([A-Za-z]+); status=None/gu)]
      return { path: join(directory, name), sha256: digest(bytes), authorityExpired: matches.length > 0,
        failures: matches.map(match => ({ kind: 'AuthorityExpired', phase: match[1], acceptance: match[2] })) }
    })
    return { authorityExpired: logs.some(item => item.authorityExpired), logs }
  }
  throw new Error('unknown Device operation')
}

async function deviceAgent(path) {
  const { device, expiresAt } = readPrivateJson(path)
  const certificate = createCertificate(device.directory)
  chmodSync(certificate.key, 0o600)
  const trust = writeDeviceTrustRoot({ certificatePath: certificate.cert,
    destination: join(device.directory, 'fixture-provider-root.der') })
  const childProgram = `const {spawn}=require('node:child_process');const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});console.log(JSON.stringify({pid:process.pid,childPid:child.pid}));setInterval(()=>{},1000)`
  const model = await startDeterministicDeviceModelServer({ certificatePath: certificate.cert,
    privateKeyPath: certificate.key, nativeCellScripts: {
      'native-cell-A': fixtureCommandCell(FIXTURE_COMPLETED_COMMAND),
      'native-cell-B': fixtureCommandCell(`${shellQuote(process.execPath)} -e ${shellQuote(childProgram)}`,
        { tty: device.toolTty ?? false }),
    } }).listen()
  let child = null
  let state = { sequence: 0, fixture: true, modelEndpoint: model.endpoint, agentPid: process.pid,
    devicePid: null, deviceExitCode: null, deviceRunning: false, active: true }
  const persist = () => writeJson(join(device.directory, 'agent-state.json'), state)
  const start = environment => {
    const log = openSync(join(device.directory, 'device.log'), 'a', 0o600)
    child = spawn(join(device.binaryDirectory, 'wwc'), ['device', 'serve', '--data-dir', join(device.directory, 'device-data')], {
      detached: true, stdio: ['ignore', log, log], env: { ...runtimeChildEnvironment(),
        WWC_WORKER_HELPER_EXECUTABLE: join(device.binaryDirectory, 'winwincode-kernel-helper'),
        WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE: trust, ...environment },
    })
    state.devicePid = child.pid; state.deviceExitCode = null; state.deviceRunning = true
    child.once('exit', code => { state.deviceExitCode = code; state.deviceRunning = false; persist() })
  }
  start({}); persist()
  try {
    for (; Date.now() < expiresAt;) {
      const controlPath = join(device.directory, 'agent-control.json')
      if (existsSync(controlPath)) {
        const control = JSON.parse(readFileSync(controlPath, 'utf8'))
        if (control.sequence > state.sequence) {
          await stopProcessGroup(child, { deviceData: join(device.directory, 'device-data') })
          if (control.operation === 'restart') start(control.environment ?? {})
          else if (control.operation === 'stop') state.active = false
          else throw new Error('unknown Device fixture control')
          state.sequence = control.sequence; persist()
          if (!state.active) break
        }
      }
      await delay(100)
    }
    state.active = false; persist()
  } finally { await stopProcessGroup(child, { deviceData: join(device.directory, 'device-data') }); await model.close() }
}

export async function runRemoteDeviceFaultAcceptance(configuration, output) {
  validateRemoteFaultConfiguration(configuration)
  configuration = { ...configuration, runId: identifier('remote_acceptance'),
    diagnosticDirectory: join(dirname(output), `${basename(output)}-diagnostics`) }
  const report = emptyRemoteFaultReport()
  report.server = { url: configuration.server.url, tlsMode: configuration.server.tlsMode }
  report.buildFeatures = { declaredTestSupport: configuration.testSupport === true, observedTestSupportHooks: [] }
  report.execution = { model: 'fixture', toolTty: configuration.device.toolTty ?? false,
    networkFaultConfigured: configuration.networkFault !== undefined }
  const save = () => writeJson(output, report)
  const result = (id, status, evidence, reason = null) => {
    Object.assign(report.scenarios.find(item => item.id === id), { status, evidence, reason }); save()
  }
  const attempt = async (id, run) => {
    try { const evidence = await run(); result(id, 'passed', evidence); return evidence }
    catch (error) { result(id, 'failed', { diagnosticPath: error.diagnosticPath ?? null,
      remoteDiagnosticPath: error.remoteDiagnosticPath ?? null },
    `${error.code ?? error.name}: ${error.message.split('\n')[0]}`); return null }
  }
  const password = process.env[configuration.server.passwordEnvironment]
  assert.ok(password, 'Owner password environment variable is required')
  const api = new RemoteApi(configuration.server, password)
  const caPem = configuration.server.caFile ? readFileSync(configuration.server.caFile, 'utf8') : null
  const deviceCall = (operation, values = {}) => remote(configuration, operation, values)
  let agent = null; let agentStartAttempted = false; let disconnected = false
  save()
  try {
    const identity = await attempt('two-distinct-hosts', async () => {
      const local = { ...machineIdentity(), ...serverProcessIdentity(configuration.server) }
      const distant = await deviceCall('identity')
      assert.notEqual(distant.hostIdSha256, local.hostIdSha256, 'SSH target is the Server host')
      assert.equal(distant.sourceCommit, execFileSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim())
      assert.equal(distant.sourceSha256, apiProductionSourceDigest(ROOT), 'two hosts must run the same source snapshot')
      const acceptanceSources = acceptanceSourceDigests(ROOT)
      assert.deepEqual(distant.acceptanceSources, acceptanceSources, 'two hosts must execute the same fault acceptance source')
      report.acceptanceSources = acceptanceSources
      report.acceptanceLogicSha256 = digest(JSON.stringify(acceptanceSources))
      report.topology = 'two-distinct-os-hosts'
      return { serverHost: local, deviceHost: distant }
    })
    if (!identity) return finalizeRemoteFaultReport(report)
    const boundary = await attempt('server-tls-and-auth-boundary', async () => {
      const response = await api.request('/api/v1/clients', { headers: {
        'X-Forwarded-For': '127.0.0.1', 'X-Forwarded-Proto': 'https',
      } })
      assert.ok([401, 403].includes(response.status), 'unauthenticated TLS request must be denied')
      assert.equal(response.tls?.authorized, true)
      const unsignedExchange = { method: 'POST', body: { schemaVersion: SCHEMA,
        clientNodeId: 'cln_01J00000000000000000000000', frames: [], ackSequence: 0 },
      headers: { 'X-Forwarded-For': '127.0.0.1', 'X-Forwarded-Proto': 'https' } }
      const publicDevice = await api.request('/internal/v1/client/exchange', unsignedExchange)
      assert.ok([401, 403].includes(publicDevice.status), 'Device traffic requires its own authenticated identity')
      let backend = null
      let backendDevice = null
      if (configuration.server.tlsMode === 'loopback-proxy') {
        backend = await secureAcceptanceRequest(`${configuration.server.backendUrl}/api/v1/clients`, {
          allowLoopbackHttp: true, origin: configuration.server.origin ?? configuration.server.url,
          headers: { 'X-Forwarded-For': '127.0.0.1', 'X-Forwarded-Proto': 'https' },
        })
        assert.ok([401, 403].includes(backend.status), 'proxy backend must preserve authentication')
        backendDevice = await secureAcceptanceRequest(`${configuration.server.backendUrl}/internal/v1/client/exchange`, {
          ...unsignedExchange, allowLoopbackHttp: true,
        })
        assert.ok([401, 403].includes(backendDevice.status), 'loopback backend must preserve signed Device authentication')
      }
      await api.login()
      return { tls: response.tls, unauthenticatedStatus: response.status,
        unsignedDeviceStatus: publicDevice.status, backendUnauthenticatedStatus: backend?.status ?? null,
        backendUnsignedDeviceStatus: backendDevice?.status ?? null }
    })
    if (!boundary) return finalizeRemoteFaultReport(report)
    const prepared = await deviceCall('prepare', { caPem })
    const binding = await attempt('device-binding', async () => {
      assert.equal(readonly(configuration.server.database, database => database.prepare('SELECT count(*) AS count FROM device_identity_bindings')
        .get().count), 0, 'real fault acceptance requires an isolated Server with no pre-existing Device bindings')
      const dropMarkerCount = () => configuration.server.logFile
        ? readFileSync(configuration.server.logFile, 'utf8').split(BIND_RESPONSE_DROP_MARKER).length - 1 : 0
      const beforeDropMarkers = dropMarkerCount()
      const issued = await api.request('/api/v1/devices/pairing', { method: 'POST', body: { schemaVersion: SCHEMA } })
      assert.equal(issued.status, 201)
      const first = await deviceCall('bind', { serverUrl: configuration.server.url, caPem,
        pairingToken: issued.json.pairingToken, crash: configuration.testSupport === true })
      const serverReceipt = readonly(configuration.server.database, database => database.prepare(`SELECT
        public_key AS publicKey,request_id AS requestId,response_json AS responseJson
        FROM device_identity_bindings WHERE request_id = ?`).get(first.binding.requestId))
      assert.ok(serverReceipt, 'Server must commit the binding through the public TLS endpoint')
      let receipt = first.receipt
      const clientExitCodes = [first.exitCode]
      let interrupted = first
      if (first.exitCode !== 0 && first.exitCode !== 86) {
        assert.equal(first.binding.clientNodeId, null)
        interrupted = await deviceCall('bind', { crash: configuration.testSupport === true })
        clientExitCodes.push(interrupted.exitCode)
        assert.equal(interrupted.binding.requestId, first.binding.requestId)
        receipt = interrupted.receipt
      }
      if (interrupted.exitCode === 86) {
        report.buildFeatures.observedTestSupportHooks.push('WWC_DEVICE_TEST_EXIT_BEFORE_BIND_RECEIPT_SAVE')
        assert.equal(interrupted.binding.clientNodeId, null)
        assert.equal(interrupted.binding.tokenCleared, 0)
        const second = await deviceCall('bind')
        assert.equal(second.exitCode, 0)
        assert.deepEqual(second.receipt, JSON.parse(serverReceipt.responseJson))
        assert.equal(second.binding.requestId, first.binding.requestId)
        assert.equal(second.binding.tokenCleared, 1)
        receipt = second.receipt
        clientExitCodes.push(second.exitCode)
        result('binding-receipt-crash-recovery', 'passed', { exitCode: 86, requestId: first.binding.requestId,
          clientNodeId: receipt.clientNodeId, publicKeySha256: digest(serverReceipt.publicKey),
          cutPoint: 'successful-response-validated-before-local-receipt-save' })
      } else {
        assert.equal(interrupted.exitCode, 0)
        result('binding-receipt-crash-recovery', 'pending', null, 'binary未实际命中test-support绑定崩溃点')
      }
      assert.deepEqual(receipt, JSON.parse(serverReceipt.responseJson))
      if (dropMarkerCount() === beforeDropMarkers + 1) {
        report.buildFeatures.observedTestSupportHooks.push('WWC_TEST_DROP_FIRST_DEVICE_BIND_RESPONSE')
        result('binding-response-loss', 'passed', { requestId: receipt.requestId,
          publicKeySha256: digest(serverReceipt.publicKey), clientExitCodes,
          committedServerReceiptRecovered: true, realTcpBodyInterrupted: true,
          serverMarkerCountBefore: beforeDropMarkers, serverMarkerCountAfter: dropMarkerCount() })
      } else {
        result('binding-response-loss', 'pending', { requestId: receipt.requestId, clientExitCodes },
          '未观察到本次真实TCP响应中断钩子；隔离Server需test-support、WWC_TEST_DROP_FIRST_DEVICE_BIND_RESPONSE=1和logFile')
      }
      return { requestId: receipt.requestId, clientNodeId: receipt.clientNodeId,
        publicClientId: receipt.publicClientId, boundAt: receipt.boundAt, publicKeySha256: digest(serverReceipt.publicKey) }
    })
    if (!binding) return finalizeRemoteFaultReport(report)
    agentStartAttempted = true
    agent = await deviceCall('start')
    const client = await waitFor(async () => {
      const response = await api.request('/api/v1/clients')
      return response.json?.clients?.find(item => item.clientId === binding.publicClientId && item.presence === 'online')
    }, 'bound remote Device online', 60_000)
    report.device = { clientNodeId: binding.clientNodeId, publicClientId: binding.publicClientId,
      repositoryBaseline: prepared.baseline, modelEvidence: 'fixture' }; save()
    await attempt('device-restart', async () => {
      const before = await deviceCall('snapshot'); const previousDevicePid = agent.devicePid
      const restarted = await deviceCall('restart')
      assert.notEqual(restarted.devicePid, agent.devicePid)
      await waitFor(async () => {
        const snapshot = await deviceCall('snapshot')
        if (snapshot.identity.instanceId === before.identity.instanceId) return false
        return readonly(configuration.server.database, database => database.prepare(`SELECT current_instance_id AS instanceId
          FROM client_nodes WHERE client_node_id=?`).get(binding.clientNodeId)?.instanceId === snapshot.identity.instanceId)
      }, 'Device signs in again after restart', 60_000)
      const after = await deviceCall('snapshot')
      assert.equal(after.binding.requestId, before.binding.requestId)
      assert.equal(after.binding.clientNodeId, before.binding.clientNodeId)
      agent = restarted
      return { previousDevicePid,
        restartedDevicePid: restarted.devicePid, previousInstanceId: before.identity.instanceId,
        currentInstanceId: after.identity.instanceId, requestId: after.binding.requestId, clientNodeId: after.binding.clientNodeId }
    })
    const occupied = await api.request('/api/v1/clients/occupancy', { method: 'POST', body: {
      schemaVersion: SCHEMA, clientId: binding.publicClientId,
    } })
    assert.equal(occupied.status, 201)
    const repository = await deviceCall('registered-repository')
    await waitFor(async () => (await api.request(`/api/v1/repositories?clientId=${binding.publicClientId}`))
      .json?.repositories?.some(item => item.repositoryBindingId === repository.repositoryBindingId), 'remote repository projection', 60_000)
    const provider = await seedDeviceLocalProvider({ api, publicClientId: binding.publicClientId,
      providerId: 'remote-fault-fixture', modelId: 'remote-fault-fixture-model', endpoint: agent.modelEndpoint,
      apiKey: randomBytes(32).toString('hex') })
    const modelRoute = configuredDeviceModelRoute(provider)
    const startTask = marker => startRemoteTask(api, binding, repository, modelRoute, marker)
    const completed = await attempt('task-tool-execution', async () => {
      const task = await startTask('native-cell-A')
      const runtime = await waitFor(() => completedTaskRuntime(api, task),
        'remote task durable terminal and matching Worker dispatch authority', 180_000)
      const snapshot = await deviceCall('snapshot')
      const actualProcesses = taskProcessWitnesses(snapshot, task.launched)
      const actualCommand = await waitFor(() => deviceCall('tool-completion-evidence', {
        workerSessionId: task.launched.workerSessionId, jobId: runtime.executionJobId,
      }), 'actual command success in the original Core execution and accepted receipts', 30_000)
      const workerExit = await waitFor(async () => (await deviceCall('snapshot')).workers.find(item =>
        item.workerSessionId === task.launched.workerSessionId && item.state !== 'running'),
      'the completed one-shot Worker exits normally', 30_000)
      assert.equal(workerExit.exitCode, 0)
      return { ...task, executionJobId: runtime.executionJobId, actualProcesses, actualCommand,
        deviceProcessBinding: runtime.deviceProcessBinding, workerExit }
    })
    await attempt('worker-crash-process-closure', async () => {
      const task = await startTask('native-cell-B')
      const before = await waitFor(async () => {
        const snapshot = await deviceCall('snapshot')
        try { toolProcessWitnesses(snapshot, task.launched); return snapshot } catch { return false }
      }, 'real tool and child remain active at the Worker crash cut', 60_000)
      const witnesses = toolProcessWitnesses(before, task.launched)
      const killed = await deviceCall('stop-worker', { launched: task.launched })
      const after = await waitFor(async () => {
        const value = await deviceCall('observe-processes', { witnesses })
        return value.alive.length === 0 ? value : false
      }, 'Worker SIGKILL closes its actual helper, tool and child', 60_000)
      const recovered = await startTask('native-cell-A')
      assert.notEqual(recovered.launched.workerSessionId, task.launched.workerSessionId)
      const runtime = await waitFor(() => completedTaskRuntime(api, recovered),
        'a new actual Worker completes after the active Worker crash', 180_000)
      const actualCommand = await waitFor(() => deviceCall('tool-completion-evidence', {
        workerSessionId: recovered.launched.workerSessionId, jobId: runtime.executionJobId,
      }), 'recovered Worker command success in both original durable Core receipts', 30_000)
      return { crashedWorker: killed.worker, actualProcessesBeforeCrash: witnesses, after: after.alive,
        recoveredWorkerSessionId: recovered.launched.workerSessionId, recoveredExecutionJobId: runtime.executionJobId,
        deviceProcessBinding: runtime.deviceProcessBinding, actualCommand, toolTty: configuration.device.toolTty ?? false }
    })
    await attempt('task-cancel-process-closure', async () => {
      const task = await startTask('native-cell-B')
      const before = await waitFor(async () => {
        const snapshot = await deviceCall('snapshot')
        try { toolProcessWitnesses(snapshot, task.launched); return snapshot } catch { return false }
      }, 'actual helper, tool and child processes', 60_000)
      const witnesses = toolProcessWitnesses(before, task.launched)
      const session = await api.query('session.get', { productSessionId: task.productSessionId })
      await api.command('session.cancel', session.revision, { productSessionId: task.productSessionId,
        reason: 'Remote fixture cancellation and process closure' })
      const after = await waitFor(async () => {
        const observation = await deviceCall('observe-processes', { witnesses })
        return observation.alive.length === 0 ? observation : false
      }, 'cancelled helper, tool and child process identities disappear', 60_000)
      return { ...task, actualProcesses: witnesses, after: after.alive }
    })
    if (completed) {
      report.candidateGeneration = []
      try {
        await runCandidateScenarios({ configuration, api, binding, repository, deviceCall, result, attempt,
          observedHooks: report.buildFeatures.observedTestSupportHooks,
          createCandidate: async () => {
            report.candidateGeneration.push(await createRemoteDeliveryCandidate({ api, binding, repository, modelRoute,
              sourceProductSessionId: completed.productSessionId, baseline: prepared.baseline, deviceCall }))
            save()
          } })
      } catch (error) {
        result('apply-commit-crash-recovery', 'failed', { diagnosticPath: error.diagnosticPath ?? null,
          remoteDiagnosticPath: error.remoteDiagnosticPath ?? null }, `${error.code ?? error.name}: ${error.message.split('\n')[0]}`)
      }
    }
    if (configuration.networkFault) {
      await attempt('network-reconnect-replay', async () => {
        const before = await deviceCall('snapshot')
        await deviceCall('network', { command: configuration.networkFault.disconnect }); disconnected = true
        const offline = await deviceCall('probe-server', { serverUrl: configuration.server.url, caPem })
        assert.equal(offline.reachable, false, 'fault command must actually disconnect Device from Server')
        await deviceCall('restart')
        const queued = await waitFor(async () => {
          const value = await deviceCall('snapshot')
          return value.outbox.some(item => !item.published) ? value : false
        }, 'durable Device frames queued while disconnected', 30_000)
        await deviceCall('network', { command: configuration.networkFault.reconnect }); disconnected = false
        const online = await waitFor(async () => {
          const value = await deviceCall('probe-server', { serverUrl: configuration.server.url, caPem })
          return value.reachable ? value : false
        }, 'Device Server connection restored', 60_000)
        const acceptedFrames = await waitFor(() => readonly(configuration.server.database, database => {
          const pending = queued.outbox.filter(item => !item.published)
          const accepted = pending.map(item => database.prepare(`SELECT message_id AS messageId,
            payload_digest AS payloadDigest,completed FROM client_exchange_frames
            WHERE client_node_id=? AND message_id=?`).get(binding.clientNodeId, item.messageId))
          return accepted.every((item, index) => item?.completed === 1
            && item.payloadDigest === pending[index].payloadDigest) ? accepted : false
        }), 'the exact durable Device frames settle once after reconnection', 60_000)
        const after = await deviceCall('snapshot')
        assert.equal(after.binding.requestId, before.binding.requestId)
        return { disconnected: offline, restored: online, pendingFrames: queued.outbox.filter(item => !item.published),
          acceptedFrames, requestId: after.binding.requestId }
      })
    }
    await attempt('online-revoke-process-closure', async () => {
      const task = await startTask('native-cell-B')
      const before = await waitFor(async () => {
        const value = await deviceCall('snapshot')
        try { toolProcessWitnesses(value, task.launched); return value } catch { return false }
      }, 'remote task descendants before online revoke', 60_000)
      const witnesses = [...before.processes]
      const revoked = await api.request(`/api/v1/devices/${binding.clientNodeId}/revoke`, { method: 'POST', body: { schemaVersion: SCHEMA } })
      assert.equal(revoked.status, 200)
      const after = await waitFor(async () => {
        const value = await deviceCall('snapshot')
        const observed = await deviceCall('observe-processes', { witnesses })
        return value.binding.failureReason && observed.alive.length === 0
          && value.workers.every(item => item.state !== 'running') ? value : false
      }, 'revoked Device permanently fails and closes every owned process', 60_000)
      assert.equal(after.binding.failureReason, 'Device 绑定已吊销；服务已停止。')
      return { clientNodeId: binding.clientNodeId, revoked: true, before: before.processes,
        after: after.processes, authorizationFailure: after.binding.failureReason }
    })
    if (configuration.networkFault) await runOfflineScenarios({ configuration, api, caPem, attempt, result, report })
  } catch (error) {
    report.failure = { code: error.code ?? error.name, message: error.message.split('\n')[0],
      diagnosticPath: error.diagnosticPath ?? null, remoteDiagnosticPath: error.remoteDiagnosticPath ?? null }
  } finally {
    if (disconnected) {
      try { await deviceCall('network', { command: configuration.networkFault.reconnect }) }
      catch { report.cleanupNetwork = 'failed' }
    }
    if (agent !== null || agentStartAttempted) {
      try { await deviceCall('stop'); report.cleanupDevice = 'stopped' }
      catch { report.cleanupDevice = 'failed' }
    }
    finalizeRemoteFaultReport(report); save()
  }
  return report
}

async function runOfflineScenarios({ configuration, api, caPem, attempt, result, report }) {
  const distantConfiguration = { ...configuration, device: { ...configuration.device,
    directory: `${configuration.device.directory}-offline` } }
  const deviceCall = (operation, values = {}) => remote(distantConfiguration, operation, values)
  let started = false; let disconnected = false
  try {
    await deviceCall('prepare', { caPem })
    const issued = await api.request('/api/v1/devices/pairing', { method: 'POST', body: { schemaVersion: SCHEMA } })
    assert.equal(issued.status, 201)
    const bound = await deviceCall('bind', { serverUrl: configuration.server.url, caPem,
      pairingToken: issued.json.pairingToken })
    assert.equal(bound.exitCode, 0)
    const binding = bound.receipt
    const agent = await deviceCall('start'); started = true
    await waitFor(async () => (await api.request('/api/v1/clients')).json?.clients
      ?.some(item => item.clientId === binding.publicClientId && item.presence === 'online'), 'second bound Device online', 60_000)
    const occupied = await api.request('/api/v1/clients/occupancy', { method: 'POST', body: {
      schemaVersion: SCHEMA, clientId: binding.publicClientId,
    } })
    assert.equal(occupied.status, 201)
    const repository = await deviceCall('registered-repository')
    await waitFor(async () => (await api.request(`/api/v1/repositories?clientId=${binding.publicClientId}`))
      .json?.repositories?.some(item => item.repositoryBindingId === repository.repositoryBindingId), 'second Device repository', 60_000)
    const provider = await seedDeviceLocalProvider({ api, publicClientId: binding.publicClientId,
      providerId: 'offline-fault-fixture', modelId: 'offline-fault-fixture-model', endpoint: agent.modelEndpoint,
      apiKey: randomBytes(32).toString('hex') })
    const task = await startRemoteTask(api, binding, repository, configuredDeviceModelRoute(provider), 'native-cell-B')
    const witnesses = await waitFor(async () => {
      try { return toolProcessWitnesses(await deviceCall('snapshot'), task.launched) } catch { return false }
    }, 'actual offline-test helper, tool and child processes', 60_000)
    await deviceCall('network', { command: configuration.networkFault.disconnect }); disconnected = true
    const offline = await deviceCall('probe-server', { serverUrl: configuration.server.url, caPem })
    assert.equal(offline.reachable, false)
    const lease = readonly(configuration.server.database, database => database.prepare(`SELECT
      lease_id AS leaseId,job_id AS jobId,expires_at AS expiresAt FROM execution_leases
      WHERE worker_id=? AND worker_instance_id=?`).get(task.launched.workerId, task.launched.workerInstanceId))
    assert.ok(lease && Number.isFinite(Date.parse(lease.expiresAt)))
    const waitMillis = Math.max(0, Date.parse(lease.expiresAt) - Date.now()) + 30_000
    if (waitMillis > (configuration.maxOfflineWaitMillis ?? 120_000)) {
      result('offline-lease-expiry-process-closure', 'pending', { lease, actualProcesses: witnesses },
        '实际租约超出本次等待预算；缩短Server执行租约或提高maxOfflineWaitMillis后重跑')
    } else {
      await attempt('offline-lease-expiry-process-closure', async () => {
        const observed = await waitFor(async () => {
          if (Date.now() < Date.parse(lease.expiresAt)) return false
          const value = await deviceCall('observe-processes', { witnesses })
          return value.alive.length === 0 ? value : false
        }, 'offline execution lease expires and helper/tool/child really exit', waitMillis, 1_000)
        const stillOffline = await deviceCall('probe-server', { serverUrl: configuration.server.url, caPem })
        assert.equal(stillOffline.reachable, false)
        const daemon = (await deviceCall('snapshot')).agent
        assert.equal(daemon.active, true, 'fixture lifetime cleanup must not stand in for lease enforcement')
        assert.equal(daemon.deviceRunning, true, 'Device daemon shutdown must not stand in for lease enforcement')
        const failure = await waitFor(async () => {
          const value = await deviceCall('worker-expiry-diagnostic', { workerSessionId: task.launched.workerSessionId })
          return value.authorityExpired ? value : false
        }, 'the actual Worker records AuthorityExpired', 15_000)
        return { ...task, lease, expiredAtObserved: new Date().toISOString(), actualProcesses: witnesses,
          after: observed.alive, disconnected: stillOffline, devicePid: daemon.devicePid, deviceStillRunning: true,
          workerFailure: failure }
      })
    }
    await attempt('offline-revoke-reconnect-failure', async () => {
      const before = await deviceCall('snapshot')
      assert.equal(before.binding.failureReason, null, 'an offline Device has not yet received revocation')
      const revoked = await api.request(`/api/v1/devices/${binding.clientNodeId}/revoke`, {
        method: 'POST', body: { schemaVersion: SCHEMA },
      })
      assert.equal(revoked.status, 200)
      assert.equal((await deviceCall('probe-server', { serverUrl: configuration.server.url, caPem })).reachable, false)
      await deviceCall('network', { command: configuration.networkFault.reconnect }); disconnected = false
      const after = await waitFor(async () => {
        const value = await deviceCall('snapshot')
        const processes = await deviceCall('observe-processes', { witnesses: before.processes })
        return value.binding.failureReason && processes.alive.length === 0
          && value.workers.every(item => item.state !== 'running') ? value : false
      }, 'offline revoked Device reconnects, fails explicitly and closes owned processes', 60_000)
      assert.equal(after.binding.failureReason, 'Device 绑定已吊销；服务已停止。')
      const retry = await deviceCall('bind')
      assert.notEqual(retry.exitCode, 0, 'revoked original binding cannot be recovered into an authorized identity')
      return { clientNodeId: binding.clientNodeId, failureReason: after.binding.failureReason,
        bindRetryExitCode: retry.exitCode, before: before.processes, after: after.processes }
    })
  } finally {
    if (disconnected) {
      try { await deviceCall('network', { command: configuration.networkFault.reconnect }) }
      catch { report.cleanupNetwork = 'failed' }
    }
    if (started) {
      try { await deviceCall('stop') }
      catch { report.cleanupDevice = 'failed' }
    }
  }
}

async function runCandidateScenarios({ configuration, api, binding, repository, deviceCall, result, attempt, createCandidate, observedHooks }) {
  const list = async () => {
    const response = await api.request(`/api/v1/clients/${binding.publicClientId}/candidates`)
    assert.equal(response.status, 200)
    return response.json.candidates.filter(item => item.repositoryBindingId === repository.repositoryBindingId)
  }
  let candidates = await waitFor(async () => {
    const values = await list()
    return values.length ? values : false
  }, 'real Worker frozen candidate retained by Device', 30_000).catch(() => null)
  if (!candidates && configuration.testSupport === true) {
    await createCandidate()
    candidates = await waitFor(async () => {
      const values = await list()
      return values.length ? values : false
    }, 'the actual StrongFlow fixture publishes its frozen candidate', 30_000).catch(() => null)
  }
  if (!candidates) {
    for (const id of ['apply-commit-crash-recovery', 'apply-current-epoch-reconciliation',
      'baseline-stale', 'application-outcome-unknown-blocked']) {
      result(id, 'pending', null, '真实工具任务尚未产生Device上报的合法冻结候选')
    }
    return
  }
  if (configuration.testSupport !== true) {
    result('apply-commit-crash-recovery', 'pending', { candidateRef: candidates[0].candidateRef },
      '需要明确启用test-support的Device构建和真实进程崩溃点')
    return
  }
  for (let index = 0; candidates.length < 3 && index < 3; index += 1) {
    const previousCount = candidates.length
    await createCandidate()
    candidates = await waitFor(async () => {
      const values = await list()
      return values.length > previousCount ? values : false
    }, 'an additional independently frozen Worker candidate', 30_000)
  }
  assert.ok(candidates.length >= 3, 'three actual frozen candidates are required for independent application faults')
  const [applied, stale, unknown] = candidates
  assert.equal(stale.baseCommit, applied.baseCommit)
  assert.equal(unknown.baseCommit, applied.baseCommit)
  const branchFixture = await deviceCall('prepare-application-branches', { baseCommit: applied.baseCommit })
  const apply = (candidate, targetBranch) => api.request('/api/v1/clients/candidates/apply', {
    method: 'POST', timeoutMillis: 45_000, body: { schemaVersion: SCHEMA,
      clientId: binding.publicClientId, candidateRef: candidate.candidateRef,
      repositoryBindingId: repository.repositoryBindingId, targetBranch, expectedHead: candidate.baseCommit,
    },
  })
  const crash = async (candidate, point, targetBranch) => {
    await deviceCall('restart', { environment: { WWC_TEST_APPLICATION_CRASH_POINT: point } })
    const request = apply(candidate, targetBranch).catch(error => ({ status: null, code: error.code }))
    const snapshot = await waitFor(async () => {
      const value = await deviceCall('snapshot')
      return value.agent?.deviceExitCode === 86 && value.applications.some(item => item.candidateRef === candidate.candidateRef
        && item.finishedReceiptJson === null) ? value : false
    }, `actual Device process exits after ${point}`, 60_000)
    await request
    const application = snapshot.applications.find(item => item.candidateRef === candidate.candidateRef)
    assert.ok(application.applicationId && application.approvalId)
    const observed = await deviceCall('application-crash-evidence', { point, applicationId: application.applicationId })
    assert.equal(observed.markerObserved, true, 'the actual test-support crash marker must bind this application')
    observedHooks.push(`WWC_TEST_APPLICATION_CRASH_POINT:${point}`)
    return { application, snapshot }
  }
  const renewOccupancy = async () => {
    const released = await api.request('/api/v1/clients/occupancy/force-release', {
      method: 'POST', body: { schemaVersion: SCHEMA, clientId: binding.publicClientId },
    })
    assert.equal(released.status, 200)
    await deviceCall('restart')
    await waitFor(async () => {
      const response = await api.request(`/api/v1/clients/${binding.publicClientId}/occupancy`)
      return response.status === 200 && response.json.occupancy === 'available'
    }, 'a current Device occupancy can be claimed', 60_000)
    const claimed = await api.request('/api/v1/clients/occupancy', { method: 'POST', body: {
      schemaVersion: SCHEMA, clientId: binding.publicClientId,
    } })
    assert.equal(claimed.status, 201)
    return (await deviceCall('snapshot')).mirror
  }
  const recovered = await attempt('apply-commit-crash-recovery', async () => {
    const interrupted = await crash(applied, 'after_repository_commit', 'wwc-acceptance-applied')
    const before = await deviceCall('repository-state')
    const commits = before.applicationCommits.filter(item => item.applicationIds.includes(interrupted.application.applicationId))
    assert.equal(commits.length, 1, 'the real Git commit must exist before the local receipt')
    const mirror = await renewOccupancy()
    assert.ok(mirror.epoch > interrupted.application.originalEpoch)
    await apply(applied, 'wwc-acceptance-applied')
    const completed = await waitFor(async () => {
      const values = await list()
      return values.find(item => item.candidateRef === applied.candidateRef && item.application?.status === 'applied')
    }, 'current Device reports the original application result', 60_000)
    const after = await deviceCall('repository-state')
    const finalCommits = after.applicationCommits.filter(item => item.applicationIds.includes(interrupted.application.applicationId))
    assert.deepEqual(finalCommits, commits, 'recovery must preserve exactly one application commit')
    assert.equal(completed.application.applicationId, interrupted.application.applicationId)
    assert.equal(after.branches['wwc-acceptance-applied'], commits[0].commit)
    return { applicationId: interrupted.application.applicationId, originalEpoch: interrupted.application.originalEpoch,
      currentEpoch: mirror.epoch, exitCode: 86, candidateRef: applied.candidateRef, resultingCommit: commits[0].commit,
      commitCountBeforeRecovery: commits.length, commitCountAfterRecovery: finalCommits.length }
  })
  if (recovered) await attempt('apply-current-epoch-reconciliation', async () => {
    const events = await waitFor(() => readonly(configuration.server.database, database => {
      const rows = database.prepare(`SELECT kind,epoch FROM candidate_application_events WHERE application_id = ?`)
        .all(recovered.applicationId)
      return rows.some(item => item.kind === 'application.reconciled' && item.epoch === recovered.currentEpoch) ? rows : false
    }), 'Server persists reconciliation at the current occupancy epoch', 30_000)
    return { applicationId: recovered.applicationId, originalEpoch: recovered.originalEpoch,
      currentEpoch: recovered.currentEpoch, events }
  })
  await attempt('baseline-stale', async () => {
    await deviceCall('restart')
    const before = await deviceCall('repository-state')
    assert.equal(before.branches['wwc-acceptance-stale'], branchFixture.movedHead)
    assert.notEqual(before.branches['wwc-acceptance-stale'], stale.baseCommit)
    const response = await apply(stale, 'wwc-acceptance-stale')
    assert.equal(response.status, 201)
    assert.equal(response.json.receipt.result, 'base_stale')
    const after = await deviceCall('repository-state')
    assert.deepEqual(after.branches, before.branches, 'stale approval must not write any target branch')
    return { candidateRef: stale.candidateRef, approvedBaseline: stale.baseCommit,
      actualHead: before.branches['wwc-acceptance-stale'], result: 'base_stale', branchesStable: true }
  })
  await attempt('application-outcome-unknown-blocked', async () => {
    const interrupted = await crash(unknown, 'after_started', 'wwc-acceptance-unknown')
    await deviceCall('make-unknown-outcome')
    try {
      const mirror = await renewOccupancy()
      await apply(unknown, 'wwc-acceptance-unknown')
      const blocked = await waitFor(async () => (await list()).find(item => item.candidateRef === unknown.candidateRef
        && item.application?.status === 'blocked'), 'unknown application remains visibly blocked', 60_000)
      const before = await deviceCall('repository-state')
      const repeated = await apply(unknown, 'wwc-acceptance-unknown')
      assert.equal(repeated.status, 409)
      const after = await deviceCall('repository-state')
      assert.deepEqual(after.branches, before.branches)
      assert.equal(after.applicationCommits.some(item => item.applicationIds.includes(interrupted.application.applicationId)), false)
      return { applicationId: blocked.application.applicationId, originalEpoch: interrupted.application.originalEpoch,
        currentEpoch: mirror.epoch, repeatedApplyStatus: repeated.status, status: blocked.application.status,
        branchesStable: true, applicationCommitCount: 0 }
    } finally { await deviceCall('clear-unknown-fixture') }
  })
}

async function main(argv) {
  if (argv[0] === '--device-agent') { await deviceAgent(argv[1]); return }
  if (argv[0] === '--device-operation') {
    const chunks = []
    for await (const chunk of process.stdin) chunks.push(chunk)
    const payload = JSON.parse(Buffer.concat(chunks).toString('utf8'))
    try { process.stdout.write(`${JSON.stringify({ operation: payload.operation, ok: true, evidence: await deviceOperation(payload) })}\n`) }
    catch (error) {
      const marker = join(payload.device.directory, 'acceptance-owner.json')
      let diagnosticPath = null
      if (existsSync(marker) && JSON.parse(readFileSync(marker, 'utf8')).runId === payload.runId) {
        diagnosticPath = join(payload.device.directory, 'diagnostics', `operation-${Date.now()}.json`)
        writeJson(diagnosticPath, { code: error.code ?? error.name, error: error.stack ?? error.message })
      }
      process.stdout.write(`${JSON.stringify({ operation: payload.operation, ok: false,
        code: error.code ?? error.name, diagnosticPath })}\n`)
      process.exitCode = 1
    }
    return
  }
  const options = new Map()
  for (let index = 0; index < argv.length; index += 1) {
    const name = argv[index]
    assert.ok(['--output', '--config', '--plan'].includes(name), `unknown argument ${name}`)
    assert.equal(options.has(name), false, `repeated argument ${name}`)
    if (name === '--plan') options.set(name, true)
    else {
      assert.ok(argv[index + 1] && !argv[index + 1].startsWith('--'), `${name} requires a value`)
      options.set(name, argv[++index])
    }
  }
  assert.ok(options.has('--output'), 'Usage: --config PRIVATE_CONFIG --output REPORT [--plan]')
  const output = resolve(options.get('--output'))
  if (options.has('--plan')) { writeJson(output, finalizeRemoteFaultReport(emptyRemoteFaultReport())); process.exitCode = 2; return }
  assert.ok(options.has('--config'), '--config is required for real two-machine execution')
  const report = await runRemoteDeviceFaultAcceptance(readPrivateJson(resolve(options.get('--config'))), output)
  process.stdout.write(`${JSON.stringify({ status: report.status, report: output })}\n`)
  process.exitCode = report.status === 'passed' ? 0 : report.status === 'failed' ? 1 : 2
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch(error => {
    const index = process.argv.indexOf('--output')
    if (index !== -1 && process.argv[index + 1]) {
      const output = resolve(process.argv[index + 1])
      const diagnosticPath = join(dirname(output), `${basename(output)}-diagnostics`, 'startup.json')
      writeJson(diagnosticPath, { code: error.code ?? error.name, error: error.stack ?? error.message })
      const report = emptyRemoteFaultReport()
      report.failure = { code: error.code ?? error.name, message: '验收未能启动；详细原因保留在私有诊断中', diagnosticPath }
      writeJson(output, finalizeRemoteFaultReport(report))
    }
    process.stderr.write('Remote acceptance could not start; inspect the private configuration and report.\n')
    process.exitCode = 1
  })
}
