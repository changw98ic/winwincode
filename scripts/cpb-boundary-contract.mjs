import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { basename, extname, join, relative } from 'node:path'

const repositoryRuntimeRoots = Object.freeze([
  '.github/workflows/',
  'apps/',
  'crates/',
  'packages/',
  'scripts/',
  'upstream/',
])
const repositoryRuntimeFiles = new Set([
  'Cargo.lock',
  'Cargo.toml',
  'package.json',
  'pnpm-lock.yaml',
  'pnpm-workspace.yaml',
  'rust-toolchain.toml',
])
const boundaryToolFiles = new Set([
  'scripts/cpb-boundary-contract.mjs',
  'scripts/verify-cpb-boundary.mjs',
])
const filesystemInventoryExcludedNames = new Set([
  '.git',
  'dist',
  'node_modules',
  'prebuild',
  'prebuilds',
  'target',
])
const textExtensions = new Set([
  '.cjs',
  '.diff',
  '.js',
  '.json',
  '.jsonl',
  '.jsx',
  '.lock',
  '.md',
  '.mjs',
  '.patch',
  '.sh',
  '.toml',
  '.ts',
  '.tsx',
  '.txt',
  '.yaml',
  '.yml',
])
const forbiddenRuntimeMarkers = Object.freeze([
  Object.freeze({
    label: 'CodePatchBay package or runtime name',
    pattern: /(?:@codepatchbay\/|\bcodepatchbay\b)/iu,
  }),
  Object.freeze({
    label: 'CPB environment or configuration key',
    pattern: /\bcpb_[a-z][a-z0-9_]*\b/iu,
  }),
  Object.freeze({
    label: 'CPB hidden state path',
    pattern: /(?:^|[/'"`\\])\.cpb(?:$|[/'"`\\])/mu,
  }),
  Object.freeze({
    label: 'CPB runtime command',
    pattern: /\bcpb\s+(?:hub|init|jobs?|queue|run|status|stream|worker)\b/iu,
  }),
  Object.freeze({
    label: 'CPB package import',
    pattern: /(?:from\s+|import\s*\(|require\s*\()\s*['"](?:cpb|cpb-runtime)(?:[/'"])/iu,
  }),
  Object.freeze({
    label: 'CPB executable dependency',
    pattern: /(?:execFile|execFileSync|spawn|spawnSync)\s*\(\s*['"]cpb['"]/u,
  }),
])
const packageDependencySections = Object.freeze([
  'dependencies',
  'devDependencies',
  'optionalDependencies',
  'peerDependencies',
])

function normalizePath(path) {
  return path.replaceAll('\\', '/').replace(/^\.\//u, '')
}

function isCpbPackageName(name) {
  const normalized = name.toLowerCase()
  return normalized === 'cpb'
    || normalized === 'cpb-runtime'
    || normalized === 'codepatchbay'
    || normalized.startsWith('@cpb/')
    || normalized.startsWith('@codepatchbay/')
}

function isTextPath(path) {
  const name = basename(path)
  return name === 'LICENSE'
    || name === 'NOTICE'
    || textExtensions.has(extname(name).toLowerCase())
}

function isRepositoryRuntimePath(path) {
  const normalized = normalizePath(path)
  if (boundaryToolFiles.has(normalized)) return false
  return repositoryRuntimeFiles.has(normalized)
    || repositoryRuntimeRoots.some(root => normalized.startsWith(root))
}

function listRepositoryPaths(root) {
  const hasGitMetadata = existsSync(join(root, '.git'))
  if (hasGitMetadata) {
    const listed = spawnSync(
      'git',
      ['ls-files', '--cached', '--others', '--exclude-standard', '-z'],
      { cwd: root, encoding: 'utf8' },
    )
    if (listed.status !== 0) {
      return {
        error: `git file inventory failed: ${(listed.stderr || listed.stdout).trim()}`,
      }
    }
    return {
      paths: listed.stdout.split('\0').filter(Boolean).sort(),
    }
  }

  const paths = []
  const visit = directory => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      if (filesystemInventoryExcludedNames.has(entry.name)) continue
      const fullPath = join(directory, entry.name)
      const path = normalizePath(relative(root, fullPath))
      paths.push(path)
      if (entry.isDirectory()) visit(fullPath)
    }
  }
  try {
    visit(root)
  } catch (error) {
    return {
      error: `filesystem inventory failed: ${error.message}`,
    }
  }
  return { paths: paths.sort() }
}

function lockDependencyReason(path, text) {
  const normalized = normalizePath(path)
  if (normalized.endsWith('pnpm-lock.yaml')) {
    if (/['"]?(?:@codepatchbay\/[^'":\s]+|@cpb\/[^'":\s]+|codepatchbay|cpb)@/iu.test(text)) {
      return 'pnpm lock contains a CPB package'
    }
  }
  if (normalized.endsWith('Cargo.toml')) {
    if (/^\s*(?:codepatchbay|cpb(?:-runtime)?)\s*=/imu.test(text)) {
      return 'Cargo manifest contains a CPB dependency key'
    }
    if (/\bpackage\s*=\s*"(?:codepatchbay|cpb(?:-runtime)?)"/iu.test(text)) {
      return 'Cargo manifest aliases a CPB package'
    }
  }
  if (normalized.endsWith('Cargo.lock')) {
    if (/^name\s*=\s*"(?:codepatchbay|cpb(?:-runtime)?)"$/imu.test(text)) {
      return 'Cargo lock contains a CPB package'
    }
  }
  return undefined
}

export function findCpbRuntimeMarker(text) {
  return forbiddenRuntimeMarkers.find(marker => marker.pattern.test(text))
}

export function cpbStatePathReason(path) {
  const normalized = normalizePath(path).toLowerCase()
  const segments = normalized.split('/').filter(Boolean)
  if (segments.includes('.cpb')) return 'contains a .cpb state directory'
  if (segments.some(segment => /^(?:cpb|codepatchbay)[-_](?:artifacts?|cache|credentials?|data|evidence|jobs?|logs?|queues?|runtime|sessions?|state|worktrees?)$/u.test(segment))) {
    return 'contains a CPB runtime-state directory'
  }
  const name = segments.at(-1) ?? ''
  if (/^(?:cpb|codepatchbay)(?:[-_.](?:events?|jobs?|queues?|runtime|state))?\.(?:db|jsonl?|log|sqlite3?|tgz|zip)$/u.test(name)) {
    return 'contains a CPB runtime-state file'
  }
  return undefined
}

export function findForbiddenPackageDependencies(manifest) {
  const errors = []
  for (const section of packageDependencySections) {
    const dependencies = manifest[section]
    if (dependencies === undefined || dependencies === null || typeof dependencies !== 'object') continue
    for (const name of Object.keys(dependencies)) {
      if (isCpbPackageName(name)) errors.push(`${section} contains forbidden package ${name}`)
    }
  }
  for (const section of ['bundledDependencies', 'bundleDependencies']) {
    const dependencies = manifest[section]
    if (!Array.isArray(dependencies)) continue
    for (const name of dependencies) {
      if (typeof name === 'string' && isCpbPackageName(name)) {
        errors.push(`${section} contains forbidden package ${name}`)
      }
    }
  }
  return errors
}

export function scanRepositoryCpbBoundary(root) {
  const inventory = listRepositoryPaths(root)
  if (inventory.error !== undefined) return [inventory.error]

  const errors = []
  for (const path of inventory.paths) {
    const fullPath = join(root, path)
    if (!existsSync(fullPath)) continue
    const stateReason = cpbStatePathReason(path)
    if (stateReason !== undefined) errors.push(`${path}: ${stateReason}`)
    if (!isRepositoryRuntimePath(path) || !isTextPath(path)) continue

    const text = readFileSync(fullPath, 'utf8')
    const marker = findCpbRuntimeMarker(text)
    if (marker !== undefined) errors.push(`${path}: contains ${marker.label}`)
    const lockReason = lockDependencyReason(path, text)
    if (lockReason !== undefined) errors.push(`${path}: ${lockReason}`)

    if (basename(path) === 'package.json') {
      let manifest
      try {
        manifest = JSON.parse(text)
      } catch (error) {
        errors.push(`${path}: invalid package manifest: ${error.message}`)
        continue
      }
      for (const error of findForbiddenPackageDependencies(manifest)) {
        errors.push(`${path}: ${error}`)
      }
    }
  }
  return errors
}

export function scanPackedPackageCpbBoundary({
  packageDirectory,
  files,
  inheritedFileDirectory,
}) {
  const errors = []
  for (const path of files) {
    const packagePath = `package/${normalizePath(path)}`
    const stateReason = cpbStatePathReason(packagePath)
    if (stateReason !== undefined) errors.push(`${packagePath}: ${stateReason}`)
    if (!isTextPath(path)) continue

    let text
    try {
      const packagePath = join(packageDirectory, path)
      const inheritedPath = inheritedFileDirectory === undefined || path !== 'LICENSE'
        ? undefined
        : join(inheritedFileDirectory, path)
      text = readFileSync(
        existsSync(packagePath) || inheritedPath === undefined ? packagePath : inheritedPath,
        'utf8',
      )
    } catch (error) {
      errors.push(`${packagePath}: cannot scan packed file: ${error.message}`)
      continue
    }
    const marker = findCpbRuntimeMarker(text)
    if (marker !== undefined) errors.push(`${packagePath}: contains ${marker.label}`)

    if (basename(path) === 'package.json') {
      let manifest
      try {
        manifest = JSON.parse(text)
      } catch (error) {
        errors.push(`${packagePath}: invalid package manifest: ${error.message}`)
        continue
      }
      for (const error of findForbiddenPackageDependencies(manifest)) {
        errors.push(`${packagePath}: ${error}`)
      }
    }
  }
  return errors
}
