// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './community-control-plane-client.js'
import type {
  Actor,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  DeliveryAdvanceCommand,
  DeliveryAdvanceCompletedResponse,
  DeliveryCreateCommand,
  DeliveryCreateCompletedResponse,
  DeliveryId,
  DeliveryProjection,
  ProductSessionId,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'
import { CommandName } from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const

export interface ChatDeliveryCreateInput {
  readonly title: string
  readonly goal: string
  readonly baseRevision: string
  readonly scope: readonly string[]
  readonly outOfScope: readonly string[]
  readonly constraints: readonly string[]
  readonly sourceProductSessionId: ProductSessionId | null
  readonly acceptanceCriteria: readonly string[]
}

export interface ChatDeliveryCreatorState {
  readonly status: 'idle' | 'submitting' | 'waiting' | 'created' | 'error' | 'closed'
  readonly error: ControlPlaneClientError | null
}

/**
 * Structural composition seam; Chat does not import a delivery workbench
 * model.  The creator turns one confirmed Chat draft into the first Delivery
 * through the canonical DeliveryCreate + DeliveryAdvance command pair.
 */
export interface ChatDeliveryCreator {
  readonly state: ChatDeliveryCreatorState
  subscribe(listener: (state: ChatDeliveryCreatorState) => void): () => void
  create(input: ChatDeliveryCreateInput): Promise<void>
  cancelPending(): void
  close(): void
}

export interface ChatDeliveryCreatorOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextDeliveryId: () => DeliveryId
  readonly nextRequestId: () => RequestId
  readonly onCreated: (deliveryId: DeliveryId) => void
}

function clientFailure(code: string, message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
}

function createdDeliveryMatchesScope(
  delivery: DeliveryProjection,
  deliveryId: DeliveryId,
  scope: RepositoryScope,
): boolean {
  return delivery.deliveryId === deliveryId
    && delivery.ownership.organizationId === scope.organizationId
    && delivery.ownership.workspaceId === scope.workspaceId
    && delivery.ownership.projectId === scope.projectId
    && delivery.ownership.repositoryId === scope.repositoryId
}

function expectCompletedCommand(
  response: CommandAcceptedResponse | CommandCompletedResponse,
  command: CommandName.DeliveryCreate | CommandName.DeliveryAdvance,
  requestId: RequestId,
): DeliveryCreateCompletedResponse | DeliveryAdvanceCompletedResponse | null {
  if (response.requestId !== requestId || response.command !== command) throw clientFailure(
    'STRONGFLOW_CREATE_COMMAND_MISMATCH',
    'The Control Plane returned another Delivery command result.',
  )
  if (response.outcome === 'accepted') return null
  if (command === CommandName.DeliveryCreate) {
    return response as DeliveryCreateCompletedResponse
  }
  return response as DeliveryAdvanceCompletedResponse
}

function normalizedError(error: unknown, signal?: AbortSignal): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (signal?.aborted === true) return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The Delivery creation request was cancelled.',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure(
    'STRONGFLOW_CREATE_FAILURE',
    'The Delivery creation could not be completed.',
    error,
  )
}

