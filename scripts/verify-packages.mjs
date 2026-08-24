#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

import {
  NATIVE_TARGETS,
  hostNativeTarget,
  nativeTargetConfiguration,
  verifyNativePrebuild,
} from './native-package-contract.mjs'
import { scanPackedPackageCpbBoundary } from './cpb-boundary-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const releaseTarget = hostNativeTarget()
if (releaseTarget === undefined) {
  throw new Error(`package verification does not support ${process.platform}/${process.arch}`)
}
const targetConfiguration = nativeTargetConfiguration(releaseTarget)
if (targetConfiguration === undefined) throw new Error(`missing package for ${releaseTarget}`)
const nativePrebuild = 'package/prebuild'
const packages = [
  {
    directory: 'apps/host',
    required: [
      'package/dist/index.js',
      'package/dist/index.d.ts',
      'package/dist/cli.js',
      'package/dist/strongflow-cli.js',
      'package/dist/strongflow-cli.d.ts',
      'package/dist/web-host.js',
      'package/dist/web-host.d.ts',
    ],
  },
  {
    directory: 'packages/contracts',
    required: [
      'package/dist/index.js',
      'package/dist/index.d.ts',
      'package/dist/delivery.js',
      'package/dist/delivery.d.ts',
      'package/dist/delivery-candidate.js',
      'package/dist/delivery-candidate.d.ts',
      'package/dist/runtime-events.js',
      'package/dist/runtime-events.d.ts',
      'package/dist/strongflow-delivery-api.js',
      'package/dist/strongflow-delivery-api.d.ts',
      'package/dist/strongflow-role.js',
      'package/dist/strongflow-role.d.ts',
      'package/dist/strongflow-github-publication.js',
      'package/dist/strongflow-github-publication.d.ts',
      'package/dist/strongflow-github-review-package.js',
      'package/dist/strongflow-github-review-package.d.ts',
    ],
    forbidden: [
      'package/dist/strongflow-artifact.js',
      'package/dist/strongflow-handoff.js',
      'package/dist/strongflow-job.js',
      'package/dist/strongflow-operator.js',
      'package/dist/strongflow-workspace.js',
    ],
  },
  {
    directory: 'packages/dsh-profile',
    required: [
      'package/cordis.patch.yml',
      'package/dist/index.js',
      'package/dist/index.d.ts',
      'package/dist/agent-factory.js',
      'package/dist/agent-factory.d.ts',
      'package/dist/delivery-recovery.js',
      'package/dist/delivery-recovery.d.ts',
      'package/dist/github-publication-provider.js',
      'package/dist/github-publication-provider.d.ts',
    ],
    forbidden: ['package/dist/strongflow-approval.js'],
    allowedRootFiles: ['package/cordis.patch.yml'],
  },
  {
    directory: 'packages/native',
    required: ['package/dist/index.js', 'package/dist/index.d.ts'],
  },
  {
    directory: targetConfiguration.packageDirectory,
    required: [
      `${nativePrebuild}/LICENSE`,
      `${nativePrebuild}/NOTICE`,
      `${nativePrebuild}/THIRD_PARTY_NOTICES.md`,
      `${nativePrebuild}/build-info.json`,
      `${nativePrebuild}/rust-dependencies.json`,
      `${nativePrebuild}/winwincode-kernel-helper`,
      `${nativePrebuild}/winwincode_native.node`,
      ...(process.platform === 'linux'
        ? [
            `${nativePrebuild}/codex-linux-sandbox`,
            `${nativePrebuild}/codex-resources/bwrap`,
            `${nativePrebuild}/codex-resources/bwrap.LICENSE`,
          ]
        : []),
    ],
  },
  {
    directory: 'packages/strongflow',
    required: [
      'package/dist/index.js',
      'package/dist/index.d.ts',
      'package/dist/acceptance-verification.js',
      'package/dist/acceptance-verification.d.ts',
      'package/dist/candidate-evidence.js',
      'package/dist/candidate-evidence.d.ts',
      'package/dist/independent-verification.js',
      'package/dist/independent-verification.d.ts',
      'package/dist/delivery-verdict.js',
      'package/dist/delivery-verdict.d.ts',
      'package/dist/delivery-attention.js',
      'package/dist/delivery-attention.d.ts',
      'package/dist/github-publication.js',
      'package/dist/github-publication.d.ts',
      'package/dist/github-review-package.js',
      'package/dist/github-review-package.d.ts',
      'package/dist/github-publication-provider.js',
      'package/dist/github-publication-provider.d.ts',
      'package/dist/github-publication-journal.js',
      'package/dist/github-publication-journal.d.ts',
      'package/dist/github-publication-runner.js',
      'package/dist/github-publication-runner.d.ts',
      'package/dist/client.js',
      'package/dist/client.d.ts',
      'package/dist/credential-boundary.js',
      'package/dist/credential-boundary.d.ts',
      'package/dist/delivery-authenticator.js',
      'package/dist/delivery-authenticator.d.ts',
      'package/dist/delivery-invoker.js',
      'package/dist/delivery-invoker.d.ts',
      'package/dist/delivery-remote-client.js',
      'package/dist/delivery-remote-client.d.ts',
      'package/dist/delivery-remote.js',
      'package/dist/delivery-remote.d.ts',
      'package/dist/delivery-runtime-projection.js',
      'package/dist/delivery-runtime-projection.d.ts',
      'package/dist/evaluation-measures.js',
      'package/dist/evaluation-measures.d.ts',
      'package/dist/delivery-service.js',
      'package/dist/delivery-service.d.ts',
      'package/dist/delivery-store.js',
      'package/dist/delivery-store.d.ts',
    ],
    forbidden: [
      'package/dist/artifact-store.js',
      'package/dist/controller.js',
      'package/dist/job-store.js',
      'package/dist/operator-service.js',
      'package/dist/role-runner.js',
      'package/dist/role-authority.js',
      'package/dist/role-session.js',
      'package/dist/security-audit.js',
      'package/dist/workspace-policy.js',
    ],
  },
]
const errors = []

