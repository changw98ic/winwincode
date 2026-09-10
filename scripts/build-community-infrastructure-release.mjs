#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import {
  createHash,
  createPrivateKey,
  createPublicKey,
  sign,
  verify,
} from 'node:crypto'
import {
  copyFile,
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  stat,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { basename, dirname, isAbsolute, join, relative, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const DEFAULT_CONTRACT = 'docs/decisions/0031-infrastructure-ownership.json'
const LOCK_SCHEMA = 'schema/winwincode/infrastructure-lock.schema.json'
const LEGAL_FILES = ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']
const FORBIDDEN_DEPENDENCY = /^(?:diesel|postgres|rusqlite|sqlx-postgres|tokio-postgres)$/u
const SHA256 = /^[0-9a-f]{64}$/u
const SOURCE_COMMIT = /^[0-9a-f]{40}$/u

export class InfrastructureReleaseError extends Error {
  constructor(code, message) {
    super(message)
    this.name = 'InfrastructureReleaseError'
    this.code = code
  }
}

function fail(code, message) {
  throw new InfrastructureReleaseError(code, message)
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function repositoryPath(value) {
  return typeof value === 'string'
    && value.length > 0
    && !isAbsolute(value)
    && value !== '..'
    && !value.startsWith('../')
    && !value.includes('/../')
    && !value.includes('\\')
}

function unique(values) {
  return new Set(values).size === values.length
}

export function validateInfrastructureContract(contract) {
  const errors = []
  if (!isObject(contract)) return ['contract must be an object']
  if (contract.schemaVersion !== 2) errors.push('schemaVersion must be 2')
  if (contract.kind !== 'winwincode.infrastructure-release-contract.v2') {
    errors.push('kind must be winwincode.infrastructure-release-contract.v2')
  }
  if (contract.status !== 'accepted') errors.push('status must be accepted')
  if (contract.ownerRepository !== 'winwincode') errors.push('ownerRepository must be winwincode')
  if (!isObject(contract.release)) errors.push('release must be an object')
  if (!isObject(contract.signing)) errors.push('signing must be an object')
  if (!Array.isArray(contract.packages) || contract.packages.length === 0) {
    errors.push('packages must be a non-empty array')
  } else {
    const names = contract.packages.map(entry => entry?.name)
    if (!names.every(name => typeof name === 'string' && name.startsWith('winwincode-'))) {
      errors.push('every package must have a winwincode-* name')
    }
    if (!unique(names)) errors.push('package names must be unique')
    for (const entry of contract.packages) {
      if (!repositoryPath(entry?.manifest)) errors.push(`invalid package manifest: ${entry?.manifest}`)
      if (typeof entry?.role !== 'string' || entry.role.length === 0) {
        errors.push(`missing package role: ${entry?.name}`)
      }
      if (FORBIDDEN_DEPENDENCY.test(entry?.name ?? '')) {
        errors.push(`database runtime package is forbidden: ${entry.name}`)
      }
    }
  }
  for (const field of ['version', 'sourceTag', 'baseUri', 'manifest', 'signature', 'consumerLock', 'sbom', 'licenseManifest', 'fixtureCargoLock']) {
    if (typeof contract.release?.[field] !== 'string' || contract.release[field].length === 0) {
      errors.push(`release.${field} must be a non-empty string`)
    }
  }
  if (!contract.release?.baseUri?.startsWith('https://')) errors.push('release.baseUri must use HTTPS')
  if (contract.signing?.algorithm !== 'Ed25519') errors.push('signing.algorithm must be Ed25519')
  if (!repositoryPath(contract.signing?.publicKey)) errors.push('signing.publicKey must be repository-relative')
  if (!SHA256.test(contract.signing?.publicKeySha256 ?? '')) {
    errors.push('signing.publicKeySha256 must be SHA-256')
  }
  if (!repositoryPath(contract.persistenceContract?.source)) {
    errors.push('persistenceContract.source must be repository-relative')
  }
  if (typeof contract.persistenceContract?.artifact !== 'string') {
    errors.push('persistenceContract.artifact must be a file name')
  }
  if (!Array.isArray(contract.consumers) || !unique(contract.consumers)) {
    errors.push('consumers must be a unique array')
  }
  return errors
}

function descriptorValid(value) {
  return isObject(value)
    && typeof value.fileName === 'string'
    && value.artifactUri?.startsWith('https://')
    && SHA256.test(value.sha256 ?? '')
    && Number.isSafeInteger(value.bytes)
    && value.bytes > 0
}

export function validateInfrastructureLock(lock, consumer) {
  const errors = []
  if (!isObject(lock)) return ['lock must be an object']
  if (lock.schemaVersion !== 1) errors.push('schemaVersion must be 1')
  if (lock.kind !== 'winwincode.infrastructure-lock.v1') {
    errors.push('kind must be winwincode.infrastructure-lock.v1')
  }
  if (consumer !== undefined && lock.consumerRepository !== consumer) {
    errors.push(`consumerRepository must be ${consumer}`)
  }
  if (typeof lock.release?.version !== 'string' || lock.release.version.length === 0) {
    errors.push('release.version must be set')
  }
  if (!SOURCE_COMMIT.test(lock.release?.sourceCommit ?? '')) {
    errors.push('release.sourceCommit must be a full Git commit')
  }
  for (const field of ['manifest', 'signature', 'sbom', 'licenseManifest', 'persistenceContract', 'fixtureCargoLock']) {
    if (!descriptorValid(lock[field])) errors.push(`${field} must be a pinned HTTPS artifact`)
  }
  if (lock.signature?.algorithm !== 'Ed25519') errors.push('signature.algorithm must be Ed25519')
  if (!SHA256.test(lock.signature?.signingKeySha256 ?? '')) {
    errors.push('signature.signingKeySha256 must be SHA-256')
  }
  if (!Array.isArray(lock.packages) || lock.packages.length === 0) {
    errors.push('packages must be a non-empty array')
  } else {
    const names = lock.packages.map(entry => entry?.name)
    if (!unique(names)) errors.push('package names must be unique')
    for (const entry of lock.packages) {
      if (!descriptorValid(entry)) errors.push(`package ${entry?.name} must pin an HTTPS artifact`)
      if (entry?.version !== lock.release?.version) errors.push(`package ${entry?.name} must use the release version`)
      if (!repositoryPath(entry?.bundlePath)) errors.push(`package ${entry?.name} must have a bundle path`)
    }
  }
  if (/"(?:git|path)"\s*:/u.test(JSON.stringify(lock))) {
    errors.push('consumer lock must not contain git or local path dependencies')
  }
  return errors
}

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    cwd: options.cwd,
    encoding: options.encoding ?? 'utf8',
    env: options.env ?? process.env,
    maxBuffer: 256 * 1024 * 1024,
    stdio: options.stdio,
  })
  if (result.error) fail('COMMAND_FAILED', `${command}: ${result.error.message}`)
  if (result.status !== 0) {
    const detail = `${result.stdout ?? ''}\n${result.stderr ?? ''}`.trim()
    fail('COMMAND_FAILED', `${command} ${arguments_.join(' ')} failed${detail ? `:\n${detail}` : ''}`)
  }
  return result.stdout ?? ''
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function jsonBytes(value) {
  return `${JSON.stringify(value, null, 2)}\n`
}

async function writeJson(path, value) {
  const bytes = jsonBytes(value)
  await writeFile(path, bytes)
  return bytes
}

async function fileDescriptor(path, baseUri, extra = {}) {
  const bytes = await readFile(path)
  const fileName = basename(path)
  return {
    ...extra,
    fileName,
    artifactUri: `${baseUri}/${fileName}`,
    sha256: sha256(bytes),
    bytes: bytes.length,
  }
}

function packageDependencies(manifest) {
  const names = []
  let section = ''
  for (const line of manifest.split(/\r?\n/u)) {
    const header = /^\s*\[([^\u005d]+)\]\s*$/u.exec(line)
    if (header) {
      section = header[1]
      continue
    }
    if (section !== 'dependencies') continue
    const dependency = /^\s*([A-Za-z0-9_-]+)(?:\.workspace)?\s*=/u.exec(line)
    if (dependency) names.push(dependency[1])
  }
  return names
}

function stripDevelopmentDependencies(manifest) {
  const lines = manifest.split(/\r?\n/u)
  const output = []
  let skip = false
  for (const line of lines) {
    const header = /^\s*\[([^\u005d]+)\]\s*$/u.exec(line)
    if (header) skip = header[1] === 'dev-dependencies'
    if (!skip) output.push(line)
  }
  return `${output.join('\n').trimEnd()}\n`
}

function workspaceDependencies(manifest) {
  const dependencies = new Map()
  let inWorkspaceDependencies = false
  for (const line of manifest.split(/\r?\n/u)) {
    const header = /^\s*\[([^\u005d]+)\]\s*$/u.exec(line)
    if (header) {
      inWorkspaceDependencies = header[1] === 'workspace.dependencies'
      continue
    }
    if (!inWorkspaceDependencies) continue
    const dependency = /^\s*([A-Za-z0-9_-]+)\s*=\s*(.+)$/u.exec(line)
    if (dependency) dependencies.set(dependency[1], dependency[2])
  }
  return dependencies
}

function workspaceManifest(contract, dependencyLines) {
  const members = contract.packages.map(entry => dirname(entry.manifest))
  const dependencies = [...dependencyLines]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([name, value]) => `${name} = ${value}`)
    .join('\n')
  return `[workspace]\nmembers = ${JSON.stringify(members, null, 2)}\nresolver = "2"\n\n[workspace.package]\nversion = "${contract.release.version}"\nedition = "2024"\nrust-version = "1.95"\nlicense = "Apache-2.0"\nrepository = "https://github.com/changw98ic/winwincode"\n\n[workspace.dependencies]\n${dependencies}\n\n[workspace.lints.rust]\nunsafe_code = "deny"\nunused_qualifications = "warn"\n\n[workspace.lints.clippy]\nall = { level = "warn", priority = -1 }\npedantic = { level = "warn", priority = -1 }\nmodule_name_repetitions = "allow"\nmust_use_candidate = "allow"\n`
}

