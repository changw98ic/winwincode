// SPDX-License-Identifier: Apache-2.0
import { execFile, spawn } from 'node:child_process'
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises'
import { basename, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'

const execute = promisify(execFile)
const evidenceRoot = resolve('.cache/native-kernel-startup')
const maxSampleBytes = 512 * 1024

export function selectKernelProcess(table, rootPid) {
  const rows = table.split('\n').flatMap(line => {
    const match = /^\s*(\d+)\s+(\d+)\s+(.+)$/u.exec(line)
    return match === null ? [] : [{ pid: Number(match[1]), parent: Number(match[2]), command: match[3] }]
  })
  const owned = new Set([rootPid])
  for (let previous = -1; previous !== owned.size;) {
    previous = owned.size
    for (const row of rows) if (owned.has(row.parent)) owned.add(row.pid)
  }
  return rows.find(row => owned.has(row.pid) && basename(row.command).startsWith('code_mode_kernel-'))
}

async function inspectProcesses() {
  const { stdout } = await execute('ps', ['-axo', 'pid=,ppid=,comm='], {
    timeout: 2_000, maxBuffer: 2 * 1024 * 1024,
  })
  return stdout
}

async function retainInspectionFailure(failure) {
  await mkdir(evidenceRoot, { recursive: true })
  await writeFile(join(evidenceRoot, 'inspection-failure.json'), `${JSON.stringify(failure, null, 2)}\n`)
}

async function captureStack(target) {
  await mkdir(evidenceRoot, { recursive: true })
  const temporary = join(evidenceRoot, 'sample.tmp')
  let failure
  try {
    await execute('sample', [String(target.pid), '1', '10', '-file', temporary], {
      timeout: 15_000, maxBuffer: 64 * 1024,
    })
  } catch (error) {
    failure = { code: error.code ?? null, signal: error.signal ?? null }
  }
  const sample = await readFile(temporary).catch(() => Buffer.alloc(0))
  await writeFile(join(evidenceRoot, 'sample.txt'), sample.subarray(0, maxSampleBytes))
  await rm(temporary, { force: true })
  await writeFile(join(evidenceRoot, 'evidence.json'), `${JSON.stringify({
    schemaVersion: 1, capturedAt: new Date().toISOString(), target,
    sampleBytes: sample.length, retainedBytes: Math.min(sample.length, maxSampleBytes),
    failure: failure ?? null,
  }, null, 2)}\n`)
}

// The probe observes the actual test process after Cargo compilation. It never
// changes startup deadlines, serializes tests, retries, or terminates the test.
export async function runNativeKernelAcceptance(command, args, {
  platform = process.platform,
  inspect = inspectProcesses,
  capture = captureStack,
  recordInspectionFailure = retainInspectionFailure,
  captureAfterMillis = 20_000,
  pollMillis = 1_000,
  stdio = 'inherit',
} = {}) {
  const child = spawn(command, args, { stdio })
  const finished = new Promise((resolveResult, reject) => {
    child.once('error', reject)
    child.once('close', (code, signal) => resolveResult({ code, signal }))
  })
  const forwardInterrupt = () => child.kill('SIGINT')
  const forwardTermination = () => child.kill('SIGTERM')
  process.on('SIGINT', forwardInterrupt)
  process.on('SIGTERM', forwardTermination)
  let firstSeen
  let pending
  let sampled = false
  let inspectionFailure
  const timer = platform === 'darwin' ? setInterval(() => {
    if (pending !== undefined || sampled) return
    pending = (async () => {
      const target = selectKernelProcess(await inspect(child.pid), child.pid)
      if (target === undefined) return
      if (firstSeen?.pid !== target.pid) firstSeen = { pid: target.pid, at: Date.now() }
      if (Date.now() - firstSeen.at < captureAfterMillis) return
      sampled = true
      await capture(target)
    })().catch(error => {
      if (sampled || inspectionFailure === undefined) {
        process.stderr.write(`Native Kernel startup evidence unavailable (${sampled ? 'stack_capture' : 'process_inspection'}): ${error.code ?? error.name}\n`)
      }
      // A failed observation during compilation is not a completed sample.
      // Keep observing this command's descendants; never rerun the test.
      if (!sampled) {
        inspectionFailure ??= {
          schemaVersion: 1, stage: 'process_inspection', count: 0,
          code: String(error.code ?? error.name).slice(0, 64),
          signal: error.signal === undefined ? null : String(error.signal).slice(0, 16),
          killed: error.killed === true,
        }
        inspectionFailure.count = Math.min(Number.MAX_SAFE_INTEGER, inspectionFailure.count + 1)
      }
    }).finally(() => { pending = undefined })
  }, pollMillis) : undefined
  try {
    return await finished
  } finally {
    clearInterval(timer)
    process.off('SIGINT', forwardInterrupt)
    process.off('SIGTERM', forwardTermination)
    await pending
    if (inspectionFailure !== undefined) {
      try {
        await recordInspectionFailure({ ...inspectionFailure, sampled })
      } catch {
        process.stderr.write('Native Kernel process inspection evidence could not be retained\n')
      }
    }
  }
}

if (process.argv[1] !== undefined && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  const [command, ...args] = process.argv.slice(2)
  if (command === undefined) throw new Error('Native Kernel acceptance requires a command')
  await rm(evidenceRoot, { recursive: true, force: true })
  const result = await runNativeKernelAcceptance(command, args)
  if (result.signal !== null) process.kill(process.pid, result.signal)
  else process.exitCode = result.code ?? 1
}
