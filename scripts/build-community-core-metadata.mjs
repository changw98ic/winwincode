#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { isAbsolute, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const SBOM_FILE = 'community-core-source.cdx.json'
const LICENSE_FILE = 'community-core-source.licenses.json'
const CHECKSUM_FILE = 'SHA256SUMS'
const LEGAL_FILES = ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']
const PACKAGE_SCOPE_PREFIXES = ['npm:', 'runtime-source:', 'rust:']
const FOREIGN_PRODUCT_MARKER = /(?:^|[-_/])(cloud|enterprise|saas|tenant|billing|hosted)(?=$|[-_/.])/iu

export class CommunityCoreMetadataError extends Error {
  constructor(code, message) {
    super(message)
    this.name = 'CommunityCoreMetadataError'
    this.code = code
  }
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function digest(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function canonicalJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`
}

function compareText(left, right) {
  if (left < right) return -1
  if (left > right) return 1
  return 0
}

function safeSourcePath(path) {
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

function expectedSourceSetDigest(files) {
  return digest(Buffer.from(JSON.stringify(files), 'utf8'))
}

export function validateCommunityCoreSourceManifest(manifest) {
  const errors = []
  if (!isRecord(manifest)) return ['manifest must be a JSON object']
  if (manifest.schemaVersion !== 1) errors.push('schemaVersion must be 1')
  if (manifest.kind !== 'winwincode.community-core-source-manifest.v1') {
    errors.push('kind must be winwincode.community-core-source-manifest.v1')
  }
  if (manifest.state !== 'source-inventory-only') errors.push('state must be source-inventory-only')
  if (typeof manifest.sourceCommit !== 'string' || !/^[0-9a-f]{40}$/u.test(manifest.sourceCommit)) {
    errors.push('sourceCommit must be a lowercase 40-character Git commit')
  }
  if (!Array.isArray(manifest.files) || manifest.files.length === 0) {
    errors.push('files must be a non-empty array')
    return errors
  }
  if (manifest.fileCount !== manifest.files.length) errors.push('fileCount must equal files.length')

  const paths = []
  for (const [index, file] of manifest.files.entries()) {
    if (!isRecord(file)) {
      errors.push(`files[${index}] must be an object`)
      continue
    }
    if (!safeSourcePath(file.path)) errors.push(`files[${index}].path must be repository-relative`)
    else paths.push(file.path)
    if (!Number.isSafeInteger(file.bytes) || file.bytes < 0) errors.push(`files[${index}].bytes must be a non-negative integer`)
    if (typeof file.sha256 !== 'string' || !/^[0-9a-f]{64}$/u.test(file.sha256)) {
      errors.push(`files[${index}].sha256 must be a lowercase SHA-256 digest`)
    }
    if (!Array.isArray(file.scopes) || file.scopes.length === 0 || file.scopes.some(scope => typeof scope !== 'string')) {
      errors.push(`files[${index}].scopes must be a non-empty string array`)
    }
  }
  if (new Set(paths).size !== paths.length) errors.push('file paths must be unique')
  if (JSON.stringify(paths) !== JSON.stringify([...paths].sort(compareText))) errors.push('files must be sorted by path')
  if (manifest.sourceSetSha256 !== expectedSourceSetDigest(manifest.files)) {
    errors.push('sourceSetSha256 does not match files')
  }
  return errors
}

function assertMetadataScope(manifest) {
  for (const file of manifest.files) {
    if (FOREIGN_PRODUCT_MARKER.test(file.path)) {
      throw new CommunityCoreMetadataError(
        'CORE_METADATA_SCOPE_REJECTED',
        `foreign product path is outside Community core metadata: ${file.path}`,
      )
    }
    for (const scope of file.scopes) {
      if (FOREIGN_PRODUCT_MARKER.test(scope)) {
        throw new CommunityCoreMetadataError(
          'CORE_METADATA_SCOPE_REJECTED',
          `foreign product scope is outside Community core metadata: ${scope}`,
        )
      }
    }
  }
}

function assertLicenseEvidence(manifest) {
  const files = new Map(manifest.files.map(file => [file.path, file]))
  for (const path of LEGAL_FILES) {
    const entry = files.get(path)
    if (!entry || !entry.scopes.includes('legal')) {
      throw new CommunityCoreMetadataError(
        'CORE_METADATA_LICENSE_MISSING',
        `required license evidence is missing from source manifest: ${path}`,
      )
    }
  }
}

function packageRecords(manifest) {
  const packages = new Map()
  for (const file of manifest.files) {
    for (const scope of file.scopes) {
      const prefix = PACKAGE_SCOPE_PREFIXES.find(candidate => scope.startsWith(candidate))
      if (!prefix) continue
      const name = scope.slice(prefix.length)
      if (!name || FOREIGN_PRODUCT_MARKER.test(name)) {
        throw new CommunityCoreMetadataError(
          'CORE_METADATA_SCOPE_REJECTED',
          `package scope is outside Community core metadata: ${scope}`,
        )
      }
      if (!packages.has(scope)) packages.set(scope, { scope, ecosystem: prefix.slice(0, -1), name, files: [] })
      packages.get(scope).files.push(file.path)
    }
  }
  return [...packages.values()]
    .map(entry => ({ ...entry, files: entry.files.sort(compareText) }))
    .sort((left, right) => compareText(left.scope, right.scope))
}

function sourceReference(manifest, sourceManifestSha256) {
  return {
    kind: manifest.kind,
    state: manifest.state,
    sourceCommit: manifest.sourceCommit,
    sourceSetSha256: manifest.sourceSetSha256,
    sourceManifestSha256,
  }
}

function fileComponent(file) {
  return {
    type: 'file',
    'bom-ref': `source-file:${encodeURIComponent(file.path)}`,
    name: file.path,
    hashes: [{ alg: 'SHA-256', content: file.sha256 }],
    properties: [
      { name: 'winwincode:bytes', value: String(file.bytes) },
      { name: 'winwincode:source-scopes', value: file.scopes.join(',') },
    ],
  }
}

function packageComponent(entry) {
  return {
    type: entry.ecosystem === 'runtime-source' ? 'application' : 'library',
    'bom-ref': `source-package:${encodeURIComponent(entry.scope)}`,
    name: entry.name,
    properties: [
      { name: 'winwincode:ecosystem', value: entry.ecosystem },
      { name: 'winwincode:distribution-state', value: 'source-inventory-only' },
      { name: 'winwincode:source-files', value: entry.files.join(',') },
    ],
  }
}

function buildSbom(manifest, packages, sourceManifestSha256) {
  return {
    bomFormat: 'CycloneDX',
    specVersion: '1.6',
    version: 1,
    metadata: {
      component: {
        type: 'framework',
        'bom-ref': 'winwincode-community-core-source',
        name: 'WinWinCode Community Core Source Inventory',
        properties: [
          { name: 'winwincode:release-state', value: 'not-published' },
          { name: 'winwincode:source-commit', value: manifest.sourceCommit },
          { name: 'winwincode:source-set-sha256', value: manifest.sourceSetSha256 },
          { name: 'winwincode:source-manifest-sha256', value: sourceManifestSha256 },
        ],
      },
    },
    components: [...packages.map(packageComponent), ...manifest.files.map(fileComponent)],
  }
}

function buildLicenseManifest(manifest, packages, sourceManifestSha256) {
  const noticeFiles = ['NOTICE', 'THIRD_PARTY_NOTICES.md']
  return {
    schemaVersion: 1,
    kind: 'winwincode.community-core-source-license-manifest.v1',
    state: 'source-inventory-only',
    source: sourceReference(manifest, sourceManifestSha256),
    projectLicenseSpdx: 'Apache-2.0',
    licenseFile: 'LICENSE',
    noticeFiles,
    packages: packages.map(entry => ({
      name: entry.name,
      ecosystem: entry.ecosystem,
      licenseSpdx: 'Apache-2.0',
      sourceFiles: entry.files,
      licenseEvidence: ['LICENSE', ...noticeFiles],
    })),
    artifacts: manifest.files.map(file => ({
      path: file.path,
      sha256: file.sha256,
      scopes: file.scopes,
      licenseSpdx: 'Apache-2.0',
      licenseEvidence: ['LICENSE', ...noticeFiles],
    })),
  }
}

async function readSourceManifest(path) {
  let manifest
  try {
    manifest = JSON.parse(await readFile(path, 'utf8'))
  } catch (error) {
    throw new CommunityCoreMetadataError('CORE_METADATA_INPUT_ERROR', `source manifest could not be read: ${error.message}`)
  }
  const errors = validateCommunityCoreSourceManifest(manifest)
  if (errors.length > 0) {
    throw new CommunityCoreMetadataError('CORE_METADATA_INPUT_ERROR', `source manifest is invalid: ${errors.join('; ')}`)
  }
  return manifest
}

function outputEscapesRoot(root, outputDirectory) {
  const path = relative(root, outputDirectory)
  return path === '..' || path.startsWith(`..${sep}`) || isAbsolute(path)
}

async function writeAtomically(directory, name, bytes) {
  const destination = join(directory, name)
  const temporary = join(directory, `.${name}.${process.pid}.tmp`)
  await writeFile(temporary, bytes, { flag: 'wx' })
  await rename(temporary, destination)
  return destination
}

export async function buildCommunityCoreMetadata(options = {}) {
  const root = resolve(options.root ?? process.cwd())
  if (!options.sourceManifestPath) {
    throw new CommunityCoreMetadataError('CORE_METADATA_INPUT_REQUIRED', 'an explicit source manifest path is required')
  }
  if (!options.outputDirectory) {
    throw new CommunityCoreMetadataError('CORE_METADATA_OUTPUT_REQUIRED', 'an explicit output directory outside the repository is required')
  }
  const outputDirectory = resolve(options.outputDirectory)
  if (!outputEscapesRoot(root, outputDirectory)) {
    throw new CommunityCoreMetadataError('CORE_METADATA_OUTPUT_INSIDE_REPOSITORY', 'output directory must be outside the repository')
  }

  const manifest = await readSourceManifest(resolve(options.sourceManifestPath))
  assertMetadataScope(manifest)
  assertLicenseEvidence(manifest)
  const packages = packageRecords(manifest)
  const canonicalSourceManifest = canonicalJson(manifest)
  const sourceManifestSha256 = digest(Buffer.from(canonicalSourceManifest, 'utf8'))
  const sbomBytes = canonicalJson(buildSbom(manifest, packages, sourceManifestSha256))
  const licenseBytes = canonicalJson(buildLicenseManifest(manifest, packages, sourceManifestSha256))
  const checksums = [
    `${digest(Buffer.from(sbomBytes, 'utf8'))}  ${SBOM_FILE}`,
    `${digest(Buffer.from(licenseBytes, 'utf8'))}  ${LICENSE_FILE}`,
  ].sort(compareText)
  const checksumBytes = `${checksums.join('\n')}\n`

  await mkdir(outputDirectory, { recursive: true })
  const sbomPath = await writeAtomically(outputDirectory, SBOM_FILE, sbomBytes)
  const licensePath = await writeAtomically(outputDirectory, LICENSE_FILE, licenseBytes)
  const checksumPath = await writeAtomically(outputDirectory, CHECKSUM_FILE, checksumBytes)
  return {
    sourceManifestSha256,
    sourceSetSha256: manifest.sourceSetSha256,
    packageCount: packages.length,
    fileCount: manifest.fileCount,
    paths: { sbom: sbomPath, licenses: licensePath, checksums: checksumPath },
  }
}

export function parseArguments(arguments_) {
  const options = {}
  for (let index = 0; index < arguments_.length; index += 1) {
    const argument = arguments_[index]
    if (argument === '--root') options.root = arguments_[index += 1]
    else if (argument === '--manifest') options.sourceManifestPath = arguments_[index += 1]
    else if (argument === '--output') options.outputDirectory = arguments_[index += 1]
    else throw new CommunityCoreMetadataError('CORE_METADATA_ARGUMENT_ERROR', `unknown argument: ${argument}`)
  }
  for (const [name, value] of Object.entries(options)) {
    if (value === undefined) throw new CommunityCoreMetadataError('CORE_METADATA_ARGUMENT_ERROR', `${name} requires a value`)
  }
  return options
}

export async function runCli(arguments_ = process.argv.slice(2)) {
  try {
    const result = await buildCommunityCoreMetadata(parseArguments(arguments_))
    process.stdout.write(`${JSON.stringify(result)}\n`)
    return 0
  } catch (error) {
    const code = error instanceof CommunityCoreMetadataError ? error.code : 'CORE_METADATA_UNEXPECTED_ERROR'
    process.stderr.write(`${code}: ${error.message}\n`)
    return 1
  }
}

const isDirectExecution = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (isDirectExecution) process.exitCode = await runCli()
