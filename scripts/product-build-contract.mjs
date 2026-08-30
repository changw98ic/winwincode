import { createHash } from 'node:crypto'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join, relative } from 'node:path'

const SOURCE_ROOT_FILES = Object.freeze([
  'Cargo.lock',
  'Cargo.toml',
  'rust-toolchain.toml',
])

const IGNORED_DIRECTORY_NAMES = new Set([
  '.beads',
  '.cache',
  '.git',
  'dist',
  'node_modules',
  'target',
])

function walk(directory, paths) {
  if (!existsSync(directory)) return
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && IGNORED_DIRECTORY_NAMES.has(entry.name)) continue
    const path = join(directory, entry.name)
    if (entry.isDirectory()) walk(path, paths)
    else if (entry.isFile()) paths.push(path)
  }
}

/**
 * Digest the source files that determine the Rust product composition.
 * Paths and bytes are both included so a rename cannot reuse an old seal.
 */
export function projectSourceDigest(root) {
  const paths = SOURCE_ROOT_FILES
    .map(path => join(root, path))
    .filter(path => existsSync(path))
  walk(join(root, 'crates'), paths)
  const hash = createHash('sha256')
  for (const path of paths.toSorted((left, right) => left.localeCompare(right))) {
    hash.update(relative(root, path).replaceAll('\\', '/'))
    hash.update('\0')
    hash.update(readFileSync(path))
    hash.update('\0')
  }
  return hash.digest('hex')
}

export function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

export const PRODUCT_TARGETS = Object.freeze([
  Object.freeze({ packageName: 'winwincode-server', binaryName: 'winwincode-server' }),
  Object.freeze({ packageName: 'winwincode-worker', binaryName: 'winwincode-worker' }),
  Object.freeze({ packageName: 'winwincode-kernel-helper', binaryName: 'winwincode-kernel-helper' }),
])

export const LOCAL_PACKAGE_NAME = 'winwincode-local'
