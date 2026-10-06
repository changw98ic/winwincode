import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { pathToFileURL } from 'node:url'
import { assertBenchmarkExecutionReceipts } from '../lib/benchmark-execution-receipts.mjs'
import { validateBenchmarkConfiguration } from './run-real-task-benchmark.mjs'

const digest = bytes => createHash('sha256').update(bytes).digest('hex')
const fail = code => Object.assign(new Error(code), { code })
const taskIdPattern = /^[a-z0-9]+(?:-[a-z0-9]+)*$/u
const objectIdPattern = /^[0-9a-f]{40}$/u
const sourcePath = path => typeof path === 'string' && path.length > 0 && !isAbsolute(path)
  && !path.includes('\\') && !/[\u0000-\u001f\u007f]/u.test(path)
  && path.split('/').every(part => part && part !== '.' && part !== '..' && part.toLowerCase() !== '.git')
const inside = (root, path) => {
  const rel = relative(root, path)
  return rel !== '' && rel !== '..' && !rel.startsWith(`..${sep}`) && !isAbsolute(rel)
}

function candidateFileBytes(file) {
  if (!sourcePath(file.path) || file.kind !== 'blob' || !['100644', '100755'].includes(file.mode)
      || file.encoding !== 'utf8' || typeof file.content !== 'string') throw fail('SUBMISSION_SOURCE_INVALID')
  const bytes = Buffer.from(file.content, 'utf8')
  const gitObject = createHash('sha1').update(`blob ${bytes.length}\0`).update(bytes).digest('hex')
  if (file.objectId !== gitObject) throw fail('SUBMISSION_SOURCE_INVALID')
  return bytes
}

