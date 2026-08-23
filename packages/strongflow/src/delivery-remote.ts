import type { Context } from '@deepseek-ai/cordis'
import {
  Remote,
  TypertRemoteService,
} from '@deepseek-ai/dsh-typert-protocol'
import {
  STRONGFLOW_DELIVERY_OPERATIONS,
  materializeStrongFlowDeliveryRequest,
  materializeStrongFlowDeliveryFailure,
  parseStrongFlowDeliveryRequest,
  parseStrongFlowDeliveryResponseForRequest,
  type StrongFlowDeliveryInvoker,
  type StrongFlowDeliveryOperation,
  type StrongFlowDeliveryRequest,
  type StrongFlowDeliveryResponse,
} from '@winwincode/contracts'

import {
  STRONGFLOW_DELIVERY_REMOTE_NAMESPACE,
  STRONGFLOW_DELIVERY_REMOTE_SERVICE,
  STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
} from './delivery-remote-client.js'

export interface StrongFlowDeliveryRemoteServiceOptions {
  readonly localSessionProof: string
}

const DSH_SESSION_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u

function sessionProof(value: string): string {
  if (typeof value !== 'string' || value.length < 16 || value.length > 8_192) {
    throw new TypeError('localSessionProof must contain between 16 and 8192 characters')
  }
  return value
}

function sessionIdentity(value: unknown): string | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null
  const id = Reflect.get(value, 'id')
  return typeof id === 'string' && DSH_SESSION_ID_PATTERN.test(id) ? id : null
}

function boundaryIdentity(value: unknown): {
  readonly requestId: string | null
  readonly operation: StrongFlowDeliveryOperation | null
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
    && STRONGFLOW_DELIVERY_OPERATIONS.includes(
      input.operation as StrongFlowDeliveryOperation,
    )
    ? input.operation as StrongFlowDeliveryOperation
    : null
  return Object.freeze({ requestId, operation })
}

/** Host-side DSH Remote for the one canonical Delivery service path. */
export class StrongFlowDeliveryRemoteService extends TypertRemoteService {
  private readonly invoker: StrongFlowDeliveryInvoker
  private readonly localSessionProof: string

  constructor(
    ctx: Context,
    invoker: StrongFlowDeliveryInvoker,
    options: StrongFlowDeliveryRemoteServiceOptions,
  ) {
    super(ctx, STRONGFLOW_DELIVERY_REMOTE_SERVICE, {
      namespace: STRONGFLOW_DELIVERY_REMOTE_NAMESPACE,
    })
    this.invoker = invoker
    this.localSessionProof = sessionProof(options.localSessionProof)
  }

  @Remote
  async invoke(
    agent: unknown,
    request: unknown,
    signal: AbortSignal,
  ): Promise<StrongFlowDeliveryResponse> {
    const dshSessionId = sessionIdentity(agent)
    let parsed: StrongFlowDeliveryRequest
    try {
      parsed = parseStrongFlowDeliveryRequest(request)
    } catch {
      const identity = boundaryIdentity(request)
      return materializeStrongFlowDeliveryFailure({
        requestId: identity.requestId,
        operation: identity.operation,
        code: 'INVALID_REQUEST',
        message: 'StrongFlow Delivery 请求格式无效。',
      })
    }
    if (dshSessionId === null) {
      return materializeStrongFlowDeliveryFailure({
        requestId: parsed.requestId,
        operation: parsed.operation,
        code: 'AUTHENTICATION_FAILED',
        message: 'StrongFlow Delivery 请求没有绑定有效的 DSH Session。',
      })
    }
    try {
      if (parsed.operation === 'resolveAttention'
        && parsed.payload.channel === 'local-ui') {
        const resolutionRequest = parsed
        const resolutionPayload = resolutionRequest.payload
        if (resolutionPayload.authentication.scheme !== 'local-session'
          || resolutionPayload.authentication.proof !== STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE) {
          return materializeStrongFlowDeliveryFailure({
            requestId: parsed.requestId,
            operation: parsed.operation,
            code: 'AUTHENTICATION_FAILED',
            message: 'StrongFlow 人工决定没有使用当前 DSH Session 身份。',
          })
        }
        const projectionRequest = materializeStrongFlowDeliveryRequest(
          'getDeliveryProjection',
          `${resolutionRequest.requestId.slice(0, 470)}:review-session`,
          { deliveryId: resolutionPayload.deliveryId },
        )
        const projectionResponse = await this.invoker.invoke(projectionRequest, { signal })
        if (!projectionResponse.ok) {
          return materializeStrongFlowDeliveryFailure({
            requestId: parsed.requestId,
            operation: parsed.operation,
            code: projectionResponse.error.code,
            message: projectionResponse.error.message,
            currentRevision: projectionResponse.error.currentRevision,
          })
        }
        const delivery = projectionResponse.result.delivery
        if (delivery.revision !== resolutionPayload.expectedRevision) {
          return materializeStrongFlowDeliveryFailure({
            requestId: parsed.requestId,
            operation: parsed.operation,
            code: 'REVISION_CONFLICT',
            message: 'StrongFlow 审核页面已过期，请读取当前 Delivery 后重新决定。',
            currentRevision: delivery.revision,
          })
        }
        const item = delivery.attentionItems.find(entry => (
          entry.id === resolutionPayload.attentionItemId
          && entry.status === 'open'
          && entry.stageRunId !== null
        ))
        const ownsReviewSession = item !== undefined && delivery.sessionBindings.some(binding => (
          binding.stageRunId === item.stageRunId
          && binding.dshSessionId === dshSessionId
          && binding.codexSessionId === null
        ))
        if (!ownsReviewSession) {
          return materializeStrongFlowDeliveryFailure({
            requestId: parsed.requestId,
            operation: parsed.operation,
            code: 'AUTHENTICATION_FAILED',
            message: '当前 DSH Session 不是这项人工审核的绑定 Session。',
            currentRevision: delivery.revision,
          })
        }
        parsed = materializeStrongFlowDeliveryRequest(
          'resolveAttention',
          resolutionRequest.requestId,
          {
            ...resolutionPayload,
            authentication: {
              scheme: 'local-session',
              proof: this.localSessionProof,
            },
          },
        )
      }
      return parseStrongFlowDeliveryResponseForRequest(
        parsed,
        await this.invoker.invoke(parsed, { signal }),
      )
    } catch {
      return materializeStrongFlowDeliveryFailure({
        requestId: parsed.requestId,
        operation: parsed.operation,
        code: signal.aborted ? 'OPERATION_ABORTED' : 'INTERNAL_ERROR',
        message: signal.aborted
          ? 'StrongFlow Delivery 请求已中止。'
          : 'StrongFlow Delivery Remote 调用失败。',
      })
    }
  }
}
