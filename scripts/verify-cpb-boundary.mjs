#!/usr/bin/env node

import { resolve } from 'node:path'

import { scanRepositoryCpbBoundary } from './cpb-boundary-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const errors = scanRepositoryCpbBoundary(root)

if (errors.length > 0) {
  for (const error of errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}

process.stdout.write('CPB design-only repository boundary verified\n')
