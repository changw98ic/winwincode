// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync, spawn, spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { basename, join, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import vm from 'node:vm'

import { createCertificate, waitForDeviceWorkerRegistered } from '../scripts/run-api-production-vertical.mjs'
import { waitFor } from '../scripts/device-production-fixture.mjs'
import { REMOTE_FAULT_SCENARIOS, RemoteApi, assertRevokedDeviceExecutionResources, deviceExecutionResources, deviceSnapshot, emptyRemoteFaultReport, finalizeRemoteFaultReport,
  fixtureCommandCell, observeProcessWitnesses, readFixtureCommandEvidence, remoteOperationCommand, runCandidateScenarios, secureAcceptanceRequest, taskProcessWitnesses,
  validateRemoteFaultConfiguration } from '../scripts/run-remote-device-fault-acceptance.mjs'

function configuration() {
  return { schemaVersion: 1, server: { url: 'https://server.example.invalid:8443', tlsMode: 'direct',
    database: '/private/server/control-plane.sqlite3', binary: '/private/bin/winwincode-server', pid: 123,
    username: 'fixture-owner',
    passwordEnvironment: 'WWC_ACCEPTANCE_OWNER_PASSWORD' },
  device: { sshHost: 'acceptance@device.example.invalid', checkout: '/opt/winwincode',
    binaryDirectory: '/opt/winwincode-bin', directory: '/private/wwc-acceptance-fixture' } }
}