async function stageWorkspace(sourceRoot, stageRoot, contract) {
  const rootManifest = await readFile(join(sourceRoot, 'Cargo.toml'), 'utf8')
  const rootDependencies = workspaceDependencies(rootManifest)
  const requiredDependencies = new Set()
  for (const entry of contract.packages) {
    const sourceManifest = join(sourceRoot, entry.manifest)
    const manifest = await readFile(sourceManifest, 'utf8')
    const packageName = /^name\s*=\s*"([^"]+)"/mu.exec(manifest)?.[1]
    if (packageName !== entry.name) {
      fail('CONTRACT_MISMATCH', `${entry.manifest} does not define ${entry.name}`)
    }
    for (const name of packageDependencies(manifest)) requiredDependencies.add(name)
    const targetDirectory = join(stageRoot, dirname(entry.manifest))
    await mkdir(targetDirectory, { recursive: true })
    await cp(join(dirname(sourceManifest), 'src'), join(targetDirectory, 'src'), { recursive: true })
    const readme = join(dirname(sourceManifest), 'README.md')
    try {
      await copyFile(readme, join(targetDirectory, 'README.md'))
    } catch (error) {
      if (error.code !== 'ENOENT') throw error
    }
    await writeFile(join(targetDirectory, 'Cargo.toml'), stripDevelopmentDependencies(manifest))
  }
  const selected = new Set(contract.packages.map(entry => entry.name))
  const dependencyLines = new Map()
  for (const name of requiredDependencies) {
    if (FORBIDDEN_DEPENDENCY.test(name)) fail('DATABASE_RUNTIME_FOUND', `forbidden runtime dependency: ${name}`)
    const value = rootDependencies.get(name)
    if (value === undefined) continue
    if (/\bpath\s*=/u.test(value) && !selected.has(name)) {
      fail('NON_RELEASE_DEPENDENCY', `${name} is a non-released local runtime dependency`)
    }
    dependencyLines.set(name, value)
  }
  await writeFile(join(stageRoot, 'Cargo.toml'), workspaceManifest(contract, dependencyLines))
  for (const file of LEGAL_FILES) await copyFile(join(sourceRoot, file), join(stageRoot, file))
}

