import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

import { selectSuccessfulMainlineRun } from '../scripts/verify-mainline-release-source.mjs'

const root = resolve(import.meta.dirname, '..')
const mainlineWorkflowPath = resolve(root, '.github/workflows/mainline.yml')
const mainlineWorkflow = readFileSync(mainlineWorkflowPath, 'utf8')

test('ordinary CI runs one exact-SHA aggregate over six independent lanes', () => {
  assert.match(mainlineWorkflow, /^name: Mainline verification$/mu)
  assert.match(mainlineWorkflow, /^  push:$/mu)
  assert.match(mainlineWorkflow, /^    branches:\n      - main$/mu)
  assert.match(mainlineWorkflow, /^  pull_request:$/mu)
  assert.match(mainlineWorkflow, /^  source:$/mu)
  assert.match(mainlineWorkflow, /^  typescript:$/mu)
  assert.match(mainlineWorkflow, /^  rust:$/mu)
  assert.match(mainlineWorkflow, /^    name: rust-\$\{\{ matrix\.lane \}\}$/mu)
  for (const lane of ['lint', 'test', 'e2e', 'runtime']) {
    assert.match(mainlineWorkflow, new RegExp(`^          - ${lane}$`, 'mu'))
  }
  assert.match(mainlineWorkflow, /^  verify:$/mu)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:source$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:typescript$/gmu)].length, 1)
  assert.doesNotMatch(mainlineWorkflow, /corepack pnpm verify:rust$/mu)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm lint:rust$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm test:rust$/gmu)].length, 2)
  assert.match(mainlineWorkflow, /^          WWC_RUST_E2E: "0"$/mu)
  assert.match(mainlineWorkflow, /^          WWC_RUST_E2E: "1"$/mu)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm build:rust$/gmu)].length, 1)
  assert.equal(
    [...mainlineWorkflow.matchAll(/corepack pnpm verify:api-production-vertical$/gmu)].length,
    1,
  )
  assert.equal(
    [...mainlineWorkflow.matchAll(
      /mozilla-actions\/sccache-action@fc920bf0ec8de6ee65d409111f7ec508035751ba/gmu,
    )].length,
    1,
  )
  assert.equal([...mainlineWorkflow.matchAll(/^          version: "v0\.17\.0"$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/^      RUSTC_WRAPPER: "sccache"$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/^      SCCACHE_GHA_ENABLED: "true"$/gmu)].length, 1)
  assert.doesNotMatch(mainlineWorkflow, /corepack pnpm verify$/mu)
  assert.ok(mainlineWorkflow.includes('github.event.pull_request.number || github.ref'))
  assert.match(mainlineWorkflow, /^      CARGO_PROFILE_DEV_DEBUG: "0"$/mu)
  assert.match(mainlineWorkflow, /^      CARGO_PROFILE_TEST_DEBUG: "0"$/mu)
  assert.match(mainlineWorkflow, /^    name: Canonical workspace verification$/mu)
  for (const lane of ['source', 'typescript', 'rust']) {
    assert.match(mainlineWorkflow, new RegExp(`^      - ${lane}$`, 'mu'))
  }
  assert.doesNotMatch(
    mainlineWorkflow,
    /WINWINCODE_HELPER_RELEASE_(?:PRIVATE|PUBLIC)_KEY_HEX|secrets\./u,
  )
  assert.ok(mainlineWorkflow.includes('sudo bash scripts/install-code-mode-linux-build-deps.sh'))
  const linuxDependencyInstaller = readFileSync(
    resolve(root, 'scripts/install-code-mode-linux-build-deps.sh'),
    'utf8',
  )
  assert.ok(linuxDependencyInstaller.includes('binutils bubblewrap pkg-config libcap-dev'))
  assert.ok(mainlineWorkflow.includes('kernel.apparmor_restrict_unprivileged_userns=0'))
  assert.ok(mainlineWorkflow.includes(
    'bwrap --ro-bind / / --unshare-user --unshare-pid --unshare-net',
  ))
})

test('mainline source verifier accepts only the default-branch exact-SHA successful push', () => {
  const defaultBranch = 'main'
  const repository = 'winwincode/project'
  const sourceCommit = '1'.repeat(40)
  const valid = {
    id: 42,
    html_url: 'https://github.example/runs/42',
    head_sha: sourceCommit,
    event: 'push',
    status: 'completed',
    conclusion: 'success',
    path: '.github/workflows/mainline.yml',
    head_repository: { full_name: repository },
    head_branch: defaultBranch,
  }
  assert.deepEqual(selectSuccessfulMainlineRun({
    defaultBranch,
    repository,
    sourceCommit,
    runs: [valid],
  }), {
    defaultBranch,
    runId: 42,
    runUrl: 'https://github.example/runs/42',
    sourceCommit,
  })
  assert.throws(
    () => selectSuccessfulMainlineRun({
      defaultBranch,
      repository,
      sourceCommit: 'HEAD',
      runs: [valid],
    }),
    /40 lowercase hexadecimal/u,
  )
  for (const changed of [
    { event: 'pull_request' },
    { head_repository: { full_name: 'foreign/project' } },
    { conclusion: 'failure' },
    { head_sha: '2'.repeat(40) },
    { head_branch: 'feature/strongflow-foundation' },
  ]) {
    assert.throws(
      () => selectSuccessfulMainlineRun({
        defaultBranch,
        repository,
        sourceCommit,
        runs: [{ ...valid, ...changed }],
      }),
      /found 0/u,
    )
  }
  assert.throws(
    () => selectSuccessfulMainlineRun({
      defaultBranch,
      repository,
      sourceCommit,
      runs: [valid, valid],
    }),
    /found 2/u,
  )
  assert.throws(
    () => selectSuccessfulMainlineRun({
      defaultBranch: '../main',
      repository,
      sourceCommit,
      runs: [valid],
    }),
    /default branch identity is invalid/u,
  )
})
