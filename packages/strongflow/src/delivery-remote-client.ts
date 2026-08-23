import type { Context } from '@deepseek-ai/cordis'
import type {
  RemoteResult,
  TypertClientRemote,
  TypertDisposer,
  TypertRemoteContribution,
} from '@deepseek-ai/dsh-typert-protocol'
import {
  parseStrongFlowDeliveryRequest,
  parseStrongFlowDeliveryResponse,
  type StrongFlowDeliveryRequest,
  type StrongFlowDeliveryResponse,
} from '@winwincode/contracts'

export const STRONGFLOW_DELIVERY_REMOTE_SERVICE = 'strongFlowDelivery' as const
export const STRONGFLOW_DELIVERY_REMOTE_NAMESPACE = 'strongflow' as const
export const STRONGFLOW_DELIVERY_REMOTE_METHOD = 'invoke' as const
export const STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE = 'dsh-reference-only' as const

type StrongFlowDeliveryRemoteInvoke = (
  agentId: string,
  request: StrongFlowDeliveryRequest,
  signal?: AbortSignal,
) => Promise<RemoteResult<StrongFlowDeliveryResponse>>

type StrongFlowDeliveryScopedRemoteInvoke = (
  request: StrongFlowDeliveryRequest,
  signal?: AbortSignal,
) => Promise<RemoteResult<StrongFlowDeliveryResponse>>

declare module '@deepseek-ai/dsh-typert-protocol' {
  interface TypertRemoteNamespace$7374726f6e67666c6f77 {
    invoke: StrongFlowDeliveryRemoteInvoke
  }

  interface TypertRemoteMap {
    'strongflow/invoke': StrongFlowDeliveryRemoteInvoke
  }

  interface TypertRemoteNamespaceMap {
    strongflow: TypertRemoteNamespace$7374726f6e67666c6f77
  }

  interface TypertRemoteScopeMap {
    'agent:strongflow/invoke': StrongFlowDeliveryScopedRemoteInvoke
  }
}

const dshSessionIdSchema = Object.freeze({
  parse(value: unknown): string {
    if (typeof value !== 'string'
      || !/^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u.test(value)) {
      throw new TypeError('dshSessionId must be a portable session identity')
    }
    return value
  },
})

const requestSchema = Object.freeze({
  parse: parseStrongFlowDeliveryRequest,
})

const responseSchema = Object.freeze({
  parse: parseStrongFlowDeliveryResponse,
})

/** Strict Client contribution mounted only by the StrongFlow advanced surface. */
export const STRONGFLOW_DELIVERY_REMOTE: TypertRemoteContribution = Object.freeze({
  package: '@winwincode/strongflow',
  descriptors: Object.freeze([
    Object.freeze({
      id: '@winwincode/strongflow#strongflow/invoke',
      service: STRONGFLOW_DELIVERY_REMOTE_SERVICE,
      namespace: STRONGFLOW_DELIVERY_REMOTE_NAMESPACE,
      method: STRONGFLOW_DELIVERY_REMOTE_METHOD,
      invocation: Object.freeze({ kind: 'direct' as const }),
      scope: Object.freeze({
        context: 'agent',
        wire: 'agentId',
      }),
      parameters: Object.freeze([
        Object.freeze({
          name: 'agent',
          wire: 'agentId',
          source: 'lookup' as const,
          lookup: 'agent',
          codec: Object.freeze({
            mode: 'strict' as const,
            typeSymbol: '@deepseek-ai/dsh-session/types#SessionId',
            schema: dshSessionIdSchema,
          }),
        }),
        Object.freeze({
          name: 'request',
          wire: 'request',
          source: 'json' as const,
          codec: Object.freeze({
            mode: 'strict' as const,
            typeSymbol: '@winwincode/contracts#StrongFlowDeliveryRequest',
            schema: requestSchema,
          }),
        }),
      ]),
      cancellation: Object.freeze({ parameter: 'signal' as const }),
      result: Object.freeze({
        mode: 'strict' as const,
        typeSymbol: '@winwincode/contracts#StrongFlowDeliveryResponse',
        schema: responseSchema,
      }),
    }),
  ]),
})

interface StrongFlowRemoteClientContext extends Context {
  readonly remote: TypertClientRemote
}

/** Mount the canonical Delivery Remote contribution in the calling Client fiber. */
export function mountStrongFlowDeliveryRemote(ctx: Context): Promise<TypertDisposer> {
  return (ctx as StrongFlowRemoteClientContext).remote.$mount(STRONGFLOW_DELIVERY_REMOTE)
}