async function sourceBoundaryCheck(stageRoot, contract) {
  const patterns = contract.forbiddenReleaseContent.sourcePatterns.map(value => new RegExp(value, 'u'))
  const pathPatterns = contract.forbiddenReleaseContent.pathPatterns
  for (const entry of contract.packages) {
    const sourceDirectory = join(stageRoot, dirname(entry.manifest), 'src')
    const pending = [sourceDirectory]
    while (pending.length > 0) {
      const directory = pending.pop()
      for (const item of await readdir(directory, { withFileTypes: true })) {
        const path = join(directory, item.name)
        const bundlePath = relative(stageRoot, path).replaceAll('\\', '/')
        if (pathPatterns.some(pattern => bundlePath.includes(pattern))) {
          fail('DATABASE_RUNTIME_FOUND', `forbidden release path: ${bundlePath}`)
        }
        if (item.isDirectory()) pending.push(path)
        if (!item.isFile()) continue
        const source = await readFile(path, 'utf8')
        const pattern = patterns.find(candidate => candidate.test(source))
        if (pattern) fail('DATABASE_RUNTIME_FOUND', `forbidden source in ${bundlePath}: ${pattern.source}`)
      }
    }
  }
}

function metadataGraph(metadata, roots) {
  const packages = new Map(metadata.packages.map(entry => [entry.id, entry]))
  const nodes = new Map((metadata.resolve?.nodes ?? []).map(entry => [entry.id, entry]))
  const rootIds = metadata.packages
    .filter(entry => roots.has(entry.name) && entry.source === null)
    .map(entry => entry.id)
  const closure = new Set(rootIds)
  const pending = [...rootIds]
  while (pending.length > 0) {
    const id = pending.pop()
    for (const dependency of nodes.get(id)?.deps ?? []) {
      const runtime = dependency.dep_kinds.some(kind => kind.kind !== 'dev')
      if (!runtime || closure.has(dependency.pkg)) continue
      closure.add(dependency.pkg)
      pending.push(dependency.pkg)
    }
  }
  return { closure, nodes, packages }
}

