#!/usr/bin/env node

import { homedir } from 'node:os'
import { join, resolve } from 'node:path'

import {
  StrongFlowLocalJobService,
  createStrongFlowLocalProofAuthenticator,
} from '@winwincode/strongflow'

import { describeHost } from './index.js'
import { runStrongFlowCli } from './strongflow-cli.js'

const VERSION = '0.0.0-dev.0'
const args = process.argv.slice(2)

function configuredProof(value: string | undefined): string | undefined {
  return value === undefined || value.length === 0 ? undefined : value
}

if (args.length === 1 && (args[0] === '--version' || args[0] === '-v')) {
  process.stdout.write(`${VERSION}\n`)
} else if (args.length === 1 && args[0] === '--print-scaffold') {
  process.stdout.write(`${JSON.stringify(describeHost(), null, 2)}\n`)
} else if (args.length > 0) {
  const home = resolve(process.env.WINWINCODE_HOME ?? join(homedir(), '.winwincode'))
  const localPeerProof = configuredProof(process.env.WINWINCODE_CLI_AUTH_PROOF)
  const service = new StrongFlowLocalJobService({
    home,
    authenticator: createStrongFlowLocalProofAuthenticator({
      ...(localPeerProof === undefined ? {} : { localPeerProof }),
    }),
  })
  const abort = new AbortController()
  let interruptedBy: 'SIGINT' | 'SIGTERM' | undefined
  const interrupt = (signal: 'SIGINT' | 'SIGTERM'): void => {
    interruptedBy ??= signal
    abort.abort()
  }
  const sigint = (): void => interrupt('SIGINT')
  const sigterm = (): void => interrupt('SIGTERM')
  process.once('SIGINT', sigint)
  process.once('SIGTERM', sigterm)
  try {
    process.exitCode = await runStrongFlowCli(args, service, {
      signal: abort.signal,
      interruptedBy: () => interruptedBy,
    })
  } finally {
    process.removeListener('SIGINT', sigint)
    process.removeListener('SIGTERM', sigterm)
  }
} else {
  const host = describeHost()
  process.stdout.write(
    `WinWinCode ${VERSION} (${host.target}); default surface: ${host.defaultSurface.id}\n`,
  )
}
