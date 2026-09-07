#!/usr/bin/env node

import { existsSync } from 'node:fs'
import { readdir, readFile, realpath } from 'node:fs/promises'
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const CONTRACT_RELATIVE_PATH = 'docs/decisions/0031-product-editions.json'
const IDENTITY_RELATIVE_PATH = 'product-repository.json'
const SKIPPED_DIRECTORIES = new Set([
  '.beads',
  '.git',
  '.turbo',
  'coverage',
  'dist',
  'node_modules',
  'target',
  'third_party',
  'upstream',
])
const SOURCE_ROOTS = new Set(['apps', 'crates', 'packages', 'scripts', 'tests'])
const LOCAL_PACKAGE_PROTOCOLS = ['file:', 'link:', 'portal:']
const HIGH_SIGNAL_MARKERS = Object.freeze({
  cloud: ['cloud', 'saas', 'tenant', 'billing', 'hosted'],
  community: ['community'],
  enterprise: ['enterprise', 'private-deployment'],
})

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function compareText(left, right) {
  if (left < right) return -1
  if (left > right) return 1
  return 0
}

function nonEmptyStrings(value) {
  return Array.isArray(value) && value.length > 0 && value.every(item => typeof item === 'string' && item.length > 0)
}

function normalizedMarker(value) {
  return value.toLowerCase().replaceAll('\\', '/').replaceAll('_', '-').replaceAll(/[^a-z0-9/-]+/g, '-')
}

function markerMatches(value, marker) {
  const normalizedValue = normalizedMarker(value)
  const normalizedNeedle = normalizedMarker(marker)
  const escaped = normalizedNeedle.replaceAll(/[.*+?^${}()|[\]\\]/g, '\\$&')
  return new RegExp(`(?:^|[-/])${escaped}(?=$|[-/.])`, 'u').test(normalizedValue)
}

function sourceOwnershipByProduct(contract) {
  const result = new Map()
  const repositoryToProduct = new Map(
    Object.entries(contract.repositories).map(([product, value]) => [value.repository, product]),
  )

  for (const ownership of contract.sourceOwnership) {
    const product = repositoryToProduct.get(ownership.repository)
    if (product) result.set(product, ownership.areas)
  }
  return result
}

function foreignProductForValue(value, currentProduct, contract) {
  const ownership = sourceOwnershipByProduct(contract)
  for (const product of Object.keys(contract.repositories).sort()) {
    if (product === currentProduct) continue
    const repository = contract.repositories[product].repository
    const markers = [
      product,
      repository,
      ...(HIGH_SIGNAL_MARKERS[product] ?? []),
      ...(ownership.get(product) ?? []),
    ]
    if (markers.some(marker => markerMatches(value, marker))) return product
  }
  return undefined
}