function buildSbom(metadata, contract, sourceCommit) {
  const roots = new Set(contract.packages.map(entry => entry.name))
  const { closure, nodes, packages } = metadataGraph(metadata, roots)
  const reference = package_ => `pkg:cargo/${encodeURIComponent(package_.name)}@${encodeURIComponent(package_.version)}`
  const components = [...closure]
    .map(id => packages.get(id))
    .sort((left, right) => `${left.name}@${left.version}`.localeCompare(`${right.name}@${right.version}`))
    .map(package_ => ({
      type: 'library',
      'bom-ref': reference(package_),
      name: package_.name,
      version: package_.version,
      licenses: package_.license ? [{ license: { id: package_.license } }] : undefined,
      purl: reference(package_),
    }))
    .map(component => Object.fromEntries(Object.entries(component).filter(([, value]) => value !== undefined)))
  const dependencies = [...closure]
    .map(id => ({
      ref: reference(packages.get(id)),
      dependsOn: (nodes.get(id)?.deps ?? [])
        .filter(dependency => closure.has(dependency.pkg) && dependency.dep_kinds.some(kind => kind.kind !== 'dev'))
        .map(dependency => reference(packages.get(dependency.pkg)))
        .sort(),
    }))
    .sort((left, right) => left.ref.localeCompare(right.ref))
  return {
    bomFormat: 'CycloneDX',
    specVersion: '1.5',
    version: 1,
    metadata: {
      component: {
        type: 'application',
        name: 'winwincode-community-infrastructure',
        version: contract.release.version,
      },
      properties: [{ name: 'winwincode:sourceCommit', value: sourceCommit }],
    },
    components,
    dependencies,
  }
}

async function buildAndCheckBundle(sourceRoot, workRoot, outputDirectory, contract) {
  const bundleDirectoryName = `winwincode-community-infrastructure-${contract.release.version}`
  const bundleRoot = join(workRoot, bundleDirectoryName)
  await mkdir(bundleRoot, { recursive: true })
  await stageWorkspace(sourceRoot, bundleRoot, contract)
  await sourceBoundaryCheck(bundleRoot, contract)
  const targetDirectory = join(workRoot, 'target')
  const cargoEnvironment = { ...process.env, CARGO_NET_OFFLINE: 'true', CARGO_TARGET_DIR: targetDirectory }
  run('cargo', ['generate-lockfile', '--offline'], { cwd: bundleRoot, env: cargoEnvironment })
  run('cargo', ['check', '--workspace', '--locked', '--offline'], { cwd: bundleRoot, env: cargoEnvironment })
  const fixtureLockPath = join(outputDirectory, contract.release.fixtureCargoLock)
  await copyFile(join(bundleRoot, 'Cargo.lock'), fixtureLockPath)

  const bundlePath = join(outputDirectory, `community-infrastructure-rust-${contract.release.version}.tar.gz`)
  run('tar', ['-czf', bundlePath, '-C', workRoot, bundleDirectoryName], {
    env: { ...process.env, COPYFILE_DISABLE: '1' },
  })
  const verifyRoot = join(workRoot, 'download-check')
  await mkdir(verifyRoot)
  run('tar', ['-xzf', bundlePath, '-C', verifyRoot])
  run('cargo', ['check', '--workspace', '--frozen', '--offline'], {
    cwd: join(verifyRoot, bundleDirectoryName),
    env: { ...cargoEnvironment, CARGO_TARGET_DIR: join(workRoot, 'verify-target') },
  })
  const metadata = JSON.parse(run('cargo', ['metadata', '--locked', '--offline', '--format-version', '1'], {
    cwd: bundleRoot,
    env: cargoEnvironment,
  }))
  return { bundleDirectoryName, bundlePath, fixtureLockPath, metadata }
}

