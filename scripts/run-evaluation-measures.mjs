#!/usr/bin/env node

import { readFile, rename, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { randomBytes } from 'node:crypto'

import { measureLiveEvaluationResult } from './evaluation-measures.mjs'

function usage(message) {
  if (message !== undefined) process.stderr.write(`${message}\n`)
  process.stderr.write(
    'Usage: node scripts/run-evaluation-measures.mjs --result FILE [--output FILE] [--check]\n',
  )
  process.exitCode = 2
}

function parseArgs(argv) {
  const options = { result: null, output: null, check: false }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--check') {
      options.check = true
      continue
    }
    if (argument === '--result' || argument === '--output') {
      const value = argv[index + 1]
      if (value === undefined || value.startsWith('--')) {
        usage(`missing value for ${argument}`)
        return null
      }
      options[argument.slice(2)] = resolve(value)
      index += 1
      continue
    }
    usage(`unexpected argument: ${argument}`)
    return null
  }
  if (options.result === null) {
    usage('--result is required')
    return null
  }
  return options
}

async function writeAtomic(path, value) {
  const temporary = `${path}.tmp-${process.pid}-${randomBytes(6).toString('hex')}`
  await writeFile(temporary, value, { mode: 0o600 })
  await rename(temporary, path)
}

const options = parseArgs(process.argv.slice(2))
if (options !== null) {
  try {
    const result = JSON.parse(await readFile(options.result, 'utf8'))
    const projection = measureLiveEvaluationResult(result)
    const projectionText = `${JSON.stringify(projection, null, 2)}\n`
    if (options.check) {
      const stored = result.measures === null || result.measures === undefined
        ? null
        : JSON.stringify(result.measures)
      if (stored !== JSON.stringify(projection)) {
        throw new Error('stored Delivery measures differ from the reproducible projection')
      }
    }
    if (options.output === null) process.stdout.write(projectionText)
    else await writeAtomic(options.output, projectionText)
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`)
    process.exitCode = 1
  }
}
