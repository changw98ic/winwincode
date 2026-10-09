import assert from 'node:assert/strict'
import test from 'node:test'
import { execFileSync, spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { once } from 'node:events'
import { chmodSync, lstatSync, mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync,
  writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { deviceTaskIdentities, prepareDeviceTaskBaseline,
  prepareDeviceBenchmarkProviderSlots, ensureBenchmarkDeviceOccupancy } from '../scripts/lib/device-task-runtime.mjs'
import { pendingDeviceTaskWorkRuns, cancelStoppedDeviceTask } from '../scripts/acceptance/run-device-task-vertical.mjs'
import { deviceTaskLaunchResult } from '../scripts/lib/device-production-fixture.mjs'

function providerSlotFixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'device-provider-slots-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const profiles = ['main-A', 'main-B', 'main-C', 'main-D'].map(configurationId => ({ configurationId }))
  const providers = [{ providerId: 'provider-a' }, { providerId: 'provider-b' }]
  const input = { directory, profiles, providers }
  const digest = provider => createHash('sha256').update(provider).digest('hex')
  const slots = (profile, provider = 'provider-a') => join(profile === 0
    ? join(directory, 'device-data') : join(directory, 'devices', profiles[profile].configurationId),
  'providers', 'model-provider-slots', digest(provider))
  return { input, slots, shared: provider => join(directory, 'model-provider-slots', digest(provider)) }
}

test('benchmark profiles retain shared private Provider slot inodes across restart', t => {
  const { input, slots, shared } = providerSlotFixture(t)
  prepareDeviceBenchmarkProviderSlots(input)
  const before = input.providers.flatMap(({ providerId }) => Array.from({ length: 3 }, (_, index) => {
    const source = lstatSync(join(shared(providerId), `slot-${index}`))
    assert.equal(source.mode & 0o777, 0o600)
    assert.equal(lstatSync(shared(providerId)).mode & 0o777, 0o700)
    assert.equal(source.nlink, input.profiles.length + 1)
    for (let profile = 0; profile < input.profiles.length; profile++) {
      const device = lstatSync(join(slots(profile, providerId), `slot-${index}`))
      assert.equal(lstatSync(slots(profile, providerId)).mode & 0o777, 0o700)
      assert.equal(device.mode & 0o777, 0o600)
      assert.deepEqual([device.dev, device.ino], [source.dev, source.ino])
    }
    return [source.dev, source.ino]
  }))
  prepareDeviceBenchmarkProviderSlots(input)
  assert.deepEqual(input.providers.flatMap(({ providerId }) => Array.from({ length: 3 }, (_, index) => {
    const source = lstatSync(join(shared(providerId), `slot-${index}`))
    return [source.dev, source.ino]
  })), before)
})

test('benchmark profile expansion preserves the base Device and rejects reassignment before provisioning', t => {
  const { input } = providerSlotFixture(t)
  const onlyC = { ...input, profiles: [input.profiles[2]] }
  prepareDeviceBenchmarkProviderSlots(onlyC)
  const identityPath = join(input.directory, 'base-profile.json')
  const original = lstatSync(identityPath)
  const bytes = readFileSync(identityPath, 'utf8')
  assert.equal(original.mode & 0o777, 0o600)
  assert.equal(JSON.parse(bytes).configurationId, 'main-C')
  prepareDeviceBenchmarkProviderSlots(onlyC)
  assert.throws(() => prepareDeviceBenchmarkProviderSlots({ ...input,
    profiles: [input.profiles[0], input.profiles[2]] }), { code: 'BENCHMARK_BASE_PROFILE_MISMATCH' })
  assert.deepEqual([lstatSync(identityPath).dev, lstatSync(identityPath).ino], [original.dev, original.ino])
  assert.equal(readFileSync(identityPath, 'utf8'), bytes)
  assert.throws(() => lstatSync(join(input.directory, 'devices')), { code: 'ENOENT' },
    'a mismatched base profile must not provision another Device')

  const retained = providerSlotFixture(t)
  prepareDeviceBenchmarkProviderSlots({ ...retained.input,
    profiles: [retained.input.profiles[0], retained.input.profiles[2]] })
  const source = lstatSync(join(retained.slots(2), 'slot-0'))
  prepareDeviceBenchmarkProviderSlots(retained.input)
  assert.deepEqual([lstatSync(join(retained.slots(2), 'slot-0')).dev,
    lstatSync(join(retained.slots(2), 'slot-0')).ino], [source.dev, source.ino])
  assert.equal(JSON.parse(readFileSync(join(retained.input.directory, 'base-profile.json'), 'utf8')).configurationId,
    'main-A')
})

test('retained benchmark Device data cannot acquire a guessed base profile identity', t => {
  const { input } = providerSlotFixture(t)
  const device = join(input.directory, 'device-data')
  mkdirSync(device, { mode: 0o700 })
  writeFileSync(join(device, 'device-client.sqlite3'), 'retained Device\n', { mode: 0o600 })
  assert.throws(() => prepareDeviceBenchmarkProviderSlots(input), {
    code: 'BENCHMARK_BASE_PROFILE_IDENTITY_MISSING',
  })
  assert.throws(() => lstatSync(join(input.directory, 'base-profile.json')), { code: 'ENOENT' })
  assert.equal(readFileSync(join(device, 'device-client.sqlite3'), 'utf8'), 'retained Device\n')
})

test('benchmark startup preserves and rejects independent, public, or symbolic slot files', t => {
  for (const kind of ['independent', 'public', 'symbolic']) {
    const { input, slots } = providerSlotFixture(t)
    mkdirSync(slots(1), { recursive: true, mode: 0o700 })
    const retained = join(slots(1), 'slot-0')
    if (kind === 'symbolic') symlinkSync('other-slot', retained)
    else {
      writeFileSync(retained, 'retained\n', { mode: 0o600 })
      if (kind === 'public') chmodSync(retained, 0o644)
    }
    const original = lstatSync(retained)
    assert.throws(() => prepareDeviceBenchmarkProviderSlots(input),
      kind === 'independent' ? /independent inode/u : /private regular file/u)
    assert.deepEqual([lstatSync(retained).dev, lstatSync(retained).ino], [original.dev, original.ino])
    if (kind !== 'symbolic') assert.equal(readFileSync(retained, 'utf8'), 'retained\n')
  }
  const { input, shared } = providerSlotFixture(t)
  prepareDeviceBenchmarkProviderSlots(input)
  chmodSync(shared('provider-a'), 0o755)
  assert.throws(() => prepareDeviceBenchmarkProviderSlots(input), /directory must be private/u)
})

test('three real processes fill one Provider across profile Devices and release slots on exit', async t => {
  const { input, slots } = providerSlotFixture(t)
  prepareDeviceBenchmarkProviderSlots(input)
  const children = []
  const release = async child => {
    if (child.exitCode !== null) return
    const exited = once(child, 'exit')
    child.stdin.end('\n')
    await exited
  }
  t.after(async () => { await Promise.all(children.map(release)) })
  const acquire = async directory => {
    const child = spawn('python3', ['-I', '-c', `import fcntl, json, os, sys
held = None
for index in range(3):
    slot = open(os.path.join(sys.argv[1], 'slot-' + str(index)), 'r+b')
    try:
        fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        slot.close()
        continue
    held = index
    break
print(json.dumps(held), flush=True)
if held is not None:
    sys.stdin.readline()
`, directory], { stdio: ['pipe', 'pipe', 'pipe'] })
    children.push(child)
    child.stdout.setEncoding('utf8')
    return await new Promise((resolve, reject) => {
      let output = '', stderr = ''
      child.stderr.setEncoding('utf8')
      child.stderr.on('data', chunk => { stderr += chunk })
      child.once('error', reject)
      child.stdout.on('data', chunk => {
        output += chunk
        if (output.includes('\n')) resolve({ child, slot: JSON.parse(output.trim()) })
      })
      child.once('exit', code => {
        if (!output.includes('\n')) reject(new Error(`Provider slot process exited ${code}: ${stderr}`))
      })
    })
  }
  const holders = []
  for (let profile = 0; profile < 3; profile++) holders.push(await acquire(slots(profile)))
  assert.deepEqual(holders.map(holder => holder.slot), [0, 1, 2])
  // Preparing again while locks are held must leave the same live lock inodes.
  prepareDeviceBenchmarkProviderSlots(input)
  assert.equal((await acquire(slots(3))).slot, null)
  assert.equal((await acquire(slots(3, 'provider-b'))).slot, 0)
  await release(holders[1].child)
  assert.equal((await acquire(slots(3))).slot, 1)
})

function completedRoleFixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'device-completed-role-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  mkdirSync(join(directory, 'server-data'))
  const db = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT, work_run_id TEXT,
    product_session_id TEXT, payload_digest TEXT, dispatch_payload BLOB, state TEXT, attempt INTEGER);
    CREATE TABLE device_execution_current_facts (job_id TEXT, client_node_id TEXT, client_instance_id TEXT,
    holder_user_id TEXT, repository_binding_id TEXT, occupancy_lease_id TEXT, occupancy_fencing_token INTEGER,
    worker_launch_grant_id TEXT, worker_session_id TEXT, worker_id TEXT, worker_instance_id TEXT,
    product_session_id TEXT, work_run_id TEXT);
    CREATE TABLE client_nodes (client_node_id TEXT, public_client_id TEXT);
    CREATE TABLE worker_launch_grants (worker_launch_grant_id TEXT, client_node_id TEXT, client_instance_id TEXT,
    holder_user_id TEXT, repository_binding_id TEXT, occupancy_lease_id TEXT, occupancy_fencing_token INTEGER,
    worker_session_id TEXT, worker_id TEXT, worker_instance_id TEXT, product_session_id TEXT,
    work_run_id TEXT, state TEXT, consumed_at TEXT);
    CREATE TABLE execution_leases (job_id TEXT, lease_id TEXT, payload_digest TEXT, worker_id TEXT,
    worker_instance_id TEXT, attempt INTEGER, fencing_token TEXT);
    CREATE TABLE execution_lease_terminals (job_id TEXT, lease_id TEXT, worker_id TEXT, worker_instance_id TEXT,
    attempt INTEGER, fencing_token TEXT, outcome TEXT)`)
  db.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?,?,?,?,?,?)').run(
    'job-a', 'delivery-a', 'run-a', 'role-a', 'digest-a', Buffer.from(JSON.stringify({
      jobId: 'job-a', payloadDigest: 'digest-a', attempt: 2,
      scope: { kind: 'work-run', workRunId: 'run-a', productSessionId: 'role-a', attempt: 2 },
    })), 'completed', 2)
  db.exec(`INSERT INTO device_execution_current_facts VALUES ('job-a','node-a','instance-a','user-a',
    'binding-a','occupancy-a',3,'grant-a','session-a','worker-a','worker-instance-a','role-a','run-a');
    INSERT INTO client_nodes VALUES ('node-a','client-a');
    INSERT INTO worker_launch_grants VALUES ('grant-a','node-a','instance-a','user-a','binding-a',
    'occupancy-a',3,'session-a','worker-a','worker-instance-a','role-a','run-a','consumed','earlier');
    INSERT INTO execution_leases VALUES ('job-a','lease-a','digest-a','worker-a','worker-instance-a',2,'4');
    INSERT INTO execution_lease_terminals VALUES ('job-a','lease-a','worker-a','worker-instance-a',2,'4','completed')`)
  const input = { directory, deliveryId: 'delivery-a', workRunId: 'run-a', publicClientId: 'client-a',
    holderUserId: 'user-a', repositoryBindingId: 'binding-a',
    response: { status: 400, json: { error: { code: 'INVALID_REQUEST' } }, text: 'launch request is invalid' } }
  return { db, input }
}

test('a role completed before manual launch retains its accepted anchor without another execution', t => {
  const { db, input } = completedRoleFixture(t)
  const before = db.prepare('SELECT * FROM worker_launch_grants').all()
  assert.deepEqual(deviceTaskLaunchResult(input), {
    workRunId: 'run-a', productSessionId: 'role-a', workerSessionId: 'session-a',
    workerId: 'worker-a', workerInstanceId: 'worker-instance-a', recoveredCompleted: true,
  })
  assert.deepEqual(db.prepare('SELECT * FROM worker_launch_grants').all(), before)
  assert.equal(db.prepare('SELECT count(*) AS n FROM scheduler_execution_jobs').get().n, 1)
})

test('completed anchor recovery rejects unrelated scopes and incomplete or replaced execution proof', t => {
  const { input } = completedRoleFixture(t)
  const rejected = value => assert.throws(() => deviceTaskLaunchResult(value),
    error => error.code === 'DEVICE_LAUNCH_FAILED')
  for (const key of ['deliveryId', 'workRunId', 'publicClientId', 'holderUserId', 'repositoryBindingId']) {
    rejected({ ...input, [key]: 'foreign' })
    rejected({ ...input, [key]: null })
  }
  rejected({ ...input, response: { ...input.response, status: 403 } })
  rejected({ ...input, response: { ...input.response, json: { error: { code: 'CAPACITY_EXHAUSTED' } } } })
  for (const mutation of [
    "UPDATE scheduler_execution_jobs SET state = 'failed'",
    "UPDATE scheduler_execution_jobs SET state = 'running'",
    "UPDATE scheduler_execution_jobs SET attempt = 3",
    "UPDATE scheduler_execution_jobs SET product_session_id = 'foreign'",
    "UPDATE scheduler_execution_jobs SET payload_digest = 'foreign'",
    "UPDATE worker_launch_grants SET state = 'issued', consumed_at = NULL",
    "UPDATE worker_launch_grants SET worker_session_id = 'foreign'",
    "UPDATE worker_launch_grants SET client_instance_id = 'foreign'",
    "UPDATE worker_launch_grants SET occupancy_fencing_token = 4",
    "UPDATE execution_lease_terminals SET outcome = 'failed'",
    "UPDATE execution_lease_terminals SET fencing_token = '5'",
    "UPDATE execution_lease_terminals SET worker_instance_id = 'foreign'",
    "DELETE FROM execution_lease_terminals",
    "UPDATE scheduler_execution_jobs SET dispatch_payload = '{\"scope\":{\"kind\":\"product-session\"}}'",
  ]) {
    const changed = completedRoleFixture(t)
    changed.db.exec(mutation)
    rejected(changed.input)
  }
  const accepted = { workerSessionId: 'fresh' }
  assert.equal(deviceTaskLaunchResult({ response: { status: 201, json: accepted } }), accepted)
})

test('task baselines preserve a dirty shared checkout and isolate frozen task trees', t => {
  const directory = mkdtempSync(join(tmpdir(), 'device-task-baselines-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const repository = join(directory, 'source')
  mkdirSync(repository)
  const git = (...args) => execFileSync('git', ['-C', repository, ...args], { encoding: 'utf8' }).trim()
  git('init', '--quiet')
  git('config', 'user.name', 'fixture')
  git('config', 'user.email', 'fixture@example.invalid')
  writeFileSync(join(repository, 'existing.txt'), 'committed\n')
  git('add', '.')
  git('commit', '--quiet', '-m', 'fixture')
  writeFileSync(join(repository, 'existing.txt'), 'user change\n')
  const before = git('status', '--porcelain')
  const head = git('rev-parse', 'HEAD')
  const input = content => ({ title: 'Frozen task', goal: 'Implement task', scope: ['answer.txt'],
    constraints: [], outOfScope: [], verificationCommand: 'test -s answer.txt',
    acceptanceCriteria: [{ id: 'answer', title: 'Answer exists', required: true }],
    files: { 'answer.txt': content } })
  const firstPath = join(directory, 'first.json'), secondPath = join(directory, 'second.json')
  writeFileSync(firstPath, JSON.stringify(input('first\n')))
  writeFileSync(secondPath, JSON.stringify(input('second\n')))
  const a = prepareDeviceTaskBaseline(repository, firstPath, join(directory, 'a'))
  const b = prepareDeviceTaskBaseline(repository, secondPath, join(directory, 'b'))
  const replay = prepareDeviceTaskBaseline(repository, firstPath, join(directory, 'replay'))
  assert.deepEqual(replay, a)
  assert.notEqual(a.baseline, b.baseline)
  assert.equal(git('show', `${a.baseline}:answer.txt`), 'first')
  assert.equal(git('show', `${b.baseline}:answer.txt`), 'second')
  assert.equal(git('rev-parse', 'HEAD'), head)
  assert.equal(git('status', '--porcelain'), before)
  assert.equal(readFileSync(join(repository, 'existing.txt'), 'utf8'), 'user change\n')
})

test('frozen launches retain identities across restart and distinct calls own distinct Sessions', () => {
  const first = deviceTaskIdentities('experiment', 'task:provider')
  assert.deepEqual(deviceTaskIdentities('experiment', 'task:provider'), first)
  for (const second of [deviceTaskIdentities('experiment', 'task:member:provider'),
    deviceTaskIdentities('other-experiment', 'task:provider')]) {
    assert.notEqual(first.productSessionId, second.productSessionId)
    assert.notEqual(first.deliveryId, second.deliveryId)
  }
  assert.match(first.productSessionId, /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
})

test('queued Controller roles are discovered only for their own Delivery before lease projection', t => {
  const directory = mkdtempSync(join(tmpdir(), 'device-pending-roles-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  mkdirSync(join(directory, 'server-data'))
  const db = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT, work_run_id TEXT,
    dispatch_payload BLOB, state TEXT, submitted_at TEXT)`)
  const run = 'wrn_01J00000000000000000000001'
  const insert = db.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?,?,?,?)')
  for (const [job, delivery, id, state] of [['reviewer', 'task-a', run, 'queued'],
    ['foreign-corrupt', 'task-b', 'invalid', 'queued'], ['done', 'task-a', 'invalid', 'completed']]) {
    insert.run(job, delivery, id, Buffer.from(JSON.stringify({ scope: { kind: 'work-run', workRunId: id } })), state, 'now')
  }
  assert.deepEqual(pendingDeviceTaskWorkRuns(directory, 'task-a'), [run])
  db.prepare("UPDATE scheduler_execution_jobs SET state = 'running' WHERE job_id = 'reviewer'").run()
  assert.deepEqual(pendingDeviceTaskWorkRuns(directory, 'task-a'), [])
  db.prepare("UPDATE scheduler_execution_jobs SET state = 'queued', dispatch_payload = '{}' WHERE job_id = 'reviewer'").run()
  assert.throws(() => pendingDeviceTaskWorkRuns(directory, 'task-a'))
})