/** Create the first Delivery through canonical commands on behalf of one Chat session. */
export function createChatDeliveryCreator(
  options: ChatDeliveryCreatorOptions,
): ChatDeliveryCreator {
  const listeners = new Set<(state: ChatDeliveryCreatorState) => void>()
  let currentState: ChatDeliveryCreatorState = Object.freeze({ status: 'idle', error: null })
  let active: AbortController | null = null
  let closed = false
  let attempt: {
    readonly inputKey: string
    readonly createRequest: DeliveryCreateCommand
    created: DeliveryProjection | null
    advanceRequest: DeliveryAdvanceCommand | null
  } | null = null

  function publish(status: ChatDeliveryCreatorState['status'], error: ControlPlaneClientError | null): void {
    currentState = Object.freeze({ status, error })
    for (const listener of listeners) listener(currentState)
  }

  function invalid(code: string, message: string): void {
    publish('error', clientFailure(code, message))
  }

  function completedDelivery(
    response: CommandAcceptedResponse | CommandCompletedResponse,
    command: CommandName.DeliveryCreate | CommandName.DeliveryAdvance,
    requestId: RequestId,
    deliveryId: DeliveryId,
  ): DeliveryProjection | null {
    const completed = expectCompletedCommand(response, command, requestId)
    if (completed === null) return null
    if (
      !createdDeliveryMatchesScope(completed.result, deliveryId, options.scope)
      || completed.result.revision !== completed.currentRevision
    ) throw clientFailure(
      'STRONGFLOW_CREATE_RESPONSE_MISMATCH',
      'The Delivery command returned another repository revision.',
    )
    return completed.result
  }

  async function create(input: ChatDeliveryCreateInput): Promise<void> {
    if (closed) throw clientFailure(
      'STRONGFLOW_CREATE_VIEW_MODEL_CLOSED',
      'The Delivery creation view-model is closed.',
    )
    if (currentState.status === 'submitting' || currentState.status === 'waiting') {
      return
    }
    const title = input.title.trim()
    const goal = input.goal.trim()
    const baseRevision = input.baseRevision.trim()
    const deliveryScope = [...new Set(input.scope.map(value => value.trim()).filter(Boolean))]
    const outOfScope = [...new Set(input.outOfScope.map(value => value.trim()).filter(Boolean))]
    const constraints = [...new Set(input.constraints.map(value => value.trim()).filter(Boolean))]
    const acceptanceCriteria = input.acceptanceCriteria
      .map(value => value.trim())
      .filter(value => value.length > 0)
    if (title.length === 0) {
      invalid('STRONGFLOW_CREATE_TITLE_REQUIRED', 'Enter a title for the new Delivery.')
      return
    }
    if (goal.length === 0) {
      invalid('STRONGFLOW_CREATE_GOAL_REQUIRED', 'Enter the Delivery goal.')
      return
    }
    if (baseRevision.length === 0) {
      invalid('STRONGFLOW_CREATE_BASE_REVISION_REQUIRED', 'Enter the repository baseline revision.')
      return
    }
    if (deliveryScope.length === 0) {
      invalid('STRONGFLOW_CREATE_SCOPE_REQUIRED', 'Enter at least one in-scope result.')
      return
    }
    if (acceptanceCriteria.length === 0) {
      invalid(
        'STRONGFLOW_CREATE_ACCEPTANCE_REQUIRED',
        'Enter at least one initial acceptance criterion.',
      )
      return
    }
    const inputKey = JSON.stringify({
      title,
      goal,
      baseRevision,
      deliveryScope,
      outOfScope,
      constraints,
      sourceProductSessionId: input.sourceProductSessionId,
      acceptanceCriteria,
    })
    if (attempt !== null && attempt.inputKey !== inputKey) {
      invalid(
        'STRONGFLOW_CREATE_DRAFT_CHANGED_AFTER_SUBMIT',
        'Retry the submitted Delivery draft before starting another conversion.',
      )
      return
    }
    if (currentState.status === 'created') return
    if (attempt === null) {
      const deliveryId = options.nextDeliveryId()
      attempt = {
        inputKey,
        createRequest: {
          schemaVersion: SCHEMA_VERSION,
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          command: CommandName.DeliveryCreate,
          expectedRevision: 0,
          payload: {
            deliveryId,
            spec: {
              acceptanceCriteria: acceptanceCriteria.map((criterion, index) => ({
                id: `criterion:${String(index + 1)}`,
                required: true,
                title: criterion,
              })),
              baseRevision,
              constraints,
              goal,
              outOfScope,
              publicationTarget: null,
              repositoryId: options.scope.repositoryId,
              scope: deliveryScope,
              sourceProductSessionId: input.sourceProductSessionId,
              title,
            },
            tasks: [],
          },
        },
        created: null,
        advanceRequest: null,
      }
    }
    const currentAttempt = attempt
    const deliveryId = currentAttempt.createRequest.payload.deliveryId
    active?.abort()
    const controller = new AbortController()
    active = controller
    publish('submitting', null)
    try {
      if (currentAttempt.created === null) {
        const createResponse = await options.client.command(
          currentAttempt.createRequest,
          { signal: controller.signal },
        )
        if (closed || active !== controller) return
        const created = completedDelivery(
          createResponse,
          CommandName.DeliveryCreate,
          currentAttempt.createRequest.requestId,
          deliveryId,
        )
        if (created === null) {
          publish('waiting', null)
          return
        }
        currentAttempt.created = created
      }
      currentAttempt.advanceRequest ??= {
        schemaVersion: SCHEMA_VERSION,
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        command: CommandName.DeliveryAdvance,
        expectedRevision: currentAttempt.created.revision,
        payload: { deliveryId },
      }
      const advanceRequest = currentAttempt.advanceRequest
      const advanceResponse = await options.client.command(
        advanceRequest,
        { signal: controller.signal },
      )
      if (closed || active !== controller) return
      const advanced = completedDelivery(
        advanceResponse,
        CommandName.DeliveryAdvance,
        advanceRequest.requestId,
        deliveryId,
      )
      if (advanced === null) {
        publish('waiting', null)
        return
      }
      if (advanced.activeStageRunId === null) throw clientFailure(
        'STRONGFLOW_CREATE_STAGE_REQUIRED',
        'The new Delivery did not expose its executable stage.',
      )
      publish('created', null)
      options.onCreated(deliveryId)
    } catch (error) {
      if (closed || active !== controller) return
      publish('error', normalizedError(error, controller.signal))
    } finally {
      if (active === controller) active = null
    }
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async create(input) {
      await create(input)
    },
    cancelPending() {
      if (closed) return
      if (active === null) {
        if (currentState.status === 'waiting') publish('error', new ControlPlaneClientError({
          kind: 'cancelled',
          code: 'REQUEST_CANCELLED',
          message: 'Delivery creation was cancelled locally.',
          requestId: null,
          retryable: false,
        }))
        return
      }
      const controller = active
      active = null
      controller.abort()
      publish('error', new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'Delivery creation was cancelled locally.',
        requestId: null,
        retryable: false,
      }))
    },
    close() {
      if (closed) return
      closed = true
      active?.abort()
      active = null
      publish('closed', null)
      listeners.clear()
    },
  }
}
