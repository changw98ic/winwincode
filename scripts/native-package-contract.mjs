import { createHash } from 'node:crypto'
import {
  existsSync,
  readFileSync,
  readdirSync,
  statSync,
} from 'node:fs'
import { join, relative, resolve } from 'node:path'

export const NATIVE_TARGETS = Object.freeze([
  Object.freeze({
    target: 'aarch64-apple-darwin',
    host: 'darwin/arm64',
    os: 'darwin',
    cpu: 'arm64',
    packageDirectory: 'packages/native-darwin-arm64',
    packageName: '@winwincode/native-darwin-arm64',
  }),
  Object.freeze({
    target: 'x86_64-apple-darwin',
    host: 'darwin/x64',
    os: 'darwin',
    cpu: 'x64',
    packageDirectory: 'packages/native-darwin-x64',
    packageName: '@winwincode/native-darwin-x64',
  }),
  Object.freeze({
    target: 'aarch64-unknown-linux-gnu',
    host: 'linux/arm64',
    os: 'linux',
    cpu: 'arm64',
    packageDirectory: 'packages/native-linux-arm64',
    packageName: '@winwincode/native-linux-arm64',
  }),
  Object.freeze({
    target: 'x86_64-unknown-linux-gnu',
    host: 'linux/x64',
    os: 'linux',
    cpu: 'x64',
    packageDirectory: 'packages/native-linux-x64',
    packageName: '@winwincode/native-linux-x64',
  }),
])

export function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

export function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

export function nativeTargetConfiguration(target) {
  return NATIVE_TARGETS.find(configuration => configuration.target === target)
}

export function hostNativeTarget(
  platform = process.platform,
  architecture = process.arch,
) {
  return NATIVE_TARGETS.find(
    configuration => configuration.host === `${platform}/${architecture}`,
  )?.target
}

export function projectSourceDigest(root) {
  const paths = [
    join(root, 'Cargo.lock'),
    join(root, 'Cargo.toml'),
    join(root, 'rust-toolchain.toml'),
  ]
  const visit = directory => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name)
      if (entry.isDirectory()) visit(path)
      else if (entry.isFile()) paths.push(path)
    }
  }
  visit(join(root, 'crates'))
  const hash = createHash('sha256')
  for (const path of paths.sort()) {
    hash.update(relative(root, path))
    hash.update('\0')
    hash.update(readFileSync(path))
    hash.update('\0')
  }
  return hash.digest('hex')
}

function descriptorError(errors, prebuildRoot, descriptor, label) {
  if (
    typeof descriptor?.path !== 'string'
    || !/^[A-Za-z0-9_./-]+$/u.test(descriptor.path)
    || descriptor.path.includes('..')
  ) {
    errors.push(`${label} has an invalid path`)
    return
  }
  if (!/^[0-9a-f]{64}$/u.test(descriptor.sha256)) {
    errors.push(`${label} has an invalid SHA-256`)
    return
  }
  if (!Number.isSafeInteger(descriptor.bytes) || descriptor.bytes < 0) {
    errors.push(`${label} has an invalid byte size`)
    return
  }
  const path = join(prebuildRoot, descriptor.path)
  if (!existsSync(path)) {
    errors.push(`${label} is missing ${descriptor.path}`)
    return
  }
  if (sha256(path) !== descriptor.sha256) errors.push(`${label} SHA-256 does not match`)
  if (statSync(path).size !== descriptor.bytes) errors.push(`${label} byte size does not match`)
}

