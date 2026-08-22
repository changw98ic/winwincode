import type { Context } from '@deepseek-ai/cordis'
import type {
  RemoteResult,
  TypertClientRemote,
  TypertDisposer,
  TypertRemoteContribution,
} from '@deepseek-ai/dsh-typert-protocol'
import {
  parseStrongFlowOperatorRequest,
  parseStrongFlowOperatorResponse,
  type StrongFlowOperatorRequest,
  type StrongFlowOperatorResponse,
} from '@winwincode/contracts'

export const STRONGFLOW_OPERATOR_REMOTE_SERVICE = 'strongFlowOperator' as const
export const STRONGFLOW_OPERATOR_REMOTE_NAMESPACE = 'strongflow' as const
export const STRONGFLOW_OPERATOR_REMOTE_METHOD = 'invoke' as const

type StrongFlowOperatorRemoteInvoke = (
  request: StrongFlowOperatorRequest,
  signal?: AbortSignal,
) => Promise<RemoteResult<StrongFlowOperatorResponse>>

declare module '@deepseek-ai/dsh-typert-protocol' {
  interface TypertRemoteNamespace$7374726f6e67666c6f77 {
    invoke: StrongFlowOperatorRemoteInvoke
  }

  interface TypertRemoteMap {
    'strongflow/invoke': StrongFlowOperatorRemoteInvoke
  }

  interface TypertRemoteNamespaceMap {
    strongflow: TypertRemoteNamespace$7374726f6e67666c6f77
  }
}

const requestSchema = Object.freeze({
  parse: parseStrongFlowOperatorRequest,
})

const responseSchema = Object.freeze({
  parse: parseStrongFlowOperatorResponse,
})

/** Strict Client contribution mounted only by the StrongFlow advanced surface. */
export const STRONGFLOW_OPERATOR_REMOTE: TypertRemoteContribution = Object.freeze({
  package: '@winwincode/strongflow',
  descriptors: Object.freeze([
    Object.freeze({
      id: '@winwincode/strongflow#strongflow/invoke',
      service: STRONGFLOW_OPERATOR_REMOTE_SERVICE,
      namespace: STRONGFLOW_OPERATOR_REMOTE_NAMESPACE,
      method: STRONGFLOW_OPERATOR_REMOTE_METHOD,
      invocation: Object.freeze({ kind: 'direct' as const }),
      parameters: Object.freeze([
        Object.freeze({
          name: 'request',
          wire: 'request',
          source: 'json' as const,
          codec: Object.freeze({
            mode: 'strict' as const,
            typeSymbol: '@winwincode/contracts#StrongFlowOperatorRequest',
            schema: requestSchema,
          }),
        }),
      ]),
      cancellation: Object.freeze({ parameter: 'signal' as const }),
      result: Object.freeze({
        mode: 'strict' as const,
        typeSymbol: '@winwincode/contracts#StrongFlowOperatorResponse',
        schema: responseSchema,
      }),
    }),
  ]),
})

interface StrongFlowRemoteClientContext extends Context {
  readonly remote: TypertClientRemote
}

/** Mount the strict Remote contribution in the calling Client fiber. */
export function mountStrongFlowOperatorRemote(ctx: Context): Promise<TypertDisposer> {
  return (ctx as StrongFlowRemoteClientContext).remote.$mount(STRONGFLOW_OPERATOR_REMOTE)
}
