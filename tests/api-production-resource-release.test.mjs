import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { delimiter, join, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'
import test from 'node:test'
import { pathToFileURL } from 'node:url'

const root = resolve(import.meta.dirname, '..')
const sqliteBinary = process.env.WWC_SQLITE3_BIN ?? 'sqlite3'
const configuredDirectories = process.env.WWC_API_RESOURCE_AUDIT_DIRECTORIES
  ?? process.env.WWC_API_RESOURCE_AUDIT_DIRECTORY

function auditDirectories() {
  assert.ok(
    configuredDirectories,
    'set WWC_API_RESOURCE_AUDIT_DIRECTORY (or ..._DIRECTORIES) to a sealed API run',
  )
  return configuredDirectories
    .split(delimiter)
    .map(directory => resolve(root, directory))
    .filter((directory, index, all) => all.indexOf(directory) === index)
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function sqliteJson(database, query) {
  // A cleanly checkpointed WAL database may retain the WAL header in the main
  // file while having no sidecars. sqlite3 -readonly then tries to initialise
  // WAL bookkeeping and fails with SQLITE_CANTOPEN. The fixture is sealed, so
  // immutable mode is the exact read-only representation in that case. When a
  // sidecar exists we must use the regular read-only path so its transactions
  // remain part of the audit.
  const hasWalSidecars = existsSync(`${database}-wal`) || existsSync(`${database}-shm`)
  const arguments_ = hasWalSidecars
    ? ['-readonly', '-json', database, query]
    : ['-json', `${pathToFileURL(database).href}?immutable=1`, query]
  const result = spawnSync(sqliteBinary, arguments_, {
    cwd: root,
    encoding: 'utf8',
    env: {
      ...process.env,
      GIT_CONFIG_GLOBAL: '/dev/null',
      GIT_CONFIG_NOSYSTEM: '1',
    },
  })
  assert.equal(
    result.status,
    0,
    `sqlite resource audit failed for ${database}: ${result.stderr.trim()}`,
  )
  return result.stdout.trim().length === 0 ? [] : JSON.parse(result.stdout)
}

function gitReferenceTarget(repository, gitCommonDirectory, reference) {
  const arguments_ = existsSync(repository)
    ? ['-C', repository, 'rev-parse', '--verify', '--quiet', reference]
    : existsSync(gitCommonDirectory)
      ? ['--git-dir', gitCommonDirectory, 'rev-parse', '--verify', '--quiet', reference]
      : null
  if (arguments_ === null) return null
  const result = spawnSync('git', arguments_, {
      cwd: root,
      encoding: 'utf8',
      env: {
        ...process.env,
        GIT_CONFIG_GLOBAL: '/dev/null',
        GIT_CONFIG_NOSYSTEM: '1',
      },
    },
  )
  assert.ok(
    result.status === 0 || result.status === 1,
    `Git reference audit failed for ${repository ?? gitCommonDirectory}: ${result.stderr.trim()}`,
  )
  return result.status === 0 ? result.stdout.trim() : null
}

function assertRestartAndTerminalProjections(report, directory) {
  assert.equal(report.health.initial, 'ready', `${directory}: initial Server health`)
  assert.equal(report.health.afterRestart, 'ready', `${directory}: restart Server health`)
  assert.equal(report.flow.chat.status, 'Completed', `${directory}: Chat terminal`)
  assert.equal(report.flow.cancel.state, 'cancelled', `${directory}: cancellation terminal`)
  assert.equal(report.flow.strongflow.status, 'delivered', `${directory}: Delivery terminal`)
  assert.equal(report.flow.strongflow.verdictStatus, 'pass', `${directory}: verdict terminal`)
  assert.equal(report.restart.deliveryBytesStable, true, `${directory}: Delivery restart bytes`)
  assert.equal(report.restart.messageBytesStable, true, `${directory}: Chat restart bytes`)
  assert.equal(report.restart.status, 'delivered', `${directory}: restart Delivery status`)
  assert.equal(report.deterministic.contentEqual, true, `${directory}: repeated Chat content`)
}

function assertReadClosure(database, directory) {
  const closure = sqliteJson(
    database,
    `SELECT stream_id, revision
       FROM product_state
      WHERE stream_id LIKE 'delivery-candidate-reads-closed:%'`,
  )
  assert.equal(closure.length, 1, `${directory}: expected one reads-closed state`)
  assert.equal(closure[0].revision, 1, `${directory}: reads-closed state revision`)
  const outbox = sqliteJson(
    database,
    `SELECT event_id
       FROM outbox
      WHERE topic = 'delivery.candidate.git-reads-closed.v1'`,
  )
  assert.equal(outbox.length, 1, `${directory}: expected one reads-closed outbox event`)
}

function retentionRows(database) {
  return sqliteJson(
    database,
    `SELECT binding_key, artifact_id, reference_name, state, record_json
       FROM git_candidate_retentions
      ORDER BY binding_key`,
  ).map(row => ({ ...row, record: JSON.parse(row.record_json) }))
}

function assertNoActiveRuntimeResources(database, directory) {
  const checks = [
    [
      'active leases',
      `SELECT l.job_id
         FROM execution_leases AS l
         LEFT JOIN execution_lease_terminals AS t ON t.lease_id = l.lease_id
        WHERE t.lease_id IS NULL`,
    ],
    [
      'active dispatch authorities',
      `SELECT d.job_id
         FROM execution_dispatch_authorities AS d
         LEFT JOIN execution_lease_terminals AS t ON t.lease_id = d.lease_id
        WHERE t.lease_id IS NULL`,
    ],
    [
      'active Worker slots',
      `SELECT worker_session_id
         FROM worker_session_slots
        WHERE state IN ('running', 'cancelling')`,
    ],
    [
      'active execution reservations',
      `SELECT job_id
         FROM execution_admission_reservations
        WHERE state IN ('queued', 'running')`,
    ],
    [
      'active scheduler jobs',
      `SELECT job_id
         FROM scheduler_execution_jobs
        WHERE state IN ('queued', 'leased', 'running', 'cancelling')`,
    ],
    [
      'pending Worker frames',
      'SELECT message_id FROM internal_worker_outbound_messages',
    ],
  ]
  for (const [label, query] of checks) {
    const rows = sqliteJson(database, query)
    assert.equal(rows.length, 0, `${directory}: ${label} remain (${rows.length})`)
  }
}

test('API terminal resource audit releases every candidate pin after restart', () => {
  for (const directory of auditDirectories()) {
    const reportPath = join(directory, 'report.json')
    const database = join(directory, 'server-data', 'control-plane.sqlite3')
    const report = readJson(reportPath)
    assertRestartAndTerminalProjections(report, directory)
    assertReadClosure(database, directory)

    const rows = retentionRows(database)
    assert.equal(rows.length, 3, `${directory}: expected three terminal candidate pins`)
    assert.equal(
      rows.filter(row => row.state === 'pinned').length,
      0,
      `${directory}: candidate pins remain pinned after terminal/restart`,
    )
    assert.equal(
      rows.every(row => row.state === 'released'),
      true,
      `${directory}: every candidate retention must be released`,
    )
    assert.equal(
      rows.every(row => row.record.release !== null),
      true,
      `${directory}: every candidate retention needs a durable release receipt`,
    )
    assert.equal(
      rows.every(row => row.record.release.terminalOutcome === report.flow.strongflow.status),
      true,
      `${directory}: release outcome must match the terminal Delivery outcome`,
    )

    for (const row of rows) {
      const record = row.record
      const target = gitReferenceTarget(
        record.repositoryPath,
        record.gitCommonDirectory,
        row.reference_name,
      )
      assert.equal(
        target,
        null,
        `${directory}: released candidate Git reference must be absent`,
      )
    }
    assertNoActiveRuntimeResources(database, directory)
  }
})