export function validateEditionContract(contract) {
  const errors = []
  if (!isRecord(contract)) return ['contract must be a JSON object']
  if (contract.schemaVersion !== 2) errors.push('schemaVersion must be 2')
  if (contract.status !== 'accepted') errors.push('status must be accepted')

  const expectedProducts = ['cloud', 'community', 'enterprise']
  if (!isRecord(contract.repositories)) {
    errors.push('repositories must be an object')
  } else {
    const products = Object.keys(contract.repositories).sort()
    if (JSON.stringify(products) !== JSON.stringify(expectedProducts)) {
      errors.push('repositories must contain exactly cloud, community, and enterprise')
    }
    const names = []
    for (const product of expectedProducts) {
      const entry = contract.repositories[product]
      if (!isRecord(entry) || typeof entry.repository !== 'string' || entry.repository.length === 0) {
        errors.push(`repositories.${product}.repository must be a non-empty string`)
      } else {
        names.push(entry.repository)
      }
    }
    if (new Set(names).size !== names.length) errors.push('repository names must be distinct')
  }

  if (!Array.isArray(contract.sourceOwnership)) {
    errors.push('sourceOwnership must be an array')
  } else if (isRecord(contract.repositories)) {
    const expectedRepositories = new Set(
      Object.values(contract.repositories)
        .filter(isRecord)
        .map(entry => entry.repository)
        .filter(value => typeof value === 'string'),
    )
    const actualRepositories = new Set()
    for (const [index, ownership] of contract.sourceOwnership.entries()) {
      if (!isRecord(ownership) || typeof ownership.repository !== 'string' || !nonEmptyStrings(ownership.areas)) {
        errors.push(`sourceOwnership[${index}] must name a repository and at least one source area`)
        continue
      }
      if (actualRepositories.has(ownership.repository)) {
        errors.push(`sourceOwnership repeats repository ${ownership.repository}`)
      }
      actualRepositories.add(ownership.repository)
    }
    for (const repository of expectedRepositories) {
      if (!actualRepositories.has(repository)) errors.push(`sourceOwnership is missing ${repository}`)
    }
  }

  const release = contract.communityCoreRelease
  if (!isRecord(release)) {
    errors.push('communityCoreRelease must be an object')
  } else if (isRecord(contract.repositories)) {
    const community = contract.repositories.community?.repository
    const downstream = [contract.repositories.cloud?.repository, contract.repositories.enterprise?.repository].sort()
    if (release.ownerRepository !== community) {
      errors.push('communityCoreRelease.ownerRepository must be the Community repository')
    }
    if (!Array.isArray(release.consumers) || JSON.stringify([...release.consumers].sort()) !== JSON.stringify(downstream)) {
      errors.push('communityCoreRelease.consumers must be the Cloud and Enterprise repositories')
    }
    if (release.localPathDependency !== false) {
      errors.push('communityCoreRelease.localPathDependency must be false')
    }
  }

  if (!Array.isArray(contract.hardRules) || !contract.hardRules.includes('cross-repository-local-path-dependencies-are-rejected')) {
    errors.push('hardRules must reject cross-repository local path dependencies')
  }
  return errors
}

export function validateRepositoryIdentity(contract, identity) {
  const errors = []
  if (!isRecord(identity)) return ['identity must be a JSON object']
  if (identity.schemaVersion !== 1) errors.push('identity.schemaVersion must be 1')
  if (typeof identity.product !== 'string' || !isRecord(contract.repositories?.[identity.product])) {
    errors.push('identity.product must name a product from the edition contract')
    return errors
  }

  const expectedRepository = contract.repositories[identity.product].repository
  if (identity.repository !== expectedRepository) {
    errors.push(`identity.repository must be ${expectedRepository}`)
  }
  if (
    !nonEmptyStrings(identity.allowedSourceOwners) ||
    identity.allowedSourceOwners.length !== 1 ||
    identity.allowedSourceOwners[0] !== expectedRepository
  ) {
    errors.push(`identity.allowedSourceOwners must contain only ${expectedRepository}`)
  }

  const coreOwner = contract.communityCoreRelease?.ownerRepository
  if (
    !nonEmptyStrings(identity.allowedCoreReleaseOwners) ||
    identity.allowedCoreReleaseOwners.length !== 1 ||
    identity.allowedCoreReleaseOwners[0] !== coreOwner
  ) {
    errors.push(`identity.allowedCoreReleaseOwners must contain only ${coreOwner}`)
  }
  return errors
}

async function readJson(path, label) {
  let source
  try {
    source = await readFile(path, 'utf8')
  } catch (error) {
    throw new Error(`${label} could not be read at ${path}: ${error.message}`)
  }
  try {
    return JSON.parse(source)
  } catch (error) {
    throw new Error(`${label} is not valid JSON at ${path}: ${error.message}`)
  }
}

async function walkFiles(root) {
  const files = []
  async function visit(directory) {
    const entries = await readdir(directory, { withFileTypes: true })
    entries.sort((left, right) => compareText(left.name, right.name))
    for (const entry of entries) {
      if (entry.isSymbolicLink()) continue
      const path = join(directory, entry.name)
      if (entry.isDirectory()) {
        if (!SKIPPED_DIRECTORIES.has(entry.name)) await visit(path)
      } else if (entry.isFile()) {
        files.push(path)
      }
    }
  }
  await visit(root)
  return files
}

function repositoryRelative(root, path) {
  return relative(root, path).split(sep).join('/')
}