function verifyRustDependencies(errors, root, prebuildRoot, target, buildInfo) {
  const descriptor = buildInfo.legal?.rustDependencies
  descriptorError(errors, prebuildRoot, descriptor, 'rust dependency inventory')
  if (typeof descriptor?.path !== 'string') return
  let inventory
  try {
    inventory = readJson(join(prebuildRoot, descriptor.path))
  } catch (error) {
    errors.push(`rust dependency inventory is invalid: ${error.message}`)
    return
  }
  if (inventory.schemaVersion !== 1) errors.push('rust dependency schemaVersion is not 1')
  if (inventory.target !== target) errors.push('rust dependency target does not match package')
  if (inventory.cargoLockSha256 !== sha256(join(root, 'Cargo.lock'))) {
    errors.push('rust dependency inventory does not match Cargo.lock')
  }
  if (!Array.isArray(inventory.dependencies)) {
    errors.push('rust dependency inventory has no dependency array')
    return
  }
  if (descriptor.dependencyCount !== inventory.dependencies.length) {
    errors.push('rust dependency count does not match build-info')
  }
  const referencedLicenses = new Set()
  for (const dependency of inventory.dependencies) {
    const identity = `${String(dependency.name)}@${String(dependency.version)}`
    if (
      typeof dependency.name !== 'string'
      || typeof dependency.version !== 'string'
      || typeof dependency.declaredLicense !== 'string'
      || dependency.declaredLicense.length === 0
      || !Array.isArray(dependency.authors)
      || !Array.isArray(dependency.features)
      || !Array.isArray(dependency.licenseFiles)
    ) {
      errors.push(`rust dependency ${identity} has incomplete license metadata`)
      continue
    }
    for (const licenseFile of dependency.licenseFiles) {
      const expectedPath = `licenses/${licenseFile.sha256}.txt`
      if (
        typeof licenseFile.name !== 'string'
        || !/^[0-9a-f]{64}$/u.test(licenseFile.sha256)
        || licenseFile.path !== expectedPath
      ) {
        errors.push(`rust dependency ${identity} has an invalid license-file record`)
        continue
      }
      const path = join(prebuildRoot, licenseFile.path)
      if (!existsSync(path) || sha256(path) !== licenseFile.sha256) {
        errors.push(`rust dependency ${identity} license file does not match its SHA-256`)
        continue
      }
      referencedLicenses.add(licenseFile.path)
    }
  }
  const licenseRoot = join(prebuildRoot, 'licenses')
  const packagedLicenses = existsSync(licenseRoot)
    ? readdirSync(licenseRoot).map(name => `licenses/${name}`).sort()
    : []
  if (
    JSON.stringify(packagedLicenses)
    !== JSON.stringify([...referencedLicenses].sort())
  ) {
    errors.push('packaged Rust license files do not match the dependency inventory')
  }
  if (descriptor.licenseFileCount !== referencedLicenses.size) {
    errors.push('Rust license-file count does not match build-info')
  }
}

