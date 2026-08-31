import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const workflowPath = resolve(root, '.github/workflows/native-release.yml')
const workflow = readFileSync(workflowPath, 'utf8')

test('product release workflow exposes separate manual Linux and macOS lanes', () => {
  assert.match(workflow, /^name: Product release matrix$/mu)
  assert.match(workflow, /^  workflow_dispatch:$/mu)
  assert.doesNotMatch(workflow, /^  (?:pull_request|push):$/mu)
  const platformOptions = [...workflow.matchAll(/^          - (linux|macos)$/gmu)]
    .map(match => match[1])
  assert.deepEqual(platformOptions, ['linux', 'macos'])
  assert.ok(workflow.includes('binutils bubblewrap pkg-config libcap-dev'))
  assert.ok(workflow.includes('kernel.apparmor_restrict_unprivileged_userns=0'))
  assert.ok(workflow.includes('bwrap --ro-bind / / --unshare-user --unshare-pid --unshare-net'))
  for (const [target, runner] of [
    ['aarch64-unknown-linux-gnu', 'ubuntu-24.04-arm'],
    ['x86_64-unknown-linux-gnu', 'ubuntu-24.04'],
    ['aarch64-apple-darwin', 'macos-15'],
    ['x86_64-apple-darwin', 'macos-15-intel'],
  ]) {
    assert.ok(
      workflow.includes(`\"target\":\"${target}\",\"runner\":\"${runner}\"`),
      `native release workflow is missing ${target} on ${runner}`,
    )
  }
  assert.notEqual(readFileSync(resolve(root, '.node-version'), 'utf8').trim(), '')
  assert.ok(workflow.includes('node-version-file: .node-version'))
  assert.ok(workflow.includes('rustup toolchain install 1.95.0'))
  assert.ok(workflow.includes('rustup target add "${{ matrix.target }}" --toolchain 1.95.0'))
  assert.doesNotMatch(workflow, /windows|win32|msvc/iu)
})

test('product release workflow passes immutable source identity to the canonical gate', () => {
  assert.ok(existsSync(resolve(root, 'scripts/run-release-artifact-gate.mjs')))
  assert.ok(workflow.includes('node scripts/run-release-artifact-gate.mjs'))
  assert.ok(workflow.includes('--source-commit "${GITHUB_SHA}"'))
  assert.ok(workflow.includes('--source-date-epoch "$(git show -s --format=%ct "${GITHUB_SHA}")"'))
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
  assert.ok(workflow.includes('--expected-commit "${GITHUB_SHA}"'))
  assert.ok(workflow.includes('--evidence release-artifacts'))
  assert.ok(workflow.includes('--output release-artifact-security-report.json'))
  const securityOffset = workflow.indexOf(securityCommand)
  const productUploadOffset = workflow.indexOf('      - name: Upload release artifacts')
  const securityUploadOffset = workflow.indexOf('name: release-security-${{ matrix.target }}')
  assert.ok(securityOffset < productUploadOffset)
  assert.ok(securityOffset < securityUploadOffset)
  assert.ok(workflow.includes('path: release-artifact-security-report.json'))
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