function isProductSourcePath(path) {
  return SOURCE_ROOTS.has(path.split('/')[0])
}

function localPackagePath(specifier) {
  if (typeof specifier !== 'string') return undefined
  const protocol = LOCAL_PACKAGE_PROTOCOLS.find(candidate => specifier.startsWith(candidate))
  if (protocol) return specifier.slice(protocol.length)
  if (specifier.startsWith('workspace:./') || specifier.startsWith('workspace:../')) return specifier.slice('workspace:'.length)
  if (specifier.startsWith('./') || specifier.startsWith('../') || isAbsolute(specifier)) return specifier
  return undefined
}

async function canonicalTarget(path) {
  if (!existsSync(path)) return resolve(path)
  return realpath(path)
}

function targetEscapesRoot(root, target) {
  const path = relative(root, target)
  return path === '..' || path.startsWith(`..${sep}`) || isAbsolute(path)
}

function displayTarget(root, target) {
  return repositoryRelative(root, target) || '.'
}

function dependencySections(packageManifest) {
  return [
    ['dependencies', packageManifest.dependencies],
    ['devDependencies', packageManifest.devDependencies],
    ['optionalDependencies', packageManifest.optionalDependencies],
    ['peerDependencies', packageManifest.peerDependencies],
  ]
}

async function scanPackageManifest(root, path, contract, currentProduct) {
  const relativePath = repositoryRelative(root, path)
  const manifest = await readJson(path, 'package manifest')
  const violations = []
  const entryValues = [manifest.name]
  if (typeof manifest.bin === 'string') entryValues.push(manifest.bin)
  if (isRecord(manifest.bin)) entryValues.push(...Object.keys(manifest.bin), ...Object.values(manifest.bin))
  if (isRecord(manifest.scripts)) entryValues.push(...Object.keys(manifest.scripts), ...Object.values(manifest.scripts))
  const foreignEntry = entryValues
    .filter(value => typeof value === 'string')
    .map(value => foreignProductForValue(value, currentProduct, contract))
    .find(Boolean)
  if (foreignEntry) {
    violations.push({
      code: 'FOREIGN_PRODUCT_ENTRYPOINT',
      path: relativePath,
      product: foreignEntry,
      message: `manifest declares a ${foreignEntry} product entry point`,
    })
  }

  for (const [section, dependencies] of dependencySections(manifest)) {
    if (!isRecord(dependencies)) continue
    for (const [dependency, specifier] of Object.entries(dependencies).sort(([left], [right]) => compareText(left, right))) {
      const localPath = localPackagePath(specifier)
      if (localPath === undefined) continue
      const target = await canonicalTarget(resolve(dirname(path), localPath))
      if (!targetEscapesRoot(root, target)) continue
      violations.push({
        code: 'CROSS_REPOSITORY_PATH_DEPENDENCY',
        path: relativePath,
        dependency: `${section}.${dependency}`,
        target: displayTarget(root, target),
        message: 'package dependency resolves outside this repository',
      })
    }
  }
  return violations
}

function cargoEntryValues(source) {
  const values = []
  let section = ''
  for (const line of source.split(/\r?\n/u)) {
    const sectionMatch = /^\s*(\[\[?[^\]]+\]\]?)\s*$/u.exec(line)
    if (sectionMatch) {
      section = sectionMatch[1]
      continue
    }
    if (section !== '[package]' && section !== '[[bin]]') continue
    const nameMatch = /^\s*name\s*=\s*["']([^"']+)["']/u.exec(line)
    if (nameMatch) values.push(nameMatch[1])
  }
  return values
}