function lockFromManifest(manifest, manifestDescriptor, signatureDescriptor, consumer) {
  const byName = new Map(manifest.artifacts.map(entry => [entry.fileName, entry]))
  const copy = value => structuredClone(value)
  return {
    schemaVersion: 1,
    kind: 'winwincode.infrastructure-lock.v1',
    consumerRepository: consumer,
    release: {
      version: manifest.version,
      sourceCommit: manifest.sourceCommit,
      sourceTag: manifest.sourceTag,
    },
    manifest: copy(manifestDescriptor),
    signature: {
      ...copy(signatureDescriptor),
      algorithm: 'Ed25519',
      signingKeySha256: manifest.signing.publicKeySha256,
    },
    sbom: copy(byName.get(manifest.sbom.fileName)),
    licenseManifest: copy(byName.get(manifest.licenseManifest.fileName)),
    persistenceContract: copy(byName.get(manifest.persistenceContract.fileName)),
    fixtureCargoLock: copy(byName.get(manifest.fixtureCargoLock.fileName)),
    packages: copy(manifest.packages),
  }
}

export async function verifyInfrastructureRelease(outputDirectory, consumers) {
  const manifestPath = join(outputDirectory, 'community-infrastructure-release-manifest.json')
  const signaturePath = join(outputDirectory, 'community-infrastructure-release-manifest.json.sig')
  const manifestBytes = await readFile(manifestPath)
  const manifest = JSON.parse(manifestBytes)
  const publicKeyArtifact = manifest.artifacts.find(entry => entry.fileName === manifest.signing.publicKeyFile)
  if (!publicKeyArtifact) fail('VERIFY_FAILED', 'public key artifact is missing')
  const publicKeyBytes = await readFile(join(outputDirectory, publicKeyArtifact.fileName))
  const publicKey = createPublicKey(publicKeyBytes)
  const fingerprint = sha256(publicKey.export({ type: 'spki', format: 'der' }))
  if (fingerprint !== manifest.signing.publicKeySha256) fail('VERIFY_FAILED', 'public key fingerprint differs')
  const signatureBytes = Buffer.from((await readFile(signaturePath, 'utf8')).trim(), 'base64')
  if (!verify(null, manifestBytes, publicKey, signatureBytes)) fail('VERIFY_FAILED', 'manifest signature differs')
  for (const descriptor of manifest.artifacts) {
    const bytes = await readFile(join(outputDirectory, descriptor.fileName))
    if (bytes.length !== descriptor.bytes || sha256(bytes) !== descriptor.sha256) {
      fail('VERIFY_FAILED', `artifact differs: ${descriptor.fileName}`)
    }
  }
  for (const consumer of consumers) {
    const lockPath = join(outputDirectory, `${consumer}.infrastructure.lock.json`)
    const lock = JSON.parse(await readFile(lockPath, 'utf8'))
    const errors = validateInfrastructureLock(lock, consumer)
    if (errors.length > 0) fail('VERIFY_FAILED', errors.join('; '))
  }
  return { manifest, sourceCommit: manifest.sourceCommit }
}