/** Validates a completed product result before it can enter the public submission repository. */
export function prepareBenchmarkSubmission(record, { experimentId, preparedInputsDirectory, preparedCatalogSha256,
  productSourceSealSha256 }) {
  if (!experimentId || !preparedCatalogSha256 || record?.status !== 'completed' || record.termination || record.failure
      || !taskIdPattern.test(record.taskId)) throw fail('SUBMISSION_NOT_COMPLETED')
  const model = record.fusionKind === 'independent-aggregate' ? record.aggregate : record.model
  if (model?.status !== 'completed' || !model.candidate || !model.submissionManifest) {
    throw fail('SUBMISSION_NOT_COMPLETED')
  }
  const directory = resolve(model.directory)
  const manifestPath = resolve(model.submissionManifest.path)
  if (!inside(directory, manifestPath)) throw fail('SUBMISSION_EVIDENCE_INVALID')
  const manifestBytes = readFileSync(manifestPath)
  if (digest(manifestBytes) !== model.submissionManifest.sha256) throw fail('SUBMISSION_EVIDENCE_INVALID')
  const manifest = JSON.parse(manifestBytes)
  if (manifest.productComplete !== true || JSON.stringify(manifest.candidate) !== JSON.stringify(model.candidate)
      || JSON.stringify(manifest.configuration) !== JSON.stringify(validateBenchmarkConfiguration(record))
      || manifest.productSourceSealSha256 !== model.productSourceSealSha256
      || (productSourceSealSha256 && manifest.productSourceSealSha256 !== productSourceSealSha256)) {
    throw fail('SUBMISSION_EVIDENCE_INVALID')
  }
  try {
    const receiptBytes = readFileSync(join(dirname(manifestPath), 'execution-receipts.json'))
    if (digest(receiptBytes) !== manifest.executionReceiptsSha256
        || JSON.stringify(JSON.parse(receiptBytes)) !== JSON.stringify(model.executionReceipts)) {
      throw fail('SUBMISSION_EVIDENCE_INVALID')
    }
    assertBenchmarkExecutionReceipts(model.executionReceipts)
  }
  catch { throw fail('SUBMISSION_EVIDENCE_INVALID') }
  const bindingBytes = readFileSync(join(directory, 'task-source-binding.json'))
  const binding = JSON.parse(bindingBytes)
  if (digest(bindingBytes) !== model.taskSourceBindingSha256 || binding.runId !== record.runId
      || binding.taskId !== record.taskId || binding.taskInputSha256 !== manifest.taskInputSha256) {
    throw fail('SUBMISSION_EVIDENCE_INVALID')
  }
  if (digest(readFileSync(resolve(dirname(manifestPath), 'task-input.json'))) !== manifest.taskInputSha256) {
    throw fail('SUBMISSION_EVIDENCE_INVALID')
  }
  const catalog = JSON.parse(readFileSync(resolve(preparedInputsDirectory, 'prepared-inputs.json'), 'utf8'))
  if (digest(JSON.stringify(catalog)) !== preparedCatalogSha256) throw fail('SUBMISSION_TASK_INVALID')
  const taskRecord = catalog.tasks.find(task => task.taskId === record.taskId)
  if (!taskRecord) throw fail('SUBMISSION_TASK_INVALID')
  const artifactPath = resolve(preparedInputsDirectory, `${record.taskId}.json`)
  const artifactBytes = readFileSync(artifactPath)
  const artifact = JSON.parse(artifactBytes)
  if (digest(artifactBytes) !== taskRecord.sha256 || artifact.taskId !== record.taskId
      || artifact.spec?.id !== record.taskId
      || !sourcePath(artifact.spec.entry) || !Array.isArray(artifact.spec.allowed_suffixes)
      || !Number.isSafeInteger(artifact.spec.max_submission_files)
      || !Number.isSafeInteger(artifact.spec.max_submission_bytes)) throw fail('SUBMISSION_TASK_INVALID')
  const filesPath = resolve(dirname(manifestPath), 'candidate-files.json')
  const filesBytes = readFileSync(filesPath)
  if (digest(filesBytes) !== manifest.candidateFilesSha256) throw fail('SUBMISSION_EVIDENCE_INVALID')
  const candidate = JSON.parse(filesBytes)
  if (!objectIdPattern.test(candidate.commit) || !objectIdPattern.test(candidate.tree)
      || candidate.commit !== manifest.candidate.candidateCommitId
      || candidate.tree !== manifest.candidate.candidateTreeId || !Array.isArray(candidate.files)) {
    throw fail('SUBMISSION_EVIDENCE_INVALID')
  }
  const files = candidate.files.filter(file => artifact.spec.allowed_suffixes
    .some(suffix => file.path?.endsWith(suffix)))
    .map(file => ({ path: file.path, mode: file.mode, bytes: candidateFileBytes(file) }))
  const names = new Set(files.map(file => file.path))
  if (!names.has(artifact.spec.entry) || names.size !== files.length || files.length > artifact.spec.max_submission_files
      || files.reduce((sum, file) => sum + file.bytes.length, 0) > artifact.spec.max_submission_bytes) {
    throw fail('SUBMISSION_SOURCE_INVALID')
  }
  verifyCandidateBundle(dirname(manifestPath), manifest, candidate, files)
  const branch = `submit/${record.taskId}/run-${digest(`${experimentId}\n${record.runId}`).slice(0, 32)}`
  return { branch, experimentId, runId: record.runId, taskId: record.taskId,
    candidateCommitId: manifest.candidate.candidateCommitId, manifestSha256: digest(manifestBytes),
    taskSha256: digest(artifactBytes), files }
}

function gitBytes(directory, args, env) {
  const result = spawnSync('git', ['-C', directory, ...args], { env,
    stdio: ['ignore', 'pipe', 'pipe'] })
  if (result.status !== 0) throw fail('SUBMISSION_GIT_FAILED')
  return result.stdout
}
const git = (directory, args, env) => gitBytes(directory, args, env).toString('utf8').trim()

