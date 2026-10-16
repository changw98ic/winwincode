import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

import { selectSuccessfulMainlineRun } from '../scripts/verify-mainline-release-source.mjs'

const root = resolve(import.meta.dirname, '..')
const mainlineWorkflowPath = resolve(root, '.github/workflows/mainline.yml')
const mainlineWorkflow = readFileSync(mainlineWorkflowPath, 'utf8')
const rustBuildWorkflow = readFileSync(resolve(root, '.github/workflows/rust-build.yml'), 'utf8')
const buildWorkflows = `${mainlineWorkflow}\n${rustBuildWorkflow}`

test('ordinary CI aggregates every lane and shares exact-SHA Rust test and runtime producers', () => {
  assert.match(mainlineWorkflow, /^name: Mainline verification$/mu)
  assert.match(mainlineWorkflow, /^  push:$/mu)
  assert.match(mainlineWorkflow, /^    branches:\n      - main$/mu)
  assert.match(mainlineWorkflow, /^  pull_request:$/mu)
  assert.match(mainlineWorkflow, /^  source:$/mu)
  assert.match(mainlineWorkflow, /^  typescript:$/mu)
  assert.match(mainlineWorkflow, /^  rust:$/mu)
  assert.match(mainlineWorkflow, /^    name: rust-\$\{\{ matrix\.lane \}\}$/mu)
  assert.match(mainlineWorkflow, /^    name: rust-lint$/mu)
  assert.match(mainlineWorkflow, /^      lane: lint$/mu)
  assert.match(mainlineWorkflow, /^  rust-runtime-build:$/mu)
  assert.match(mainlineWorkflow, /^      lane: runtime$/mu)
  assert.match(mainlineWorkflow, /^    needs: rust-runtime-build$/mu)
  assert.match(mainlineWorkflow, /^        group: \[load, api, recovery, boundaries\]$/mu)
  assert.match(mainlineWorkflow, /^          artifact-ids: \$\{\{ needs\.rust-runtime-build\.outputs\.artifact_id \}\}$/mu)
  for (const output of ['producer_attempt', 'archive_sha256']) {
    assert.ok(mainlineWorkflow.includes(`needs.rust-runtime-build.outputs.${output}`))
    assert.ok(rustBuildWorkflow.includes(`steps.runtime-produce.outputs.${output}`))
  }
  assert.ok(rustBuildWorkflow.includes('steps.runtime-upload.outputs.artifact-id'))
  assert.match(mainlineWorkflow, /^        lane: \[test, e2e\]$/mu)
  assert.match(mainlineWorkflow, /^  rust-test-build:$/mu)
  assert.match(mainlineWorkflow, /^      lane: archive$/mu)
  assert.match(mainlineWorkflow, /^    needs: rust-test-build$/mu)
  assert.match(mainlineWorkflow, /^          artifact-ids: \$\{\{ needs\.rust-test-build\.outputs\.artifact_id \}\}$/mu)
  for (const output of ['producer_attempt', 'archive_sha256']) {
    assert.ok(mainlineWorkflow.includes(`needs.rust-test-build.outputs.${output}`))
    assert.ok(rustBuildWorkflow.includes(`steps.produce.outputs.${output}`))
  }
  assert.ok(rustBuildWorkflow.includes('steps.upload.outputs.artifact-id'))
  assert.equal([...mainlineWorkflow.matchAll(/^    uses: \.\/\.github\/workflows\/rust-build\.yml$/gmu)].length, 3)
  for (const lane of ['lint', 'runtime', 'archive']) assert.ok(rustBuildWorkflow.includes(`inputs.lane == '${lane}'`))
  assert.match(mainlineWorkflow, /^  verify:$/mu)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:source$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:typescript$/gmu)].length, 1)
  assert.doesNotMatch(mainlineWorkflow, /corepack pnpm verify:rust$/mu)
  assert.equal([...rustBuildWorkflow.matchAll(/corepack pnpm lint:rust$/gmu)].length, 1)
  assert.doesNotMatch(buildWorkflows, /corepack pnpm test:rust$/mu)
  assert.equal([...rustBuildWorkflow.matchAll(/node scripts\/rust-test-archive\.mjs produce$/gmu)].length, 1)
  assert.match(mainlineWorkflow, /run: node scripts\/rust-test-archive\.mjs \$\{\{ matrix\.lane \}\}/u)
  assert.equal([...rustBuildWorkflow.matchAll(/corepack pnpm build:rust$/gmu)].length, 1)
  assert.equal(
    [...rustBuildWorkflow.matchAll(/corepack pnpm verify:api-production-vertical$/gmu)].length,
    1,
  )
  assert.equal(
    [...rustBuildWorkflow.matchAll(
      /mozilla-actions\/sccache-action@fc920bf0ec8de6ee65d409111f7ec508035751ba/gmu,
    )].length,
    1,
  )
  assert.equal([...rustBuildWorkflow.matchAll(/^          version: "v0\.17\.0"$/gmu)].length, 1)
  assert.equal([...rustBuildWorkflow.matchAll(/^      RUSTC_WRAPPER: "sccache"$/gmu)].length, 1)
  assert.equal([...rustBuildWorkflow.matchAll(/^      SCCACHE_GHA_ENABLED: "true"$/gmu)].length, 1)
  assert.doesNotMatch(mainlineWorkflow, /corepack pnpm verify$/mu)
  assert.ok(mainlineWorkflow.includes('github.event.pull_request.number || github.ref'))
  assert.match(rustBuildWorkflow, /^      CARGO_INCREMENTAL: "0"$/mu)
  assert.doesNotMatch(rustBuildWorkflow, /corepack pnpm verify:api-production-sessions$/mu)
  assert.match(rustBuildWorkflow, /run: node scripts\/runtime-products\.mjs produce/u)
  const sessions = mainlineWorkflow.split('  rust-runtime-sessions:\n')[1].split('  rust-test-build:\n')[0]
  assert.match(sessions, /^      fail-fast: false$/mu)
  assert.match(sessions, /node scripts\/runtime-products\.mjs restore/u)
  assert.match(sessions, /node scripts\/run-api-production-sessions\.mjs --group \$\{\{ matrix\.group \}\}/u)
  assert.match(sessions, /^      WWC_API_SKIP_BUILD: "1"$/mu)
  assert.ok(sessions.includes('WWC_CODE_MODE_ACCEPTANCE_BIN_DIR: ${{ github.workspace }}/target/debug'))
  assert.ok(sessions.includes('WWC_CLI_BINARY: ${{ github.workspace }}/target/debug/wwc'))
  assert.ok(sessions.includes('for compiler in cargo rustc; do'))
  assert.doesNotMatch(sessions, /rustup|build:rust|sccache|install-code-mode-linux-build-deps/u)
  assert.match(rustBuildWorkflow, /^      CARGO_PROFILE_DEV_DEBUG: "0"$/mu)
  assert.match(rustBuildWorkflow, /^      CARGO_PROFILE_TEST_DEBUG: "0"$/mu)
  assert.match(mainlineWorkflow, /^    name: Canonical workspace verification$/mu)
  for (const lane of ['source', 'typescript', 'rust', 'rust-runtime-build', 'rust-runtime-sessions', 'rust-test-build', 'rust-tests']) {
    assert.match(mainlineWorkflow, new RegExp(`^      - ${lane}$`, 'mu'))
  }
  assert.match(mainlineWorkflow, /^    if: \$\{\{ always\(\) \}\}$/mu)
  for (const [variable, need] of Object.entries({ SOURCE_RESULT: 'source', TYPESCRIPT_RESULT: 'typescript', RUST_RESULT: 'rust', RUNTIME_BUILD_RESULT: 'rust-runtime-build', RUNTIME_SESSIONS_RESULT: 'rust-runtime-sessions', ARCHIVE_RESULT: 'rust-test-build', TESTS_RESULT: 'rust-tests' })) {
    assert.ok(mainlineWorkflow.includes(variable + ': ${{ needs.' + need + '.result }}'))
    assert.ok(mainlineWorkflow.includes('test "${' + variable + '}" = "success"'))
  }
  assert.doesNotMatch(
    buildWorkflows,
    /WINWINCODE_HELPER_RELEASE_(?:PRIVATE|PUBLIC)_KEY_HEX|secrets\./u,
  )
  assert.ok(rustBuildWorkflow.includes('sudo bash scripts/install-code-mode-linux-build-deps.sh'))
  const linuxDependencyInstaller = readFileSync(
    resolve(root, 'scripts/install-code-mode-linux-build-deps.sh'),
    'utf8',
  )
  assert.ok(linuxDependencyInstaller.includes('binutils bubblewrap pkg-config libcap-dev'))
  assert.ok(rustBuildWorkflow.includes('kernel.apparmor_restrict_unprivileged_userns=0'))
  assert.ok(rustBuildWorkflow.includes('bwrap --ro-bind / / --unshare-user --unshare-pid --unshare-net'))
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
