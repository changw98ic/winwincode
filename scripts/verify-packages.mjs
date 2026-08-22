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
    ],
  },
  {
    directory: 'packages/contracts',
    required: [
      'package/dist/index.js',
      'package/dist/index.d.ts',
      'package/dist/runtime-events.js',
      'package/dist/runtime-events.d.ts',
      'package/dist/strongflow-artifact.js',
      'package/dist/strongflow-artifact.d.ts',
      'package/dist/strongflow-handoff.js',
      'package/dist/strongflow-handoff.d.ts',
      'package/dist/strongflow-job.js',
      'package/dist/strongflow-job.d.ts',
      'package/dist/strongflow-operator.js',
      'package/dist/strongflow-operator.d.ts',
      'package/dist/strongflow-permission.js',
      'package/dist/strongflow-permission.d.ts',
      'package/dist/strongflow-role.js',
      'package/dist/strongflow-role.d.ts',
      'package/dist/strongflow-workspace.js',
      'package/dist/strongflow-workspace.d.ts',
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
      'package/dist/strongflow-approval.js',
      'package/dist/strongflow-approval.d.ts',
    ],
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
      'package/dist/artifact-store.js',
      'package/dist/artifact-store.d.ts',
      'package/dist/artifact-validator.js',
      'package/dist/artifact-validator.d.ts',
      'package/dist/client.js',
      'package/dist/client.d.ts',
      'package/dist/controller.js',
      'package/dist/controller.d.ts',
      'package/dist/definition-diagrams.js',
      'package/dist/definition-diagrams.d.ts',
      'package/dist/human-review-gate.js',
      'package/dist/human-review-gate.d.ts',
      'package/dist/git-workspace.js',
      'package/dist/git-workspace.d.ts',
      'package/dist/handoff.js',
      'package/dist/handoff.d.ts',
      'package/dist/job-store.js',
      'package/dist/job-store.d.ts',
      'package/dist/operator-remote-client.js',
      'package/dist/operator-remote-client.d.ts',
      'package/dist/operator-remote.js',
      'package/dist/operator-remote.d.ts',
      'package/dist/operator-service.js',
      'package/dist/operator-service.d.ts',
      'package/dist/role-runner.js',
      'package/dist/role-runner.d.ts',
      'package/dist/role-authority.js',
      'package/dist/role-authority.d.ts',
      'package/dist/role-session.js',
      'package/dist/role-session.d.ts',
      'package/dist/workspace-policy.js',
      'package/dist/workspace-policy.d.ts',
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
  const files = report.files.map(file => `package/${file.path}`)
  for (const required of entry.required) {
    if (!files.includes(required)) errors.push(`${entry.directory}: package is missing ${required}`)
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