function candidateScenarioFixture(t, applyFailure = null) {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-application-'))
  const database = new DatabaseSync(join(directory, 'control-plane.sqlite3'))
  database.exec(`CREATE TABLE client_nodes(client_node_id,current_instance_id,presence_state);
    CREATE TABLE client_occupancy_leases(client_node_id,occupancy_lease_id,fencing_token,holder_user_id,state);
    CREATE TABLE candidate_application_events(application_id,kind,epoch);
    INSERT INTO client_nodes VALUES('device','instance-0','online');
    INSERT INTO client_occupancy_leases VALUES('device','lease-1',1,'owner','occupied');`)
  const revisions = new DatabaseSync(join(directory, 'client-occupancy-mirror.sqlite3'))
  revisions.exec(`CREATE TABLE client_occupancy_mirror_revisions(client_node_id,mirror_revision);
    INSERT INTO client_occupancy_mirror_revisions VALUES('device',1);`)
  t.after(() => { database.close(); revisions.close(); rmSync(directory, { recursive: true, force: true }) })
  const candidates = ['applied', 'stale', 'unknown'].map(name => ({ candidateRef: name,
    repositoryBindingId: 'repository', baseCommit: 'a'.repeat(40) }))
  const branches = { 'wwc-acceptance-applied': 'a'.repeat(40), 'wwc-acceptance-stale': 'b'.repeat(40),
    'wwc-acceptance-unknown': 'a'.repeat(40) }
  const applications = []; const results = []; const sends = []; const restarts = []; const releases = []
  let instanceId = 'instance-0'; let epoch = 1; let revision = 1; let running = true
  let mirror = { leaseId: 'lease-1', epoch, holderUserId: 'owner', revision }
  let transition = null; let crashPoint = null; let dirty = false; let prematureApply = false
  const deviceCall = async (operation, values = {}) => {
    if (operation === 'restart') {
      transition = { previous: instanceId, next: `instance-${restarts.length + 1}`, checks: 0 }
      restarts.push(transition); running = true; revision += 1
      crashPoint = values.environment?.WWC_TEST_APPLICATION_CRASH_POINT
      return { deviceRunning: true }
    }
    if (operation === 'snapshot') {
      assert.equal(prematureApply, false, 'apply issued before readiness')
      if (transition && transition.checks < 5) {
        transition.checks += 1
        if (transition.checks >= 2) { instanceId = transition.next; mirror = null }
        if (transition.checks >= 3) database.prepare('UPDATE client_nodes SET current_instance_id=?').run(instanceId)
        if (transition.checks >= 4) mirror = { leaseId: `lease-${epoch}`, epoch, holderUserId: 'owner', revision }
        if (transition.checks >= 5) revisions.prepare('UPDATE client_occupancy_mirror_revisions SET mirror_revision=?').run(revision)
      }
      return { identity: { instanceId }, mirror, applications,
        agent: { deviceRunning: running, deviceExitCode: running ? null : 86 } }
    }
    if (operation === 'prepare-application-branches') return { movedHead: branches['wwc-acceptance-stale'] }
    if (operation === 'repository-state') return { branches: { ...branches },
      applicationCommits: applications.filter(item => item.candidateRef === 'applied')
        .map(item => ({ commit: 'c'.repeat(40), applicationIds: [item.applicationId] })) }
    if (operation === 'application-crash-evidence') return { markerObserved: true }
    if (operation === 'make-unknown-outcome') { dirty = true; return {} }
    if (operation === 'clear-unknown-fixture') { dirty = false; return {} }
    assert.fail(`unexpected Device operation ${operation}`)
  }
  const api = { request: async (path, options = {}) => {
    if (path.endsWith('/candidates')) return { status: 200, json: { candidates } }
    if (path === '/api/v1/clients/occupancy/force-release') {
      return { status: 409, json: { error: { code: 'WRONG_STATE', message: 'only a recovery-pending lease can be force-released' } } }
    }
    if (path === '/api/v1/clients/device-public/occupancy') return { status: 200, json: { occupancy:
      database.prepare("SELECT 1 FROM client_occupancy_leases WHERE state='occupied'").get() ? 'occupied' : 'available' } }
    if (path === '/api/v1/clients/occupancy') {
      assert.equal(database.prepare('SELECT current_instance_id AS instanceId FROM client_nodes').get().instanceId, instanceId)
      assert.equal(transition.checks, 5, 'occupancy operations must wait for the current Device authority')
      if (options.method === 'DELETE') {
        assert.equal(options.body.mode, 'release')
        assert.ok(running && mirror, 'restart the crashed Device before releasing its occupancy')
        database.prepare("UPDATE client_occupancy_leases SET state='released'").run()
        releases.push({ instanceId, epoch, revision })
        return { status: 200, json: { occupancy: 'released' } }
      }
      assert.equal(options.method, 'POST')
      assert.equal(releases.at(-1).epoch, epoch, 'claim must follow release of the previous occupancy')
      assert.equal(mirror.epoch, epoch, 'normal release retains the Device mirror until the next offer')
      epoch += 1
      revision += 1
      mirror = { leaseId: `lease-${epoch}`, epoch, holderUserId: 'owner', revision }
      revisions.prepare('UPDATE client_occupancy_mirror_revisions SET mirror_revision=?').run(revision)
      database.prepare('INSERT INTO client_occupancy_leases VALUES(?,?,?,?,?)').run('device', `lease-${epoch}`, epoch, 'owner', 'occupied')
      if (dirty) candidates[2].application = { status: 'blocked', applicationId: 'application-unknown' }
      else database.prepare('INSERT INTO candidate_application_events VALUES(?,?,?)')
        .run('application-applied', 'application.reconciled', epoch)
      return { status: 201 }
    }
    if (path === '/api/v1/clients/candidates/apply') {
      const serverInstance = database.prepare('SELECT current_instance_id AS instanceId FROM client_nodes').get().instanceId
      const serverRevision = revisions.prepare('SELECT mirror_revision AS revision FROM client_occupancy_mirror_revisions').get().revision
      prematureApply = serverInstance !== instanceId || instanceId !== transition?.next
        || !mirror || mirror.epoch !== epoch || mirror.revision !== serverRevision
      if (prematureApply) throw new Error('apply issued before readiness')
      sends.push({ candidateRef: options.body.candidateRef, instanceId, checks: transition.checks })
      if (applyFailure) return applyFailure
      const candidate = candidates.find(item => item.candidateRef === options.body.candidateRef)
      if (crashPoint && !applications.some(item => item.candidateRef === candidate.candidateRef)) {
        applications.push({ candidateRef: candidate.candidateRef, applicationId: `application-${candidate.candidateRef}`,
          approvalId: 'approval', originalEpoch: epoch, finishedReceiptJson: null })
        if (candidate.candidateRef === 'applied') branches['wwc-acceptance-applied'] = 'c'.repeat(40)
        running = false
        return { status: 504, json: { error: { code: 'APPLY_RESULT_TIMEOUT' } } }
      }
      if (candidate.candidateRef === 'unknown') return { status: 409, json: { error: { code: 'WRONG_STATE' } } }
      if (candidate.candidateRef === 'applied') candidate.application = { status: 'applied', applicationId: 'application-applied' }
      return { status: 201, json: { receipt: { result: 'base_stale' } } }
    }
    assert.fail(`unexpected API request ${path}`)
  } }
  return { api, restarts, sends, results, releases, run: () => runCandidateScenarios({
    configuration: { ...configuration(), testSupport: true, server: { database: join(directory, 'control-plane.sqlite3') } },
    api, binding: { clientNodeId: 'device', publicClientId: 'device-public' }, repository: { repositoryBindingId: 'repository' },
    deviceCall, observedHooks: [], result: (...args) => results.push(args),
    attempt: async (id, run) => { const evidence = await run(); results.push({ id, evidence }); return evidence },
    createCandidate: () => assert.fail('the fixture already has three frozen candidates'),
  }) }
}

