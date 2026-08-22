import type { Context } from '@deepseek-ai/cordis'
import {
  Remote,
  TypertRemoteService,
} from '@deepseek-ai/dsh-typert-protocol'
import {
  STRONGFLOW_OPERATOR_OPERATIONS,
  materializeStrongFlowOperatorFailure,
  parseStrongFlowOperatorRequest,
  parseStrongFlowOperatorResponse,
  type StrongFlowOperatorInvoker,
  type StrongFlowOperatorOperation,
  type StrongFlowOperatorRequest,
  type StrongFlowOperatorResponse,
} from '@winwincode/contracts'

import {
  STRONGFLOW_OPERATOR_REMOTE_NAMESPACE,
  STRONGFLOW_OPERATOR_REMOTE_SERVICE,
} from './operator-remote-client.js'

function boundaryIdentity(value: unknown): {
  readonly requestId: string | null
  readonly operation: StrongFlowOperatorOperation | null
} {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return Object.freeze({ requestId: null, operation: null })
  }
  const input = value as Record<string, unknown>
  const requestId = typeof input.requestId === 'string'
    && /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u.test(input.requestId)
    ? input.requestId
    : null
  const operation = typeof input.operation === 'string'
    && STRONGFLOW_OPERATOR_OPERATIONS.includes(
      input.operation as StrongFlowOperatorOperation,
    )
    ? input.operation as StrongFlowOperatorOperation
    : null
  return Object.freeze({ requestId, operation })
}

/** Host-side DSH Remote that keeps the wire boundary on the shared operator contract. */
export class StrongFlowOperatorRemoteService extends TypertRemoteService {
  private readonly invoker: StrongFlowOperatorInvoker

  constructor(ctx: Context, invoker: StrongFlowOperatorInvoker) {
    super(ctx, STRONGFLOW_OPERATOR_REMOTE_SERVICE, {
      namespace: STRONGFLOW_OPERATOR_REMOTE_NAMESPACE,
    })
    this.invoker = invoker
  }

  @Remote
  async invoke(request: unknown, signal: AbortSignal): Promise<StrongFlowOperatorResponse> {
    let parsed: StrongFlowOperatorRequest
    try {
      parsed = parseStrongFlowOperatorRequest(request)
    } catch {
      const identity = boundaryIdentity(request)
      return materializeStrongFlowOperatorFailure({
        requestId: identity.requestId,
        operation: identity.operation,
        code: 'INVALID_REQUEST',
        message: 'StrongFlow 操作请求格式无效。',
      })
    }
    try {
      return parseStrongFlowOperatorResponse(
        await this.invoker.invoke(parsed, { signal }),
      )
    } catch {
      return materializeStrongFlowOperatorFailure({
        requestId: parsed.requestId,
        operation: parsed.operation,
        code: signal.aborted ? 'OPERATION_ABORTED' : 'INTERNAL_ERROR',
        message: signal.aborted
          ? 'StrongFlow 操作请求已中止。'
          : 'StrongFlow Remote 调用失败。',
      })
    }
  }
}
