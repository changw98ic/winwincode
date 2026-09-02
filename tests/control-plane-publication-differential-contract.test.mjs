// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-publication-differential.rules.json',
)
const documentationPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-publication-differential.md',
)

const read = path => readFileSync(path, 'utf8')
const repositoryPath = path => join(root, path)
const rules = () => JSON.parse(read(rulesPath))

const run = (command, args) => {
  const env = { ...process.env }
  delete env.NODE_TEST_CONTEXT
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: 'utf8',
    env,
  })
  const output = `${result.stdout ?? ''}${result.stderr ?? ''}`
  assert.equal(
    result.status,
    0,
    `${command} ${args.join(' ')} failed:\n${output}`,
  )
  return output
}

test('phase 3.7 freezes one Rust Control Plane Publication writer', () => {
  const contract = rules()
  assert.deepEqual(Object.keys(contract), [
    'schemaVersion',
    'status',
    'issueId',
    'decision',
    'documentation',
    'cutoverBoundary',
    'scenarioOrder',
    'scenarios',
    'gates',
    'removal',
  ])
  assert.deepEqual(
    {
      schemaVersion: contract.schemaVersion,
      status: contract.status,
      issueId: contract.issueId,
      decision: contract.decision,
      documentation: contract.documentation,
    },
    {
      schemaVersion: 'winwincode.control-plane-publication-cutover-rules.v1',
      status: 'implemented-enforced',
      issueId: 'winwincode-9c4.16.3.7',
      decision: 'docs/decisions/0028-control-plane-worker-migration.md',
      documentation:
        'docs/contracts/control-plane-publication-differential.md',
    },
  )
  assert.deepEqual(contract.cutoverBoundary, {
    canonicalWriter: 'rust-control-plane',
    canonicalRuntime: [
      'control-plane-publication-preparation',
      'artifact-store-review-package',
      'generated-publication.publish',
      'control-plane-policy-and-immutable-audit',
      'sqlite-publication-intent',
      'policy-guarded-resume',
      'github-publication-port',
    ],
    typescriptWriterAllowed: false,
    typescriptReviewPackageWriterAllowed: false,
    compatibilityNormalizerAllowed: false,
    productionFallbackAllowed: false,
  })
})

test('all required success and failure samples have one canonical disposition', () => {
  const contract = rules()
  assert.deepEqual(contract.scenarioOrder, [
    'success',
    'authentication-failure',
    'rate-limit',
    'pull-request-conflict',
    'comment-failure',
    'artifact-object-corruption',
    'policy-denied',
    'duplicate-command',
  ])
  assert.deepEqual(
    contract.scenarios.map(scenario => scenario.id),
    contract.scenarioOrder,
  )
  for (const scenario of contract.scenarios) {
    assert.deepEqual(Object.keys(scenario), [
      'id',
      'canonicalEvidence',
      'externalOrder',
      'canonicalOutcome',
    ])
    assert.ok(scenario.canonicalEvidence.length > 0)
    assert.ok(scenario.canonicalOutcome.length > 0)
  }
  assert.deepEqual(contract.scenarios[0].externalOrder, [
    'branch.lookup',
    'branch.apply',
    'pull-request.lookup',
    'pull-request.apply',
    'issue-comment.lookup',
    'issue-comment.apply',
    'issue-comment.lookup',
    'commit-status.lookup',
    'commit-status.apply',
  ])
  assert.equal(
    contract.scenarios.find(scenario => scenario.id === 'comment-failure')
      .externalOrder.includes('commit-status.apply'),
    false,
  )
})

test('the cutover gate is anchored to the Rust preparation, audit, provider, and Artifact behavior', () => {
  const contract = rules()
  assert.deepEqual(Object.keys(contract.gates), [
    'cutover',
    'controlPlane',
    'githubAdapter',
    'artifactStore',
  ])
  const allTests = Object.values(contract.gates)
    .flatMap(gate => gate.requiredTests)
  const sources = [
    read(join(
      root,
      'crates',
      'winwincode-control-plane',
      'tests',
      'publication_cutover.rs',
    )),
    read(join(
      root,
      'crates',
      'winwincode-control-plane',
      'tests',
      'publication_policy.rs',
    )),
    read(join(
      root,
      'crates',
      'winwincode-publication',
      'tests',
      'github_adapter.rs',
    )),
    read(join(
      root,
      'crates',
      'winwincode-storage',
      'tests',
      'artifact_store.rs',
    )),
  ].join('\n')
  for (const testName of allTests) {
    assert.ok(sources.includes(testName), `missing behavior tracer: ${testName}`)
  }

  const preparation = read(join(
    root,
    'crates',
    'winwincode-control-plane',
    'src',
    'publication_preparation.rs',
  ))
  const policy = read(join(
    root,
    'crates',
    'winwincode-control-plane',
    'src',
    'publication_policy.rs',
  ))
  assert.ok(preparation.includes('pub fn prepare_publication('))
  assert.ok(preparation.includes('artifact_store'))
  assert.ok(policy.includes('pub fn commit_publication_publish('))
  assert.ok(policy.includes('pub fn resume_publication('))
  assert.ok(policy.includes('append_publication_result_audit'))
})