export async function buildInfrastructureRelease({ root, contractPath, outputDirectory, signingKeyPath }) {
  const repositoryRoot = resolve(root)
  const outputRoot = resolve(outputDirectory)
  const contractRelative = relative(repositoryRoot, resolve(contractPath)).replaceAll('\\', '/')
  if (!repositoryPath(contractRelative)) fail('INVALID_ARGUMENT', 'contract must be inside the repository')
  await rm(outputRoot, { recursive: true, force: true })
  await mkdir(outputRoot, { recursive: true })
  const workRoot = await mkdtemp(join(tmpdir(), 'winwincode-infrastructure-release-'))
  try {
    const sourceCommit = run('git', ['rev-parse', 'HEAD'], { cwd: repositoryRoot }).trim()
    if (!SOURCE_COMMIT.test(sourceCommit)) fail('GIT_ERROR', 'HEAD is not a full Git commit')
    const archivePath = join(workRoot, 'source.tar')
    run('git', ['archive', '--format=tar', '--output', archivePath, 'HEAD'], { cwd: repositoryRoot })
    const sourceRoot = join(workRoot, 'source')
    await mkdir(sourceRoot)
    run('tar', ['-xf', archivePath, '-C', sourceRoot])
    const contract = JSON.parse(await readFile(join(sourceRoot, contractRelative), 'utf8'))
    const contractErrors = validateInfrastructureContract(contract)
    if (contractErrors.length > 0) fail('CONTRACT_INVALID', contractErrors.join('; '))

    const privateKey = createPrivateKey(await readFile(signingKeyPath))
    const publicKey = createPublicKey(privateKey)
    const publicKeyBytes = Buffer.from(publicKey.export({ type: 'spki', format: 'pem' }))
    const publicKeySha256 = sha256(publicKey.export({ type: 'spki', format: 'der' }))
    if (publicKeySha256 !== contract.signing.publicKeySha256) fail('SIGNING_KEY_MISMATCH', 'private key does not match the release contract')
    const committedPublicKey = await readFile(join(sourceRoot, contract.signing.publicKey))
    if (!publicKeyBytes.equals(committedPublicKey)) fail('SIGNING_KEY_MISMATCH', 'committed public key differs')

    const { bundleDirectoryName, bundlePath, fixtureLockPath, metadata } = await buildAndCheckBundle(
      sourceRoot,
      workRoot,
      outputRoot,
      contract,
    )
    const baseUri = contract.release.baseUri
    const bundleDescriptor = await fileDescriptor(bundlePath, baseUri, { format: 'cargo-workspace-tar-gzip' })
    const fixtureLockDescriptor = await fileDescriptor(fixtureLockPath, baseUri)

    const persistencePath = join(outputRoot, contract.persistenceContract.artifact)
    await copyFile(join(sourceRoot, contract.persistenceContract.source), persistencePath)
    const persistenceDescriptor = await fileDescriptor(persistencePath, baseUri)
    const schemaPath = join(outputRoot, basename(LOCK_SCHEMA))
    await copyFile(join(sourceRoot, LOCK_SCHEMA), schemaPath)
    const schemaDescriptor = await fileDescriptor(schemaPath, baseUri)
    const publicKeyPath = join(outputRoot, basename(contract.signing.publicKey))
    await writeFile(publicKeyPath, publicKeyBytes)
    const publicKeyDescriptor = await fileDescriptor(publicKeyPath, baseUri)
    const legalDescriptors = []
    for (const file of LEGAL_FILES) {
      const target = join(outputRoot, file)
      await copyFile(join(sourceRoot, file), target)
      legalDescriptors.push(await fileDescriptor(target, baseUri))
    }

    const sbomPath = join(outputRoot, contract.release.sbom)
    await writeJson(sbomPath, buildSbom(metadata, contract, sourceCommit))
    const sbomDescriptor = await fileDescriptor(sbomPath, baseUri)
    const licensePath = join(outputRoot, contract.release.licenseManifest)
    await writeJson(licensePath, {
      schemaVersion: 1,
      kind: 'winwincode.infrastructure-license-manifest.v1',
      sourceCommit,
      license: 'Apache-2.0',
      packages: contract.packages.map(entry => ({ name: entry.name, version: contract.release.version, license: 'Apache-2.0' })),
      coveredArtifacts: [
        bundleDescriptor.fileName,
        persistenceDescriptor.fileName,
        fixtureLockDescriptor.fileName,
        schemaDescriptor.fileName,
        publicKeyDescriptor.fileName,
        contract.release.sbom,
        ...LEGAL_FILES,
      ].sort(),
      legalFiles: LEGAL_FILES,
    })
    const licenseDescriptor = await fileDescriptor(licensePath, baseUri)

    const packages = contract.packages.map(entry => ({
      name: entry.name,
      role: entry.role,
      version: contract.release.version,
      sourceCommit,
      bundlePath: `${bundleDirectoryName}/${dirname(entry.manifest)}`,
      ...bundleDescriptor,
    }))
    const manifest = {
      schemaVersion: 1,
      kind: 'winwincode.community-infrastructure-release.v1',
      channel: contract.release.channel,
      version: contract.release.version,
      sourceCommit,
      sourceTag: contract.release.sourceTag,
      packages,
      persistenceContract: persistenceDescriptor,
      sbom: sbomDescriptor,
      licenseManifest: licenseDescriptor,
      fixtureCargoLock: fixtureLockDescriptor,
      signing: {
        algorithm: 'Ed25519',
        publicKeyFile: publicKeyDescriptor.fileName,
        publicKeySha256,
        signatureFile: contract.release.signature,
      },
      artifacts: [
        bundleDescriptor,
        persistenceDescriptor,
        fixtureLockDescriptor,
        sbomDescriptor,
        licenseDescriptor,
        schemaDescriptor,
        publicKeyDescriptor,
        ...legalDescriptors,
      ].sort((left, right) => left.fileName.localeCompare(right.fileName)),
      boundaries: {
        productDatabaseRuntimeIncluded: false,
        productConfigurationIncluded: false,
        crossRepositoryPathDependencies: false,
        gitBranchDependencies: false,
      },
    }
    const manifestPath = join(outputRoot, contract.release.manifest)
    const manifestBytes = await writeJson(manifestPath, manifest)
    const signaturePath = join(outputRoot, contract.release.signature)
    await writeFile(signaturePath, `${sign(null, Buffer.from(manifestBytes), privateKey).toString('base64')}\n`)
    const manifestDescriptor = await fileDescriptor(manifestPath, baseUri)
    const signatureDescriptor = await fileDescriptor(signaturePath, baseUri)
    for (const consumer of contract.consumers) {
      const lock = lockFromManifest(manifest, manifestDescriptor, signatureDescriptor, consumer)
      const errors = validateInfrastructureLock(lock, consumer)
      if (errors.length > 0) fail('LOCK_INVALID', errors.join('; '))
      await writeJson(join(outputRoot, `${consumer}.infrastructure.lock.json`), lock)
    }
    const checksumEntries = []
    for (const entry of await readdir(outputRoot)) {
      if (entry === 'SHA256SUMS') continue
      const path = join(outputRoot, entry)
      if (!(await stat(path)).isFile()) continue
      checksumEntries.push(`${sha256(await readFile(path))}  ${entry}`)
    }
    await writeFile(join(outputRoot, 'SHA256SUMS'), `${checksumEntries.sort().join('\n')}\n`)
    await verifyInfrastructureRelease(outputRoot, contract.consumers)
    return { manifest, outputDirectory: outputRoot }
  } finally {
    await rm(workRoot, { recursive: true, force: true })
  }
}