test('application faults wait for each new Device instance and matching occupancy before apply', async t => {
  const fixture = candidateScenarioFixture(t)
  await fixture.run()
  assert.equal(fixture.restarts.length, 5)
  assert.equal(fixture.releases.length, 2)
  assert.deepEqual(fixture.releases.map(item => item.epoch), [1, 2])
  assert.deepEqual(fixture.sends.map(item => item.candidateRef), ['applied', 'applied', 'stale', 'unknown', 'unknown', 'unknown'])
  assert.ok(fixture.sends.every(item => item.checks >= 5))
  assert.deepEqual(fixture.results.map(item => item.id), ['apply-commit-crash-recovery', 'apply-current-epoch-reconciliation',
    'baseline-stale', 'application-outcome-unknown-blocked'])
})

test('application faults retain an unexpected API response instead of waiting for a crash', async t => {
  const response = { status: 504, json: { error: { code: 'UNEXPECTED_TIMEOUT', message: 'fixture response diagnostic' } } }
  const fixture = candidateScenarioFixture(t, response)
  await assert.rejects(fixture.run(), error => error.status === response.status
    && JSON.stringify(error.response) === JSON.stringify(response.json))
})

test('baseline application retains a 504 response as a failure', async t => {
  const fixture = candidateScenarioFixture(t)
  const request = fixture.api.request
  const response = { status: 504, json: { error: { code: 'APPLY_RESULT_TIMEOUT', message: 'baseline diagnostic' } } }
  fixture.api.request = (path, options) => path === '/api/v1/clients/candidates/apply' && options.body.candidateRef === 'stale'
    ? response : request(path, options)
  await assert.rejects(fixture.run(), error => error.status === response.status
    && JSON.stringify(error.response) === JSON.stringify(response.json))
})

test('application recovery retains occupancy release and claim errors', async t => {
  for (const method of ['DELETE', 'POST']) {
    const fixture = candidateScenarioFixture(t)
    const request = fixture.api.request
    const response = { status: 409, json: { error: { code: 'WRONG_STATE', message: `${method} occupancy diagnostic` } } }
    fixture.api.request = (path, options) => path === '/api/v1/clients/occupancy' && options.method === method
      ? response : request(path, options)
    await assert.rejects(fixture.run(), error => error.status === response.status
      && JSON.stringify(error.response) === JSON.stringify(response.json))
  }
})

test('Worker registration retries only the original RESOURCE_NOT_FOUND response', async () => {
  const api = new RemoteApi(configuration().server, 'fixture-password')
  let attempts = 0
  api.request = async (_path, { body }) => {
    assert.deepEqual(body.page, { cursor: null, limit: 1 })
    attempts += 1
    return attempts === 1
      ? { status: 404, json: { error: { code: 'RESOURCE_NOT_FOUND' } } }
      : { status: 200, json: { requestId: body.requestId, result: {
        id: 'fixture-worker', state: 'enabled', lastHeartbeatAt: '2026-10-15T00:00:00Z',
      } } }
  }
  const client = { query: async (...args) => ({ result: await api.query(...args) }) }
  const worker = await waitForDeviceWorkerRegistered(client, { workerId: 'fixture-worker' }, 5000)
  assert.equal(worker.id, 'fixture-worker')
  assert.equal(attempts, 2)
  api.request = async () => ({ status: 403, json: { error: { code: 'PERMISSION_DENIED' } } })
  await assert.rejects(waitForDeviceWorkerRegistered(client, { workerId: 'fixture-worker' }, 5000),
    { code: 'PERMISSION_DENIED', status: 403 })
  await assert.rejects(api.command('session.cancel', 0, {}), { code: 'PERMISSION_DENIED', status: 403 })
})

test('Device outbox evidence compares the message digest and retains the exact envelope digest', () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-outbox-'))
  mkdirSync(join(directory, 'device-data'))
  const database = new DatabaseSync(join(directory, 'device-data/device-client.sqlite3'))
  database.exec(`CREATE TABLE remote_device_binding(request_id,client_node_id,public_client_id,bound_at,failure_reason,pairing_token);
    CREATE TABLE worker_process_registry(worker_session_id,worker_id,worker_instance_id,pid,process_start_identity,state,exit_code);
    CREATE TABLE occupancy_mirror(occupancy_lease_id,fencing_token,holder_user_id,mirror_revision);
    CREATE TABLE device_identity(current_instance_id);
    CREATE TABLE client_outbox(message_id,envelope_sequence,kind,payload,published);
    CREATE TABLE candidate_applications(application_id,approval_id,candidate_ref,base_commit,original_epoch,finished_receipt_json,recovery_outcome);`)
  const message = { kind: 'hello', payload: { clientNodeId: 'fixture-device', displayName: '验收 Device' } }
  const envelope = JSON.stringify({ schemaVersion: 'winwincode/v1', messageId: 'fixture-frame', sequence: 24, ...message })
  const sha256 = bytes => `sha256:${createHash('sha256').update(bytes).digest('hex')}`
  database.prepare('INSERT INTO client_outbox VALUES(?,?,?,?,?)').run('fixture-frame', 24, 'hello', Buffer.from(envelope), 0)
  database.close()
  try {
    const [frame] = deviceSnapshot({ directory }).outbox
    assert.equal(frame.payloadDigest, sha256(JSON.stringify(message)))
    assert.equal(frame.envelopeDigest, sha256(envelope))
    assert.notEqual(frame.payloadDigest, frame.envelopeDigest)
    assert.equal(frame.published, 0)
  } finally { rmSync(directory, { recursive: true, force: true }) }
})

