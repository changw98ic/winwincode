#!/usr/bin/env node

import { resolve } from 'node:path'

import { runDifferentialGate } from './delivery-strongflow-differential-contract.mjs'

if (process.argv.length !== 3 || process.argv[2] !== '--check') {
  throw new TypeError('usage: run-delivery-strongflow-rust-differential.mjs --check')
}

const result = await runDifferentialGate({
  root: resolve(import.meta.dirname, '..'),
})

if (result.status === 'contract-only') {
  process.stdout.write(
    `Rust differential runner is absent; validated the frozen ${result.scenarioCount}-scenario plan\n`,
  )
} else {
  process.stdout.write(
    `Rust differential runner matched all ${result.scenarioCount} canonical scenarios\n`,
  )
}
