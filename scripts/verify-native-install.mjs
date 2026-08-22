#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, join, resolve } from 'node:path'

import {
  NATIVE_TARGETS,
  hostNativeTarget,
  nativeTargetConfiguration,
  verifyNativePrebuild,
} from './native-package-contract.mjs'

function parseArguments(argv) {
  let target
  let requireRelease = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--require-release') {
      requireRelease = true
      continue
    }
    if (argument === '--target') {
      target = argv[index + 1]
      if (target === undefined) throw new Error('--target requires a Rust target triple')
      index += 1
      continue
    }
    if (argument.startsWith('--target=')) {
      target = argument.slice('--target='.length)
      continue
    }
    throw new Error(`unknown verify-native-install argument: ${argument}`)
  }
  return { target: target ?? hostNativeTarget(), requireRelease }
}

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    encoding: 'utf8',
    ...options,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new Error(
      `${command} ${arguments_.join(' ')} failed\n${result.stdout}${result.stderr}`,
    )
  }
  return result.stdout
}

function pack(root, directory, destination) {
  const output = run('corepack', [
    'pnpm',
    'pack',
    '--json',
    '--pack-destination',
    destination,
  ], { cwd: join(root, directory) })
  const report = JSON.parse(output)
  const filename = report.filename ?? report[0]?.filename
  if (typeof filename !== 'string') throw new Error(`${directory}: pnpm pack did not report a file`)
  return resolve(join(root, directory), filename)
}

function hasSafeGovernedDiagnostics(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false
  const expectedNames = [
    'environment',
    'workspaceWrite',
    'outsideWrite',
    'credentialRead',
    'network',
    'timeout',
    'readOnlyWrite',
  ]
  if (JSON.stringify(Object.keys(value).sort()) !== JSON.stringify([...expectedNames].sort())) {
    return false
  }
  const statuses = new Set(['exited', 'sandbox-denied', 'timed-out', 'cancelled', 'output-limit'])
  return Object.values(value).every(diagnostic => {
    if (diagnostic === null || typeof diagnostic !== 'object' || Array.isArray(diagnostic)) {
      return false
    }
    if (
      JSON.stringify(Object.keys(diagnostic).sort())
      !== JSON.stringify(['category', 'exitCode', 'status', 'stderr', 'stdout'])
    ) return false
    if (!statuses.has(diagnostic.status)) return false
    if (diagnostic.exitCode !== null && !Number.isInteger(diagnostic.exitCode)) return false
    const expectedCategory = diagnostic.status === 'exited'
      ? (diagnostic.exitCode === 0 ? 'exited-zero' : 'exited-nonzero')
      : {
          'sandbox-denied': 'sandbox-policy-denial',
          'timed-out': 'deadline-enforced',
          cancelled: 'cancelled',
          'output-limit': 'output-limit-enforced',
        }[diagnostic.status]
    if (diagnostic.category !== expectedCategory) return false
    return ['stdout', 'stderr'].every(stream => {
      const summary = diagnostic[stream]
      return summary !== null
        && typeof summary === 'object'
        && !Array.isArray(summary)
        && Number.isSafeInteger(summary.bytes)
        && summary.bytes >= 0
        && /^[a-f0-9]{64}$/u.test(summary.sha256)
        && JSON.stringify(Object.keys(summary).sort()) === JSON.stringify(['bytes', 'sha256'])
    })
  })
}

const root = resolve(import.meta.dirname, '..')
const { target, requireRelease } = parseArguments(process.argv.slice(2))
if (target === undefined) {
  throw new Error(
    `unsupported host ${process.platform}/${process.arch}; expected one of `
    + NATIVE_TARGETS.map(configuration => configuration.host).join(', '),
  )
}
const configuration = nativeTargetConfiguration(target)
if (configuration === undefined) throw new Error(`unsupported native target ${target}`)
const prebuildVerification = verifyNativePrebuild({
  root,
  target,
  requireRelease,
  requireCurrentHost: true,
})
if (prebuildVerification.errors.length > 0) {
  throw new Error(prebuildVerification.errors.join('\n'))
}