test('revocation requires Server resources to close and keeps interrupted usage unknown', () => {
  const lease = { jobId: 'job-fixture', leaseId: 'lease-fixture', workerId: 'worker-fixture',
    workerInstanceId: 'instance-fixture', attempt: 1, fencingToken: 'fence-fixture', jobState: 'running',
    leaseOutcome: null, slotState: 'running', admissionState: 'running', actualTokens: null, actualCostMicrounits: null }
  const before = { leases: [lease], workers: [{ workerId: lease.workerId, workerInstanceId: lease.workerInstanceId,
    health: 'healthy' }], slots: [] }
  const after = { leases: [{ ...lease, jobState: 'failed', leaseOutcome: 'cancelled', slotState: 'recovery_failed',
    admissionState: 'released', releaseReason: 'cancelled', accountingStatus: 'pending',
    terminalUsage: { accountingStatus: 'unknown', tokens: null, costMicrounits: null, knownTokens: 2 } }],
  workers: [{ ...before.workers[0], health: 'timed_out' }], slots: [] }
  assert.throws(() => assertRevokedDeviceExecutionResources(before, before), /close the execution job/u)
  assert.doesNotThrow(() => assertRevokedDeviceExecutionResources(before, after))
  for (const mutate of [
    value => { value.leases = [] },
    value => { value.leases[0].slotState = 'running' },
    value => { value.leases[0].admissionState = 'running' },
    value => { value.leases[0].admissionState = null; value.leases[0].accountingStatus = null; value.leases[0].terminalUsage = null },
    value => { value.leases[0].terminalUsage.tokens = 0 },
    value => { value.workers[0].health = 'healthy' },
    value => { value.slots.push({ workerSessionId: 'old-session', leaseId: 'old-lease', attempt: 1, state: 'running' }) },
  ]) {
    const changed = structuredClone(after); mutate(changed)
    assert.throws(() => assertRevokedDeviceExecutionResources(before, changed))
  }
  const unreserved = { ...lease, admissionState: null }
  const releasedUnreserved = { ...after.leases[0], admissionState: null, releaseReason: null,
    accountingStatus: null, terminalUsage: null }
  assert.doesNotThrow(() => assertRevokedDeviceExecutionResources({ ...before, leases: [unreserved] },
    { ...after, leases: [releasedUnreserved] }))
  const completed = { ...lease, leaseId: 'completed-lease', jobId: 'completed-job', jobState: 'completed',
    leaseOutcome: 'completed', slotState: 'completed', admissionState: 'settled', actualTokens: 7,
    accountingStatus: 'pending', terminalUsage: { accountingStatus: 'known', tokens: 7, costMicrounits: null } }
  before.leases.push(completed); after.leases.push(structuredClone(completed))
  assert.doesNotThrow(() => assertRevokedDeviceExecutionResources(before, after))
  after.leases[1].accountingStatus = 'settled'
  assert.doesNotThrow(() => assertRevokedDeviceExecutionResources(before, after))
  after.leases[1].terminalUsage = { accountingStatus: 'unknown', tokens: null, costMicrounits: null }
  assert.throws(() => assertRevokedDeviceExecutionResources(before, after))
  after.leases[1].terminalUsage = structuredClone(completed.terminalUsage)
  after.leases[1].actualTokens = 0
  assert.throws(() => assertRevokedDeviceExecutionResources(before, after))
  const failed = { ...completed, jobState: 'failed', leaseOutcome: 'failed', slotState: 'failed' }
  assert.throws(() => assertRevokedDeviceExecutionResources({ ...before, leases: [failed] },
    { ...after, leases: [{ ...failed, leaseOutcome: 'cancelled' }] }))
})

