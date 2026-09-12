#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const DEFAULT_CONTRACT = 'docs/decisions/0031-community-core-release.json'
const OUTPUT_FILE = 'community-core-source-manifest.json'
const FOREIGN_PRODUCT_MARKER = /(?:^|[-_/])(cloud|enterprise|saas|tenant|billing|hosted)(?=$|[-_/.])/iu
const LEGAL_FILES = ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']

export class CommunityCoreSourceError extends Error {
  constructor(code, message) {
    super(message)
    this.name = 'CommunityCoreSourceError'
    this.code = code
  }
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function namedEntries(value) {
  return Array.isArray(value) && value.length > 0 && value.every(entry => isRecord(entry) && typeof entry.name === 'string')
}

export function validateCommunityCoreReleaseContract(contract) {
  const errors = []
  if (!isRecord(contract)) return ['contract must be a JSON object']
  if (contract.schemaVersion !== 1) errors.push('schemaVersion must be 1')
  if (contract.kind !== 'winwincode.community-core-release-contract.v1') {
    errors.push('kind must be winwincode.community-core-release-contract.v1')
  }
  if (!['accepted', 'proposed'].includes(contract.status)) errors.push('status must be proposed or accepted')
  if (contract.ownerRepository !== 'winwincode') errors.push('ownerRepository must be winwincode')

  const current = contract.currentState
  const target = contract.targetState
  if (!isRecord(current)) errors.push('currentState must be an object')
  if (!isRecord(target)) errors.push('targetState must be an object')
  if (isRecord(current)) {
    if (!namedEntries(current.rustPackages?.intendedLinkableCore)) {
      errors.push('currentState.rustPackages.intendedLinkableCore must contain named crate manifests')
    }
    if (!namedEntries(current.npmPackages?.packages)) {
      errors.push('currentState.npmPackages.packages must contain named package manifests')
    }
    const contracts = current.contracts
    if (!isRecord(contracts) || typeof contracts.generator !== 'string') {
      errors.push('currentState.contracts.generator must name a file')
    }
    if (!Array.isArray(current.rustPackages?.runtimeOnlySourcePackages)) {
      errors.push('currentState.rustPackages.runtimeOnlySourcePackages must be an array')
    }
    if (!namedEntries(current.rustPackages?.communityOnlyAdapters)) {
      errors.push('currentState.rustPackages.communityOnlyAdapters must contain named crate manifests')
    }
  }
  if (isRecord(target)) {
    if (!namedEntries(target.consumableRustCrates)) errors.push('targetState.consumableRustCrates must contain named crates')
    if (!namedEntries(target.consumableNpmPackages)) errors.push('targetState.consumableNpmPackages must contain named packages')
    const bundle = target.contractBundle
    for (const field of ['canonicalSchemas', 'generatedContracts', 'protocolSamples']) {
      if (!isRecord(bundle) || !Array.isArray(bundle[field]) || bundle[field].length === 0) {
        errors.push(`targetState.contractBundle.${field} must be a non-empty array`)
      }
    }
    if (!isRecord(target.forbiddenCoreContent)) errors.push('targetState.forbiddenCoreContent must be an object')
    if (!isRecord(target.coreLockManifest) || typeof target.coreLockManifest.schemaPath !== 'string') {
      errors.push('targetState.coreLockManifest.schemaPath must name a file')
    }
    const signature = target.releaseManifest?.signature
    if (!isRecord(signature) || signature.algorithm !== 'Ed25519') {
      errors.push('targetState.releaseManifest.signature.algorithm must be Ed25519')
    }
    if (!isRecord(signature) || !safeRepositoryPath(signature.publicKeyFile)) {
      errors.push('targetState.releaseManifest.signature.publicKeyFile must be repository-relative')
    }
    if (!isRecord(signature) || !/^[0-9a-f]{64}$/u.test(signature.publicKeySha256 ?? '')) {
      errors.push('targetState.releaseManifest.signature.publicKeySha256 must be SHA-256')
    }
  }
  if (isRecord(current) && isRecord(target) && namedEntries(current.rustPackages?.communityOnlyAdapters)) {
    const released = new Set((target.consumableRustCrates ?? []).map(entry => entry?.name))
    const forbidden = new Set(target.forbiddenCoreContent?.rustCrates ?? [])
    for (const adapter of current.rustPackages.communityOnlyAdapters) {
      if (released.has(adapter.name)) errors.push(`Community-only adapter must not be released as core: ${adapter.name}`)
      if (!forbidden.has(adapter.name)) errors.push(`Community-only adapter must be forbidden from core: ${adapter.name}`)
    }
  }
  return errors
}

function runGit(root, arguments_, encoding = 'utf8') {
  const result = spawnSync('git', arguments_, {
    cwd: root,
    encoding,
    maxBuffer: 128 * 1024 * 1024,
  })
  if (result.status !== 0) {
    const detail = Buffer.isBuffer(result.stderr) ? result.stderr.toString('utf8') : result.stderr
    throw new CommunityCoreSourceError('CORE_GIT_ERROR', detail.trim() || `git ${arguments_.join(' ')} failed`)
  }
  return result.stdout
}

function gitInventory(root) {
  const output = runGit(root, ['ls-tree', '-r', '--name-only', '-z', 'HEAD'], 'buffer')
  return output
    .toString('utf8')
    .split('\0')
    .filter(Boolean)
    .sort()
}

function gitBytes(root, path) {
  return runGit(root, ['show', `HEAD:${path}`], 'buffer')
}

function gitText(root, path) {
  return gitBytes(root, path).toString('utf8')
}

function digest(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function compareText(left, right) {
  if (left < right) return -1
  if (left > right) return 1
  return 0
}

function safeRepositoryPath(path) {
  return (
    typeof path === 'string' &&
    path.length > 0 &&
    !isAbsolute(path) &&
    path !== '..' &&
    !path.startsWith('../') &&
    !path.includes('/../') &&
    !path.includes('\\')
  )
}

function assertAllowedPath(path) {
  if (!safeRepositoryPath(path)) {
    throw new CommunityCoreSourceError('CORE_SCOPE_REJECTED', `source path is not repository-relative: ${path}`)
  }
  if (FOREIGN_PRODUCT_MARKER.test(path)) {
    throw new CommunityCoreSourceError('CORE_SCOPE_REJECTED', `foreign product source is outside Community core: ${path}`)
  }
}

function cargoPackageName(source) {
  let inPackage = false
  for (const line of source.split(/\r?\n/u)) {
    if (/^\s*\[package\]\s*$/u.test(line)) {
      inPackage = true
      continue
    }
    if (/^\s*\[/u.test(line)) inPackage = false
    if (!inPackage) continue
    const match = /^\s*name\s*=\s*["']([^"']+)["']/u.exec(line)
    if (match) return match[1]
  }
  return undefined
}

function addSelection(selection, path, scope, inventory) {
  assertAllowedPath(path)
  if (!inventory.has(path)) {
    throw new CommunityCoreSourceError('CORE_SOURCE_MISSING', `required Git source is missing: ${path}`)
  }
  if (!selection.has(path)) selection.set(path, new Set())
  selection.get(path).add(scope)
}

function addTree(selection, rootPath, scope, inventory) {
  assertAllowedPath(rootPath)
  const prefix = `${rootPath}/`
  const matches = [...inventory].filter(path => path === rootPath || path.startsWith(prefix))
  if (matches.length === 0) {
    throw new CommunityCoreSourceError('CORE_SOURCE_MISSING', `required Git source tree is missing: ${rootPath}`)
  }
  for (const path of matches) addSelection(selection, path, scope, inventory)
}

function forbiddenNames(contract) {
  const forbidden = contract.targetState.forbiddenCoreContent
  return new Set([...(forbidden.rustCrates ?? []), ...(forbidden.npmPackages ?? [])])
}

function assertPackageScope(name, forbidden) {
  if (forbidden.has(name) || FOREIGN_PRODUCT_MARKER.test(name)) {
    throw new CommunityCoreSourceError('CORE_SCOPE_REJECTED', `package is outside Community core: ${name}`)
  }
}

function sourceDescriptors(contract) {
  const rust = new Map(contract.currentState.rustPackages.intendedLinkableCore.map(entry => [entry.name, entry]))
  const npm = new Map(contract.currentState.npmPackages.packages.map(entry => [entry.name, entry]))
  return { npm, rust }
}

function locateCargoPackages(root, trackedPaths) {
  const packages = new Map()
  for (const path of trackedPaths.filter(path => basename(path) === 'Cargo.toml')) {
    const name = cargoPackageName(gitText(root, path))
    if (name && !packages.has(name)) packages.set(name, path)
  }
  return packages
}

function sameNames(left, right) {
  return JSON.stringify([...left].sort()) === JSON.stringify([...right].sort())
}

function selectCommunityCoreSource(root, contract, trackedPaths) {
  const inventory = new Set(trackedPaths)
  const selection = new Map()
  const forbidden = forbiddenNames(contract)
  const descriptors = sourceDescriptors(contract)
  const targetRust = contract.targetState.consumableRustCrates.map(entry => entry.name)
  const targetNpm = contract.targetState.consumableNpmPackages.map(entry => entry.name)
  if (!sameNames(targetRust, descriptors.rust.keys())) {
    throw new CommunityCoreSourceError('CORE_CONTRACT_MISMATCH', 'target Rust crates do not match current source descriptors')
  }
  if (!sameNames(targetNpm, descriptors.npm.keys())) {
    throw new CommunityCoreSourceError('CORE_CONTRACT_MISMATCH', 'target npm packages do not match current source descriptors')
  }

  for (const name of [...targetRust].sort()) {
    assertPackageScope(name, forbidden)
    const manifest = descriptors.rust.get(name).manifest
    assertAllowedPath(manifest)
    if (!inventory.has(manifest)) {
      throw new CommunityCoreSourceError('CORE_SOURCE_MISSING', `required Git source is missing: ${manifest}`)
    }
    if (cargoPackageName(gitText(root, manifest)) !== name) {
      throw new CommunityCoreSourceError('CORE_CONTRACT_MISMATCH', `Cargo package name does not match ${manifest}`)
    }
    addTree(selection, dirname(manifest), `rust:${name}`, inventory)
  }

  for (const name of [...targetNpm].sort()) {
    assertPackageScope(name, forbidden)
    const manifest = descriptors.npm.get(name).manifest
    assertAllowedPath(manifest)
    if (!inventory.has(manifest)) {
      throw new CommunityCoreSourceError('CORE_SOURCE_MISSING', `required Git source is missing: ${manifest}`)
    }
    let packageManifest
    try {
      packageManifest = JSON.parse(gitText(root, manifest))
    } catch (error) {
      throw new CommunityCoreSourceError('CORE_CONTRACT_MISMATCH', `${manifest} is invalid JSON: ${error.message}`)
    }
    if (packageManifest.name !== name) {
      throw new CommunityCoreSourceError('CORE_CONTRACT_MISMATCH', `npm package name does not match ${manifest}`)
    }
    addTree(selection, dirname(manifest), `npm:${name}`, inventory)
  }

  const cargoPackages = locateCargoPackages(root, trackedPaths)
  for (const name of [...contract.currentState.rustPackages.runtimeOnlySourcePackages].sort()) {
    assertPackageScope(name, forbidden)
    const manifest = cargoPackages.get(name)
    if (!manifest) {
      throw new CommunityCoreSourceError('CORE_SOURCE_MISSING', `runtime-only Cargo package is missing: ${name}`)
    }
    addTree(selection, dirname(manifest), `runtime-source:${name}`, inventory)
  }

  const bundle = contract.targetState.contractBundle
  for (const [scope, paths] of [
    ['contract-schema', bundle.canonicalSchemas],
    ['contract-generated', bundle.generatedContracts],
    ['protocol-sample', bundle.protocolSamples],
  ]) {
    for (const path of [...paths].sort()) addSelection(selection, path, scope, inventory)
  }
  addSelection(selection, contract.currentState.contracts.generator, 'contract-generator', inventory)
  addSelection(selection, contract.targetState.coreLockManifest.schemaPath, 'core-lock-schema', inventory)
  addSelection(selection, contract.targetState.releaseManifest.signature.publicKeyFile, 'release-key', inventory)
  for (const path of LEGAL_FILES) addSelection(selection, path, 'legal', inventory)
  return selection
}

async function readContract(path) {
  let contract
  try {
    contract = JSON.parse(await readFile(path, 'utf8'))
  } catch (error) {
    throw new CommunityCoreSourceError('CORE_CONTRACT_ERROR', `release contract could not be read: ${error.message}`)
  }
  const errors = validateCommunityCoreReleaseContract(contract)
  if (errors.length > 0) {
    throw new CommunityCoreSourceError('CORE_CONTRACT_ERROR', `release contract is invalid: ${errors.join('; ')}`)
  }
  return contract
}

function outputEscapesRoot(root, outputDirectory) {
  const path = relative(root, outputDirectory)
  return path === '..' || path.startsWith(`..${sep}`) || isAbsolute(path)
}

export async function buildCommunityCoreSourceManifest(options = {}) {
  const root = resolve(options.root ?? process.cwd())
  if (!options.outputDirectory) {
    throw new CommunityCoreSourceError('CORE_OUTPUT_REQUIRED', 'an explicit output directory outside the repository is required')
  }
  const outputDirectory = resolve(options.outputDirectory)
  if (!outputEscapesRoot(root, outputDirectory)) {
    throw new CommunityCoreSourceError('CORE_OUTPUT_INSIDE_REPOSITORY', 'output directory must be outside the repository')
  }

  const contractPath = resolve(options.contractPath ?? join(root, DEFAULT_CONTRACT))
  const contract = await readContract(contractPath)
  const trackedPaths = gitInventory(root)
  const selection = selectCommunityCoreSource(root, contract, trackedPaths)
  const files = [...selection.entries()]
    .sort(([left], [right]) => compareText(left, right))
    .map(([path, scopes]) => {
      const bytes = gitBytes(root, path)
      return {
        path,
        bytes: bytes.byteLength,
        sha256: digest(bytes),
        scopes: [...scopes].sort(),
      }
    })
  const sourceSetSha256 = digest(Buffer.from(JSON.stringify(files), 'utf8'))
  const sourceCommit = runGit(root, ['rev-parse', 'HEAD']).trim()
  const manifest = {
    schemaVersion: 1,
    kind: 'winwincode.community-core-source-manifest.v1',
    state: 'source-inventory-only',
    contract: {
      kind: contract.kind,
      status: contract.status,
      ownerRepository: contract.ownerRepository,
    },
    sourceCommit,
    fileCount: files.length,
    sourceSetSha256,
    files,
  }
  const bytes = `${JSON.stringify(manifest, null, 2)}\n`
  await mkdir(outputDirectory, { recursive: true })
  const outputPath = join(outputDirectory, OUTPUT_FILE)
  const temporaryPath = join(outputDirectory, `.${OUTPUT_FILE}.${process.pid}.tmp`)
  await writeFile(temporaryPath, bytes, { flag: 'wx' })
  await rename(temporaryPath, outputPath)
  return { manifest, outputPath, bytes }
}

export function parseArguments(arguments_) {
  const options = {}
  for (let index = 0; index < arguments_.length; index += 1) {
    const argument = arguments_[index]
    if (argument === '--root') options.root = arguments_[index += 1]
    else if (argument === '--contract') options.contractPath = arguments_[index += 1]
    else if (argument === '--output') options.outputDirectory = arguments_[index += 1]
    else throw new CommunityCoreSourceError('CORE_ARGUMENT_ERROR', `unknown argument: ${argument}`)
  }
  for (const [name, value] of Object.entries(options)) {
    if (value === undefined) throw new CommunityCoreSourceError('CORE_ARGUMENT_ERROR', `${name} requires a value`)
  }
  return options
}

export async function runCli(arguments_ = process.argv.slice(2)) {
  try {
    const result = await buildCommunityCoreSourceManifest(parseArguments(arguments_))
    process.stdout.write(
      `${JSON.stringify({ output: result.outputPath, fileCount: result.manifest.fileCount, sha256: result.manifest.sourceSetSha256 })}\n`,
    )
    return 0
  } catch (error) {
    const code = error instanceof CommunityCoreSourceError ? error.code : 'CORE_UNEXPECTED_ERROR'
    process.stderr.write(`${code}: ${error.message}\n`)
    return 1
  }
}

const isDirectExecution = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (isDirectExecution) process.exitCode = await runCli()