function verifyCandidateBundle(directory, manifest, candidate, selectedFiles) {
  const bundle = readFileSync(join(directory, 'candidate.bundle'))
  if (digest(bundle) !== manifest.bundleSha256) throw fail('SUBMISSION_EVIDENCE_INVALID')
  const scratch = mkdtempSync(join(tmpdir(), 'wwc-benchmark-verify-'))
  const env = { PATH: process.env.PATH, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' }
  try {
    git(scratch, ['init', '--bare', '--quiet'], env)
    git(scratch, ['bundle', 'verify', join(directory, 'candidate.bundle')], env)
    git(scratch, ['bundle', 'unbundle', join(directory, 'candidate.bundle')], env)
    if (git(scratch, ['rev-parse', `${candidate.commit}^{commit}`], env) !== candidate.commit
        || git(scratch, ['rev-parse', `${candidate.commit}^{tree}`], env) !== candidate.tree) {
      throw fail('SUBMISSION_EVIDENCE_INVALID')
    }
    const entries = gitBytes(scratch, ['ls-tree', '-r', '-z', candidate.commit], env)
      .toString('utf8').split('\0').filter(Boolean).map(row => {
        const separator = row.indexOf('\t')
        if (separator <= 0) throw fail('SUBMISSION_EVIDENCE_INVALID')
        const [mode, kind, objectId] = row.slice(0, separator).split(' ')
        return { path: row.slice(separator + 1), mode, kind, objectId }
      })
    if (JSON.stringify(entries) !== JSON.stringify(candidate.files.map(({ path, mode, kind, objectId }) =>
      ({ path, mode, kind, objectId })))) throw fail('SUBMISSION_EVIDENCE_INVALID')
    for (const file of selectedFiles) {
      const objectId = candidate.files.find(entry => entry.path === file.path)?.objectId
      if (!gitBytes(scratch, ['cat-file', 'blob', objectId], env).equals(file.bytes)) {
        throw fail('SUBMISSION_EVIDENCE_INVALID')
      }
    }
  } finally { rmSync(scratch, { recursive: true, force: true }) }
}

/** Pushes an orphan commit containing only allowed source files. Existing refs are immutable. */
export function publishBenchmarkSubmission(prepared, { repositoryUrl, receiptDirectory, scratchRoot = tmpdir() }) {
  if (typeof repositoryUrl !== 'string' || !repositoryUrl || repositoryUrl.startsWith('-')) {
    throw fail('SUBMISSION_REPOSITORY_INVALID')
  }
  if (!prepared || !taskIdPattern.test(prepared.taskId)
      || prepared.branch !== `submit/${prepared.taskId}/run-${digest(`${prepared.experimentId}\n${prepared.runId}`).slice(0, 32)}`
      || !Array.isArray(prepared.files) || prepared.files.length === 0
      || prepared.files.some(file => !sourcePath(file.path) || !['100644', '100755'].includes(file.mode)
        || !Buffer.isBuffer(file.bytes))) throw fail('SUBMISSION_SOURCE_INVALID')
  const directory = mkdtempSync(join(scratchRoot, 'wwc-benchmark-submit-'))
  const baseEnvironment = Object.fromEntries(['PATH', 'HOME', 'USER', 'LOGNAME', 'SSH_AUTH_SOCK']
    .filter(key => process.env[key] !== undefined).map(key => [key, process.env[key]]))
  const buildEnvironment = { ...baseEnvironment, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_AUTHOR_NAME: 'WinWinCode Benchmark Scheduler', GIT_AUTHOR_EMAIL: 'benchmark@winwincode.invalid',
    GIT_COMMITTER_NAME: 'WinWinCode Benchmark Scheduler', GIT_COMMITTER_EMAIL: 'benchmark@winwincode.invalid',
    GIT_AUTHOR_DATE: '2000-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2000-01-01T00:00:00Z' }
  try {
    git(directory, ['init', '--quiet'], buildEnvironment)
    for (const file of prepared.files) {
      const path = join(directory, prepared.taskId, file.path)
      mkdirSync(dirname(path), { recursive: true, mode: 0o700 })
      writeFileSync(path, file.bytes, { flag: 'wx', mode: 0o600 })
      if (file.mode === '100755') chmodSync(path, 0o700)
    }
    git(directory, ['-c', 'core.hooksPath=/dev/null', 'add', '--', prepared.taskId], buildEnvironment)
    git(directory, ['-c', 'core.hooksPath=/dev/null', 'commit', '--quiet', '-m',
      `Benchmark ${prepared.experimentId} ${prepared.runId}\nCandidate ${prepared.candidateCommitId}\nManifest ${prepared.manifestSha256}`],
    buildEnvironment)
    const commit = git(directory, ['rev-parse', 'HEAD'], buildEnvironment)
    assert.match(commit, objectIdPattern)
    const ref = `refs/heads/${prepared.branch}`
    const remote = git(directory, ['ls-remote', '--heads', repositoryUrl, ref], baseEnvironment)
    const existing = remote ? remote.split('\t')[0] : null
    if (existing && existing !== commit) throw fail('SUBMISSION_REF_CONFLICT')
    if (!existing) git(directory, ['-c', 'core.hooksPath=/dev/null', 'push', '--porcelain', repositoryUrl,
      `${commit}:${ref}`], baseEnvironment)
    const observed = git(directory, ['ls-remote', '--heads', repositoryUrl, ref], baseEnvironment).split('\t')[0]
    if (observed !== commit) throw fail('SUBMISSION_REMOTE_MISMATCH')
    const receipt = { experimentId: prepared.experimentId, runId: prepared.runId,
      taskId: prepared.taskId, branch: prepared.branch, commit,
      candidateCommitId: prepared.candidateCommitId, manifestSha256: prepared.manifestSha256,
      taskSha256: prepared.taskSha256 }
    mkdirSync(receiptDirectory, { recursive: true, mode: 0o700 })
    const path = join(receiptDirectory, `${digest(`${prepared.experimentId}\n${prepared.runId}`)}.publication.json`)
    const bytes = `${JSON.stringify(receipt, null, 2)}\n`
    try { writeFileSync(path, bytes, { flag: 'wx', mode: 0o600 }) } catch (error) {
      if (error.code !== 'EEXIST') throw error
      assert.equal(readFileSync(path, 'utf8'), bytes)
    }
    return receipt
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
}

/** Publishes a sealed 700-row ledger after execution, with no model or product command. */
export function publishBenchmarkLedger({ ledgerPath, preparedInputsDirectory, repositoryUrl, receiptDirectory }) {
  const database = new DatabaseSync(resolve(ledgerPath), { readOnly: true })
  let identity
  let rows
  try {
    identity = JSON.parse(database.prepare('SELECT identity FROM benchmark_identity WHERE id = 1').get().identity)
    rows = database.prepare('SELECT run_id, record FROM benchmark_cell ORDER BY ordinal').all()
  } finally { database.close() }
  const binding = identity.experimentBinding
  if (!binding?.experimentId || !binding.preparedCatalogSha256 || !binding.productSourceSealSha256
      || rows.length !== 700 || identity.plan?.cells?.length !== 700
      || rows.some((row, index) => row.record === null || row.run_id !== identity.plan.cells[index].runId)) {
    throw fail('SUBMISSION_LEDGER_INVALID')
  }
  const receipts = []
  for (const [index, row] of rows.entries()) {
    const record = JSON.parse(row.record)
    const cell = identity.plan.cells[index]
    if (record.runId !== row.run_id || record.taskId !== cell.taskId
        || record.configurationId !== cell.configurationId || record.comparison !== cell.comparison
        || !['completed', 'failed', 'not_run_runner_terminated'].includes(record.status)
        || record.score !== null || record.verdict !== null) throw fail('SUBMISSION_LEDGER_INVALID')
    if (record.status !== 'completed') continue
    const prepared = prepareBenchmarkSubmission(record, { experimentId: binding.experimentId,
      preparedInputsDirectory, preparedCatalogSha256: binding.preparedCatalogSha256,
      productSourceSealSha256: binding.productSourceSealSha256 })
    receipts.push(publishBenchmarkSubmission(prepared, { repositoryUrl, receiptDirectory }))
  }
  return { denominator: rows.length, published: receipts.length, receipts }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    assert.equal(process.argv.length, 3, 'Usage: publish-benchmark-submission.mjs config.json')
    const config = JSON.parse(readFileSync(resolve(process.argv[2]), 'utf8'))
    const result = publishBenchmarkLedger(config)
    console.log(JSON.stringify({ denominator: result.denominator, published: result.published }))
  } catch (error) {
    console.error(JSON.stringify({ code: typeof error.code === 'string' && /^[A-Z][A-Z0-9_]{0,127}$/u.test(error.code)
      ? error.code : 'SUBMISSION_PUBLISH_FAILED' }))
    process.exitCode = 1
  }
}
