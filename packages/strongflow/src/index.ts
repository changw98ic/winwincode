import { randomBytes } from 'node:crypto'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'

import type { Context } from '@deepseek-ai/cordis'
import type {
  Delivery,
  RuntimeEvent,
  SurfaceDescriptor,
  WorkspaceComponentDescriptor,
} from '@winwincode/contracts'

import {
  createStrongFlowDeliveryLocalProofAuthenticator,
} from './delivery-authenticator.js'
import { StrongFlowServiceInvoker } from './delivery-invoker.js'
import { StrongFlowDeliveryRemoteService } from './delivery-remote.js'
import { StrongFlowService } from './delivery-service.js'

export interface Config {
  /** Durable WinWinCode home. DSH supplies this through dshHomePath('winwincode'). */
  readonly home?: string
  /** Optional local browser-session proof override; omitted values use one ephemeral host proof. */
  readonly localSessionProof?: string
  /** Local CLI peer proof. Usually injected by the packaged launcher. */
  readonly localPeerProof?: string
}

/** StrongFlow starts only after the DSH-owned Codex ledger reader is available. */
export const inject = ['winwincodeAgentFactory'] as const

function configuredProof(value: string | undefined): string | undefined {
  return value === undefined || value.length === 0 ? undefined : value
}

interface DshRuntimeEventReader {
  readRuntimeSessionEvents(dshSessionId: string): Promise<readonly RuntimeEvent[]>
}

function dshRuntimeEventReader(ctx: Context): DshRuntimeEventReader | null {
  const candidate = ctx.get('winwincodeAgentFactory') as unknown
  return typeof candidate === 'object'
    && candidate !== null
    && typeof Reflect.get(candidate, 'readRuntimeSessionEvents') === 'function'
    ? candidate as DshRuntimeEventReader
    : null
}

async function deliveryRuntimeEvents(
  reader: DshRuntimeEventReader,
  delivery: Delivery,
): Promise<readonly RuntimeEvent[]> {
  const bindings = delivery.sessionBindings.filter(binding => (
    binding.dshSessionId !== null && binding.codexSessionId !== null
  ))
  const eventSets = await Promise.all(bindings.map(binding => (
    reader.readRuntimeSessionEvents(binding.dshSessionId!)
  )))
  return Object.freeze(eventSets.flat())
}

/** Register the durable Delivery service and its DSH Host Remote. */
export function apply(ctx: Context, config: Config = {}): void {
  const home = resolve(
    config.home
      ?? join(resolve(process.env.DSH_HOME ?? join(homedir(), '.dsh')), 'winwincode'),
  )
  const localSessionProof = configuredProof(
    config.localSessionProof ?? process.env.WINWINCODE_UI_AUTH_PROOF,
  ) ?? randomBytes(32).toString('base64url')
  const localPeerProof = configuredProof(
    config.localPeerProof ?? process.env.WINWINCODE_CLI_AUTH_PROOF,
  )
  const runtimeReader = dshRuntimeEventReader(ctx)
  const service = new StrongFlowService({
    home,
    authenticator: createStrongFlowDeliveryLocalProofAuthenticator({
      localSessionProof,
      ...(localPeerProof === undefined ? {} : { localPeerProof }),
    }),
    ...(runtimeReader === null
      ? {}
      : {
        diagramExecutionSource: {
          async read(delivery: Delivery) {
            return Object.freeze({
              runtimeEvents: await deliveryRuntimeEvents(runtimeReader, delivery),
              candidate: null,
            })
          },
        },
      }),
  })
  new StrongFlowDeliveryRemoteService(
    ctx,
    new StrongFlowServiceInvoker(service),
    { localSessionProof },
  )
}

export * from './delivery-service.js'
export * from './delivery-store.js'
export * from './delivery-authenticator.js'
export * from './credential-boundary.js'
export * from './delivery-invoker.js'
export * from './delivery-remote.js'
export * from './delivery-remote-client.js'
export * from './delivery-runtime-projection.js'
export * from './evaluation-measures.js'
export * from './diagram-execution-projection.js'
export * from './acceptance-verification.js'
export * from './candidate-evidence.js'
export * from './independent-verification.js'
export * from './delivery-verdict.js'
export * from './delivery-attention.js'
export * from './plan-review.js'
export * from './github-publication.js'
export * from './github-review-package.js'
export * from './github-publication-provider.js'
export * from './github-publication-journal.js'
export * from './github-publication-runner.js'

export const strongFlowSurface: SurfaceDescriptor = Object.freeze({
  id: 'strongflow',
  label: 'StrongFlow',
  default: false,
})

export const strongFlowComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/strongflow',
  kind: 'surface',
})
