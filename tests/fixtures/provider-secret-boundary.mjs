// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, globSync, lstatSync, readFileSync, realpathSync } from 'node:fs'
import { join, relative } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { captureDeviceWorkerProcess, waitFor } from '../../scripts/device-production-fixture.mjs'

const runtimeTables = Object.freeze([
  'accounting_closed_attempts', 'exchanges', 'jev_attempt_diagnostics',
  'jev_context_exchanges', 'jev_invocation_attempts', 'jev_judge_exchanges',
  'model_attempt_diagnostics', 'model_invocation_attempts', 'model_response_exchange',
  'model_response_frame', 'model_retry_state', 'runtime_origin',
].sort())

export function assertProviderSecretsAbsent(bytes, secrets, label) {
  const input = Buffer.from(bytes)
  for (const secret of secrets) {
    const raw = Buffer.from(secret)
    assert.ok(raw.length >= 16, 'Provider boundary sentinels must contain at least sixteen bytes')
    const needles = [raw, ...[0, 1, 2].map(skip => Buffer.from(raw
      .subarray(skip, raw.length - (raw.length - skip) % 3).toString('base64')))]
    assert.equal(needles.some(needle => input.includes(needle)), false,
      `${label} must not contain a Provider credential or private header`)
  }
}

export async function assertManagedProviderWorker({ deviceData, launched, secrets, jevSettingsPath }) {
  const worker = await waitFor(() => captureDeviceWorkerProcess({ deviceData, launched }),
    'the exact managed Worker process for credential boundary inspection', 30_000)
  const registry = new DatabaseSync(join(deviceData, 'device-client.sqlite3'), { readOnly: true })
  let dataDirectory
  try {
    registry.exec('PRAGMA busy_timeout=1000')
    dataDirectory = registry.prepare('SELECT data_directory FROM worker_process_registry WHERE worker_instance_id=?')
      .get(worker.workerInstanceId).data_directory
  } finally { registry.close() }
  const configuration = readFileSync(join(dataDirectory, 'managed-session.json'))
  const managed = JSON.parse(configuration)
  assert.equal(Object.hasOwn(managed, 'providerDirectory'), false)
  assert.equal(realpathSync(managed.providerRuntimeDirectory), realpathSync(join(deviceData, 'providers/runtime')))
  assertProviderSecretsAbsent(configuration, secrets, 'managed Worker configuration')
  let processBytes
  if (process.platform === 'linux') {
    processBytes = Buffer.concat([readFileSync(`/proc/${worker.pid}/cmdline`), readFileSync(`/proc/${worker.pid}/environ`)])
  } else {
    const observed = spawnSync('ps', ['eww', '-p', String(worker.pid), '-o', 'command='],
      { timeout: 5000, maxBuffer: 2 * 1024 * 1024 })
    assert.equal(observed.status, 0, 'managed Worker command and environment must be inspectable')
    processBytes = observed.stdout
  }
  assertProviderSecretsAbsent(processBytes, secrets, 'managed Worker argv and environment')
  assert.equal(processBytes.includes(Buffer.from('WWC_DEVICE_JEV_SETTINGS_FILE=')), false)
  if (jevSettingsPath) {
    assert.equal(processBytes.includes(Buffer.from(jevSettingsPath)), false,
      'the Worker must not inherit the Device JEV secret settings path')
    assert.equal(configuration.includes(Buffer.from(jevSettingsPath)), false)
  }
  assert.deepEqual(captureDeviceWorkerProcess({ deviceData, launched }), worker)
  return { workerInstanceId: worker.workerInstanceId, pid: worker.pid, processStartIdentity: worker.processStartIdentity }
}

export function assertProviderRuntimeBoundary({ directory, secrets }) {
  const deviceData = join(directory, 'device-data')
  const runtime = join(deviceData, 'providers/runtime')
  const database = new DatabaseSync(join(runtime, 'provider-runtime.sqlite3'), { readOnly: true })
  try {
    database.exec('PRAGMA busy_timeout=5000')
    const tables = database.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
      .all().map(row => row.name)
    assert.deepEqual(tables, runtimeTables, 'the Worker Provider runtime contains only the approved non-secret tables')
  } finally { database.close() }
  const configurations = globSync('**/managed-session.json', { cwd: deviceData })
  assert.ok(configurations.length > 0, 'the boundary probe must inspect actual managed Worker configurations')
  const paths = new Set(globSync('device-process*.log', { cwd: directory }).map(path => join(directory, path)))
  for (const subtree of [runtime, join(deviceData, 'worker-logs'), ...configurations.map(path => join(deviceData, path, '..'))]) {
    if (!existsSync(subtree)) continue
    for (const path of globSync('**/*', { cwd: subtree })) {
      const absolute = join(subtree, path)
      if (lstatSync(absolute).isFile()) paths.add(absolute)
    }
  }
  for (const path of paths) assertProviderSecretsAbsent(readFileSync(path), secrets, relative(directory, path))
  return { configurations: configurations.length, files: paths.size, runtimeTables: [...runtimeTables] }
}
