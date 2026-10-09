import assert from 'node:assert/strict'
import test from 'node:test'
import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'

const supervisor = resolve(import.meta.dirname, '../scripts/benchmark/supervise-real-task-benchmark.py')
const record = async (directory, name) => JSON.parse(await readFile(resolve(directory, name), 'utf8'))

test('benchmark supervisor retains ordinary and signal exits and rejects duplicate launches', async t => {
  for (const terminated of [false, true]) {
    const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-supervisor-'))
    t.after(() => rm(directory, { recursive: true, force: true }))
    await writeFile(resolve(directory, 'config.json'), JSON.stringify({ experimentId: 'fixture', frozenSource: directory }))
    await writeFile(resolve(directory, 'build-artifacts.json'), JSON.stringify({ cargoBuildRan: true, exitCode: 0, sourceSnapshotMatches: true, artifacts: [] }))
    await writeFile(resolve(directory, 'build-progress.json'), JSON.stringify({ phase: 'complete' }))
    await writeFile(resolve(directory, 'run.mjs'), terminated
      ? 'process.kill(process.pid, "SIGTERM")' : 'process.exit(86)')
    const run = spawn('python3', [supervisor, directory, '--node', process.execPath], { stdio: 'pipe' })
    const [status] = await once(run, 'exit')
    assert.equal(status, 0)
    const launch = await record(directory, 'launch.json')
    const exit = await record(directory, 'controller-process-exit.json')
    assert.ok(launch.pid > 0 && launch.supervisorPid > 0)
    assert.equal(exit.pid, launch.pid)
    assert.equal(exit.returncode, terminated ? -15 : 86)
    assert.equal(exit.signal, terminated ? 'SIGTERM' : null)
    assert.equal(exit.batchResultPresent, false, 'process exit never fabricates batch completion')
    const duplicate = spawn('python3', [supervisor, directory, '--node', process.execPath], { stdio: 'pipe' })
    assert.notEqual((await once(duplicate, 'exit'))[0], 0)
    assert.deepEqual(await record(directory, 'launch.json'), launch)
  }
})
