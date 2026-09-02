#!/usr/bin/env node

import {
  existsSync,
  readdirSync,
  readFileSync,
} from 'node:fs'
import { join, relative, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const gateFile = relative(root, new URL(import.meta.url).pathname)
const sourceDirectories = Object.freeze(['apps', 'packages', 'crates', 'scripts'])
const forbiddenDirectories = Object.freeze([
  'apps/host',
  'apps/web',
  'packages/dsh-profile',
  'packages/native',
  'packages/native-darwin-arm64',
  'packages/native-darwin-x64',
  'packages/native-linux-arm64',
  'packages/native-linux-x64',
  'crates/native',
])
const textExtensions = new Set([
  '.css',
  '.html',
  '.js',
  '.json',
  '.mjs',
  '.rs',
  '.toml',
  '.ts',
  '.tsx',
  '.yaml',
  '.yml',
])
const ignoredDirectoryNames = new Set([
  '.cache',
  '.git',
  'node_modules',
  'prebuild',
  'prebuilds',
  'target',
])
const ignoredSourceDirectoryNames = new Set([
  ...ignoredDirectoryNames,
  'dist',
  'examples',
  'fixtures',
  'test-results',
  'tests',
])

const forbiddenMarkers = Object.freeze([
  {
    id: 'legacy-host-path',
    pattern: /\bapps\/(?:host|web)(?:[/'"`]|$)/iu,
  },
  {
    id: 'legacy-package-path',
    pattern: /\b(?:packages\/(?:dsh-profile|native(?:[-/]|[/'"`]|$))|crates\/native(?:[/'"`]|$))/iu,
  },
  {
    id: 'legacy-package-import',
    pattern: /@winwincode\/(?:dsh-profile|native)(?:[/'"`@]|$)/iu,
  },
  {
    id: 'deepseek-runtime',
    pattern: /@deepseek-ai\/(?:cordis|dsh(?:[-/]|[/'"`@]|$)|schemastery)(?:[-/]|[/'"`@]|$)/iu,
  },
  {
    id: 'dsh-model-port',
    pattern: /\bDshModelPort\b/u,
  },
  {
    id: 'dsh-context-model-port',
    pattern: /\bctx\.llm\b/u,
  },
  {
    id: 'cordis-identifier',
    pattern: /\bCordis\b/u,
  },
  {
    id: 'n-api-identifier',
    pattern: /\bN-?API\b|\bnapi(?:-build|-derive)?\b/iu,
  },
  {
    id: 'node-addon-artifact',
    pattern: /\bwinwincode_native\.node\b|\bprebuilds?\b/iu,
  },
  {
    id: 'cli-fallback',
    pattern: /@openai\/codex|codex-cli\b|installed[- ]cli|\bexternal_fallback\s*:\s*true\b|\b(?:spawn|exec)(?:File|Sync)?\([^)]*['"]codex/iu,
  },
])

function relativePath(path) {
  return relative(root, path).replaceAll('\\', '/')
}

function isTextFile(path) {
  const name = path.split('/').pop() ?? ''
  const extension = name.slice(name.lastIndexOf('.')).toLowerCase()
  return textExtensions.has(extension)
}

function walk(directory, options = {}) {
  const files = []
  if (!existsSync(directory)) return files
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (options.skip?.has(entry.name)) continue
    const path = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...walk(path, options))
    else if (entry.isFile()) files.push(path)
  }
  return files
}

function sourceFiles() {
  return sourceDirectories
    .flatMap(directory => walk(join(root, directory), { skip: ignoredSourceDirectoryNames }))
    .filter(path => {
      if (!isTextFile(path) || relativePath(path) === gateFile) return false
      const name = relativePath(path)
      return name.startsWith('scripts/')
        || /(?:^|\/)src\//u.test(name)
        || /(?:^|\/)build\.rs$/u.test(name)
        || (name.startsWith('apps/client/')
          && !/(?:^|\/)(?:package\.json|tsconfig[^/]*\.json)$/u.test(name))
    })
}

function readText(path) {
  try {
    return readFileSync(path, 'utf8')
  } catch {
    return null
  }
}

function lineNumber(text, offset) {
  return text.slice(0, offset).split('\n').length
}

function finding(path, marker, line = null) {
  return Object.freeze({
    marker,
    path: relativePath(path),
    ...(line === null ? {} : { line }),
  })
}

function markerFindings(files, markers) {
  const results = []
  for (const path of files) {
    const text = readText(path)
    if (text === null) continue
    for (const marker of markers) {
      const match = marker.pattern.exec(text)
      marker.pattern.lastIndex = 0
      if (match !== null) results.push(finding(path, marker.id, lineNumber(text, match.index)))
    }
  }
  return results
}

function uniqueFindings(values) {
  const seen = new Set()
  return values.filter(value => {
    const key = `${value.marker}:${value.path}:${value.line ?? ''}`
    if (seen.has(key)) return false
    seen.add(key)
    return true
  })
}

function gate(id, findings, details = {}) {
  const unique = uniqueFindings(findings)
  return Object.freeze({
    id,
    status: unique.length === 0 ? 'green' : 'red',
    findingCount: unique.length,
    findings: unique.slice(0, 40),
    ...details,
  })
}

function explicitManifestPaths() {
  const paths = [
    'package.json',
    'pnpm-workspace.yaml',
    'pnpm-lock.yaml',
    'Cargo.toml',
    'Cargo.lock',
  ]
  for (const parent of ['apps', 'packages', 'crates']) {
    for (const path of walk(join(root, parent), { skip: ignoredDirectoryNames })) {
      if (path.endsWith('/package.json') || path.endsWith('/Cargo.toml')) paths.push(relativePath(path))
    }
  }
  return paths
    .map(path => join(root, path))
    .filter(path => existsSync(path))
}

function forbiddenDirectoryFindings() {
  return forbiddenDirectories
    .filter(path => existsSync(join(root, path)))
    .map(path => Object.freeze({ marker: 'forbidden-directory', path }))
}

function artifactFindings() {
  const findings = []
  for (const path of forbiddenDirectories) {
    if (existsSync(join(root, path))) {
      findings.push(Object.freeze({ marker: 'forbidden-package-artifact-root', path }))
    }
  }
  const artifactRoots = ['apps', 'packages']
  for (const parent of artifactRoots) {
    for (const path of walk(join(root, parent), {
      skip: new Set(['.cache', '.git', 'node_modules', 'prebuild', 'prebuilds', 'src', 'scripts', 'target']),
    })) {
      const name = relativePath(path)
      if (/(?:^|\/)(?:prebuild|prebuilds)(?:\/|$)/iu.test(name)
        || /\.node$/iu.test(name)) {
        findings.push(Object.freeze({ marker: 'forbidden-package-artifact', path: name }))
      }
    }
  }
  return findings
}

function distLegacyFindings() {
  const files = ['apps', 'packages']
    .flatMap(parent => walk(join(root, parent), {
      skip: new Set(['.cache', '.git', 'node_modules', 'prebuild', 'prebuilds', 'src', 'scripts', 'target']),
    }))
    .filter(path => relativePath(path).split('/').includes('dist') && isTextFile(path))
  return markerFindings(files, forbiddenMarkers)
}

function clientNetworkFindings() {
  const findings = []
  const clientRoot = join(root, 'apps/client/src')
  const allowed = 'apps/client/src/generated/control-plane-client.ts'
  const pattern = /Reflect\.get\(globalThis,\s*['"](?:fetch|WebSocket)['"]\)|\b(?:fetch|WebSocket)\s*\(/u
  for (const path of walk(clientRoot, { skip: ignoredSourceDirectoryNames })) {
    if (!isTextFile(path)) continue
    const text = readText(path)
    if (text === null || !pattern.test(text)) continue
    const name = relativePath(path)
    if (name !== allowed) findings.push(finding(path, 'second-client-network-authority', lineNumber(text, text.search(pattern))))
  }
  return findings
}

function providerNetworkFindings() {
  const files = walk(join(root, 'crates/winwincode-control-plane/src'), { skip: ignoredSourceDirectoryNames })
    .filter(path => /\/provider_[^/]+\.rs$/u.test(path))
    .filter(path => /ureq::(?:Agent|Body|http)|ureq::tls/u.test(readText(path) ?? ''))
  const findings = files
    .filter(path => relativePath(path) !== 'crates/winwincode-control-plane/src/provider_https_sse.rs')
    .map(path => Object.freeze({ marker: 'second-provider-network-authority', path: relativePath(path) }))
  if (files.length !== 1) {
    findings.push(Object.freeze({
      marker: 'provider-network-authority-count',
      path: `expected=1 actual=${String(files.length)}`,
    }))
  }
  return { findings, files: files.map(relativePath) }
}

function executionNetworkFindings() {
  const findings = []
  for (const directory of [
    'crates/winwincode-worker/src',
    'crates/winwincode-codex/src',
    'crates/winwincode-local/src',
  ]) {
    for (const path of walk(join(root, directory), { skip: ignoredSourceDirectoryNames })) {
      if (!path.endsWith('.rs')) continue
      const text = readText(path)
      if (text === null) continue
      if (/\b(?:ureq|reqwest|hyper)::|@deepseek-ai\/|\bDshModelPort\b|\bctx\.llm\b/iu.test(text)) {
        findings.push(finding(path, 'execution-network-or-legacy-authority'))
      }
    }
  }
  return findings
}

function canonicalFacadeFindings() {
  const files = [
    'apps/client/src/control-plane-client.ts',
    'apps/client/src/generated/control-plane-client.ts',
  ]
  return files
    .filter(path => !existsSync(join(root, path)))
    .map(path => Object.freeze({ marker: 'missing-canonical-client-facade', path }))
}

const source = sourceFiles()
const manifests = explicitManifestPaths()
const artifacts = artifactFindings()
const distLegacy = distLegacyFindings()
const provider = providerNetworkFindings()
const report = Object.freeze({
  schemaVersion: 1,
  gate: 'winwincode-9c4.16.6.6.6',
  generatedAt: new Date().toISOString(),
  sourceFiles: source.length,
  manifestFiles: manifests.length,
  gates: Object.freeze([
    gate('legacy-directories', forbiddenDirectoryFindings()),
    gate('source-legacy-identifiers', markerFindings(source, forbiddenMarkers)),
    gate('manifest-lock-dependencies', markerFindings(manifests, forbiddenMarkers)),
    gate('package-artifacts', artifacts),
    gate('dist-artifact-content', distLegacy),
    gate('canonical-client-facade', canonicalFacadeFindings()),
    gate('client-network-authority', clientNetworkFindings()),
    gate('provider-network-authority', provider.findings, { providerFiles: provider.files }),
    gate('execution-boundary-network-authority', executionNetworkFindings()),
  ]),
})
const red = report.gates.filter(entry => entry.status === 'red')

if (process.argv.includes('--json')) {
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
} else {
  process.stdout.write(`phase-6.6.6 negative gate ${red.length === 0 ? 'GREEN' : 'RED'}\n`)
  for (const entry of report.gates) {
    process.stdout.write(`${entry.status === 'green' ? 'GREEN' : 'RED'} ${entry.id}`
      + ` findings=${String(entry.findingCount)}\n`)
    for (const value of entry.findings.slice(0, 12)) {
      process.stdout.write(`  ${value.marker} ${value.path}${value.line === undefined ? '' : `:${String(value.line)}`}\n`)
    }
    if (entry.findingCount > entry.findings.length) {
      process.stdout.write(`  ... ${String(entry.findingCount - entry.findings.length)} more\n`)
    }
  }
}

if (red.length > 0) process.exitCode = 1