export function verifyNativePrebuild({
  root,
  target,
  requireRelease = false,
  requireCurrentHost = false,
}) {
  const errors = []
  const configuration = nativeTargetConfiguration(target)
  if (configuration === undefined) return { errors: [`unsupported native target ${target}`] }
  const packageRoot = join(root, configuration.packageDirectory)
  const prebuildRoot = join(packageRoot, 'prebuild')
  const manifest = readJson(join(packageRoot, 'package.json'))
  if (manifest.name !== configuration.packageName) errors.push('native package name is wrong')
  if (manifest.license !== 'Apache-2.0') errors.push('native package license is not Apache-2.0')
  if (JSON.stringify(manifest.os) !== JSON.stringify([configuration.os])) {
    errors.push('native package OS selector is wrong')
  }
  if (JSON.stringify(manifest.cpu) !== JSON.stringify([configuration.cpu])) {
    errors.push('native package CPU selector is wrong')
  }
  if (manifest.winwincodeNativeTarget !== target) errors.push('native package target is wrong')
  if (requireCurrentHost && hostNativeTarget() !== target) {
    errors.push(`native package ${target} cannot be loaded on ${process.platform}/${process.arch}`)
  }

  const buildInfoPath = join(prebuildRoot, 'build-info.json')
  let buildInfo
  try {
    buildInfo = readJson(buildInfoPath)
  } catch (error) {
    return { errors: [...errors, `build-info is invalid: ${error.message}`] }
  }
  if (buildInfo.schemaVersion !== 2) errors.push('build-info schemaVersion is not 2')
  if (buildInfo.target !== target) errors.push('build-info target does not match package')
  if (buildInfo.nativeInterfaceVersion !== 2) {
    errors.push('build-info native interface version is not 2')
  }
  if (buildInfo.package?.name !== configuration.packageName) {
    errors.push('build-info package name does not match')
  }
  if (buildInfo.package?.version !== manifest.version) {
    errors.push('build-info package version does not match')
  }
  if (!['debug', 'release'].includes(buildInfo.profile)) {
    errors.push('build-info profile is invalid')
  }
  if (requireRelease && buildInfo.profile !== 'release') {
    errors.push('release verification received a non-release native package')
  }

  const workspaceManifest = readJson(join(root, 'package.json'))
  const source = buildInfo.source?.winwincode
  if (source?.repository !== workspaceManifest.repository) {
    errors.push('build-info WinWinCode repository does not match')
  }
  if (source?.version !== workspaceManifest.version) {
    errors.push('build-info WinWinCode version does not match')
  }
  if (source?.sourceSha256 !== projectSourceDigest(root)) {
    errors.push('build-info WinWinCode source SHA-256 does not match')
  }
  if (source?.cargoLockSha256 !== sha256(join(root, 'Cargo.lock'))) {
    errors.push('build-info Cargo.lock SHA-256 does not match')
  }
  if (source?.rustToolchainSha256 !== sha256(join(root, 'rust-toolchain.toml'))) {
    errors.push('build-info Rust toolchain SHA-256 does not match')
  }

  const upstreamPath = join(root, 'third_party', 'codex.UPSTREAM.json')
  const upstream = readJson(upstreamPath)
  const codex = buildInfo.source?.codex
  for (const key of ['repository', 'tag', 'commit', 'archiveSha256']) {
    if (codex?.[key] !== upstream[key]) errors.push(`build-info Codex ${key} does not match`)
  }
  if (codex?.metadataSha256 !== sha256(upstreamPath)) {
    errors.push('build-info Codex metadata SHA-256 does not match')
  }
  const expectedPatches = upstream.patchesApplied.map(path => ({
    path,
    sha256: sha256(join(root, path)),
  }))
  if (JSON.stringify(codex?.patches) !== JSON.stringify(expectedPatches)) {
    errors.push('build-info Codex patch identities do not match')
  }

  if (!String(buildInfo.toolchain?.rustc).startsWith('rustc 1.95.0 ')) {
    errors.push('build-info Rust compiler is not 1.95.0')
  }
  if (!String(buildInfo.toolchain?.cargo).startsWith('cargo 1.95.0 ')) {
    errors.push('build-info Cargo is not 1.95.0')
  }
  if (buildInfo.toolchain?.host !== target) {
    errors.push('native release was not built on its target architecture')
  }
  if (!/^v24\./u.test(String(buildInfo.toolchain?.node))) {
    errors.push('build-info Node.js compiler runtime is not Node.js 24')
  }
  if (requireRelease) {
    const pinnedNode = readFileSync(join(root, '.node-version'), 'utf8').trim()
    if (buildInfo.toolchain?.node !== `v${pinnedNode}`) {
      errors.push(`native release was not built with pinned Node.js ${pinnedNode}`)
    }
  }

  const expectedArtifactNames = [
    'winwincode-kernel-helper',
    'winwincode_native.node',
    ...(configuration.os === 'linux' ? ['codex-linux-sandbox'] : []),
  ].sort()
  const artifacts = buildInfo.artifacts ?? {}
  if (JSON.stringify(Object.keys(artifacts).sort()) !== JSON.stringify(expectedArtifactNames)) {
    errors.push('build-info artifact list does not match target')
  }
  for (const name of expectedArtifactNames) {
    descriptorError(errors, prebuildRoot, artifacts[name], `native artifact ${name}`)
    const path = join(prebuildRoot, name)
    if (existsSync(path) && (statSync(path).mode & 0o111) === 0) {
      errors.push(`native artifact ${name} is not executable`)
    }
  }
  for (const [key, name] of [
    ['license', 'LICENSE'],
    ['notice', 'NOTICE'],
    ['thirdPartyNotices', 'THIRD_PARTY_NOTICES.md'],
  ]) {
    const descriptor = buildInfo.legal?.[key]
    descriptorError(errors, prebuildRoot, descriptor, `legal file ${name}`)
    if (descriptor?.path !== name) errors.push(`legal file ${name} has the wrong path`)
    if (existsSync(join(root, name)) && descriptor?.sha256 !== sha256(join(root, name))) {
      errors.push(`legal file ${name} does not match the canonical repository copy`)
    }
  }
  verifyRustDependencies(errors, root, prebuildRoot, target, buildInfo)
  return { errors, buildInfo, configuration, packageRoot, prebuildRoot }
}
