#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import {
  chmodSync,
  copyFileSync,
  existsSync,
  linkSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'

import {
  NATIVE_TARGETS,
  hostNativeTarget,
  nativeTargetConfiguration,
  projectSourceDigest,
} from './native-package-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const legalFilePattern = /^(?:license|licence|copying|notice|copyright)(?:[._-]|$)/iu

function parseArguments(argv) {
  let release = false
  let requestedTarget
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--release') {
      release = true
      continue
    }
    if (argument === '--target') {
      requestedTarget = argv[index + 1]
      if (requestedTarget === undefined) throw new Error('--target requires a Rust target triple')
      index += 1
      continue
    }
    if (argument.startsWith('--target=')) {
      requestedTarget = argument.slice('--target='.length)
      continue
    }
    throw new Error(`unknown build-native argument: ${argument}`)
  }
  return { release, requestedTarget }
}

function sha256Bytes(value) {
  return createHash('sha256').update(value).digest('hex')
}

function digest(path) {
  return sha256Bytes(readFileSync(path))
}

function fileDescriptor(path, name) {
  return {
    path: name,
    sha256: digest(path),
    bytes: statSync(path).size,
  }
}

function copyExecutable(source, destination) {
  copyFileSync(source, destination)
  chmodSync(destination, 0o755)
}

