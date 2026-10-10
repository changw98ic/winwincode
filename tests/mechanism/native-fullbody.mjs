// SPDX-License-Identifier: Apache-2.0
// Explicit offline mechanism audit; see docs/engineering-runtime/mechanism-audit-20261010.md.
import assert from 'node:assert/strict'
import { createHash, X509Certificate } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, writeFileSync, mkdtempSync, realpathSync, readdirSync } from 'node:fs'
import { basename, dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { DatabaseSync } from 'node:sqlite'
import { pathToFileURL } from 'node:url'
import { execFileSync } from 'node:child_process'
import https from 'node:https'
import {syncBuiltinESMExports} from 'node:module'
import { snapshotPendingModelOpen, lookupAfterRequest } from './owned-slot-provider-witness.mjs'
import { setTimeout as delay } from 'node:timers/promises'
import { startOwnedSseProvider } from './owned-sse-provider.mjs'

const root = resolve(process.env.MECHANISM_SOURCE_ROOT
  ?? resolve(import.meta.dirname, '../..'))
const requestedArtifactRoot = resolve(process.env.WWC_MECHANISM_AUDIT_OUTPUT ?? join(tmpdir(), 'winwincode-mechanism-audit'))
mkdirSync(requestedArtifactRoot, { recursive: true, mode: 0o700 })
const artifactRoot = realpathSync(requestedArtifactRoot)
process.env.WWC_MECHANISM_AUDIT_OUTPUT = artifactRoot
let directory
if (process.argv[2]) {
  const requestedDirectory = resolve(process.argv[2])
  assert.equal(existsSync(requestedDirectory), false, 'explicit runtime directory must not exist')
  const parent = realpathSync(dirname(requestedDirectory))
  assert.ok(parent === artifactRoot || parent.startsWith(`${artifactRoot}/`), 'runtime parent must remain inside owned artifact directory')
  directory = join(parent, basename(requestedDirectory))
  mkdirSync(directory, { mode: 0o700 })
  directory = realpathSync(directory)
} else {
  directory = mkdtempSync(join(artifactRoot, 'native-fullbody-'))
}
const nativeHttpsRequest=https.request
https.request=function(...args) {
  const target=args[0] instanceof URL?args[0]:typeof args[0]==='string'?new URL(args[0]):null
  const point={path:target?.pathname??args[0]?.path??null,method:args[1]?.method??'GET',startedAtUtc:new Date().toISOString(),startedMonotonicMillis:performance.now(),secureConnected:false,status:null,errorCode:null}
  const persist=()=>writeFileSync(join(directory,'http-attempts.jsonl'),JSON.stringify(point)+'\n',{flag:'a',mode:0o600})
  const request=nativeHttpsRequest.apply(this,args)
  request.once('socket',socket=>socket.once('secureConnect',()=>{point.secureConnected=true}))
  request.once('response',response=>{point.status=response.statusCode;let text='';let bytes=0
    if(response.statusCode>=400) response.on('data',chunk=>{bytes+=Buffer.byteLength(chunk);if(bytes<=32768)text+=chunk.toString()})
    response.once('end',()=>{if(text && bytes<=32768){try{const value=JSON.parse(text);point.errorCode=value.error?.code??value.code??null}catch{}}
      point.finishedAtUtc=new Date().toISOString();point.elapsedMillis=performance.now()-point.startedMonotonicMillis;persist()})})
  request.once('error',error=>{point.ioErrorCode=error.code??null;point.finishedAtUtc=new Date().toISOString();point.elapsedMillis=performance.now()-point.startedMonotonicMillis;persist()})
  return request
}
syncBuiltinESMExports()
const { runApiProductionVertical, createCertificate, workItemCreatePayload,
  waitForDeviceWorkerRegistered, ApiClient } = await import(pathToFileURL(join(root, 'scripts/run-api-production-vertical.mjs')))
const apiPhases = []
const captureOwnedProcesses = () => {
  const now = new Date().toISOString()
  const text = execFileSync('/bin/ps', ['-axo','pid=,ppid=,lstart=,command='], {encoding:'utf8'})
  const allRows = text.split('\n').map(row => {
    const match = row.match(/^\s*(\d+)\s+(\d+)\s+(.{24})\s+(.*)$/)
    return match ? {pid:Number(match[1]),ppid:Number(match[2]),lstart:match[3],executableName:match[4].split(' ')[0].split('/').at(-1),commandSha256:createHash('sha256').update(match[4]).digest('hex')} : null
  }).filter(Boolean)
  const owned=new Set([process.pid]);let changed=true
  while(changed) {changed=false;for(const row of allRows) if(owned.has(row.ppid) && !owned.has(row.pid)){owned.add(row.pid);changed=true}}
  const rows=allRows.filter(row=>owned.has(row.pid))
  writeFileSync(join(directory,'owned-process-identities.jsonl'), JSON.stringify({capturedAtUtc:now,rows})+'\n',{flag:'a',mode:0o600})
}
for (const name of ['command','query','request']) {
  const original = ApiClient.prototype[name]
  ApiClient.prototype[name] = async function(...args) {
    const point = {method:name,operation:args[0],startedAtUtc:new Date().toISOString(),startedMonotonicMillis:performance.now(),requestId:null}
    if(name==='command') { args[3] ??= this.requestId();point.requestId=args[3] }
    if(name==='request') {point.httpMethod=args[1]?.method ?? 'GET'; point.configuredTimeoutMillis=args[1]?.timeoutMillis ?? 30000}
    apiPhases.push(point); captureOwnedProcesses()
    const persist=()=>writeFileSync(join(directory,'api-phases.json'),JSON.stringify(apiPhases,null,2),{mode:0o600})
    persist()
    try {const result=await original.apply(this,args);point.outcome=result?.outcome??null;point.status=result?.status??null;return result}
    catch(error) {point.failure={name:error.name,message:error.message,code:error.code??null,failure:error.failure??error.networkFailure??null,stack:error.stack?.split('\n').slice(0,8)};throw error}
    finally {point.finishedAtUtc=new Date().toISOString();point.elapsedMillis=performance.now()-point.startedMonotonicMillis;captureOwnedProcesses();persist()}
  }
}
const {stopUnusedDeviceTaskAnchor}=await import(pathToFileURL(join(root,'scripts/device-production-fixture.mjs')))
const certificate = createCertificate(directory)
const trust = join(directory, 'owned-provider-root.der')
writeFileSync(trust, new X509Certificate(readFileSync(certificate.cert)).raw, { mode: 0o600 })
// The established Device fixture reads this explicit process-level trust setting.
process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE = trust
const setupOnly=process.env.MECHANISM_SETUP_ONLY==='1'
const protocol = process.env.MECHANISM_PROVIDER_PROTOCOL ?? 'openai_responses'
const deltaCount = Number(process.env.MECHANISM_DELTA_COUNT ?? 2522)
const deltaBytes = Number(process.env.MECHANISM_DELTA_BYTES ?? 120)
const burstObservationMillis = Number(process.env.MECHANISM_BURST_OBSERVATION_MILLIS ?? 1500000)
assert.ok(Number.isInteger(burstObservationMillis) && burstObservationMillis >= 120000 && burstObservationMillis <= 1800000)
assert.ok(Number.isInteger(deltaCount) && deltaCount > 0 && deltaCount <= 8000)
assert.ok(Number.isInteger(deltaBytes) && deltaBytes > 0 && deltaBytes <= 1024)
const provider = await startOwnedSseProvider({ key: readFileSync(certificate.key),
  cert: readFileSync(certificate.cert), protocol, deltaCount, deltaBytes,
  finalText: JSON.stringify({ acceptanceCriteriaIds: ['changed'], disposition: 'final',
    patch: '*** Begin Patch\n*** Update File: TASK.md\n@@\n-status: pending\n+status: complete\n*** End Patch\n',
    schemaVersion: 1, validationProfile: 'changed' }) })
const startedAtUtc = new Date().toISOString()
const origin = performance.now()
const snapshots = []
const observationReadFailures = []
let identity = null
let releasedAt = null
let heldControl = null
let vertical = null
let failure = null
let exactOpen = null
let nativeOpen = null
let cursorKey = null
let workspaceManifestPath = null
const modelStateSamples=[]
const save = () => writeFileSync(join(directory, 'fullchain-observations.json'), JSON.stringify({
  schemaVersion: 1, startedAtUtc, capturedAtUtc: new Date().toISOString(),
  baselineCommit: 'e994faa55ac2baad964bea5c43a23d919497a9de',
  authority: 'owned offline Server/Device/Worker fixture', protocol, deltaCount, deltaBytes,
  leaseSeconds: 30, productionHeartbeatMillis: 5000, burstObservationMillis, heldControl, identity, releasedAt, observationReadFailures,
  providerRequests: provider.requests, providerEvents: provider.events, snapshots,
  vertical, failure, modelStateSamples, nativeSessionIdentity:nativeOpen?.sessionIdentity??null, cursorKey, apiPhasePath:join(directory,'api-phases.json'), exactOpen, limits: [
    'Short owned lease is not a replay of the original 900-second production lease.',
    'Provider emits synthetic content and never contacts an external model.',
    'Held response is a waiting baseline, not saturation of three shared OS request slots.',
    'Final proposal is legal JSON padded with whitespace; the large assistant frame precedes the small final done frame.',
    'Snapshot wallclock differs from sender-reported lastHeartbeatAt.',
    'Single WorkRun completion is not formal benchmark completion or a product fix.',
  ],
}, null, 2), { mode: 0o600 })

function observeReadonly(stage, operation) {
  const attemptedAtUtc = new Date().toISOString()
  try { return operation() }
  catch (error) {
    const baseCode = typeof error?.errcode === 'number' ? (error.errcode & 255) : null
    const busy = error?.code === 'ERR_SQLITE_ERROR'
      && ([5, 6].includes(baseCode) || /(?:database|table).*locked/iu.test(error.message ?? ''))
    if (!busy) throw error
    observationReadFailures.push({ stage, attemptedAtUtc, observedAtUtc: new Date().toISOString(),
      code: error.code, sqliteBaseCode: baseCode, message: String(error.message).slice(0, 160),
      disposition: 'observation_gap_no_state_or_success_inferred', busyTimeoutMillis: 200 })
    return null
  }
}
function appliedRenewal(held) { return observeReadonly('appliedRenewal', () => appliedRenewalUnsafe(held)) }
function modelState() { return observeReadonly('modelState', modelStateUnsafe) }
function snapshot(workRunId) { return observeReadonly('snapshot', () => snapshotUnsafe(workRunId)) }
function appliedRenewalUnsafe(held) {
  if(!held) return null
  const db=new DatabaseSync(join(directory,'device-data','device-client.sqlite3'),{readOnly:true})
  let row
  try {db.exec('PRAGMA query_only=ON');db.exec('PRAGMA busy_timeout=200');row=db.prepare('SELECT worker_session_id,data_directory FROM worker_process_registry WHERE worker_instance_id=?').get(held.lease.worker_instance_id)}finally{db.close()}
  if(!row) return null
  const path=join(row.data_directory,'mechanism-timing.jsonl')
  if(!existsSync(path)) return null
  const matching=readFileSync(path,'utf8').split('\n').filter(Boolean).flatMap(line=>{try{return[JSON.parse(line)]}catch{return[]}}).filter(point=>point.stage==='lease_renewal_consumed' && point.details?.jobId===held.job.job_id && point.details?.leaseId===held.lease.lease_id && point.details?.workerInstanceId===held.lease.worker_instance_id && point.details?.attempt===held.lease.attempt && point.details?.fencingToken===held.lease.fencing_token && point.details?.accepted && point.details?.currentWorkerLeaseExpiresAt===held.lease.expires_at && point.details?.currentWorkspaceLeaseExpiresAt===held.lease.expires_at)
  return matching.length?{path,physicalWorkerSessionId:row.worker_session_id,record:matching.at(-1)}:null
}
function bindNativeCursor() {
  const db=new DatabaseSync(exactOpen.adapterPath,{readOnly:true})
  try{db.exec('PRAGMA query_only=ON');db.exec('PRAGMA busy_timeout=200');const rows=db.prepare("SELECT frame_json FROM execution_outbox WHERE json_extract(frame_json,'$.kind')='model.open' AND json_extract(frame_json,'$.modelExchangeId')=?").all(identity.modelExchangeId);assert.equal(rows.length,1);nativeOpen=JSON.parse(Buffer.from(rows[0].frame_json).toString());assert.equal(nativeOpen.lease.jobId,identity.jobId);assert.equal(nativeOpen.lease.leaseId,identity.leaseId);assert.equal(nativeOpen.sessionIdentity.workRunId,identity.workRunId);identity.sourceProductSessionId=identity.productSessionId;identity.executionProductSessionId=nativeOpen.sessionIdentity.productSessionId;identity.logicalWorkerSessionId=nativeOpen.workerSessionId;identity.codexThreadId=nativeOpen.sessionIdentity.codexThreadId;
    const components=[nativeOpen.lease.jobId,nativeOpen.lease.leaseId,nativeOpen.lease.workerId,nativeOpen.lease.workerInstanceId,String(nativeOpen.lease.attempt),nativeOpen.lease.fencingToken,nativeOpen.workerSessionId,nativeOpen.sessionIdentity.productSessionId,nativeOpen.sessionIdentity.workRunId??'',nativeOpen.sessionIdentity.codexThreadId,nativeOpen.modelExchangeId];cursorKey='model-worker-replay:v1'+components.map(value=>'/'+Buffer.byteLength(value,'utf8')+':'+value).join('')
  }finally{db.close()}
  const candidates=readdirSync(join(directory,'device-data','execution-workspaces'),{recursive:true}).filter(path=>path.endsWith('.winwincode-workspace.json'))
  for(const candidate of candidates) {const path=join(directory,'device-data','execution-workspaces',candidate);const manifest=JSON.parse(readFileSync(path,'utf8'));if(manifest.currentProvenance?.executionJobId===identity.jobId && manifest.currentProvenance?.leaseId===identity.leaseId) {assert.equal(workspaceManifestPath,null);workspaceManifestPath=path}}
  assert.ok(workspaceManifestPath,'target real workspace recovery manifest must exist before response release')
}
function modelStateUnsafe() {
  if(!cursorKey || !identity) return null
  const observedBeforeQueryUtc=new Date().toISOString();const db=new DatabaseSync(exactOpen.adapterPath,{readOnly:true})
  let ledger,cursor,frames,lineage
  try{db.exec('PRAGMA query_only=ON');db.exec('PRAGMA busy_timeout=200');db.exec('BEGIN');ledger=db.prepare('SELECT run_key,model_call_id,model_exchange_id,completed,provider_final,core_committed FROM model_call_ledger WHERE model_exchange_id=?').get(identity.modelExchangeId)??null;cursor=db.prepare("SELECT stream_key,length(snapshot_json) AS snapshotBytes,json_extract(CAST(snapshot_json AS TEXT),'$.confirmedSequence') AS confirmedSequence,json_extract(CAST(snapshot_json AS TEXT),'$.termination') AS termination,json_extract(CAST(snapshot_json AS TEXT),'$.cancellation.phase') AS cancellationPhase FROM model_cursor WHERE stream_key=?").get(cursorKey)??null;frames=ledger?db.prepare("SELECT count(*) AS frameCount,min(sequence) AS minSequence,max(sequence) AS maxSequence,sum(length(frame_json)) AS sumFrameBytes,max(length(frame_json)) AS maxFrameBytes,sum(CASE WHEN json_extract(CAST(frame_json AS TEXT),'$.isFinal')=1 THEN 1 ELSE 0 END) AS finalFrames FROM model_call_frame WHERE run_key=? AND model_call_id=?").get(ledger.run_key,ledger.model_call_id):null;lineage=ledger?db.prepare('SELECT authority_json FROM model_thread_lineage WHERE run_key=?').get(ledger.run_key):null;if(lineage){const value=JSON.parse(Buffer.from(lineage.authority_json).toString());lineage={lease:value.lease,sessionIdentity:value.session_identity??value.sessionIdentity,authoritySha256:createHash('sha256').update(Buffer.from(lineage.authority_json)).digest('hex')}}db.exec('COMMIT')}finally{db.close()}
  const pd=new DatabaseSync(exactOpen.providerPath,{readOnly:true});let provider
  try{pd.exec('PRAGMA query_only=ON');pd.exec('PRAGMA busy_timeout=200');pd.exec('BEGIN');provider={exchange:pd.prepare('SELECT cancelled,length(chunks) AS chunksJsonBytes,json_array_length(CAST(chunks AS TEXT)) AS normalizedChunkCount FROM exchanges WHERE exchange_id=?').get(identity.modelExchangeId),diagnostics:pd.prepare('SELECT sequence,policy_attempt,started_ms,finished_ms,outcome,stop_reason FROM model_attempt_diagnostics WHERE exchange_id=? ORDER BY sequence').all(identity.modelExchangeId),invocations:pd.prepare('SELECT attempt_number,state FROM model_invocation_attempts WHERE exchange_id=?').all(identity.modelExchangeId)};pd.exec('COMMIT')}finally{pd.close()}
  let workspace=null;if(workspaceManifestPath && existsSync(workspaceManifestPath)){const manifest=JSON.parse(readFileSync(workspaceManifestPath,'utf8'));workspace={path:workspaceManifestPath,phase:manifest.phase,originProvenance:manifest.originProvenance,currentProvenance:manifest.currentProvenance}}
  const value={observedBeforeQueryUtc,observedAfterQueryUtc:new Date().toISOString(),modelExchangeId:identity.modelExchangeId,cursorKey,ledger,cursor,frames,lineage,provider,workspace,boundary:'separate coherent read-only Adapter and Provider transactions plus exact workspace manifest; no cross-database atomic snapshot claimed'};modelStateSamples.push(value);return value
}
function timingEvidence() {
  if(!identity || !exactOpen?.physicalWorkerSessionId || !identity.modelExchangeId) return {status:'exact_identity_not_available',sources:[],finalCheckpoint:null}
  const path=join(directory,'device-data','worker-sessions',exactOpen.physicalWorkerSessionId,'data','mechanism-timing.jsonl')
  if(!existsSync(path)) return {status:'exact_timing_file_missing',path,finalCheckpoint:null}
  const records=readFileSync(path,'utf8').split('\n').filter(Boolean).flatMap(line=>{try{return[JSON.parse(line)]}catch{return[]}})
  const appliedRenewals=records.filter(row=>row.stage==='lease_renewal_consumed' && row.details?.jobId===identity.jobId && row.details?.leaseId===identity.leaseId && row.details?.attempt===identity.attempt && row.details?.fencingToken===identity.fencingToken)
  const checkpoints=records.filter(row=>row.stage==='frame_intake_checkpoint' && row.details?.jobId===identity.jobId && row.details?.leaseId===identity.leaseId && row.details?.modelExchangeId===identity.modelExchangeId && row.details?.attempt===identity.attempt && row.details?.fencingToken===identity.fencingToken)
  const finalCheckpoint=checkpoints.filter(row=>row.details?.isFinal && row.details?.accepted).at(-1)??null
  return {path,physicalWorkerSessionId:exactOpen.physicalWorkerSessionId,instrumentationRecords:records.length,appliedRenewals,checkpoints,finalCheckpoint,driverReturnedAfterFinal:finalCheckpoint!==null && records.some(row=>row.stage==='drive_exit' && row.actualWallUnixMillis>=finalCheckpoint.actualWallUnixMillis),heartbeatAfterFinal:finalCheckpoint!==null && records.some(row=>row.stage==='heartbeat_exit' && row.actualWallUnixMillis>=finalCheckpoint.actualWallUnixMillis),boundary:'exact native Worker file, exact Job/Lease/exchange/attempt/fence checkpoints every250/final; hook cost not independently measured'}
}

function snapshotUnsafe(workRunId) {
  const db = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
  const observedBeforeQueryUtc = new Date().toISOString()
  try {
    db.exec('PRAGMA query_only=ON')
    db.exec('PRAGMA busy_timeout=200')
    db.exec('BEGIN')
    const job = db.prepare('SELECT job_id,work_run_id,delivery_id,state,attempt,updated_at FROM scheduler_execution_jobs WHERE work_run_id=?').get(workRunId)
    if (!job) return null
    const lease = db.prepare('SELECT job_id,lease_id,worker_id,worker_instance_id,attempt,fencing_token,issued_at,expires_at FROM execution_leases WHERE job_id=?').get(job.job_id)
    if (!lease) return null
    const worker = db.prepare('SELECT worker_id,worker_instance_id,last_heartbeat_at,heartbeat_sequence FROM execution_workers WHERE worker_id=? AND worker_instance_id=?').get(lease.worker_id, lease.worker_instance_id)
    const terminal = db.prepare('SELECT job_id,lease_id,worker_id,worker_instance_id,attempt,fencing_token,outcome,terminal_at FROM execution_lease_terminals WHERE job_id=? AND lease_id=?').get(job.job_id, lease.lease_id) ?? null
    const allReceipts = db.prepare("SELECT operation,request_id,response_json FROM execution_lease_request_receipts WHERE job_id=? AND operation='renew'").all(job.job_id).map(row => {
      const response = JSON.parse(row.response_json)
      const exact = response.lease?.job_id === lease.job_id && response.lease?.lease_id === lease.lease_id
        && response.lease?.worker_id === lease.worker_id && response.lease?.worker_instance_id === lease.worker_instance_id
        && response.lease?.attempt === lease.attempt && response.lease?.fencing_token === lease.fencing_token
      return { operation: row.operation, requestId: row.request_id, responseStatus: response.status,
        exactCurrentAttempt: exact, acceptedLease: response.lease,
        responseSha256: createHash('sha256').update(row.response_json).digest('hex') }
    })
    const receipts = allReceipts.filter(row => row.responseStatus === 'accepted' && row.exactCurrentAttempt)
    db.exec('COMMIT')
    const value = { observedBeforeQueryUtc, observedAfterQueryUtc: new Date().toISOString(), coherentReadTransaction: true, observedAtUtc: new Date().toISOString(), elapsedMillis: performance.now() - origin,
      phase: releasedAt === null ? 'held_response' : 'complete_response_released', job, lease, worker,
      terminal, renewReceiptCount: receipts.length, renewReceipts: receipts, allRenewReceiptCount: allReceipts.length }
    snapshots.push(value)
    if(snapshots.length===1 || snapshots.length%25===0) {captureOwnedProcesses();if(cursorKey)modelState()}
    return value
  } finally { db.close() }
}

try {
  const bin = process.env.MECHANISM_BIN_DIRECTORY
    ?? join(root, 'target/debug')
  vertical = await runApiProductionVertical({ build: false, root, directory,
    serverBinary: join(bin, 'winwincode-server'), wwcBinary: join(bin, 'wwc'),
    restart: false, repeat: false, retainRepository: true, timeoutMillis: burstObservationMillis + 180_000,
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30',
      WWC_DEBUG_RUNTIME: '1', WWC_DEBUG_RUNTIME_LOG: join(directory, 'server-runtime.log') },
    deviceAgentEnvironment: { ...process.env, WWC_DEVICE_PROVIDER_HTTPS_PROXY: undefined, WWC_MECHANISM_TIMING: '1' },
    deviceProviderSecrets: ['owned-offline-secret'],
    deviceProvider: { providerId: 'mechanism-owned', modelId: 'mechanism-offline-model',
      endpoint: provider.endpoint, protocol, displayName: 'Owned offline regression Provider' },
    scenario: { files: { 'TASK.md': 'status: pending\n' }, async run({ api, baseline, modelRoute, devicePath }) {
      const productSessionId = 'psn_01J00000000000000000000017'
      const deliveryId = 'dlv_01J00000000000000000000017'
      const session = await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Offline lease causality', modelRoute })
      assert.equal(session.outcome, 'completed')
      const path = { ...devicePath, ...devicePath.forProductSession(productSessionId) }
      const anchor = await path.launchAnchor({ productSessionId })
      const created = await api.command('delivery.create', 0, { deliveryId, spec: {
        title: 'Offline lease causality', goal: 'Change TASK.md to status: complete.', scope: ['TASK.md'],
        constraints: ['Only edit TASK.md'], outOfScope: ['Other files'], baseRevision: baseline,
        repositoryId: 'rep_01J00000000000000000000000', publicationTarget: null,
        sourceProductSessionId: productSessionId, verificationCommand: "npm run verify",
        acceptanceCriteria: [{ id: 'changed', required: true, title: 'TASK.md changed' }],
      } })
      assert.equal(created.outcome, 'completed')
      const aggregate = (await api.query('workrun.get', { deliveryId, workItemId: null, atCursor: null })).result
      const items = await api.command('workitems.create', created.currentRevision,
        workItemCreatePayload(aggregate, created.currentRevision))
      assert.equal(items.outcome, 'completed')
      await waitForDeviceWorkerRegistered(api, anchor, 60_000)
      const started = await api.command('workrun.start', items.currentRevision, { deliveryId, dispatchProfile: 'executor' })
      assert.equal(started.outcome, 'completed')
      const workRunId = started.result.activeWorkRunId
      assert.ok(workRunId)
      const sourceAnchorClosure=await stopUnusedDeviceTaskAnchor({api,directory,deviceData:devicePath.deviceData,launched:anchor,productSessionId})
      writeFileSync(join(directory,'source-anchor-closure.json'),JSON.stringify(sourceAnchorClosure,null,2),{mode:0o600})
      const executionAnchor=await devicePath.launchAnchor({workRunId,deliveryId})
      await waitForDeviceWorkerRegistered(api,executionAnchor,60_000)
      const deadline = performance.now() + 60_000
      let first = null
      let held = null
      while (performance.now() < deadline) {
        held = snapshot(workRunId)
        if (held && first === null) first = held
        if (held?.terminal) throw new Error('owned Job ended before complete response release')
        if (provider.requests.length > 0 && first && held.renewReceiptCount > 0
          && Date.parse(held.lease.expires_at) > Date.parse(first.lease.expires_at)
          && held.worker.heartbeat_sequence >= first.worker.heartbeat_sequence + 2 && appliedRenewal(held)) break
        await delay(200)
      }
      assert.ok(held?.renewReceiptCount > 0, 'held response baseline must actually renew its lease')
      assert.ok(Date.parse(held.lease.expires_at) > Date.parse(first.lease.expires_at))
      assert.ok(provider.requests.length > 0, 'held response must have an actual owned Provider request')
      assert.ok(held.worker.heartbeat_sequence >= first.worker.heartbeat_sequence + 2,
        'held response baseline must actually deliver at least two new Worker heartbeats')
      heldControl = { durationMillis: held.elapsedMillis - first.elapsedMillis,
        heartbeatSequenceStart: first.worker.heartbeat_sequence,
        heartbeatSequenceEnd: held.worker.heartbeat_sequence,
        renewReceiptCount: held.renewReceiptCount, leaseExpiresAtBeforeRelease: held.lease.expires_at, appliedRenewal:appliedRenewal(held) }
      identity = { productSessionId, deliveryId, workRunId, jobId: held.job.job_id,
        workerId: held.lease.worker_id, workerInstanceId: held.lease.worker_instance_id,
        leaseId: held.lease.lease_id, attempt: held.lease.attempt, fencingToken: held.lease.fencing_token }
      exactOpen=snapshotPendingModelOpen(join(directory,'device-data'),identity)
      assert.ok(exactOpen.modelExchangeId,'one exact model exchange must map the target Job')
      identity.modelExchangeId=exactOpen.modelExchangeId
      const httpMapping=lookupAfterRequest(exactOpen,provider.requests,identity.modelExchangeId)
      assert.ok(exactOpen.nativeThreadSha256)
      assert.equal(httpMapping.firstAttemptRequestCount,1)
      assert.ok(httpMapping.matches.every(row=>row.threadSha256Matches && row.nativeThreadSha256))
      exactOpen={...exactOpen,httpMapping}
      bindNativeCursor()
      modelState()
      assert.ok(heldControl.appliedRenewal,'real Worker and workspace must apply the exact accepted renewal before release')
      if(setupOnly) {save();return {identity,heldControl,phase:'held_response_baseline_only',completeResponseReleased:false}}
      releasedAt = { atUtc: new Date().toISOString(), elapsedMillis: performance.now() - origin }
      provider.release()
      save()
      const burstDeadline = performance.now() + burstObservationMillis
      while (performance.now() < burstDeadline) {
        const current = snapshot(workRunId)
        if(current?.terminal) {
          const progress=modelStateSamples.at(-1);const timingNow=timingEvidence();
          if(progress?.cursor?.confirmedSequence===2527 && progress?.cursor?.termination==='completed' && timingNow.driverReturnedAfterFinal) break
          const registry=new DatabaseSync(join(directory,'device-data','device-client.sqlite3'),{readOnly:true});let workerState
          try{registry.exec('PRAGMA query_only=ON');workerState=registry.prepare('SELECT state,exit_code FROM worker_process_registry WHERE worker_instance_id=?').get(identity.workerInstanceId)}finally{registry.close()}
          if(workerState && workerState.state!=='running') {captureOwnedProcesses();break}
        }
        if (snapshots.length % 25 === 0) save()
        await delay(200)
      }
      const last = snapshots.at(-1)
      const timing = timingEvidence()
      const finalModelState=modelState()
      const heartbeatChanges = snapshots.filter((value, i) => i === 0
        || value.worker.heartbeat_sequence !== snapshots[i - 1].worker.heartbeat_sequence)
      const gaps = heartbeatChanges.slice(1).map((value, i) => value.elapsedMillis - heartbeatChanges[i].elapsedMillis)
      const expiredWhileUnterminal = snapshots.some(value => value.phase === 'complete_response_released'
        && value.terminal === null && Date.parse(value.observedBeforeQueryUtc) >= Date.parse(value.lease.expires_at))
      return { identity, heldControl, expiredWhileUnterminal, maxObservedHeartbeatGapMillis: Math.max(0, ...gaps),
        timing, finalModelState, complete2527CheckpointObserved: timing.finalCheckpoint?.details?.sequence === 2527, terminal: last?.terminal ?? null, jobState: last?.job.state ?? null,
        behaviorStatus: expiredWhileUnterminal ? 'expired_during_complete_response_intake_observation'
          : 'expiry_not_reproduced_at_this_scale', snapshotCount: snapshots.length }
    } },
  })
} catch (error) {
  failure = { name: error.name, message: error.message, code: error.code ?? null, failure:error.failure??error.networkFailure??null, stack:error.stack?.split('\n').slice(0,10) }
  process.exitCode = 1
} finally {
  await provider.close()
  captureOwnedProcesses()
  save()
}
