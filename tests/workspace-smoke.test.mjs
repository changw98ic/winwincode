import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

import { selectSuccessfulMainlineRun } from '../scripts/verify-mainline-release-source.mjs'

const root = resolve(import.meta.dirname, '..')
const workflowPath = resolve(root, '.github/workflows/native-release.yml')
const workflow = readFileSync(workflowPath, 'utf8')
const mainlineWorkflowPath = resolve(root, '.github/workflows/mainline.yml')
const mainlineWorkflow = readFileSync(mainlineWorkflowPath, 'utf8')

test('ordinary CI runs one exact-SHA aggregate over three independent lanes', () => {
  assert.match(mainlineWorkflow, /^name: Mainline verification$/mu)
  assert.match(mainlineWorkflow, /^  push:$/mu)
  assert.match(mainlineWorkflow, /^    branches:\n      - main$/mu)
  assert.match(mainlineWorkflow, /^  pull_request:$/mu)
  assert.match(mainlineWorkflow, /^  source:$/mu)
  assert.match(mainlineWorkflow, /^  typescript:$/mu)
  assert.match(mainlineWorkflow, /^  rust:$/mu)
  assert.match(mainlineWorkflow, /^  verify:$/mu)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:source$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:typescript$/gmu)].length, 1)
  assert.equal([...mainlineWorkflow.matchAll(/corepack pnpm verify:rust$/gmu)].length, 1)
  assert.doesNotMatch(mainlineWorkflow, /corepack pnpm verify$/mu)
  assert.ok(mainlineWorkflow.includes('github.event.pull_request.number || github.ref'))
  assert.match(mainlineWorkflow, /^    name: Canonical workspace verification$/mu)
  for (const lane of ['source', 'typescript', 'rust']) {
    assert.match(mainlineWorkflow, new RegExp(`^      - ${lane}$`, 'mu'))
  }
  assert.doesNotMatch(
    mainlineWorkflow,
    /WINWINCODE_HELPER_RELEASE_(?:PRIVATE|PUBLIC)_KEY_HEX|secrets\./u,
  )
  assert.ok(mainlineWorkflow.includes('binutils bubblewrap pkg-config libcap-dev'))
  assert.ok(mainlineWorkflow.includes('kernel.apparmor_restrict_unprivileged_userns=0'))
  assert.ok(mainlineWorkflow.includes(
    'bwrap --ro-bind / / --unshare-user --unshare-pid --unshare-net',
  ))
})