test('revocation snapshots retain prepared admissions with no execution lease', t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-revoke-prepared-'))
  const path = join(directory, 'control-plane.sqlite3')
  const database = new DatabaseSync(path)
  t.after(() => { database.close(); rmSync(directory, { recursive: true, force: true }) })
  database.exec(`CREATE TABLE execution_leases(job_id,lease_id,worker_id,worker_instance_id,attempt,fencing_token);
    CREATE TABLE scheduler_execution_jobs(job_id,state,product_session_id,work_run_id);
    CREATE TABLE execution_lease_terminals(lease_id,outcome,terminal_at);
    CREATE TABLE worker_session_slots(worker_session_id,job_id,lease_id,worker_id,worker_instance_id,attempt,fencing_token,state,terminal_at);
    CREATE TABLE execution_admission_reservations(job_id,worker_pool_id,product_session_id,submitted_at,state,release_reason,actual_tokens,actual_cost_microunits);
    CREATE TABLE execution_admission_reconciliation(job_id,accounting_status,terminal_usage_json);
    CREATE TABLE worker_launch_grants(worker_launch_grant_id,client_node_id,worker_id,worker_instance_id,product_session_id,work_run_id,created_at);
    CREATE TABLE execution_workers(worker_id,worker_instance_id,health);
    CREATE TABLE device_execution_current_facts(job_id,client_node_id);
    CREATE TABLE device_execution_bindings(client_node_id,worker_launch_grant_id,product_session_id,work_run_id);
    INSERT INTO scheduler_execution_jobs VALUES('queued','queued','session-1',NULL),('started','queued','session-2',NULL),('other','queued','session-3',NULL),('before-facts','queued','session-4',NULL),('successor','queued','session-5',NULL);
    INSERT INTO execution_admission_reservations VALUES('queued','pool','session-1','submitted','queued',NULL,NULL,NULL),('started','pool','session-2','submitted','running',NULL,NULL,NULL),('other','pool','session-3','submitted','running',NULL,NULL,NULL),('before-facts','pool','session-4','submitted','queued',NULL,NULL,NULL),('successor','pool','session-5','submitted','running',NULL,NULL,NULL);
    INSERT INTO device_execution_current_facts VALUES('queued','device'),('started','device'),('other','other-device');
    INSERT INTO worker_launch_grants VALUES('grant','device','worker','instance','session-4',NULL,'first'),('old-grant','device','old-worker','old-instance','session-5',NULL,'first'),('new-grant','other-device','new-worker','new-instance','session-5',NULL,'second');
    INSERT INTO device_execution_bindings VALUES('device','grant','session-4',NULL),('device','old-grant','session-5',NULL);`)
  const before = deviceExecutionResources(path, 'device')
  assert.deepEqual(before.leases, [])
  assert.deepEqual(before.admissions.map(row => [row.jobId, row.admissionState]),
    [['before-facts', 'queued'], ['queued', 'queued'], ['started', 'running']])
  assert.throws(() => assertRevokedDeviceExecutionResources(before, before))
  database.exec(`UPDATE scheduler_execution_jobs SET state='failed' WHERE job_id NOT IN ('other','successor');
    UPDATE execution_admission_reservations SET state='released',release_reason='cancelled' WHERE job_id NOT IN ('other','successor');`)
  const after = deviceExecutionResources(path, 'device')
  assert.doesNotThrow(() => assertRevokedDeviceExecutionResources(before, after))
  assert.throws(() => assertRevokedDeviceExecutionResources(before, { ...after, admissions: [] }))
  const lost = structuredClone(after)
  lost.admissions[0].admissionState = null; lost.admissions[0].accountingStatus = null; lost.admissions[0].terminalUsage = null
  assert.throws(() => assertRevokedDeviceExecutionResources(before, lost))
  const unclaimable = structuredClone(after)
  unclaimable.admissions[0].accountingStatus = 'pending'; unclaimable.admissions[0].terminalUsage = { accountingStatus: 'unknown', tokens: null, costMicrounits: null }
  assert.throws(() => assertRevokedDeviceExecutionResources(before, unclaimable))
  assert.equal(database.prepare("SELECT state FROM execution_admission_reservations WHERE job_id='other'").get().state, 'running')
  assert.equal(database.prepare("SELECT state FROM execution_admission_reservations WHERE job_id='successor'").get().state, 'running')
})

test('remote acceptance configuration requires TLS, a non-loopback SSH host and isolated Device data', () => {
  assert.doesNotThrow(() => validateRemoteFaultConfiguration(configuration()))
  const mutations = [
    value => { value.server.url = 'http://server.example.invalid:8443' },
    value => { value.server.url = 'https://user:secret@server.example.invalid' },
    value => { value.device.sshHost = 'user@127.0.0.1' },
    value => { value.device.sshHost = '-oProxyCommand=malicious' },
    value => { value.device.directory = '/opt/winwincode/runtime' },
    value => { value.server.tlsMode = 'loopback-proxy'; value.server.backendUrl = 'http://external.invalid' },
    value => { value.networkFault = { disconnect: ['false'], reconnect: ['true'], preservesSsh: false } },
  ]
  for (const mutate of mutations) {
    const value = configuration(); mutate(value)
    assert.throws(() => validateRemoteFaultConfiguration(value))
  }
})

