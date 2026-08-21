#!/usr/bin/env node

import { describeHost } from './index.js'

const VERSION = '0.0.0-dev.0'
const args = new Set(process.argv.slice(2))

if (args.has('--version') || args.has('-v')) {
  process.stdout.write(`${VERSION}\n`)
} else if (args.has('--print-scaffold')) {
  process.stdout.write(`${JSON.stringify(describeHost(), null, 2)}\n`)
} else {
  const host = describeHost()
  process.stdout.write(
    `WinWinCode ${VERSION} (${host.target}); default surface: ${host.defaultSurface.id}\n`,
  )
}
