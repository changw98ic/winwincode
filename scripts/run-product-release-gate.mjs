#!/usr/bin/env node

import { randomBytes } from 'node:crypto'
import {
  existsSync,
  readdirSync,
  statSync,
} from 'node:fs'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'

import {
  ProductReleaseGateError,
  createProductReleaseGateReport,
  productReleaseReportSha256,
} from './product-release-gate.mjs'

function usage(message) {
  if (message !== undefined) process.stderr.write(`${message}\n`)
  process.stderr.write([
    'Usage: node scripts/run-product-release-gate.mjs',
    '  --expected-commit COMMIT',
    '  --native-evidence FILE_OR_DIRECTORY [--native-evidence ...]',
    '  --live-evaluation RESULT [--live-evaluation ...]',
    '  --output FILE',
    '',
  ].join(' '))
  process.exitCode = 2
}

function parseArguments(argv) {
  const options = {
    expectedCommit: null,
    nativeEvidence: [],
    liveEvaluation: [],
    output: null,
  }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (![
      '--expected-commit',
      '--native-evidence',
      '--live-evaluation',
      '--output',
    ].includes(argument)) {
      usage(`unexpected argument: ${argument}`)
      return null
    }
    const value = argv[index + 1]
    if (value === undefined || value.startsWith('--')) {
      usage(`missing value for ${argument}`)
      return null
    }
    if (argument === '--native-evidence') options.nativeEvidence.push(resolve(value))
    else if (argument === '--live-evaluation') options.liveEvaluation.push(resolve(value))
    else if (argument === '--expected-commit') options.expectedCommit = value
    else options.output = resolve(value)
    index += 1
  }
  if (options.expectedCommit === null
    || options.nativeEvidence.length === 0
    || options.liveEvaluation.length === 0
    || options.output === null) {
    usage('all release gate inputs are required')
    return null
  }
  return options
}

function findEvidenceFiles(path) {
  if (!existsSync(path)) throw new Error(`native evidence input does not exist: ${path}`)
  const status = statSync(path)
  if (status.isFile()) return [path]
  if (!status.isDirectory()) throw new Error(`native evidence input is not a file or directory: ${path}`)
  const result = []
  const visit = (directory) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const child = join(directory, entry.name)
      if (entry.isDirectory()) visit(child)
      else if (entry.isFile() && entry.name === 'native-release-evidence.json') result.push(child)
    }
  }
  visit(path)
  return result
}

async function writeReport(path, report) {
  const text = `${JSON.stringify(report, null, 2)}\n`
  if (existsSync(path)) {
    const current = await readFile(path, 'utf8')
    if (current !== text) throw new Error('release gate output already contains different facts')
    return
  }
  await mkdir(dirname(path), { recursive: true })
  const temporary = `${path}.tmp-${process.pid}-${randomBytes(6).toString('hex')}`
  await writeFile(temporary, text, { mode: 0o600 })
  await rename(temporary, path)
}

const options = parseArguments(process.argv.slice(2))
if (options !== null) {
  try {
    const nativeEvidencePaths = [...new Set(options.nativeEvidence.flatMap(findEvidenceFiles))]
    const report = createProductReleaseGateReport({
      root: resolve(import.meta.dirname, '..'),
      expectedCommit: options.expectedCommit,
      nativeEvidencePaths,
      liveEvaluationPaths: [...new Set(options.liveEvaluation)],
    })
    await writeReport(options.output, report)
    process.stdout.write(`${JSON.stringify({
      status: report.status,
      sourceCommit: report.source.commit,
      nativeTargets: report.nativeTargets.map(entry => entry.target),
      liveRuns: report.evaluations.live.map(entry => entry.runId),
      reportSha256: productReleaseReportSha256(report),
      output: options.output,
    })}\n`)
  } catch (error) {
    process.stderr.write(`${JSON.stringify({
      code: error instanceof ProductReleaseGateError ? error.code : 'RELEASE_GATE_FAILED',
      message: error instanceof Error ? error.message : String(error),
    })}\n`)
    process.exitCode = 1
  }
}