test('remote acceptance never converts pending scenarios or cleanup failures into a passed report', () => {
  const report = emptyRemoteFaultReport()
  assert.deepEqual(report.scenarios.map(item => item.id), REMOTE_FAULT_SCENARIOS)
  assert.equal(report.modelEvidence, 'fixture')
  report.scenarios[0].status = 'passed'
  assert.equal(finalizeRemoteFaultReport(report).status, 'incomplete')
  for (const item of report.scenarios) item.status = 'passed'
  assert.equal(finalizeRemoteFaultReport(report).status, 'passed')
  report.cleanupDevice = 'failed'
  assert.equal(finalizeRemoteFaultReport(report).status, 'failed')
  delete report.cleanupDevice
  report.failure = { code: 'FIXTURE_FAILURE' }
  assert.equal(finalizeRemoteFaultReport(report).status, 'failed')
})

test('SSH transport quotes paths and carries no binding token or Owner password on the command line', () => {
  const device = { ...configuration().device, checkout: "/opt/fixture 'quoted' $(touch /tmp/never)", port: 2222 }
  const arguments_ = remoteOperationCommand(device)
  assert.deepEqual(arguments_.slice(0, 6), ['-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', '-p', '2222'])
  assert.equal(arguments_[6], device.sshHost)
  assert.ok(arguments_[7].includes("'\\''quoted'\\''"))
  assert.ok(arguments_[7].endsWith("' --device-operation"))
  assert.doesNotMatch(arguments_.join(' '), /PAIRING_TOKEN|passwordEnvironment|pairingToken/u)
})

test('SSH transport passes an explicit identity file as one local argument', () => {
  const value = configuration()
  value.device.identityFile = "/private/keys/test 'identity'"
  assert.doesNotThrow(() => validateRemoteFaultConfiguration(value))
  const arguments_ = remoteOperationCommand(value.device)
  assert.deepEqual(arguments_.slice(4, 7), ['-i', value.device.identityFile, value.device.sshHost])
  value.device.identityFile = 'relative-key'
  assert.throws(() => validateRemoteFaultConfiguration(value))
})

test('process witnesses select the exact Worker descendants while preserving unrelated processes', () => {
  const snapshot = { workers: [{ workerSessionId: 'fixture-session', pid: 10 }], processes: [
    { pid: 10, parent: 1, start: 'worker' }, { pid: 11, parent: 10, start: 'helper' },
    { pid: 12, parent: 11, start: 'tool' }, { pid: 13, parent: 12, start: 'child' },
    { pid: 99, parent: 1, start: 'unrelated' },
  ] }
  assert.deepEqual(taskProcessWitnesses(snapshot, { workerSessionId: 'fixture-session' }).map(item => item.pid), [11, 12, 13])
  assert.throws(() => taskProcessWitnesses(snapshot, { workerSessionId: 'wrong-session' }))
})

test('acceptance SSH reuses an explicitly configured control socket', () => {
  const value = configuration()
  value.device.controlPath = '/private/ssh control/socket'
  validateRemoteFaultConfiguration(value)
  const arguments_ = remoteOperationCommand(value.device)
  assert.deepEqual(arguments_.slice(4, 7), ['-S', value.device.controlPath, value.device.sshHost])
  value.device.controlPath = 'relative-socket'
  assert.throws(() => validateRemoteFaultConfiguration(value))
})

test('process witnesses include reparented guardians only in positively owned process groups', () => {
  const snapshot = { workers: [{ workerSessionId: 'fixture-session', pid: 10 }], processes: [
    { pid: 10, parent: 1, group: 10 }, { pid: 11, parent: 10, group: 11 },
    { pid: 12, parent: 11, group: 12 }, { pid: 13, parent: 12, group: 12 },
    { pid: 14, parent: 1, group: 12 }, { pid: 15, parent: 1, group: 11 },
    { pid: 90, parent: 1, group: 90 }, { pid: 91, parent: 1 },
    { pid: 92, parent: 1, group: 0 }, { pid: 93, parent: 1, group: -12 },
  ] }
  assert.deepEqual(taskProcessWitnesses(snapshot, { workerSessionId: 'fixture-session' })
    .map(row => row.pid), [11, 12, 13, 14, 15])
})

test('fixture command cells preserve running tool sessions and reject a failed real command result', async () => {
  const calls = []; const output = []
  const context = { ALL_TOOLS: [{ name: 'fixture_exec_command' }, { name: 'fixture_write_stdin' }],
    tools: { fixture_exec_command: async input => { calls.push(input); return { session_id: 23 } },
      fixture_write_stdin: async input => { calls.push(input); return { session_id: 23, exit_code: 0 } } },
    text: value => output.push(value) }
  await vm.runInNewContext(`(async()=>{${fixtureCommandCell('printf fixture')}})()`, context)
  assert.equal(calls.length, 2)
  assert.equal(calls[1].session_id, 23)
  assert.equal(output.length, 1)
  context.tools.fixture_exec_command = async () => ({ exit_code: 3 })
  await assert.rejects(vm.runInNewContext(`(async()=>{${fixtureCommandCell('false')}})()`, context),
    /fixture command did not complete successfully/u)
})

