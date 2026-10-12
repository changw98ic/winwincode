// SPDX-License-Identifier: Apache-2.0

// Production acceptance observes the original Core cell and real OS processes.
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { existsSync, globSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readRetainedOutcomes } from './fixtures/chat-execution-transport-probe.mjs'

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'production acceptance requires explicitly sealed binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'),
    wwcBinary: join(directory, 'wwc') }
}

function realRun(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      const initialized = store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()
      if (!initialized) return []
      return store.prepare('SELECT record_json FROM codex_run').all()
        .map(row => JSON.parse(Buffer.from(row.record_json).toString('utf8')))
        .filter(run => run.job.jobId === jobId)
    } finally { store.close() }
  })[0]
}

function processState(pid) {
  let output
  try {
    output = execFileSync('ps', ['-p', String(pid), '-o', 'pid=', '-o', 'ppid=',
      '-o', 'pgid=', '-o', 'stat=', '-o', 'lstart=', '-o', 'command='], { encoding: 'utf8' }).trim()
  } catch (error) {
    assert.equal(error.status, 1, 'unexpected OS process observation error')
    assert.equal(Buffer.from(error.stdout ?? '').toString('utf8').trim(), '')
    return null
  }
  const match = /^(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(\S+\s+\S+\s+\d+\s+\S+\s+\d+)\s+(.*)$/u.exec(output)
  assert.ok(match, 'the OS process observation includes its identity and execution state')
  assert.equal(Number(match[1]), pid)
  return { pid, parentPid: Number(match[2]), processGroup: Number(match[3]),
    state: match[4], startedAt: match[5], command: match[6] }
}

function stoppedState(original) {
  const current = processState(original.pid)
  if (!current || current.startedAt !== original.startedAt) return 'original-gone'
  // A zombie is no longer executing, but its process entry has not been reaped.
  return current.state.startsWith('Z') ? 'zombie-awaiting-reap' : null
}

function shellQuote(value) { return `'${value.replaceAll("'", "'\\''")}'` }

for (const cut of ['approval-before-spawn', 'process-started']) {
  test(`Chat formal cancellation at ${cut} preserves raw Cancelled and fences actual shell side effects`, async t => {
    const directory = mkdtempSync(join(tmpdir(), `wwc-chat-tool-cut-${cut}-`))
    const observationDirectory = join(directory, 'tool-boundary-observation')
    mkdirSync(observationDirectory, { mode: 0o700 })
    const productSessionId = cut === 'approval-before-spawn'
      ? 'psn_01J00000000000000000000141' : 'psn_01J00000000000000000000142'
    const requestId = cut === 'approval-before-spawn'
      ? 'req_01J00000000000000000000141' : 'req_01J00000000000000000000142'
    let complete = false
    t.after(() => {
      if (complete) rmSync(directory, { recursive: true, force: true })
      else t.diagnostic(`Failed tool boundary evidence retained at ${directory}`)
    })
    // Terminal cleanup removes the Job checkout. This fixture-owned directory
    // survives that cleanup, so an absent after marker is observable evidence.
    // The exact original shell requires its formal approval to write here.
    const command = `observation=${shellQuote(observationDirectory)}; sleep 120 & child=$!; printf "%s\\n%s\\n" "$$" "$child" > "$observation/CHAT-TOOL-PIDS.txt"; printf "started\\n" > "$observation/CHAT-TOOL-STARTED.txt"; wait "$child"; printf "after\\n" >> "$observation/CHAT-TOOL-AFTER.txt"`
    const argumentsValue = { cmd: command, yield_time_ms: 1000,
      sandbox_permissions: 'require_escalated',
      justification: 'Observe this isolated shell and its durable fixture markers across formal cancellation.' }
    const native = `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); text(await tools[command.name](${JSON.stringify(argumentsValue)})); await new Promise(resolve=>setTimeout(resolve,120000)); text("cell remained alive after the observed shell call");`
    const report = await runApiProductionVertical({
      directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
      serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
      deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: { 'native-cell-A': native } },
      scenario: { async run({ api, modelRoute, devicePath }) {
        const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
        database.exec('PRAGMA busy_timeout=5000')
        try {
          await api.command('session.create', 0, { productSessionId,
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Real shell cancellation ${cut}`, modelRoute })
          await devicePath.launchAnchor({ productSessionId })
          await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, requestId)
          const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(requestId)
          const run = await waitFor(() => realRun(devicePath.deviceData, job.job_id), 'the real embedded Core run exists', 60_000)
          const pidPath = join(observationDirectory, 'CHAT-TOOL-PIDS.txt')
          const startedPath = join(observationDirectory, 'CHAT-TOOL-STARTED.txt')
          const afterPath = join(observationDirectory, 'CHAT-TOOL-AFTER.txt')
          const activities = async () => (await api.query('runtime.projection.get', {
            kind: 'product-session', productSessionId,
          })).result.sessions.find(value => value.productSessionId === productSessionId)?.activities ?? []
          let processes = []
          const approval = await waitFor(async () => {
            const rows = (await api.query('approval.list', { states: ['pending'] })).result.items
            for (const row of rows) {
              const value = (await api.query('approval.get', { approvalId: row.id })).result
              if (value.binding?.productSessionId === productSessionId && value.category === 'shell'
                && value.decisionEnabled) return value
            }
            return false
          }, 'the actual original shell request awaits its formal approval before OS spawn', 60_000)
          const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          assert.equal(approval.binding.executionJobId, job.job_id)
          assert.equal(approval.binding.productSessionId, productSessionId)
          assert.equal(approval.binding.workerSessionId, authority.worker_session_id)
          assert.equal(approval.binding.sessionIdentity.workerSessionId, authority.worker_session_id)
          assert.equal(approval.binding.sessionIdentity.productSessionId, productSessionId)
          assert.equal(approval.binding.sessionIdentity.codexThreadId, run.canonicalThreadId)
          assert.equal(existsSync(pidPath), false)
          assert.equal(existsSync(startedPath), false)
          if (cut === 'process-started') {
            const decision = await api.command('approval.decide', approval.revision, {
              approvalId: approval.id, binding: approval.binding, decision: 'approve',
              reason: 'Allow the original isolated shell to write only this fixture observation directory.',
            })
            assert.equal(decision.outcome, 'completed')
            processes = await waitFor(() => {
              if (!existsSync(pidPath) || !existsSync(startedPath)) return false
              const values = readFileSync(pidPath, 'utf8').trim().split('\n').map(Number)
              if (values.length !== 2 || !values.every(value => Number.isSafeInteger(value) && value > 1)) return false
              const observed = values.map(processState)
              if (observed.some(value => !value || value.state.startsWith('Z'))) return false
              assert.equal(observed[1].parentPid, observed[0].pid, 'the blocked child belongs to the original shell')
              return observed
            }, 'the real shell and its real blocked child both exist after OS spawn', 60_000)
            assert.equal(readFileSync(startedPath, 'utf8'), 'started\n')
            assert.equal(existsSync(afterPath), false)
          }
          const liveCell = await waitFor(async () => (await activities())
            .find(value => value.coreTool?.cell?.lifecycle === 'live')?.coreTool.cell,
          'the original Core cell remains live at the observed cancellation boundary', 60_000)
          const session = (await api.query('session.get', { productSessionId })).result
          const count = devicePath.modelServer.requests.length
          const started = performance.now()
          await api.command('session.cancel', session.revision, {
            productSessionId, reason: `Cancel at the observed ${cut} boundary`,
          })
          const settled = await waitFor(async () => {
            const current = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(job.job_id)
            const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
            const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
            const accepted = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
              .get(`execution-outcome:${job.job_id}:1`)
            const raw = accepted && JSON.parse(Buffer.from(accepted.payload).toString('utf8'))
            const [worker] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
            const messages = (await api.query('session.messages.list', { productSessionId })).result.items
            return current?.state === 'failed' && terminal?.outcome === 'cancelled'
              && reservation?.state === 'released' && reservation.release_reason === 'cancelled'
              && accepted?.revision === 2 && raw?.acknowledgementReady && worker?.workerAcknowledged
              && messages.some(value => value.role === 'assistant' && value.state === 'cancelled')
              ? { raw, worker, terminal, reservation } : false
          }, 'the exact original Worker Cancelled outcome receives its business ACK within ten seconds', 10_000)
          assert.ok(performance.now() - started < 10_000)
          assert.equal(settled.raw.message.outcome.status, 'cancelled')
          assert.deepEqual(settled.raw.message, settled.worker.message)
          const cleanupDeadline = () => Math.max(1, 10_000 - (performance.now() - started))
          await waitFor(async () => (await activities()).some(value =>
            value.coreTool?.cell?.cellId === liveCell.cellId && value.coreTool.cell.lifecycle === 'closed'),
          'the exact original Core cell is closed after formal cancellation', cleanupDeadline())
          let processTerminalStates = []
          if (cut === 'process-started') processTerminalStates = await waitFor(() => {
            const states = processes.map(stoppedState)
            return states.every(Boolean) ? states : false
          }, 'the cancelled Core cell stops both original OS processes and records their actual reap state', cleanupDeadline())
          else {
            const retainedApproval = (await api.query('approval.get', { approvalId: approval.id })).result
            assert.deepEqual(retainedApproval.binding, approval.binding)
            assert.deepEqual(retainedApproval.sanitizedDetail, approval.sanitizedDetail)
            assert.equal(retainedApproval.decisionEnabled, approval.decisionEnabled,
              'decisionEnabled records trusted action-fact completeness; current authority is rechecked by approval.decide')
            await assert.rejects(api.command('approval.decide', approval.revision, {
              approvalId: approval.id, binding: approval.binding, decision: 'approve',
              reason: 'A late decision must not spawn the obsolete shell operation.',
            }), error => {
              assert.equal(error.status, 403)
              assert.equal(error.code, 'PERMISSION_DENIED')
              return true
            })
            assert.equal(existsSync(pidPath), false)
            assert.equal(existsSync(startedPath), false)
          }
          assert.equal(existsSync(afterPath), false)
          assert.ok(existsSync(observationDirectory), 'the observation survives actual Job workspace cleanup')
          if (cut === 'process-started') {
            assert.equal(readFileSync(startedPath, 'utf8'), 'started\n')
            assert.deepEqual(readFileSync(pidPath, 'utf8').trim().split('\n').map(Number),
              processes.map(value => value.pid))
          }
          assert.equal(devicePath.modelServer.requests.length, count)
          assert.ok(performance.now() - started < 10_000,
            'terminal acceptance, original cell closure and observed process stopping share the real ten-second deadline')
          assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 1)
          return { cut, jobId: job.job_id, observedProcesses: processes, processTerminalStates,
            cellId: liveCell.cellId, cellClosed: true,
            actualProcessesStopped: processTerminalStates.length === 2,
            allProcessEntriesReaped: processTerminalStates.length === 2 && processTerminalStates.every(value => value === 'original-gone'),
            shellAfterSideEffects: 0, rawStatus: 'cancelled', businessAcknowledged: settled.worker.workerAcknowledged }
        } finally { database.close() }
      } },
    })
    t.diagnostic(JSON.stringify(report.flow.scenario))
    complete = true
  })
}