test('product release verifies one successful exact-commit mainline run before the four-target matrix', () => {
  assert.match(workflow, /^name: Product release matrix$/mu)
  assert.match(workflow, /^  workflow_dispatch:$/mu)
  assert.match(workflow, /^      source_commit:$/mu)
  assert.match(workflow, /^        required: true$/mu)
  assert.doesNotMatch(workflow, /^  (?:pull_request|push):$/mu)
  assert.doesNotMatch(workflow, /corepack pnpm verify(?:\s|$)/u)
  assert.match(workflow, /^  actions: read$/mu)
  assert.match(workflow, /^  verify-source:$/mu)
  assert.ok(workflow.includes('test "${SELECTED_REF}" = "refs/heads/${DEFAULT_BRANCH}"'))
  assert.ok(workflow.includes('ref: ${{ github.event.repository.default_branch }}'))
  assert.ok(workflow.includes('node scripts/verify-mainline-release-source.mjs'))
  assert.ok(workflow.includes('--source-commit "${SOURCE_COMMIT}"'))
  assert.ok(workflow.includes('--default-branch "${DEFAULT_BRANCH}"'))
  assert.match(workflow, /^    needs: verify-source$/mu)
  assert.ok(workflow.includes('SOURCE_COMMIT: ${{ needs.verify-source.outputs.source_commit }}'))
  assert.ok(workflow.includes('ref: ${{ env.SOURCE_COMMIT }}'))
  const targetMatrix = [
    ['aarch64-unknown-linux-gnu', 'ubuntu-24.04-arm'],
    ['x86_64-unknown-linux-gnu', 'ubuntu-24.04'],
    ['aarch64-apple-darwin', 'macos-15'],
    ['x86_64-apple-darwin', 'macos-15-intel'],
  ]
  assert.deepEqual(
    [...workflow.matchAll(/^          - target: (.+)$/gmu)].map(match => match[1]).toSorted(),
    targetMatrix.map(([target]) => target).toSorted(),
  )
  for (const [target, runner] of targetMatrix) {
    assert.match(
      workflow,
      new RegExp(`- target: ${target}\\n            runner: ${runner}`, 'u'),
      `native release workflow is missing ${target} on ${runner}`,
    )
  }
  assert.notEqual(readFileSync(resolve(root, '.node-version'), 'utf8').trim(), '')
  assert.ok(workflow.includes('node-version-file: .node-version'))
  assert.ok(workflow.includes('rustup toolchain install 1.95.0'))
  assert.ok(workflow.includes('rustup target add "${{ matrix.target }}" --toolchain 1.95.0'))
  assert.doesNotMatch(workflow, /windows|win32|msvc/iu)
  const releaseRunner = readFileSync(resolve(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.doesNotMatch(releaseRunner, /pnpm', 'verify|pnpm verify/u)
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

test('product release workflow passes immutable source identity to the canonical gate', () => {
  assert.ok(existsSync(resolve(root, 'scripts/run-release-artifact-gate.mjs')))
  assert.ok(workflow.includes('node scripts/run-release-artifact-gate.mjs'))
  assert.ok(workflow.includes('--source-commit "${SOURCE_COMMIT}"'))
  assert.ok(workflow.includes('--source-date-epoch "$(git show -s --format=%ct "${SOURCE_COMMIT}")"'))
  assert.doesNotMatch(workflow, /GITHUB_SHA/u)
  assert.ok(workflow.includes('--output release-artifacts'))
  const productUploadStep = workflow.match(
    /      - name: Upload release artifacts\n[\s\S]*?(?=\n      - name:)/u,
  )?.[0]
  assert.notEqual(productUploadStep, undefined)
  assert.ok(productUploadStep.includes('name: ${{ matrix.target }}'))
  assert.doesNotMatch(workflow, /name: release-\$\{\{ matrix\.target \}\}/u)
  assert.ok(workflow.includes('path: release-artifacts/${{ matrix.target }}/'))
  assert.ok(workflow.includes('if-no-files-found: error'))
  assert.doesNotMatch(workflow, /run-native-release-gate|native-package|name: native-/u)
  assert.ok(workflow.includes('timeout-minutes: ${{ matrix.timeout_minutes }}'))
  assert.match(
    workflow,
    /target: x86_64-apple-darwin\n\s+runner: macos-15-intel\n\s+timeout_minutes: 300/u,
  )
})

test('product release workflow binds the helper signing keys without embedding key material', () => {
  const releaseStep = workflow.match(
    /      - name: Build product release artifacts and write evidence\n[\s\S]*?(?=\n      - name:)/u,
  )?.[0]
  assert.notEqual(releaseStep, undefined)
  assert.ok(releaseStep.includes(
    'WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX: ${{ secrets.WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX }}',
  ))
  assert.ok(releaseStep.includes(
    'WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX: ${{ vars.WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX }}',
  ))
  assert.doesNotMatch(
    releaseStep,
    /WINWINCODE_HELPER_RELEASE_(?:PRIVATE|PUBLIC)_KEY_HEX:\s*["']?[0-9a-f]{64}["']?/u,
  )
})

test('product release workflow blocks uploads until target security verification passes', () => {
  const securityCommand = 'node scripts/verify-release-artifact-security.mjs'
  const securityStep = workflow.match(
    /      - name: Verify release artifact security\n[\s\S]*?(?=\n      - name:)/u,
  )?.[0]
  assert.notEqual(securityStep, undefined)
  assert.ok(securityStep.includes(
    'WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX: ${{ secrets.WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX }}',
  ))
  assert.ok(workflow.includes(securityCommand))
  assert.ok(workflow.includes('--target "${{ matrix.target }}"'))
  assert.ok(workflow.includes('--expected-commit "${SOURCE_COMMIT}"'))
  assert.ok(workflow.includes('--evidence release-artifacts'))
  assert.ok(workflow.includes('--output release-artifact-security-report.json'))
  assert.ok(securityStep.includes('id: security'))
  assert.ok(securityStep.includes('continue-on-error: true'))
  const securityOffset = workflow.indexOf(securityCommand)
  const productUploadOffset = workflow.indexOf('      - name: Upload release artifacts')
  const securityUploadOffset = workflow.indexOf('name: release-security-${{ matrix.target }}')
  assert.ok(securityOffset < productUploadOffset)
  assert.ok(securityOffset < securityUploadOffset)
  assert.ok(workflow.includes('path: release-artifact-security-report.json'))
  assert.ok(workflow.includes("if: ${{ always() && steps.security.outcome != 'skipped' }}"))
  assert.ok(workflow.includes("if: ${{ steps.security.outcome == 'success' }}"))
  assert.ok(workflow.includes('      - name: Enforce release artifact security'))
  assert.ok(workflow.includes('SECURITY_OUTCOME: ${{ steps.security.outcome }}'))
  assert.ok(workflow.includes('test "${SECURITY_OUTCOME}" = "success"'))
  assert.ok(workflow.includes('key_file="${RUNNER_TEMP}/helper-release-private-input"'))
  assert.ok(workflow.includes('umask 077'))
  assert.ok(workflow.includes('trap \'rm -f -- "$key_file"\' EXIT'))
  assert.ok(workflow.includes(
    'printf \'%s\' "${WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX}" > "$key_file"',
  ))
  assert.ok(workflow.includes('--sensitive-input "$key_file"'))
  assert.doesNotMatch(
    workflow,
    /--output\s+["']?release-artifacts[/\\]release-artifact-security/u,
  )
})

test('release download instructions recreate the exact aggregate evidence roots', () => {
  const releasing = readFileSync(resolve(root, 'docs/releasing.md'), 'utf8')
  assert.ok(releasing.includes('gh run download "$RUN_ID" --name "$TARGET" --dir "release-artifacts/$TARGET"'))
  assert.ok(releasing.includes(
    'gh run download "$RUN_ID" --name "release-security-$TARGET" --dir "release-security-reports/$TARGET"',
  ))
  assert.ok(releasing.includes('release-artifacts/` 的一级目录因此精确为四个 Rust target'))
  assert.ok(releasing.includes('release-security-reports/` 与产品 evidence root 分离'))
})
