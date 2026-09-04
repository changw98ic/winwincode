#!/usr/bin/env node

// Phase 0 source-boundary lint (multi-user client plan §8.1, §13, §17.3,
// §20.7, task 6). Absolute local paths must never enter the multi-user
// Client public surface: schemas, generated contracts, contract docs,
// public fixtures, and the ADR that freezes access and occupancy. These
// files are projected to the Server verbatim, so a committed absolute path
// leaks machine-local structure (plan gate: "no absolute path in public
// projections").
//
// The scope is deliberately narrow. Client-local code may resolve absolute
// paths freely (§13: RepositoryBinding stays local); the ban applies only
// to the public contract surface listed in scanTargets below. Targets that
// do not exist yet (parallel Phase 0 lanes) are skipped so the gate stays
// green until those files land.

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

export const NO_ABSOLUTE_PATHS_SCHEMA_VERSION = 1

const root = resolve(import.meta.dirname, '..')

// Public multi-user Client surface. `match` filters directory targets by
// entry name; null means the target path itself (file or directory tree).
const scanTargets = Object.freeze([
  {
    path: 'schema/winwincode/v1',
    recursive: true,
    match: { prefix: 'client-control', suffix: '.json' },
  },
  {
    path: 'packages/contracts/src/client-control.ts',
    recursive: false,
    match: null,
  },
  {
    path: 'docs/contracts',
    recursive: false,
    match: { prefix: 'client-control', suffix: '.md' },
  },
  {
    path: 'tests/fixtures/client-control',
    recursive: true,
    match: null,
  },
  {
    path: 'docs/decisions/0030-multi-user-client-access-and-occupancy.md',
    recursive: false,
    match: null,
  },
])

// Lines containing one of these literals are exempt. The allowlist exists
// for legitimate path-shaped text that is not a filesystem path (for
// example HTTP route paths in contract prose). Every entry must carry a
// reason so the exemption stays auditable.
const defaultAllowedLiterals = Object.freeze([
  {
    literal: '/internal/v1/client/exchange',
    reason: 'ClientControlPort HTTP transport route; a URL path in contract prose, not a filesystem path',
  },
])