test('the old TypeScript Publication writer, package exports, and DSH row are absent', () => {
  const removal = rules().removal
  assert.deepEqual(removal.typescriptWriterSources, [
    'packages/contracts/src/strongflow-github-review-package.ts',
    'packages/dsh-profile/src/github-publication-provider.ts',
    'packages/strongflow/src/github-review-package.ts',
    'packages/strongflow/src/github-publication-provider.ts',
    'packages/strongflow/src/github-publication-journal.ts',
    'packages/strongflow/src/github-publication-runner.ts',
  ])
  assert.deepEqual(removal.typescriptWriterTests, [
    'tests/github-review-package.test.mjs',
    'tests/dsh-github-publication-provider.test.mjs',
  ])
  assert.deepEqual(removal.generatedExtensions, [
    '.js',
    '.js.map',
    '.d.ts',
    '.d.ts.map',
  ])
  for (const path of [
    ...removal.typescriptWriterSources,
    ...removal.typescriptWriterTests,
  ]) {
    assert.equal(existsSync(repositoryPath(path)), false, path)
  }
  for (const sourcePath of removal.typescriptWriterSources) {
    for (const extension of removal.generatedExtensions) {
      const generatedPath = sourcePath
        .replace('/src/', '/dist/')
        .replace(/\.ts$/u, extension)
      assert.equal(existsSync(repositoryPath(generatedPath)), false, generatedPath)
    }
  }

  const strongflowIndex = read(repositoryPath('packages/strongflow/src/index.ts'))
  const contractsIndex = read(repositoryPath('packages/contracts/src/index.ts'))
  const dshIndex = read(repositoryPath('packages/dsh-profile/src/index.ts'))
  const dshManifest = JSON.parse(read(repositoryPath('packages/dsh-profile/package.json')))
  const dshPatch = read(repositoryPath('packages/dsh-profile/cordis.patch.yml'))
  for (const module of [
    'github-review-package',
    'github-publication-provider',
    'github-publication-journal',
    'github-publication-runner',
  ]) assert.equal(strongflowIndex.includes(module), false, module)
  assert.equal(contractsIndex.includes('strongflow-github-review-package'), false)
  assert.equal(dshIndex.includes('github-publication-provider'), false)
  assert.equal(Object.hasOwn(dshManifest.exports, removal.dshPackageExport), false)
  assert.equal(dshPatch.includes(removal.dshPatchRow), false)
  assert.equal(
    existsSync(repositoryPath(removal.remainingPresentationModule)),
    true,
  )
})

test('the cutover gate executes the canonical Rust samples', () => {
  for (const gate of Object.values(rules().gates)) {
    const output = run(gate.command[0], gate.command.slice(1))
    for (const testName of gate.requiredTests) {
      assert.match(
        output,
        new RegExp(`test ${testName} \\.\\.\\. ok`, 'u'),
      )
    }
  }
})

test('plain-language documentation explains the completed cutover and recovery result', () => {
  const documentation = read(documentationPath)
  for (const statement of [
    '旧 TypeScript 写入入口已经删除',
    'Delivery → review package Artifact → policy/audit → GitHub',
    'branch → pull-request → issue-comment → commit-status',
    '`github-permission-denied`',
    '`github-rate-limited`',
    'PR conflict',
    'comment rejection',
    'Artifact object corruption',
    '`PERMISSION_DENIED`',
    '`publication.intent-recorded`',
    '`publication.incomplete`',
    '`publication.published`',
    '失败不会被记录成 Published',
    '阶段 4 可以开始',
  ]) {
    assert.ok(
      documentation.includes(statement),
      `documentation is missing: ${statement}`,
    )
  }
})