for (const entry of packages) {
  const directory = join(root, entry.directory)
  const manifest = JSON.parse(readFileSync(join(directory, 'package.json'), 'utf8'))
  const packed = spawnSync('npm', ['pack', '--dry-run', '--json', '--ignore-scripts'], {
    cwd: directory,
    encoding: 'utf8',
  })
  if (packed.status !== 0) {
    errors.push(`${entry.directory}: npm pack failed: ${(packed.stderr || packed.stdout).trim()}`)
    continue
  }
  let report
  try {
    report = JSON.parse(packed.stdout)[0]
  } catch (error) {
    errors.push(`${entry.directory}: invalid npm pack report: ${error.message}`)
    continue
  }
  const packedPaths = report.files.map(file => file.path)
  const files = packedPaths.map(path => `package/${path}`)
  for (const required of entry.required) {
    if (!files.includes(required)) errors.push(`${entry.directory}: package is missing ${required}`)
  }
  for (const forbidden of entry.forbidden ?? []) {
    if (files.includes(forbidden)) errors.push(`${entry.directory}: package contains obsolete ${forbidden}`)
  }
  for (const path of files) {
    const allowed = entry.allowedRootFiles?.includes(path) === true
      || path === 'package/package.json'
      || path.startsWith('package/dist/')
      || path.startsWith('package/prebuild/')
      || path === 'package/LICENSE'
      || path === 'package/NOTICE'
      || path === 'package/README.md'
    if (!allowed) errors.push(`${entry.directory}: undeclared package file ${path}`)
    if (path.endsWith('.tsbuildinfo')) errors.push(`${entry.directory}: build metadata leaked into package`)
  }
  if (manifest.license !== 'Apache-2.0') {
    errors.push(`${entry.directory}: published package license is not Apache-2.0`)
  }
  errors.push(...scanPackedPackageCpbBoundary({
    packageDirectory: directory,
    files: packedPaths,
  }).map(error => `${entry.directory}: ${error}`))
}

const loaderManifest = JSON.parse(
  readFileSync(join(root, 'packages', 'native', 'package.json'), 'utf8'),
)
const expectedOptionalDependencies = Object.fromEntries(
  NATIVE_TARGETS.map(configuration => [configuration.packageName, 'workspace:*']),
)
if (
  JSON.stringify(loaderManifest.optionalDependencies)
  !== JSON.stringify(expectedOptionalDependencies)
) {
  errors.push('packages/native: optional platform packages do not match supported targets')
}

errors.push(...verifyNativePrebuild({
  root,
  target: releaseTarget,
  requireCurrentHost: true,
}).errors.map(error => `${targetConfiguration.packageDirectory}: ${error}`))

if (errors.length > 0) {
  for (const error of errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}

process.stdout.write(`publish file allowlists verified for ${releaseTarget}\n`)
