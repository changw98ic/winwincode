import {
  materializeStrongFlowDeliveryFailure,
  materializeStrongFlowDeliverySuccess,
  parseStrongFlowDeliveryRequest,
  type StrongFlowDeliveryErrorCode,
  type StrongFlowDeliveryInvoker,
  type StrongFlowDeliveryOperation,
  type StrongFlowDeliveryRequest,
  type StrongFlowDeliveryRequestFor,
  type StrongFlowDeliveryResponseFor,
} from '@winwincode/contracts'

import {
  StrongFlowService,
  StrongFlowServiceError,
} from './delivery-service.js'
import { containsRawCredentialMaterial } from './credential-boundary.js'

function failure<Operation extends StrongFlowDeliveryOperation>(
  request: StrongFlowDeliveryRequestFor<Operation>,
  code: StrongFlowDeliveryErrorCode,
  message: string,
  currentRevision: number | null = null,
): StrongFlowDeliveryResponseFor<Operation> {
  return materializeStrongFlowDeliveryFailure({
    requestId: request.requestId,
    operation: request.operation,
    code,
    message,
    currentRevision,
  })
}

async function dispatch(
  service: StrongFlowService,
  request: StrongFlowDeliveryRequest,
): Promise<{
  readonly delivery: Awaited<ReturnType<StrongFlowService['createDelivery']>>
  readonly diagramExecution: Awaited<
    ReturnType<StrongFlowService['getDeliveryProjection']>
  >['diagramExecution']
  readonly runtimeExecution: Awaited<
    ReturnType<StrongFlowService['getDeliveryProjection']>
  >['runtimeExecution']
}> {
  switch (request.operation) {
    case 'createDelivery':
      return Object.freeze({ delivery: await service.createDelivery({
        requestId: request.requestId,
        ...request.payload,
      }), diagramExecution: null, runtimeExecution: null })
    case 'updateDeliverySpec':
      return Object.freeze({ delivery: await service.updateDeliverySpec({
        requestId: request.requestId,
        ...request.payload,
      }), diagramExecution: null, runtimeExecution: null })
    case 'startStage':
      return Object.freeze({ delivery: await service.startStage({
        requestId: request.requestId,
        ...request.payload,
      }), diagramExecution: null, runtimeExecution: null })
    case 'bindSession':
      return Object.freeze({ delivery: await service.bindSession({
        requestId: request.requestId,
        ...request.payload,
      }), diagramExecution: null, runtimeExecution: null })
    case 'resolveAttention':
      return Object.freeze({ delivery: await service.resolveAttention({
        requestId: request.requestId,
        ...request.payload,
      }), diagramExecution: null, runtimeExecution: null })
    case 'submitVerdict':
      return Object.freeze({ delivery: await service.submitVerdict({
        requestId: request.requestId,
        ...request.payload,
      }), diagramExecution: null, runtimeExecution: null })
    case 'getDeliveryProjection':
      return service.getDeliveryProjection(request.payload.deliveryId)
  }
}

/** The only request adapter between DSH/CLI transports and StrongFlowService. */
export class StrongFlowServiceInvoker implements StrongFlowDeliveryInvoker {
  readonly #service: StrongFlowService

  constructor(service: StrongFlowService) {
    this.#service = service
  }

  async invoke<Operation extends StrongFlowDeliveryOperation>(
    requestValue: StrongFlowDeliveryRequestFor<Operation>,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<StrongFlowDeliveryResponseFor<Operation>> {
    let request: StrongFlowDeliveryRequestFor<Operation>
    try {
      request = parseStrongFlowDeliveryRequest(requestValue) as (
        StrongFlowDeliveryRequestFor<Operation>
      )
    } catch {
      return materializeStrongFlowDeliveryFailure({
        requestId: null,
        operation: null,
        code: 'INVALID_REQUEST',
        message: 'StrongFlow Delivery request is invalid.',
      })
    }
    if (options.signal?.aborted === true) {
      return failure(request, 'OPERATION_ABORTED', 'StrongFlow Delivery request was aborted.')
    }
    try {
      const projection = await dispatch(this.#service, request as StrongFlowDeliveryRequest)
      if (containsRawCredentialMaterial(projection)) {
        return failure(
          request,
          'STORE_FAILURE',
          'StrongFlow Delivery projection contains prohibited credential material.',
        )
      }
      return materializeStrongFlowDeliverySuccess(
        request,
        projection.delivery,
        projection.diagramExecution,
        projection.runtimeExecution,
      )
    } catch (error) {
      if (error instanceof StrongFlowServiceError) {
        return failure(request, error.code, error.message, error.currentRevision)
      }
      return failure(request, 'INTERNAL_ERROR', 'StrongFlow Delivery service failed.')
    }
  }
}