test('actual command evidence requires its own Worker job and both original durable completion results', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-tool-ledger-'))
  const workerData = join(directory, 'worker-data')
  mkdirSync(workerData)
  const device = new DatabaseSync(join(directory, 'device-client.sqlite3'))
  device.exec('CREATE TABLE worker_process_registry(worker_session_id TEXT,data_directory TEXT)')
  device.prepare('INSERT INTO worker_process_registry VALUES(?,?)').run('fixture-worker', workerData)
  device.close()
  const adapter = new DatabaseSync(join(workerData, 'worker-codex.sqlite3'))
  adapter.exec('CREATE TABLE codex_run(record_json BLOB)')
  adapter.prepare('INSERT INTO codex_run VALUES(?)').run(JSON.stringify({ job: { jobId: 'fixture-job' }, kernelSessionId: 'fixture-core' }))
  adapter.close()
  const core = new DatabaseSync(join(workerData, 'state_5.sqlite'))
  core.exec(`CREATE TABLE tool_requests(sequence INTEGER,thread_id TEXT,request_json TEXT);
    CREATE TABLE tool_attempts(request_sequence INTEGER,attempt_id TEXT,effective_input TEXT,
      execution TEXT,disposition TEXT,execution_result TEXT,accepted_result TEXT)`)
  const input = value => JSON.stringify({ input: JSON.stringify(value) })
  const result = value => JSON.stringify({ code_mode_result: value })
  core.prepare('INSERT INTO tool_requests VALUES(?,?,?)').run(1, 'fixture-core',
    JSON.stringify({ tool_name: 'fixture_exec_command', turn_id: 'fixture-turn' }))
  core.prepare('INSERT INTO tool_attempts VALUES(?,?,?,?,?,?,?)').run(1, 'fixture-attempt', input({ cmd: 'true' }),
    'completed', 'accepted', result({ exit_code: 7 }), result({ exit_code: 7 }))
  const inspect = overrides => readFixtureCommandEvidence({ deviceData: directory, workerSessionId: 'fixture-worker',
    jobId: 'fixture-job', command: 'true', ...overrides })
  try {
    assert.equal(inspect({ jobId: 'other-job' }), null)
    assert.equal(inspect({ command: 'other-command' }), null)
    assert.throws(() => inspect(), /original actual command must exit successfully/u)
    core.prepare('UPDATE tool_attempts SET execution_result=?,accepted_result=?').run(result({ session_id: 23 }), result({ session_id: 23 }))
    assert.equal(inspect(), null)
    core.prepare('INSERT INTO tool_requests VALUES(?,?,?)').run(2, 'fixture-core',
      JSON.stringify({ tool_name: 'fixture_write_stdin', turn_id: 'fixture-turn' }))
    core.prepare('INSERT INTO tool_attempts VALUES(?,?,?,?,?,?,?)').run(2, 'fixture-poll', input({ session_id: 23 }),
      'completed', 'accepted', result({ exit_code: 0 }), result({ exit_code: 3 }))
    assert.throws(() => inspect(), /original actual command must exit successfully/u)
    core.prepare('UPDATE tool_attempts SET accepted_result=? WHERE request_sequence=2').run(result({ exit_code: 0 }))
    const evidence = inspect()
    assert.deepEqual(evidence.exitCodes, [0, 0])
    assert.equal(evidence.receipts.length, 2)
    assert.equal(evidence.kernelSessionId, 'fixture-core')
    assert.throws(() => inspect({ workerSessionId: 'other-worker' }), /actual tool must belong/u)
    await t.test('custom exec JavaScript stays opaque while malformed target function JSON fails', () => {
      core.prepare('INSERT INTO tool_requests VALUES(?,?,?)').run(0, 'fixture-core',
        JSON.stringify({ tool_name: 'exec', turn_id: 'fixture-turn' }))
      core.prepare('INSERT INTO tool_attempts VALUES(?,?,?,?,?,?,?)').run(0, 'fixture-parent',
        JSON.stringify({ input: 'const execute=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));', kind: 'custom' }),
        'completed', 'accepted', result({ exit_code: 0 }), result({ exit_code: 0 }))
      assert.deepEqual(inspect().exitCodes, [0, 0])
      core.prepare('UPDATE tool_attempts SET effective_input=? WHERE request_sequence=1').run(
        JSON.stringify({ input: 'malformed function input', kind: 'function' }))
      assert.throws(() => inspect(), SyntaxError)
    })
  } finally { core.close(); rmSync(directory, { recursive: true, force: true }) }
})

