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
import {
  StrongFlowDeliveryStageCoordinator,
} from './delivery-stage-coordinator.js'
import {
  DshStrongFlowStageRuntime,
  type StrongFlowDshAgentFactoryPort,
} from './dsh-stage-runtime.js'
import { StrongFlowServiceInvoker } from './delivery-invoker.js'
import { StrongFlowDeliveryRemoteService } from './delivery-remote.js'
import { StrongFlowService } from './delivery-service.js'
import { LocalGitDeliveryWorkspace } from './local-git-delivery-workspace.js'

export interface Config {
  /** Durable WinWinCode home. DSH supplies this through dshHomePath('winwincode'). */
  readonly home?: string
  /** Optional local browser-session proof override; omitted values use one ephemeral host proof. */
  readonly localSessionProof?: string
  /** Local CLI peer proof. Usually injected by the packaged launcher. */
  readonly localPeerProof?: string
}

/** StrongFlow starts only after the DSH-owned Codex ledger reader is available. */
export const inject = ['agents', 'winwincodeAgentFactory'] as const

function configuredProof(value: string | undefined): string | undefined {
  return value === undefined || value.length === 0 ? undefined : value
}

function dshAgentFactory(ctx: Context): StrongFlowDshAgentFactoryPort {
  const candidate = ctx.get('winwincodeAgentFactory') as unknown
  if (typeof candidate === 'object'
    && candidate !== null
    && typeof Reflect.get(candidate, 'readRuntimeSessionEvents') === 'function'
    && typeof Reflect.get(candidate, 'readRuntimeSessionManifest') === 'function'
    && typeof Reflect.get(candidate, 'reconcileDelivery') === 'function') {
    return candidate as StrongFlowDshAgentFactoryPort
  }
  throw new TypeError('StrongFlow requires the WinWinCode DSH AgentFactory')
}

async function deliveryRuntimeEvents(
  reader: StrongFlowDshAgentFactoryPort,
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
  const runtimeFactory = dshAgentFactory(ctx)
  const workspace = new LocalGitDeliveryWorkspace({ home })
  const service = new StrongFlowService({
    home,
    authenticator: createStrongFlowDeliveryLocalProofAuthenticator({
      localSessionProof,
      ...(localPeerProof === undefined ? {} : { localPeerProof }),
    }),
    executionSource: {
      async read(delivery: Delivery) {
        const candidate = delivery.spec.repository.kind === 'local-git'
          ? await workspace.currentCandidateSnapshot(delivery)
          : null
        return Object.freeze({
          runtimeEvents: await deliveryRuntimeEvents(runtimeFactory, delivery),
          candidate: candidate?.candidate ?? null,
          candidateDiff: candidate?.unifiedDiff ?? null,
        })
      },
    },
  })
  const coordinator = new StrongFlowDeliveryStageCoordinator({
    service,
    runtime: new DshStrongFlowStageRuntime({
      ctx,
      agentFactory: runtimeFactory,
    }),
    workspace,
  })
  new StrongFlowDeliveryRemoteService(
    ctx,
    new StrongFlowServiceInvoker(service),
    { localSessionProof, coordinator },
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
export * from './runtime-execution-projection.js'
export * from './execution-source.js'
export * from './evaluation-measures.js'
export * from './diagram-execution-projection.js'
export * from './acceptance-verification.js'
export * from './candidate-evidence.js'
export * from './independent-verification.js'
export * from './delivery-verdict.js'
export * from './delivery-attention.js'
export * from './plan-review.js'
export * from './local-git-delivery-workspace.js'
export * from './delivery-stage-coordinator.js'
export * from './dsh-stage-runtime.js'
export * from './github-publication.js'

export const strongFlowSurface: SurfaceDescriptor = Object.freeze({
  id: 'strongflow',
  label: 'StrongFlow',
  default: false,
})

export const strongFlowComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/strongflow',
  kind: 'surface',
})
