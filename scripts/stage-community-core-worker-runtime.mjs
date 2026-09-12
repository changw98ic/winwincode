#!/usr/bin/env node

import { resolve } from 'node:path'

import {
  canonicalJson,
  stageCommunityCoreWorkerRuntime,
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
  const required = ['artifact-root', 'source-commit', 'source-date-epoch', 'target', 'output']
  for (const key of values.keys()) {
    if (!required.includes(key)) throw new Error(`unknown argument --${key}`)
  }
  for (const key of required) {
    if (!values.has(key)) throw new Error(`--${key} is required`)
  }
  const sourceDateEpoch = Number(values.get('source-date-epoch'))
  if (!Number.isSafeInteger(sourceDateEpoch) || sourceDateEpoch <= 0) {
    throw new Error('--source-date-epoch must be a positive integer')
  }
  return Object.freeze({
    artifactRoot: resolve(root, values.get('artifact-root')),
    expectedCommit: values.get('source-commit'),
    expectedSourceDateEpoch: sourceDateEpoch,
    expectedTarget: values.get('target'),
    outputRoot: resolve(root, values.get('output')),
  })
}

try {
  const result = stageCommunityCoreWorkerRuntime({ root, ...parseArguments(process.argv.slice(2)) })
  process.stdout.write(canonicalJson({ status: 'passed', ...result }))
} catch (error) {
  process.stderr.write(`${error.message}\n`)
  process.exitCode = 1
}