function capturedCommand(command, arguments_) {
  const result = spawnSync(command, arguments_, {
    cwd: root,
    encoding: 'utf8',
    env: process.env,
    maxBuffer: 128 * 1024 * 1024,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new Error(
      `${command} ${arguments_.join(' ')} failed: ${(result.stderr || result.stdout).trim()}`,
    )
  }
  return result.stdout.trim()
}

function buildBundledBwrap(target, profile) {
  const codexCargoRoot = join(root, 'third_party', 'codex', 'codex-rs')
  const targetRoot = join(root, 'target', 'codex-bwrap')
  const arguments_ = [
    'build',
    '--locked',
    '--target-dir',
    targetRoot,
    '--target',
    target,
    '--bin',
    'bwrap',
  ]
  if (profile === 'release') arguments_.push('--release')
  const build = spawnSync('cargo', arguments_, {
    cwd: codexCargoRoot,
    env: process.env,
    stdio: 'inherit',
  })
  if (build.error !== undefined) throw build.error
  if (build.status !== 0) {
    throw new Error(
      `bundled bwrap build failed with ${build.signal ?? `exit code ${build.status}`}`,
    )
  }
  const path = join(targetRoot, target, profile, 'bwrap')
  if (!existsSync(path)) throw new Error(`cargo did not produce bundled bwrap: ${path}`)
  if (profile === 'release') {
    capturedCommand('strip', ['--strip-debug', '--strip-unneeded', path])
  }
  return { path, sha256: digest(path) }
}

function rustToolchain() {
  const verbose = capturedCommand('rustc', ['-Vv'])
  const lines = verbose.split('\n')
  const fields = Object.fromEntries(lines.slice(1).map(line => {
    const separator = line.indexOf(':')
    return separator < 0
      ? [line, '']
      : [line.slice(0, separator), line.slice(separator + 1).trim()]
  }))
  return {
    rustc: lines[0],
    commitHash: fields['commit-hash'],
    host: fields.host,
    llvmVersion: fields['LLVM version'],
    cargo: capturedCommand('cargo', ['-V']),
  }
}

function cargoMetadata(target) {
  return JSON.parse(capturedCommand('cargo', [
    'metadata',
    '--locked',
    '--format-version',
    '1',
    '--filter-platform',
    target,
  ]))
}

function reachablePackageIds(metadata) {
  const nodes = new Map(metadata.resolve.nodes.map(node => [node.id, node]))
  const reachable = new Set()
  const pending = [...metadata.workspace_members]
  while (pending.length > 0) {
    const packageId = pending.pop()
    if (packageId === undefined || reachable.has(packageId)) continue
    reachable.add(packageId)
    for (const dependency of nodes.get(packageId)?.deps ?? []) pending.push(dependency.pkg)
  }
  return { nodes, reachable }
}

function legalSourceFiles(package_) {
  const packageRoot = dirname(package_.manifest_path)
  const paths = new Set()
  if (typeof package_.license_file === 'string') paths.add(resolve(package_.license_file))
  for (const entry of readdirSync(packageRoot, { withFileTypes: true })) {
    if (entry.isFile() && legalFilePattern.test(entry.name)) paths.add(join(packageRoot, entry.name))
  }
  return [...paths].filter(path => existsSync(path)).sort()
}

function stageRustDependencyNotices(target, prebuildRoot) {
  const metadata = cargoMetadata(target)
  const workspacePackages = new Set(metadata.workspace_members)
  const codexRoot = join(root, 'third_party', 'codex')
  const { nodes, reachable } = reachablePackageIds(metadata)
  const licenseRoot = join(prebuildRoot, 'licenses')
  mkdirSync(licenseRoot, { recursive: true })
  const copiedDigests = new Set()
  const dependencies = metadata.packages
    .filter(package_ => (
      reachable.has(package_.id)
      && !workspacePackages.has(package_.id)
      && !resolve(package_.manifest_path).startsWith(`${codexRoot}/`)
    ))
    .map(package_ => {
      const licenseFiles = legalSourceFiles(package_).map(path => {
        const sha256 = digest(path)
        const bundledPath = `licenses/${sha256}.txt`
        if (!copiedDigests.has(sha256)) {
          copyFileSync(path, join(prebuildRoot, bundledPath))
          copiedDigests.add(sha256)
        }
        return {
          name: relative(dirname(package_.manifest_path), path),
          path: bundledPath,
          sha256,
        }
      })
      return {
        name: package_.name,
        version: package_.version,
        source: package_.source,
        repository: package_.repository,
        authors: package_.authors,
        declaredLicense: package_.license,
        features: [...(nodes.get(package_.id)?.features ?? [])].sort(),
        licenseFiles,
      }
    })
    .sort((left, right) => (
      left.name.localeCompare(right.name)
      || left.version.localeCompare(right.version)
      || String(left.source).localeCompare(String(right.source))
    ))

  const inventory = {
    schemaVersion: 1,
    target,
    cargoLockSha256: digest(join(root, 'Cargo.lock')),
    dependencies,
  }
  const inventoryPath = join(prebuildRoot, 'rust-dependencies.json')
  writeFileSync(inventoryPath, `${JSON.stringify(inventory, null, 2)}\n`)
  return {
    inventoryPath,
    dependencyCount: dependencies.length,
    licenseFileCount: copiedDigests.size,
  }
}

const { release, requestedTarget } = parseArguments(process.argv.slice(2))
const hostTarget = hostNativeTarget()
const target = requestedTarget ?? hostTarget
const targetConfiguration = target === undefined ? undefined : nativeTargetConfiguration(target)
if (target === undefined || targetConfiguration === undefined) {
  throw new Error(
    `unsupported native target ${target ?? `${process.platform}/${process.arch}`}; `
    + `expected one of ${NATIVE_TARGETS.map(configuration => configuration.target).join(', ')}`,
  )
}

const targetPackageRoot = join(root, targetConfiguration.packageDirectory)
const targetManifest = JSON.parse(readFileSync(join(targetPackageRoot, 'package.json'), 'utf8'))
if (
  targetManifest.name !== targetConfiguration.packageName
  || targetManifest.winwincodeNativeTarget !== target
) {
  throw new Error(`${targetConfiguration.packageDirectory} does not declare native target ${target}`)
}

const profile = release ? 'release' : 'debug'
const bundledBwrap = targetConfiguration.os === 'linux'
  ? buildBundledBwrap(target, profile)
  : undefined
const cargoArguments = ['build', '--workspace', '--locked']
if (release) cargoArguments.push('--release')
if (requestedTarget !== undefined) cargoArguments.push('--target', target)
const build = spawnSync('cargo', cargoArguments, {
  cwd: root,
  env: bundledBwrap === undefined
    ? process.env
    : { ...process.env, CODEX_BWRAP_SHA256: bundledBwrap.sha256 },
  stdio: 'inherit',
})
if (build.error !== undefined) throw build.error
if (build.status !== 0) {
  throw new Error(`cargo build failed with ${build.signal ?? `exit code ${build.status}`}`)
}

const artifactRoot = requestedTarget === undefined
  ? join(root, 'target', profile)
  : join(root, 'target', target, profile)
const nativeLibraryName = target.includes('apple-darwin')
  ? 'libwinwincode_native.dylib'
  : 'libwinwincode_native.so'
const sourceNative = join(artifactRoot, nativeLibraryName)
const sourceHelper = join(artifactRoot, 'winwincode-kernel-helper')
for (const source of [sourceNative, sourceHelper]) {
  if (!existsSync(source)) throw new Error(`cargo did not produce expected artifact: ${source}`)
}

const prebuildRoot = join(targetPackageRoot, 'prebuild')
rmSync(prebuildRoot, { force: true, recursive: true })
mkdirSync(prebuildRoot, { recursive: true })
const nativeDestination = join(prebuildRoot, 'winwincode_native.node')
const helperDestination = join(prebuildRoot, 'winwincode-kernel-helper')
copyExecutable(sourceNative, nativeDestination)
copyExecutable(sourceHelper, helperDestination)

const artifactPaths = new Map([
  ['winwincode-kernel-helper', helperDestination],
  ['winwincode_native.node', nativeDestination],
])
let bubblewrapLicenseDestination
if (targetConfiguration.os === 'linux') {
  const sandboxDestination = join(prebuildRoot, 'codex-linux-sandbox')
  linkSync(helperDestination, sandboxDestination)
  chmodSync(sandboxDestination, 0o755)
  artifactPaths.set('codex-linux-sandbox', sandboxDestination)
  const resourceRoot = join(prebuildRoot, 'codex-resources')
  mkdirSync(resourceRoot)
  const bwrapDestination = join(resourceRoot, 'bwrap')
  copyExecutable(bundledBwrap.path, bwrapDestination)
  artifactPaths.set('codex-resources/bwrap', bwrapDestination)
  bubblewrapLicenseDestination = join(resourceRoot, 'bwrap.LICENSE')
  copyFileSync(
    join(root, 'third_party', 'codex', 'codex-rs', 'vendor', 'bubblewrap', 'COPYING'),
    bubblewrapLicenseDestination,
  )
}

const legalSourceNames = ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']
for (const name of legalSourceNames) copyFileSync(join(root, name), join(prebuildRoot, name))
const rustNotices = stageRustDependencyNotices(target, prebuildRoot)

const codexSourcePath = join(root, 'third_party', 'codex.UPSTREAM.json')
const codexSource = JSON.parse(readFileSync(codexSourcePath, 'utf8'))
const workspaceManifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'))
const codexPatches = codexSource.patchesApplied.map(path => ({
  path,
  sha256: digest(join(root, path)),
}))
const buildInfo = {
  schemaVersion: 2,
  package: {
    name: targetManifest.name,
    version: targetManifest.version,
  },
  target,
  profile,
  nativeInterfaceVersion: 2,
  source: {
    winwincode: {
      repository: workspaceManifest.repository
        ?? 'https://github.com/changw98ic/winwincode',
      version: workspaceManifest.version,
      sourceSha256: projectSourceDigest(root),
      cargoLockSha256: digest(join(root, 'Cargo.lock')),
      rustToolchainSha256: digest(join(root, 'rust-toolchain.toml')),
    },
    codex: {
      repository: codexSource.repository,
      tag: codexSource.tag,
      commit: codexSource.commit,
      archiveSha256: codexSource.archiveSha256,
      metadataSha256: digest(codexSourcePath),
      patches: codexPatches,
    },
  },
  toolchain: {
    node: process.version,
    ...rustToolchain(),
  },
  artifacts: Object.fromEntries([...artifactPaths].map(([name, path]) => [
    name,
    fileDescriptor(path, name),
  ])),
  legal: {
    license: fileDescriptor(join(prebuildRoot, 'LICENSE'), 'LICENSE'),
    notice: fileDescriptor(join(prebuildRoot, 'NOTICE'), 'NOTICE'),
    thirdPartyNotices: fileDescriptor(
      join(prebuildRoot, 'THIRD_PARTY_NOTICES.md'),
      'THIRD_PARTY_NOTICES.md',
    ),
    ...(bubblewrapLicenseDestination === undefined
      ? {}
      : {
          bubblewrapLicense: fileDescriptor(
            bubblewrapLicenseDestination,
            'codex-resources/bwrap.LICENSE',
          ),
        }),
    rustDependencies: {
      ...fileDescriptor(rustNotices.inventoryPath, 'rust-dependencies.json'),
      dependencyCount: rustNotices.dependencyCount,
      licenseFileCount: rustNotices.licenseFileCount,
    },
  },
}
writeFileSync(
  join(prebuildRoot, 'build-info.json'),
  `${JSON.stringify(buildInfo, null, 2)}\n`,
)
process.stdout.write(
  `staged ${target} ${profile} native package in ${targetConfiguration.packageDirectory}\n`,
)
