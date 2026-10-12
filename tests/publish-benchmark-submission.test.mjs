import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import test from 'node:test'
import { prepareBenchmarkSubmission, publishBenchmarkLedger,
  publishBenchmarkSubmission } from '../scripts/publish-benchmark-submission.mjs'
import { buildBenchmarkPlan, runBenchmarkPlan,
  validateBenchmarkConfiguration } from '../scripts/run-real-task-benchmark.mjs'

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
const gitBlob = bytes => createHash('sha1').update(`blob ${bytes.length}\0`).update(bytes).digest('hex')

async function fixture(root, { path = 'main.py', mode = '100644', content = 'print(1)\n', maxBytes = 1024 } = {}) {
  const preparedInputsDirectory = resolve(root, 'inputs')
  const directory = resolve(root, 'candidate')
  const manifestDirectory = resolve(directory, 'submission-evidence', 'candidate')
  await mkdir(preparedInputsDirectory, { recursive: true })
  await mkdir(manifestDirectory, { recursive: true })
  const sourceRepository = resolve(root, 'git-source')
  await mkdir(sourceRepository, { recursive: true })
  execFileSync('git', ['init', '--quiet', sourceRepository])
  await writeFile(resolve(sourceRepository, 'TASK.md'), 'private prompt\n')
  await writeFile(resolve(sourceRepository, 'main.py'), content)
  execFileSync('git', ['-C', sourceRepository, 'add', '.'])
  execFileSync('git', ['-C', sourceRepository, '-c', 'user.name=Benchmark Test',
    '-c', 'user.email=benchmark@example.invalid', 'commit', '--quiet', '-m', 'candidate'])
  const commit = execFileSync('git', ['-C', sourceRepository, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
  const tree = execFileSync('git', ['-C', sourceRepository, 'rev-parse', 'HEAD^{tree}'], { encoding: 'utf8' }).trim()
  const bundlePath = resolve(manifestDirectory, 'candidate.bundle')
  execFileSync('git', ['-C', sourceRepository, 'bundle', 'create', bundlePath, 'HEAD'])
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const artifact = { taskId: cell.taskId, spec: { id: cell.taskId, entry: 'main.py',
    allowed_suffixes: ['.py'], max_submission_files: 2, max_submission_bytes: maxBytes } }
  const artifactBytes = Buffer.from(`${JSON.stringify(artifact)}\n`)
  await writeFile(resolve(preparedInputsDirectory, `${cell.taskId}.json`), artifactBytes)
  const catalog = { tasks: [{ taskId: cell.taskId, sha256: sha256(artifactBytes) }] }
  await writeFile(resolve(preparedInputsDirectory, 'prepared-inputs.json'), JSON.stringify(catalog))
  const source = Buffer.from(content)
  const candidate = { candidateCommitId: commit, candidateTreeId: tree }
  const files = { commit: candidate.candidateCommitId, tree: candidate.candidateTreeId, files: [
    { path: 'TASK.md', mode: '100644', kind: 'blob', encoding: 'utf8', content: 'private prompt\n',
      objectId: gitBlob(Buffer.from('private prompt\n')) },
    { path, mode, kind: 'blob', encoding: 'utf8', content,
      objectId: gitBlob(source) },
  ] }
  const filesBytes = Buffer.from(`${JSON.stringify(files)}\n`)
  await writeFile(resolve(manifestDirectory, 'candidate-files.json'), filesBytes)
  const taskInput = Buffer.from('frozen task input\n')
  await writeFile(resolve(manifestDirectory, 'task-input.json'), taskInput)
  const taskInputSha256 = sha256(taskInput)
  const binding = { runId: cell.runId, taskId: cell.taskId, taskInputSha256 }
  const bindingBytes = Buffer.from(`${JSON.stringify(binding)}\n`)
  await writeFile(resolve(directory, 'task-source-binding.json'), bindingBytes)
  const productSourceSealSha256 = 'd'.repeat(64)
  const executionReceipts = { calls: [{ exchangeId: 'mdl_fixture', requestedModel: cell.comparison,
    reasoningEffort: 'max', actualModels: [cell.comparison], terminalType: 'completed',
    usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2 } }], jev: [],
  completeUsage: true, totalTokens: 2, actualCost: null }
  const receiptBytes = Buffer.from(`${JSON.stringify(executionReceipts)}\n`)
  await writeFile(resolve(manifestDirectory, 'execution-receipts.json'), receiptBytes)
  const manifest = { productComplete: true, candidate, configuration: validateBenchmarkConfiguration(cell),
    productSourceSealSha256, taskInputSha256,
    candidateFilesSha256: sha256(filesBytes), executionReceiptsSha256: sha256(receiptBytes),
    bundleSha256: sha256(await readFile(bundlePath)) }
  const manifestBytes = Buffer.from(`${JSON.stringify(manifest)}\n`)
  const manifestPath = resolve(manifestDirectory, 'manifest.json')
  await writeFile(manifestPath, manifestBytes)
  const record = { ...cell, status: 'completed', termination: null,
    model: { status: 'completed', directory, candidate,
      submissionManifest: { path: manifestPath, sha256: sha256(manifestBytes) },
      taskSourceBindingSha256: sha256(bindingBytes), productSourceSealSha256, executionReceipts } }
  const options = { experimentId: 'formal-700', preparedInputsDirectory,
    preparedCatalogSha256: sha256(JSON.stringify(catalog)) }
  return { record, options, manifestPath, manifestDirectory }
}

test('publishes only allowed source, keeps a stable ref, and rejects a changed remote ref', async t => {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-publish-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const remote = resolve(root, 'submissions.git')
  execFileSync('git', ['init', '--bare', '--quiet', remote])
  const { record, options } = await fixture(root)
  const prepared = prepareBenchmarkSubmission(record, options)
  const publishOptions = { repositoryUrl: remote, receiptDirectory: resolve(root, 'receipts') }
  const first = publishBenchmarkSubmission(prepared, publishOptions)
  assert.equal(first.branch, prepared.branch)
  assert.deepEqual(execFileSync('git', ['--git-dir', remote, 'ls-tree', '-r', '--name-only', first.commit],
    { encoding: 'utf8' }).trim().split('\n'), [`${record.taskId}/main.py`])
  assert.deepEqual(publishBenchmarkSubmission(prepared, publishOptions), first)
  const receipt = JSON.parse(await readFile(resolve(publishOptions.receiptDirectory,
    `${sha256(`${options.experimentId}\n${record.runId}`)}.publication.json`)))
  assert.deepEqual(receipt, first)
  assert.throws(() => publishBenchmarkSubmission({ ...prepared, candidateCommitId: 'f'.repeat(40) }, publishOptions),
    { code: 'SUBMISSION_REF_CONFLICT' })
})

test('rejects uncompleted, changed, unsafe, and oversized candidate evidence before publishing', async t => {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-invalid-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const valid = await fixture(resolve(root, 'valid'))
  assert.throws(() => prepareBenchmarkSubmission({ ...valid.record, status: 'failed' }, valid.options),
    { code: 'SUBMISSION_NOT_COMPLETED' })
  assert.throws(() => prepareBenchmarkSubmission(valid.record, { ...valid.options,
    preparedCatalogSha256: '0'.repeat(64) }), { code: 'SUBMISSION_TASK_INVALID' })
  const wrongModel = structuredClone(valid.record)
  wrongModel.model.executionReceipts.calls[0].actualModels = ['deepseek-flash']
  assert.throws(() => prepareBenchmarkSubmission(wrongModel, valid.options),
    { code: 'SUBMISSION_EVIDENCE_INVALID' })
  const wrongEffort = structuredClone(valid.record)
  wrongEffort.model.executionReceipts.calls[0].reasoningEffort = 'low'
  assert.throws(() => prepareBenchmarkSubmission(wrongEffort, valid.options),
    { code: 'SUBMISSION_EVIDENCE_INVALID' })
  await writeFile(valid.manifestPath, '{"productComplete":false}')
  assert.throws(() => prepareBenchmarkSubmission(valid.record, valid.options),
    { code: 'SUBMISSION_EVIDENCE_INVALID' })
  const escaping = await fixture(resolve(root, 'escaping'), { path: '../main.py' })
  assert.throws(() => prepareBenchmarkSubmission(escaping.record, escaping.options),
    { code: 'SUBMISSION_SOURCE_INVALID' })
  const symlink = await fixture(resolve(root, 'symlink'), { mode: '120000' })
  assert.throws(() => prepareBenchmarkSubmission(symlink.record, symlink.options),
    { code: 'SUBMISSION_SOURCE_INVALID' })
  const oversized = await fixture(resolve(root, 'oversized'), { maxBytes: 2 })
  assert.throws(() => prepareBenchmarkSubmission(oversized.record, oversized.options),
    { code: 'SUBMISSION_SOURCE_INVALID' })
  const changedBundle = await fixture(resolve(root, 'changed-bundle'))
  await writeFile(resolve(changedBundle.manifestDirectory, 'candidate.bundle'), 'changed bundle')
  assert.throws(() => prepareBenchmarkSubmission(changedBundle.record, changedBundle.options),
    { code: 'SUBMISSION_EVIDENCE_INVALID' })
  const forgedSource = await fixture(resolve(root, 'forged-source'))
  const candidateFilesPath = resolve(forgedSource.manifestDirectory, 'candidate-files.json')
  const candidateFiles = JSON.parse(await readFile(candidateFilesPath))
  const main = candidateFiles.files.find(file => file.path === 'main.py')
  main.content = 'print(999)\n'
  main.objectId = gitBlob(Buffer.from(main.content))
  const forgedFilesBytes = Buffer.from(`${JSON.stringify(candidateFiles)}\n`)
  await writeFile(candidateFilesPath, forgedFilesBytes)
  const forgedManifest = JSON.parse(await readFile(forgedSource.manifestPath))
  forgedManifest.candidateFilesSha256 = sha256(forgedFilesBytes)
  const forgedManifestBytes = Buffer.from(`${JSON.stringify(forgedManifest)}\n`)
  await writeFile(forgedSource.manifestPath, forgedManifestBytes)
  forgedSource.record.model.submissionManifest.sha256 = sha256(forgedManifestBytes)
  assert.throws(() => prepareBenchmarkSubmission(forgedSource.record, forgedSource.options),
    { code: 'SUBMISSION_EVIDENCE_INVALID' })
})

test('sealed 700-row ledger publishes completed candidates and leaves failed rows without refs', async t => {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-ledger-publish-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const remote = resolve(root, 'submissions.git')
  execFileSync('git', ['init', '--bare', '--quiet', remote])
  const { record, options } = await fixture(root)
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const ledgerPath = resolve(root, 'benchmark.sqlite3')
  await runBenchmarkPlan(plan, { ledgerPath,
    experimentBinding: { experimentId: options.experimentId,
      preparedCatalogSha256: options.preparedCatalogSha256,
      productSourceSealSha256: record.model.productSourceSealSha256 },
    executeCell: cell => cell.runId === record.runId ? { model: record.model }
      : { status: 'failed', failure: { code: 'TEST_PRODUCT_FAILED' } },
  })
  const publication = { ledgerPath, preparedInputsDirectory: options.preparedInputsDirectory,
    repositoryUrl: remote, receiptDirectory: resolve(root, 'receipts') }
  const first = publishBenchmarkLedger(publication)
  assert.equal(first.denominator, 700)
  assert.equal(first.published, 1)
  assert.equal(execFileSync('git', ['--git-dir', remote, 'for-each-ref', '--format=%(refname)',
    'refs/heads/submit'], { encoding: 'utf8' }).trim().split('\n').length, 1)
  assert.deepEqual(publishBenchmarkLedger(publication), first)
})
