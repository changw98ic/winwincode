#!/usr/bin/env node

import { readFileSync } from 'node:fs'

import {
  StrongFlowService,
  StrongFlowServiceInvoker,
  createStrongFlowDeliveryLocalProofAuthenticator,
} from '@winwincode/strongflow'

import { describeHost } from './index.js'
import {
  STRONGFLOW_DELIVERY_CLI_EXIT_CODES,
  runStrongFlowCli,
} from './strongflow-cli.js'
import { resolveWinWinCodeHome, runWinWinCodeWeb } from './web-host.js'

function installedVersion(): string {
  const manifest: unknown = JSON.parse(
    readFileSync(new URL('../package.json', import.meta.url), 'utf8'),
  )
  if (typeof manifest !== 'object' || manifest === null || !('version' in manifest)) {
    throw new Error('installed package manifest does not contain a version')
  }
  const version = manifest.version
  if (typeof version !== 'string' || version.length === 0) {
    throw new Error('installed package version must be a non-empty string')
  }
  return version
}

const VERSION = installedVersion()
const args = process.argv.slice(2)

function renderHelp(): string {
  return [
    'WinWinCode commands:',
    '  winwincode [web] [DSH_WEB_OPTIONS]  Start the stock DSH chat surface.',
    '  winwincode delivery help            Show Delivery commands.',
    '  winwincode --print-scaffold          Print the installed surface descriptor.',
    '  winwincode --version                 Print the installed version.',
    '',
  ].join('\n')
}

function configuredProof(value: string | undefined): string | undefined {
  return value === undefined || value.length === 0 ? undefined : value
}

if (args.length === 1 && (args[0] === '--version' || args[0] === '-v')) {
  process.stdout.write(`${VERSION}\n`)
} else if (args.length === 1 && args[0] === '--print-scaffold') {
  process.stdout.write(`${JSON.stringify(describeHost(), null, 2)}\n`)
} else if (args.length === 1 && (args[0] === '--help' || args[0] === '-h')) {
  process.stdout.write(renderHelp())
} else if (args.length === 0 || args[0] === 'web') {
  try {
    process.exitCode = await runWinWinCodeWeb(args[0] === 'web' ? args.slice(1) : args)
  } catch {
    process.stderr.write('WinWinCode Web 启动失败。\n')
    process.exitCode = STRONGFLOW_DELIVERY_CLI_EXIT_CODES.service
  }
} else {
  const home = resolveWinWinCodeHome()
  const localPeerProof = configuredProof(process.env.WINWINCODE_CLI_AUTH_PROOF)
  const service = new StrongFlowService({
    home,
    authenticator: createStrongFlowDeliveryLocalProofAuthenticator({
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
    process.exitCode = await runStrongFlowCli(args, new StrongFlowServiceInvoker(service), {
      signal: abort.signal,
      interruptedBy: () => interruptedBy,
    })
  } finally {
    process.removeListener('SIGINT', sigint)
    process.removeListener('SIGTERM', sigterm)
  }
}
