import { homedir } from 'node:os'
import { join, resolve } from 'node:path'

import type { Context } from '@deepseek-ai/cordis'
import type { SurfaceDescriptor, WorkspaceComponentDescriptor } from '@winwincode/contracts'

import {
  StrongFlowOperatorRemoteService,
} from './operator-remote.js'
import {
  StrongFlowLocalJobService,
  createStrongFlowLocalProofAuthenticator,
} from './operator-service.js'

export interface Config {
  /** Durable WinWinCode home. DSH supplies this through dshHomePath('winwincode'). */
  readonly home?: string
  /** Local browser-session proof. Usually injected by the packaged launcher. */
  readonly localSessionProof?: string
  /** Local CLI peer proof. Usually injected by the packaged launcher. */
  readonly localPeerProof?: string
}

function configuredProof(value: string | undefined): string | undefined {
  return value === undefined || value.length === 0 ? undefined : value
}

/** Register the durable operator service and its DSH Host Remote. */
export function apply(ctx: Context, config: Config = {}): void {
  const home = resolve(
    config.home
      ?? process.env.WINWINCODE_HOME
      ?? join(homedir(), '.winwincode'),
  )
  const localSessionProof = configuredProof(
    config.localSessionProof ?? process.env.WINWINCODE_UI_AUTH_PROOF,
  )
  const localPeerProof = configuredProof(
    config.localPeerProof ?? process.env.WINWINCODE_CLI_AUTH_PROOF,
  )
  const invoker = new StrongFlowLocalJobService({
    home,
    authenticator: createStrongFlowLocalProofAuthenticator({
      ...(localSessionProof === undefined ? {} : { localSessionProof }),
      ...(localPeerProof === undefined ? {} : { localPeerProof }),
    }),
  })
  new StrongFlowOperatorRemoteService(ctx, invoker)
}

export * from './artifact-store.js'
export * from './artifact-validator.js'
export * from './controller.js'
export * from './definition-diagrams.js'
export * from './human-review-gate.js'
export * from './git-workspace.js'
export * from './governed-process-executor.js'
export * from './handoff.js'
export * from './job-store.js'
export * from './operator-remote.js'
export * from './operator-remote-client.js'
export * from './operator-service.js'
export * from './role-runner.js'
export * from './role-authority.js'
export * from './security-audit.js'
export * from './role-session.js'
export * from './workspace-policy.js'

export const strongFlowSurface: SurfaceDescriptor = Object.freeze({
  id: 'strongflow',
  label: 'StrongFlow',
  default: false,
})

export const strongFlowComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/strongflow',
  kind: 'surface',
})
