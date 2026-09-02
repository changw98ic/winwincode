#!/usr/bin/env node

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'

import {
  RELEASE_REPORT_KIND,
  canonicalJson,
  createReleaseReport,
  jsonSha256,
} from './release-artifact-contract.mjs'

const root = resolve(import.meta.dirname, '..')

function parseArguments(argv) {
  const values = new Map()
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (!argument.startsWith('--')) throw new Error(`unexpected argument ${argument}`)
    const separator = argument.indexOf('=')
    if (separator !== -1) {
      values.set(argument.slice(2, separator), argument.slice(separator + 1))
      continue
    }
    const value = argv[index + 1]
    if (value === undefined || value.startsWith('--')) throw new Error(`${argument} requires a value`)
    values.set(argument.slice(2), value)
    index += 1
  }
  const allowed = new Set([
    'expected-commit',
    'source-date-epoch',
    'evidence',
    'output',
  ])
  for (const key of values.keys()) {
    if (!allowed.has(key)) throw new Error(`unknown argument --${key}`)
  }
  for (const key of allowed) {
    if (!values.has(key)) throw new Error(`--${key} is required`)
  }
  return Object.freeze({
    expectedCommit: values.get('expected-commit'),
    sourceDateEpoch: Number(values.get('source-date-epoch')),
    evidenceRoot: resolve(root, values.get('evidence')),
    output: resolve(root, values.get('output')),
  })
}

const { expectedCommit, sourceDateEpoch, evidenceRoot, output } = parseArguments(
  process.argv.slice(2),
)
const report = createReleaseReport({ root, evidenceRoot, expectedCommit, sourceDateEpoch })
if (report.kind !== RELEASE_REPORT_KIND || report.status !== 'passed') {
  throw new Error('release report did not reach its canonical passing state')
}
const text = canonicalJson(report)
if (existsSync(output) && readFileSync(output, 'utf8') !== text) {
  throw new Error(`existing release report differs from current evidence: ${output}`)
}
mkdirSync(dirname(output), { recursive: true })
writeFileSync(output, text)
process.stdout.write(`${canonicalJson({
  status: report.status,
  sourceCommit: expectedCommit,
  sourceDateEpoch,
  targets: report.targets.map(entry => entry.target),
  reportSha256: jsonSha256(report),
  output,
})}`)