test('a stopped task cancels only its active and queued roles and source Session', async () => {
  const commands = []
  const api = {
    query: async (name, payload) => {
      assert.equal(payload.deliveryId ?? 'task-a', 'task-a')
      return { result: name === 'workrun.get'
        ? { readCursor: { deliveryId: 'task-a' }, runs: [
          { id: 'active-a', state: 'running' }, { id: 'done-a', state: 'settled' }] }
        : name === 'delivery.get' ? { deliveryId: 'task-a', deliveryRevision: 3 }
          : { id: 'source-a', revision: 1, state: 'active' } }
    },
    command: async (name, revision, payload) => {
      commands.push({ name, revision, payload })
      return { outcome: 'completed' }
    },
  }
  const result = await cancelStoppedDeviceTask(api, '/missing-task-directory', 'task-a', 'source-a')
  assert.deepEqual(result.workRunIds, ['active-a'])
  assert.deepEqual(commands.map(command => [command.name, command.payload.workRunId ?? command.payload.productSessionId]),
    [['workrun.cancel', 'active-a'], ['session.cancel', 'source-a']])
})


test('benchmark admission waits for its original Device occupancy recovery without claiming another lease', async () => {
  const states = ['recovery_pending', 'occupied']
  const requests = []
  await ensureBenchmarkDeviceOccupancy({ devicePath: { publicClientId: 'device-a' }, api: {
    actor: { id: 'holder-a' },
    request: async (path, options) => {
      requests.push({ path, options })
      return { json: { occupancy: states.shift(), holderUserId: 'holder-a' } }
    },
  } })
  assert.equal(requests.length, 2)
  assert.ok(requests.every(request => request.path === '/api/v1/clients/device-a/occupancy' && request.options.method === undefined
    && request.options.timeoutMillis > 0 && request.options.timeoutMillis <= 300_000))
})

test('benchmark admission rejects another holder with a safe occupancy diagnostic', async () => {
  let requests = 0
  await assert.rejects(ensureBenchmarkDeviceOccupancy({ devicePath: { publicClientId: 'device-a' }, api: {
    actor: { id: 'holder-a' },
    request: async () => { requests++; return { json: { occupancy: 'recovery_pending', holderUserId: 'holder-b' } } },
  } }), { code: 'DEVICE_OCCUPANCY_FOREIGN_HOLDER', benchmarkPhase: 'occupancy' })
  assert.equal(requests, 1)
})