function parseArguments(arguments_) {
  const values = new Map()
  for (let index = 0; index < arguments_.length; index += 2) {
    const flag = arguments_[index]
    const value = arguments_[index + 1]
    if (!flag?.startsWith('--') || value === undefined) fail('INVALID_ARGUMENT', 'arguments must be --name value pairs')
    values.set(flag.slice(2), value)
  }
  const root = resolve(import.meta.dirname, '..')
  return {
    root,
    contractPath: resolve(root, values.get('contract') ?? DEFAULT_CONTRACT),
    outputDirectory: resolve(values.get('output') ?? join(root, 'release-artifacts', 'community-infrastructure')),
    signingKeyPath: resolve(values.get('signing-key') ?? ''),
  }
}

const isMain = process.argv[1] !== undefined && pathToFileURL(process.argv[1]).href === import.meta.url
if (isMain) {
  buildInfrastructureRelease(parseArguments(process.argv.slice(2)))
    .then(result => process.stdout.write(`${JSON.stringify({ status: 'passed', sourceCommit: result.manifest.sourceCommit, outputDirectory: result.outputDirectory })}\n`))
    .catch(error => {
      process.stderr.write(`${error.code ?? 'INFRASTRUCTURE_RELEASE_FAILED'}: ${error.message}\n`)
      process.exitCode = 1
    })
}
