#!/usr/bin/env node

import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'

import {
  LiveEvaluationError,
  runLiveEvaluation,
} from './live-evaluation.mjs'

function usage(message) {
  if (message !== undefined) process.stderr.write(`${message}\n`)
  process.stderr.write(
    'Usage: node scripts/run-live-evaluation.mjs --live --config FILE --output DIRECTORY\n',
  )
  process.exitCode = 2
}

function parseArgs(argv) {
  const options = { live: false, config: null, output: null }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--live') {
      options.live = true
      continue
    }
    if (argument === '--config' || argument === '--output') {
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
  if (!options.live) {
    usage('explicit --live opt-in is required because this command may spend provider credits')
    return null
  }
  if (options.config === null || options.output === null) {
    usage('--config and --output are required')
    return null
  }
  return options
}

const options = parseArgs(process.argv.slice(2))
if (options !== null) {
  const controller = new AbortController()
  let signalNumber = null
  const interrupt = (signal, number) => {
    signalNumber ??= number
    controller.abort(new LiveEvaluationError('INTERRUPTED', `evaluation received ${signal}`))
  }
  const interruptSigint = () => interrupt('SIGINT', 2)
  const interruptSigterm = () => interrupt('SIGTERM', 15)
  process.once('SIGINT', interruptSigint)
  process.once('SIGTERM', interruptSigterm)
  try {
    const config = JSON.parse(await readFile(options.config, 'utf8'))
    const result = await runLiveEvaluation({
      optIn: true,
      config,
      outputDirectory: options.output,
      signal: controller.signal,
    })
    process.stdout.write(`${JSON.stringify({
      state: result.result.state,
      resultPath: result.path,
    })}\n`)
  } catch (error) {
    process.stderr.write(`${JSON.stringify({
      code: typeof error?.code === 'string' ? error.code : 'RUNTIME_FAILURE',
      resultPath: typeof error?.evaluationResultPath === 'string'
        ? error.evaluationResultPath
        : null,
    })}\n`)
    process.exitCode = signalNumber === null ? 1 : 128 + signalNumber
  } finally {
    process.removeListener('SIGINT', interruptSigint)
    process.removeListener('SIGTERM', interruptSigterm)
  }
}
