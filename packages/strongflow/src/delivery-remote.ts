import type { Context } from '@deepseek-ai/cordis'
import {
  Remote,
  TypertRemoteService,
} from '@deepseek-ai/dsh-typert-protocol'
import {
  STRONGFLOW_DELIVERY_OPERATIONS,
  materializeStrongFlowDeliveryAdvanceFailure,
  materializeStrongFlowDeliveryAdvanceSuccess,
  materializeStrongFlowDeliveryRequest,
  materializeStrongFlowDeliveryFailure,
  parseStrongFlowDeliveryAdvanceRequest,
  parseStrongFlowDeliveryRequest,
  parseStrongFlowDeliveryResponseForRequest,
  type StrongFlowDeliveryAdvanceRequest,
  type StrongFlowDeliveryAdvanceResponse,
  type StrongFlowDeliveryInvoker,
  type StrongFlowDeliveryOperation,
  type StrongFlowDeliveryRequest,
  type StrongFlowDeliveryResponse,
} from '@winwincode/contracts'

import {
  StrongFlowDeliveryStageCoordinatorError,
  type StrongFlowAdvanceCaller,
  type StrongFlowDeliveryAdvanceResult,
} from './delivery-stage-coordinator.js'

export interface StrongFlowDeliveryAdvanceInvoker {
  advance(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    options?: { readonly signal?: AbortSignal },
  ): Promise<StrongFlowDeliveryAdvanceResult>
}

import {
  STRONGFLOW_DELIVERY_REMOTE_NAMESPACE,
  STRONGFLOW_DELIVERY_REMOTE_SERVICE,
  STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
} from './delivery-remote-client.js'

export interface StrongFlowDeliveryRemoteServiceOptions {
  readonly localSessionProof: string
  readonly coordinator: StrongFlowDeliveryAdvanceInvoker
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

function advanceCaller(value: unknown): StrongFlowAdvanceCaller | null {
  const dshSessionId = sessionIdentity(value)
  if (dshSessionId === null || typeof value !== 'object' || value === null) return null
  const options = Reflect.get(value, 'options') as unknown
  if (typeof options !== 'object' || options === null || Array.isArray(options)) return null
  const provider = Reflect.get(options, 'provider')
  const model = Reflect.get(options, 'model')
  const maxTokens = Reflect.get(options, 'maxTokens')
  if (typeof provider !== 'string'
    || provider.length === 0
    || typeof model !== 'string'
    || model.length === 0
    || (maxTokens !== undefined
      && (!Number.isSafeInteger(maxTokens) || Number(maxTokens) < 1))) return null
  return Object.freeze({
    dshSessionId,
    modelRoute: Object.freeze({
      provider,
      model,
      ...(maxTokens === undefined ? {} : { maxTokens: Number(maxTokens) }),
    }),
  })
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
  private readonly coordinator: StrongFlowDeliveryAdvanceInvoker

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
    if (typeof options.coordinator?.advance !== 'function') {
      throw new TypeError('StrongFlow Delivery Remote requires a stage coordinator')
    }
    this.coordinator = options.coordinator
  }

  @Remote
  async advance(
    agent: unknown,
    request: unknown,
    signal: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResponse> {
    let parsed: StrongFlowDeliveryAdvanceRequest
    try {
      parsed = parseStrongFlowDeliveryAdvanceRequest(request)
    } catch {
      const requestId = typeof request === 'object'
        && request !== null
        && typeof Reflect.get(request, 'requestId') === 'string'
        ? Reflect.get(request, 'requestId') as string
        : null
      return materializeStrongFlowDeliveryAdvanceFailure({
        requestId: requestId !== null && DSH_SESSION_ID_PATTERN.test(requestId)
          ? requestId
          : null,
        code: 'INVALID_REQUEST',
        message: 'StrongFlow 阶段推进请求格式无效。',
      })
    }
    const caller = advanceCaller(agent)
    if (caller === null) {
      return materializeStrongFlowDeliveryAdvanceFailure({
        requestId: parsed.requestId,
        code: 'MODEL_SELECTION_REQUIRED',
        message: '请先在当前 DSH Session 选择 Provider 和模型。',
      })
    }
    try {
      const result = await this.coordinator.advance(parsed, caller, { signal })
      return materializeStrongFlowDeliveryAdvanceSuccess(
        parsed,
        result.delivery,
        result.outcome,
      )
    } catch (error) {
      if (error instanceof StrongFlowDeliveryStageCoordinatorError) {
        return materializeStrongFlowDeliveryAdvanceFailure({
          requestId: parsed.requestId,
          code: error.code,
          message: error.message,
          currentRevision: error.currentRevision,
        })
      }
      return materializeStrongFlowDeliveryAdvanceFailure({
        requestId: parsed.requestId,
        code: signal.aborted ? 'OPERATION_ABORTED' : 'INTERNAL_ERROR',
        message: signal.aborted
          ? 'StrongFlow 阶段推进已中止。'
          : 'StrongFlow 阶段推进失败。',
      })
    }
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