async function scanCargoManifest(root, path, contract, currentProduct) {
  const relativePath = repositoryRelative(root, path)
  const source = await readFile(path, 'utf8')
  const violations = []
  const foreignEntry = cargoEntryValues(source)
    .map(value => foreignProductForValue(value, currentProduct, contract))
    .find(Boolean)
  if (foreignEntry) {
    violations.push({
      code: 'FOREIGN_PRODUCT_ENTRYPOINT',
      path: relativePath,
      product: foreignEntry,
      message: `Cargo manifest declares a ${foreignEntry} product entry point`,
    })
  }

  const pathPattern = /\bpath\s*=\s*["']([^"']+)["']/gu
  for (const match of source.matchAll(pathPattern)) {
    const localPath = match[1]
    const target = await canonicalTarget(resolve(dirname(path), localPath))
    if (!targetEscapesRoot(root, target)) continue
    violations.push({
      code: 'CROSS_REPOSITORY_PATH_DEPENDENCY',
      path: relativePath,
      dependency: `path:${localPath}`,
      target: displayTarget(root, target),
      message: 'Cargo path resolves outside this repository',
    })
  }
  return violations
}

function compareViolations(left, right) {
  return compareText(
    `${left.code}\0${left.path}\0${left.dependency ?? ''}\0${left.product ?? ''}`,
    `${right.code}\0${right.path}\0${right.dependency ?? ''}\0${right.product ?? ''}`,
  )
}

export async function scanProductRepositoryBoundary(options = {}) {
  const root = await canonicalTarget(resolve(options.root ?? process.cwd()))
  const contractPath = resolve(options.contractPath ?? join(root, CONTRACT_RELATIVE_PATH))
  const identityPath = resolve(options.identityPath ?? join(root, IDENTITY_RELATIVE_PATH))
  const contract = await readJson(contractPath, 'edition contract')
  const contractErrors = validateEditionContract(contract)
  if (contractErrors.length > 0) throw new Error(`edition contract is invalid: ${contractErrors.join('; ')}`)
  const identity = await readJson(identityPath, 'repository identity')
  const identityErrors = validateRepositoryIdentity(contract, identity)
  if (identityErrors.length > 0) throw new Error(`repository identity is invalid: ${identityErrors.join('; ')}`)

  const files = await walkFiles(root)
  const violations = []
  for (const path of files) {
    const relativePath = repositoryRelative(root, path)
    if (isProductSourcePath(relativePath)) {
      const product = foreignProductForValue(relativePath, identity.product, contract)
      if (product) {
        violations.push({
          code: 'FOREIGN_PRODUCT_PATH',
          path: relativePath,
          product,
          message: `path belongs to the ${product} product repository`,
        })
      }
    }
    if (basename(path) === 'package.json') {
      violations.push(...(await scanPackageManifest(root, path, contract, identity.product)))
    } else if (basename(path) === 'Cargo.toml') {
      violations.push(...(await scanCargoManifest(root, path, contract, identity.product)))
    }
  }

  violations.sort(compareViolations)
  return {
    schemaVersion: 1,
    checker: 'product-repository-boundary',
    product: identity.product,
    repository: identity.repository,
    root,
    status: violations.length === 0 ? 'clean' : 'violations-found',
    violationCount: violations.length,
    violations,
  }
}

export function parseArguments(arguments_) {
  const options = { mode: 'audit' }
  for (let index = 0; index < arguments_.length; index += 1) {
    const argument = arguments_[index]
    if (argument === '--mode') options.mode = arguments_[index += 1]
    else if (argument === '--root') options.root = arguments_[index += 1]
    else if (argument === '--contract') options.contractPath = arguments_[index += 1]
    else if (argument === '--identity') options.identityPath = arguments_[index += 1]
    else throw new Error(`unknown argument: ${argument}`)
  }
  if (!['audit', 'enforce'].includes(options.mode)) throw new Error('--mode must be audit or enforce')
  for (const [name, value] of Object.entries(options)) {
    if (value === undefined) throw new Error(`${name} requires a value`)
  }
  return options
}

export async function runCli(arguments_ = process.argv.slice(2)) {
  try {
    const options = parseArguments(arguments_)
    const report = await scanProductRepositoryBoundary(options)
    process.stdout.write(`${JSON.stringify({ ...report, mode: options.mode }, null, 2)}\n`)
    return options.mode === 'enforce' && report.violationCount > 0 ? 1 : 0
  } catch (error) {
    process.stderr.write(`PRODUCT_REPOSITORY_BOUNDARY_CONFIG_ERROR: ${error.message}\n`)
    return 2
  }
}

const isDirectExecution = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (isDirectExecution) process.exitCode = await runCli()