const forbiddenRules = Object.freeze([
  { id: 'macos-home-path', pattern: /\/Users\//gu },
  { id: 'linux-home-path', pattern: /\/home\//gu },
  { id: 'macos-volume-path', pattern: /\/Volumes\//gu },
  {
    id: 'windows-drive-path',
    // `C:\` and any backslash/forward drive prefix, not preceded by a word
    // character so URL schemes like `https:` do not match.
    pattern: /(?<![A-Za-z0-9])[A-Za-z]:[/\\]/gu,
  },
  {
    id: 'posix-absolute-path',
    // Long path-shaped strings with at least two segments. The lookbehind
    // skips URL schemes (`https:`), host-qualified URLs, `../` relative
    // links, `~/` home shorthand, and JSON pointers (`#/$defs/...`); none
    // of those are filesystem paths.
    pattern: /(?<![\w~#.:/])\/[A-Za-z0-9][A-Za-z0-9._-]*(?:\/[A-Za-z0-9][A-Za-z0-9._-]*)+/gu,
    minimumLength: 10,
  },
])

const maximumSnippetLength = 240

function walkFiles(directory, recursive) {
  const files = []
  if (!existsSync(directory)) return files
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) {
      if (recursive) files.push(...walkFiles(path, true))
    } else if (entry.isFile()) {
      files.push(path)
    }
  }
  return files
}

function collectTargetFiles(scanRoot, target) {
  const absolute = join(scanRoot, target.path)
  if (!existsSync(absolute)) return []
  if (target.match === null) {
    if (statSync(absolute).isDirectory()) return walkFiles(absolute, target.recursive)
    return [absolute]
  }
  return walkFiles(absolute, target.recursive).filter(path => {
    const name = path.split('/').pop() ?? ''
    return name.startsWith(target.match.prefix) && name.endsWith(target.match.suffix)
  })
}

export function collectScanFiles(scanRoot = root) {
  // Negative fixtures under an `invalid/` segment deliberately contain
  // absolute paths to prove the golden contract rejects them; they are sample
  // data, not part of the valid public surface.
  const files = scanTargets
    .flatMap(target => collectTargetFiles(scanRoot, target))
    .filter(path => !path.includes('/invalid/'))
  return Object.freeze([...new Set(files)].toSorted())
}

function allowedLiteralsFor(extraAllowedLiterals) {
  return Object.freeze([
    ...defaultAllowedLiterals.map(entry => entry.literal),
    ...extraAllowedLiterals,
  ])
}

export function scanText(text, options = {}) {
  const allowedLiterals = allowedLiteralsFor(options.extraAllowedLiterals ?? [])
  const findings = []
  text.split('\n').forEach((line, index) => {
    if (allowedLiterals.some(literal => line.includes(literal))) return
    for (const rule of forbiddenRules) {
      for (const match of line.matchAll(rule.pattern)) {
        if (rule.minimumLength !== undefined && match[0].length < rule.minimumLength) continue
        findings.push({
          ruleId: rule.id,
          line: index + 1,
          match: match[0],
          snippet: line.trim().slice(0, maximumSnippetLength),
        })
      }
    }
  })
  return findings
}

export function scanPublicSurface(options = {}) {
  const scanRoot = options.root ?? root
  const files = collectScanFiles(scanRoot)
  const findings = []
  for (const file of files) {
    const text = readFileSync(file, 'utf8')
    const name = relative(scanRoot, file).replaceAll('\\', '/')
    for (const finding of scanText(text, options)) {
      findings.push({ ...finding, path: name })
    }
  }
  const unique = [...new Map(
    findings.map(finding => [JSON.stringify(finding), finding]),
  ).values()].toSorted((left, right) => (
    `${left.path}:${String(left.line)}:${left.ruleId}`
      .localeCompare(`${right.path}:${String(right.line)}:${right.ruleId}`)
  ))
  return Object.freeze({
    schemaVersion: NO_ABSOLUTE_PATHS_SCHEMA_VERSION,
    status: unique.length === 0 ? 'green' : 'red',
    scannedFiles: files.length,
    findings: Object.freeze(unique),
  })
}

function parseAllowLiterals(argv) {
  const literals = []
  for (const argument of argv) {
    if (argument.startsWith('--allow=')) {
      const literal = argument.slice('--allow='.length)
      if (literal.length === 0) throw new Error('--allow= requires a non-empty literal')
      literals.push(literal)
      continue
    }
    throw new Error(`unknown argument: ${argument} (supported: --allow=<literal>)`)
  }
  return literals
}

function main() {
  let extraAllowedLiterals
  try {
    extraAllowedLiterals = parseAllowLiterals(process.argv.slice(2))
  } catch (error) {
    process.stderr.write(`${error.message}\n`)
    process.exit(2)
  }
  const report = scanPublicSurface({ extraAllowedLiterals })
  if (report.status !== 'green') {
    for (const finding of report.findings) {
      process.stderr.write(`${finding.path}:${finding.line}: [${finding.ruleId}] ${finding.snippet}\n`)
    }
    process.stderr.write(
      `absolute-path gate rejected the multi-user client public surface `
        + `(${report.findings.length} finding(s) across ${report.scannedFiles} file(s)); `
        + `use --allow=<literal> only for legitimate non-filesystem path text\n`,
    )
    process.exit(1)
  }
  process.stdout.write(
    `no absolute paths in the multi-user client public surface `
      + `(${report.scannedFiles} file(s) scanned)\n`,
  )
}

const invokedDirectly = process.argv[1] !== undefined
  && import.meta.url === pathToFileURL(resolve(process.argv[1])).href
if (invokedDirectly) main()
