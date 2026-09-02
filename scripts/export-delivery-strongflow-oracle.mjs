import { readFile, mkdir, writeFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'

import {
  buildLegacyDeliveryStrongFlowOracle,
} from '../tests/fixtures/delivery-strongflow-differential-oracle.mjs'

const outputPath = resolve(
  import.meta.dirname,
  '..',
  'tests',
  'fixtures',
  'oracles',
  'delivery-strongflow-typescript.v1.json',
)

function mode(arguments_) {
  if (arguments_.length !== 1 || !['--check', '--write'].includes(arguments_[0])) {
    throw new TypeError('usage: export-delivery-strongflow-oracle.mjs --check|--write')
  }
  return arguments_[0]
}

function assertPortable(serialized) {
  if (serialized.includes(process.execPath)
    || serialized.includes('fixture-local-session-proof-value')
    || serialized.includes('fixture-local-peer-proof-value')
    || /\/(?:Users|Volumes|private\/tmp)\//u.test(serialized)) {
    throw new Error('legacy Delivery oracle contains a machine path or credential value')
  }
}

const selectedMode = mode(process.argv.slice(2))
const oracle = await buildLegacyDeliveryStrongFlowOracle()
const serialized = `${JSON.stringify(oracle, null, 2)}\n`
assertPortable(serialized)

if (selectedMode === '--write') {
  await mkdir(dirname(outputPath), { recursive: true })
  await writeFile(outputPath, serialized)
  process.stdout.write(`wrote ${oracle.scenarios.length} scenarios to ${outputPath}\n`)
} else {
  const expected = await readFile(outputPath, 'utf8')
  if (expected !== serialized) {
    throw new Error('legacy Delivery oracle is stale; run pnpm oracle:delivery:export')
  }
  process.stdout.write(`legacy Delivery oracle is current (${oracle.scenarios.length} scenarios)\n`)
}