test('process closure evidence still detects a real fixture child after its parent becomes unavailable', async () => {
  const parent = spawn(process.execPath, ['-e', `const {spawn}=require('node:child_process');
    const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{detached:true,stdio:'ignore'});
    console.log(child.pid);setInterval(()=>{},1000)`], { stdio: ['ignore', 'pipe', 'ignore'] })
  let output = ''; let childPid
  parent.stdout.on('data', chunk => { output += chunk })
  try {
    childPid = await waitFor(() => output.includes('\n') && Number(output.trim()), 'fixture child PID', 5000, 10)
    let start
    if (process.platform === 'linux') {
      const stat = readFileSync(`/proc/${childPid}/stat`, 'utf8')
      start = `linux-${stat.slice(stat.lastIndexOf(')') + 1).trim().split(/\s+/u)[19]}`
    } else {
      start = execFileSync('ps', ['-p', String(childPid), '-o', 'lstart='], { encoding: 'utf8',
        env: { ...process.env, LC_ALL: 'C' } }).trim().replace(/\s+/gu, ' ')
    }
    const witnesses = [{ pid: childPid, start }]
    const observed = observeProcessWitnesses(witnesses).alive
    assert.equal(observed.length, 1)
    assert.equal(observed[0].executable, basename(process.execPath))
    parent.kill('SIGKILL')
    await waitFor(() => parent.signalCode === 'SIGKILL', 'fixture parent reaped', 5000, 10)
    assert.equal(observeProcessWitnesses(witnesses).alive.length, 1,
      'a child changing its parent must remain visible to the closure check')
    assert.equal(observeProcessWitnesses([{ pid: childPid, start: `${start}-another-boot` }]).alive.length, 0)
    process.kill(childPid, 'SIGKILL')
    await waitFor(() => observeProcessWitnesses(witnesses).alive.length === 0, 'fixture orphan stopped', 5000, 10)
  } finally {
    for (const pid of [parent.pid, childPid]) if (pid) {
      try { process.kill(pid, 'SIGKILL') } catch (error) { if (error.code !== 'ESRCH') throw error }
    }
  }
})

test('acceptance HTTPS uses verified trust and hostname for the requested endpoint', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-tls-fixture-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const certificate = createCertificate(directory)
  const server = createServer({ key: readFileSync(certificate.key), cert: readFileSync(certificate.cert) },
    (_request, response) => { response.writeHead(403, { 'Content-Type': 'application/json' }); response.end('{"code":"FIXTURE_DENIED"}') })
  await new Promise(resolvePromise => server.listen(0, '127.0.0.1', resolvePromise))
  t.after(async () => { server.closeAllConnections(); await new Promise(resolvePromise => server.close(resolvePromise)) })
  const url = `https://127.0.0.1:${server.address().port}/api/v1/clients`
  let certificateSha256
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const response = await secureAcceptanceRequest(url, { ca: readFileSync(certificate.cert) })
    assert.equal(response.status, 403)
    assert.equal(response.tls.authorized, true)
    assert.match(response.tls.certificateSha256, /^[a-f0-9]{64}$/u)
    certificateSha256 ??= response.tls.certificateSha256
    assert.equal(response.tls.certificateSha256, certificateSha256)
  }
  await assert.rejects(secureAcceptanceRequest(url), error => /SELF_SIGNED|VERIFY/u.test(error.code))
  await assert.rejects(secureAcceptanceRequest('http://127.0.0.1:1/api/v1/clients'))
})

test('plan-only execution writes an explicitly unverified fixture report and exits incomplete', () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-plan-fixture-'))
  try {
    const output = join(directory, 'report.json')
    const result = spawnSync(process.execPath, ['scripts/run-remote-device-fault-acceptance.mjs', '--plan', '--output', output], {
      cwd: resolve(import.meta.dirname, '..'), encoding: 'utf8',
    })
    assert.equal(result.status, 2, result.stderr)
    const report = JSON.parse(readFileSync(output, 'utf8'))
    assert.equal(report.status, 'incomplete')
    assert.equal(report.topology, 'unverified')
    assert.equal(report.modelEvidence, 'fixture')
    assert.ok(report.scenarios.every(item => item.status === 'pending' && item.evidence === null))
  } finally { rmSync(directory, { recursive: true, force: true }) }
})

test('an actual startup failure replaces stale success with this run failure and private diagnostics', () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-startup-fixture-'))
  try {
    const output = join(directory, 'report.json')
    writeFileSync(output, '{"status":"passed"}\n')
    const result = spawnSync(process.execPath, ['scripts/run-remote-device-fault-acceptance.mjs',
      '--config', join(directory, 'missing-private.json'), '--output', output], {
      cwd: resolve(import.meta.dirname, '..'), encoding: 'utf8',
    })
    assert.equal(result.status, 1)
    const report = JSON.parse(readFileSync(output, 'utf8'))
    assert.equal(report.status, 'failed')
    assert.equal(report.topology, 'unverified')
    assert.equal(statSync(report.failure.diagnosticPath).mode & 0o777, 0o600)
    assert.equal(statSync(output).mode & 0o777, 0o600)
  } finally { rmSync(directory, { recursive: true, force: true }) }
})
