import { createHash } from 'node:crypto'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join, relative } from 'node:path'

export const PRODUCT_RELEASE_SCHEMA_VERSION = 1

/** JavaScript packages that are part of the deployable Client surface. */
export const PRODUCT_PACKAGE_DIRECTORIES = Object.freeze([
  'apps/client',
  'packages/contracts',
  'packages/strongflow',
])

export const PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES = PRODUCT_PACKAGE_DIRECTORIES

export const PRODUCT_RELEASE_REQUIRED_CHECKS = Object.freeze([
  'format',
  'lint-and-typecheck',
  'rust-tests',
  'product-build',
  'api-production-vertical',
  'clean-install',
])

const releaseRootFiles = Object.freeze([
  '.gitignore',
  '.node-version',
  'CODE_OF_CONDUCT.md',
  'CONTRIBUTING.md',
  'Cargo.lock',
  'Cargo.toml',
  'LICENSE',
  'NOTICE',
  'README.md',
  'SECURITY.md',
  'THIRD_PARTY_NOTICES.md',
  'package.json',
  'pnpm-lock.yaml',
  'pnpm-workspace.yaml',
  'rust-toolchain.toml',
  'tsconfig.base.json',
  'tsconfig.check.json',
  'tsconfig.json',
])

const releaseRoots = Object.freeze([
  '.github',
  'apps',
  'crates',
  'docs',
  'packages',
  'schema',
  'scripts',
  'tests',
  'upstream',
])

const excludedDirectoryNames = new Set([
  '.beads',
  '.cache',
  '.git',
  'dist',
  'node_modules',
  'target',
])

function normalizedRelative(root, path) {
  return relative(root, path).replaceAll('\\', '/')
}

function visitSourceDirectory(root, directory, files) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && excludedDirectoryNames.has(entry.name)) continue
    const path = join(directory, entry.name)
    if (entry.isDirectory()) visitSourceDirectory(root, path, files)
    else if (entry.isFile()) files.push(normalizedRelative(root, path))
  }
}

/** Project-owned release input inventory. Pinned upstream source is represented by its lock. */
export function releaseSourcePaths(root) {
  const paths = releaseRootFiles.filter(path => existsSync(join(root, path)))
  for (const directory of releaseRoots) {
    const path = join(root, directory)
    if (existsSync(path)) visitSourceDirectory(root, path, paths)
  }
  const upstreamMetadata = join(root, 'third_party', 'codex.UPSTREAM.json')
  if (existsSync(upstreamMetadata)) paths.push('third_party/codex.UPSTREAM.json')
  return Object.freeze([...new Set(paths)].sort())
}

export function releaseSourceSha256(root) {
  const hash = createHash('sha256')
  for (const path of releaseSourcePaths(root)) {
    hash.update(path)
    hash.update('\0')
    hash.update(readFileSync(join(root, path)))
    hash.update('\0')
  }
  return hash.digest('hex')
}

export function fileDescriptor(path) {
  const bytes = readFileSync(path)
  return Object.freeze({
    sha256: createHash('sha256').update(bytes).digest('hex'),
    bytes: bytes.length,
  })
}

export function readCanonicalJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

export function productPackageManifests(root) {
  return Object.freeze(PRODUCT_PACKAGE_DIRECTORIES.map(directory => Object.freeze({
    directory,
    manifest: readCanonicalJson(join(root, directory, 'package.json')),
  })))
}

export function verifyReleaseLegalBoundary(root) {
  const errors = []
  const workspace = readCanonicalJson(join(root, 'package.json'))
  if (workspace.license !== 'Apache-2.0') errors.push('root package license is not Apache-2.0')
  for (const { directory, manifest } of productPackageManifests(root)) {
    if (manifest.license !== 'Apache-2.0') {
      errors.push(`${directory} license is not Apache-2.0`)
    }
    if (manifest.version !== workspace.version) {
      errors.push(`${directory} version does not match the root package`)
    }
  }
  for (const name of ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']) {
    const path = join(root, name)
    if (!existsSync(path) || readFileSync(path).length === 0) {
      errors.push(`${name} is missing or empty`)
    }
  }
  const licensePath = join(root, 'LICENSE')
  if (existsSync(licensePath)
    && !readFileSync(licensePath, 'utf8').includes('Apache License')) {
    errors.push('root LICENSE is not the Apache License text')
  }
  return Object.freeze(errors)
}
