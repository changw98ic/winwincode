import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  collectScanFiles,
  scanPublicSurface,
  scanText,
} from '../scripts/check-no-absolute-paths.mjs'

// Phase 0 path-ban lint lane: absolute local paths must never enter the
// multi-user Client public surface (plan §8.1, §13, §17.3, §20.7, gate
// "no absolute path in public projections"). These tests exercise the gate
// against fabricated temporary trees plus the current repository tree.

const root = resolve(import.meta.dirname, '..')
const scriptPath = join(root, 'scripts', 'check-no-absolute-paths.mjs')

function ruleIds(findings) {
  return [...new Set(findings.map(finding => finding.ruleId))].sort()
}

test('scanText flags every absolute path class', () => {
  const text = [
    'mac binding /Users/alice/work/repo',
    'linux binding /home/bob/project',
    'volume /Volumes/ORICO/backups',
    'windows C:\\Users\\alice\\repo',
    'generic /srv/build/artifacts',
  ].join('\n')
  const findings = scanText(text)
  assert.deepEqual(ruleIds(findings), [
    'linux-home-path',
    'macos-home-path',
    'macos-volume-path',
    'posix-absolute-path',
    'windows-drive-path',
  ])
  const lines = findings.map(finding => finding.line)
  assert.ok(lines.includes(1), '/Users/ finding must carry its line number')
  const leak = findings.find(finding => finding.match === '/Users/alice/work/repo')
  assert.ok(leak, 'posix rule must report the full matched path')
  assert.equal(leak.line, 1)
  assert.ok(leak.snippet.includes('/Users/alice/work/repo'))
})

test('scanText ignores path-shaped text that is not a filesystem path', () => {
  const text = [
    'repo-relative schema/winwincode/v1/client-control.schema.json',
    'url https://winwincode.dev/schemas/client-control/v1',
    'json pointer #/$defs/ClientControlEnvelope',
    'markdown link (../contracts/execution-port-v1.md)',
    'home shorthand ~/notes/todo.md',
    'short /a/b and api route POST /internal/v1/client/exchange',
  ].join('\n')
  assert.deepEqual(scanText(text), [])
})

test('scanText honors extra allowed literals per line', () => {
  const text = 'fixed fixture root /tmp/demo-repo/build/out.json'
  assert.equal(scanText(text).length > 0, true)
  assert.deepEqual(scanText(text, { extraAllowedLiterals: ['/tmp/demo-repo'] }), [])
})

function writeTree(target) {
  mkdirSync(join(target, 'schema/winwincode/v1'), { recursive: true })
  mkdirSync(join(target, 'packages/contracts/src'), { recursive: true })
  mkdirSync(join(target, 'docs/contracts'), { recursive: true })
  mkdirSync(join(target, 'docs/decisions'), { recursive: true })
  mkdirSync(join(target, 'tests/fixtures/client-control'), { recursive: true })
  writeFileSync(
    join(target, 'schema/winwincode/v1/client-control.schema.json'),
    '{\n  "clean": true\n}\n',
  )
  writeFileSync(
    join(target, 'packages/contracts/src/client-control.ts'),
    'export const route = \'/internal/v1/client/exchange\'\n',
  )
  writeFileSync(
    join(target, 'docs/contracts/client-control-port-v1.md'),
    '# ClientControlPort v1\n\nNo local paths here.\n',
  )
  writeFileSync(
    join(target, 'docs/contracts/client-control-state-machines.md'),
    '# Client control state machines\n',
  )
  writeFileSync(
    join(target, 'tests/fixtures/client-control/sample.json'),
    '{\n  "root": "/Users/alice/work/repo"\n}\n',
  )
  writeFileSync(
    join(target, 'docs/decisions/0030-multi-user-client-access-and-occupancy.md'),
    '# ADR-0030\n',
  )
  // Untouched target kinds must be skipped without failing the scan.
}

test('scanPublicSurface scans every target kind and skips missing ones', (t) => {
  const target = mkdtempSync(join(tmpdir(), 'wwc-path-ban-'))
  t.after(() => rmSync(target, { recursive: true, force: true }))
  writeTree(target)

  const report = scanPublicSurface({ root: target })
  assert.equal(report.schemaVersion, 1)
  assert.equal(report.scannedFiles, 6)
  assert.equal(report.status, 'red')
  assert.deepEqual(
    report.findings.map(finding => `${finding.path}:${finding.line}:${finding.ruleId}`),
    [
      'tests/fixtures/client-control/sample.json:2:macos-home-path',
      'tests/fixtures/client-control/sample.json:2:posix-absolute-path',
    ],
  )
  assert.deepEqual(
    report.findings.map(finding => finding.match),
    ['/Users/', '/Users/alice/work/repo'],
  )
})

test('collectScanFiles covers the current public surface', () => {
  const files = collectScanFiles().map(path => path.replaceAll('\\', '/'))
  assert.ok(files.some(path => path.endsWith('docs/contracts/client-control-port-v1.md')))
  assert.ok(files.some(path => path.endsWith('docs/contracts/client-control-state-machines.md')))
  assert.ok(
    files.some(path => path.endsWith('docs/decisions/0030-multi-user-client-access-and-occupancy.md')),
  )
  assert.ok(
    !files.some(path => path.includes('/src/') && !path.endsWith('client-control.ts')),
    'scan must stay limited to the client-control contract surface',
  )
})

test('current repository tree passes and the script exits green from any cwd', () => {
  assert.equal(scanPublicSurface().status, 'green')
  const result = spawnSync(process.execPath, [scriptPath], { cwd: tmpdir() })
  assert.equal(result.status, 0, `${scriptPath} must exit 0: ${result.stderr}`)
  assert.match(result.stdout.toString(), /no absolute paths in the multi-user client public surface/)
})