const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-installed-native-'))
const tarballRoot = join(temporaryRoot, 'tarballs')
const applicationRoot = join(temporaryRoot, 'application')
mkdirSync(tarballRoot)
mkdirSync(applicationRoot)

try {
  const tarballs = [
    pack(root, 'packages/contracts', tarballRoot),
    pack(root, 'packages/native', tarballRoot),
    pack(root, configuration.packageDirectory, tarballRoot),
  ]
  writeFileSync(join(applicationRoot, 'package.json'), `${JSON.stringify({
    name: 'winwincode-installed-native-smoke',
    private: true,
    type: 'module',
  }, null, 2)}\n`)
  run('npm', [
    'install',
    '--ignore-scripts',
    '--no-audit',
    '--no-fund',
    '--offline',
    '--package-lock=false',
    ...tarballs,
  ], { cwd: applicationRoot })
  const fixture = join(applicationRoot, basename('installed-native-smoke.mjs'))
  copyFileSync(join(root, 'tests', 'fixtures', 'installed-native-smoke.mjs'), fixture)
  const output = run(process.execPath, [fixture], {
    cwd: applicationRoot,
    timeout: 45_000,
  })
  const report = JSON.parse(output.trim().split('\n').at(-1))
  const expectedBuild = prebuildVerification.buildInfo
  const failures = []
  if (report.target !== target) failures.push('installed loader selected the wrong target')
  if (report.packageName !== configuration.packageName) {
    failures.push('installed loader selected the wrong optional package')
  }
  if (JSON.stringify(report.packageBuildInfo) !== JSON.stringify(expectedBuild)) {
    failures.push('installed package reported a different build identity')
  }
  if (report.kernelBuildInfo?.interfaceVersion !== 4) {
    failures.push('installed native interface version is not 4')
  }
  if (report.kernelBuildInfo?.codexCommit !== expectedBuild.source.codex.commit) {
    failures.push('installed kernel Codex commit does not match package source identity')
  }
  if (report.requests !== 2 || report.toolResultSeen !== true) {
    failures.push('keyless installed-kernel fixture did not complete its tool round trip')
  }
  if (report.workspaceWriteSucceeded !== true || report.parentWriteBlocked !== true) {
    failures.push('installed-kernel sandbox did not enforce the workspace boundary')
  }
  if (report.sandboxHelperBundled !== true) failures.push('Linux sandbox helper is missing')
  if (report.bubblewrapBundled !== true) {
    failures.push('bundled bubblewrap is missing or not executable')
  }
  const expectedSandbox = process.platform === 'darwin' ? 'macos-seatbelt' : 'linux-seccomp'
  if (
    report.governed?.sandbox !== expectedSandbox
    || report.governed?.network !== 'restricted'
    || report.governed?.environmentSecretExcluded !== true
    || report.governed?.workspaceWriteSucceeded !== true
    || report.governed?.outsideWriteBlocked !== true
    || report.governed?.credentialReadBlocked !== true
    || report.governed?.networkBlocked !== true
    || report.governed?.timeoutStopped !== true
    || report.governed?.readOnlyWriteBlocked !== true
    || report.governed?.ordinaryDenied !== true
  ) failures.push('installed governed-command boundary did not enforce every claimed mode')
  if (!hasSafeGovernedDiagnostics(report.governed?.diagnostics)) {
    failures.push('installed governed-command diagnostics are missing or expose raw output')
  }
  for (const kind of ['exec_command_begin', 'exec_command_end', 'turn_complete']) {
    if (!report.eventKinds.includes(kind)) failures.push(`installed-kernel events are missing ${kind}`)
  }
  if (!Array.isArray(report.errors) || report.errors.length > 0) {
    failures.push(`installed kernel reported errors: ${JSON.stringify(report.errors)}`)
  }
  if (failures.length > 0) {
    throw new Error(`${failures.join('\n')}\nreport=${JSON.stringify(report)}`)
  }
  process.stdout.write(
    `clean installed native package passed keyless and sandbox smokes for ${target}\n`,
  )
} finally {
  rmSync(temporaryRoot, { force: true, recursive: true })
}
