// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import { invalidateClientQueryCache } from './core/query-cache.js'
import { ControlPlaneWebSocketEventType } from './generated/contracts.js'
import type {
  Actor,
  CommandRequest,
  ControlPlaneWebSocketSubscriptionId,
  OpaqueCursor,
  QueryRequest,
  QueryResultResponse,
  RequestId,
  Revision,
  Scope,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const PAGE_SIZE = 100
const MAX_PAGES_PER_AREA = 20

export const ENTERPRISE_MANAGEMENT_AREAS = Object.freeze([
  'organization',
  'members',
  'projects',
  'policy',
  'fleet',
  'usage',
  'audit',
  'integration',
] as const)

export type EnterpriseManagementArea = typeof ENTERPRISE_MANAGEMENT_AREAS[number]

export type EnterpriseManagementStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'partial'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type EnterpriseManagementRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type EnterpriseManagementAreaStatus =
  | 'idle'
  | 'loading'
  | 'refreshing'
  | 'ready'
  | 'permission-denied'
  | 'revision-conflict'
  | 'cancelled'
  | 'error'
  | 'closed'

export type EnterpriseManagementPermission = 'unknown' | 'allowed' | 'denied'

export interface EnterpriseManagementAreaState {
  readonly area: EnterpriseManagementArea
  readonly status: EnterpriseManagementAreaStatus
  readonly permission: EnterpriseManagementPermission
  /** Complete bounded generated responses; pages narrow them by their query discriminator. */
  readonly pages: readonly QueryResultResponse[]
  readonly revision: Revision | null
  readonly error: ControlPlaneClientError | null
}

export type EnterpriseManagementInteractionStatus =
  | 'idle'
  | 'submitting'
  | 'waiting'
  | 'revision-conflict'
  | 'error'

export interface EnterpriseManagementInteractionState {
  readonly status: EnterpriseManagementInteractionStatus
  readonly area: EnterpriseManagementArea | null
  readonly command: CommandRequest['command'] | null
  readonly error: ControlPlaneClientError | null
}

export interface EnterpriseManagementViewModelState {
  readonly status: EnterpriseManagementStatus
  readonly realtime: EnterpriseManagementRealtimeStatus
  readonly areas: Readonly<Record<EnterpriseManagementArea, EnterpriseManagementAreaState>>
  readonly interaction: EnterpriseManagementInteractionState
  readonly error: ControlPlaneClientError | null
}

export interface EnterpriseManagementQueryContext {
  readonly actor: Actor
  readonly scope: Scope
  readonly requestId: RequestId
  readonly cursor: OpaqueCursor | null
  readonly limit: number
}

/**
 * One generated-query projection source. Domain pages provide these adapters after their
 * canonical generated query types exist; this module owns only loading and state transitions.
 */
export interface EnterpriseManagementSource {
  readonly area: EnterpriseManagementArea
  readonly eventTypes: readonly ControlPlaneWebSocketEventType[]
  query(context: EnterpriseManagementQueryContext): QueryRequest
  revision(response: QueryResultResponse): Revision | null
}

export interface EnterpriseManagementCommandContext {
  readonly actor: Actor
  readonly scope: Scope
  readonly requestId: RequestId
  readonly expectedRevision: Revision
}

export interface EnterpriseManagementViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: Scope
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  /** Override only for deterministic contract fakes; production uses canonical generated sources. */
  readonly sources?: readonly EnterpriseManagementSource[]
}

export type EnterpriseManagementListener = (state: EnterpriseManagementViewModelState) => void

export interface EnterpriseManagementViewModel {
  readonly state: EnterpriseManagementViewModelState
  subscribe(listener: EnterpriseManagementListener): () => void
  start(): Promise<void>
  refresh(area?: EnterpriseManagementArea): Promise<void>
  execute(
    area: EnterpriseManagementArea,
    command: (context: EnterpriseManagementCommandContext) => CommandRequest,
  ): Promise<void>
  cancelPending(): void
  reconnect(): void
  close(): void
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

function normalizedError(error: unknown, signal?: AbortSignal): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (signal?.aborted === true) return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The enterprise management request was cancelled.',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure(
    'ENTERPRISE_MANAGEMENT_FAILURE',
    'Enterprise management data could not be updated.',
    error,
  )
}

function isRevisionConflict(error: ControlPlaneClientError): boolean {
  return error.code === 'REVISION_CONFLICT'
}

function isAuthorizationFailure(error: ControlPlaneClientError): boolean {
  return error.kind === 'authorization' || error.code === 'PERMISSION_DENIED'
}

function areaState(
  area: EnterpriseManagementArea,
  status: EnterpriseManagementAreaStatus = 'idle',
  permission: EnterpriseManagementPermission = 'unknown',
  pages: readonly QueryResultResponse[] = Object.freeze([]),
  revision: Revision | null = null,
  error: ControlPlaneClientError | null = null,
): EnterpriseManagementAreaState {
  return Object.freeze({ area, status, permission, pages, revision, error })
}

function initialAreas(): Readonly<Record<EnterpriseManagementArea, EnterpriseManagementAreaState>> {
  return Object.freeze({
    organization: areaState('organization'),
    members: areaState('members'),
    projects: areaState('projects'),
    policy: areaState('policy'),
    fleet: areaState('fleet'),
    usage: areaState('usage'),
    audit: areaState('audit'),
    integration: areaState('integration'),
  })
}

function interaction(
  status: EnterpriseManagementInteractionStatus = 'idle',
  area: EnterpriseManagementArea | null = null,
  command: CommandRequest['command'] | null = null,
  error: ControlPlaneClientError | null = null,
): EnterpriseManagementInteractionState {
  return Object.freeze({ status, area, command, error })
}

function initialState(): EnterpriseManagementViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    areas: initialAreas(),
    interaction: interaction(),
    error: null,
  })
}

function sameActor(left: Actor, right: Actor): boolean {
  return left.kind === right.kind && left.id === right.id
}

function sameScope(left: Scope, right: Scope): boolean {
  if (left.kind !== right.kind || left.organizationId !== right.organizationId) return false
  if (left.kind === 'organization' && right.kind === 'organization') return true
  if (left.kind === 'workspace' && right.kind === 'workspace') {
    return left.workspaceId === right.workspaceId
  }
  if (left.kind === 'project' && right.kind === 'project') {
    return left.workspaceId === right.workspaceId && left.projectId === right.projectId
  }
  return left.kind === 'repository' && right.kind === 'repository'
    && left.workspaceId === right.workspaceId
    && left.projectId === right.projectId
    && left.repositoryId === right.repositoryId
}

function validateSources(
  sources: readonly EnterpriseManagementSource[],
): ReadonlyMap<EnterpriseManagementArea, EnterpriseManagementSource> {
  const byArea = new Map<EnterpriseManagementArea, EnterpriseManagementSource>()
  for (const source of sources) {
    if (!ENTERPRISE_MANAGEMENT_AREAS.includes(source.area)) throw clientFailure(
      'ENTERPRISE_MANAGEMENT_AREA_INVALID',
      'The enterprise management source names an unknown area.',
    )
    if (byArea.has(source.area)) throw clientFailure(
      'ENTERPRISE_MANAGEMENT_AREA_DUPLICATE',
      'Each enterprise management area must have exactly one source.',
    )
    if (source.eventTypes.length === 0 || new Set(source.eventTypes).size !== source.eventTypes.length) {
      throw clientFailure(
        'ENTERPRISE_MANAGEMENT_EVENTS_INVALID',
        'Each enterprise management source must name unique generated event types.',
      )
    }
    byArea.set(source.area, source)
  }
  if (byArea.size !== ENTERPRISE_MANAGEMENT_AREAS.length) throw clientFailure(
    'ENTERPRISE_MANAGEMENT_AREA_MISSING',
    'Every enterprise management area requires one generated query source.',
  )
  return byArea
}

function enterpriseQuery(
  area: EnterpriseManagementArea,
  context: EnterpriseManagementQueryContext,
): QueryRequest {
  const envelope = {
    schemaVersion: SCHEMA_VERSION,
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    page: { cursor: context.cursor, limit: context.limit },
  }
  switch (area) {
    case 'organization':
      return { ...envelope, query: 'enterprise.organization.list', parameters: { states: [] } }
    case 'members':
      return {
        ...envelope,
        query: 'enterprise.membership.list',
        parameters: { states: [], teamIds: [], roleIds: [] },
      }
    case 'projects':
      return {
        ...envelope,
        query: 'enterprise.project.list',
        parameters: { states: [], includeRepositories: true },
      }
    case 'policy':
      return {
        ...envelope,
        query: 'enterprise.policy.list',
        parameters: { states: [], policyKinds: [] },
      }
    case 'fleet':
      return { ...envelope, query: 'enterprise.fleet.list', parameters: { states: [] } }
    case 'usage':
      return {
        ...envelope,
        query: 'enterprise.usage.list',
        parameters: { fromInclusive: null, toExclusive: null, sourceKinds: [] },
      }
    case 'audit':
      return {
        ...envelope,
        query: 'enterprise.audit.list',
        parameters: { fromInclusive: null, toExclusive: null, categories: [] },
      }
    case 'integration':
      return {
        ...envelope,
        query: 'enterprise.integration.list',
        parameters: { kinds: [], states: [] },
      }
  }
}

function enterpriseSnapshotRevision(response: QueryResultResponse): Revision | null {
  switch (response.query) {
    case 'enterprise.organization.list':
    case 'enterprise.membership.list':
    case 'enterprise.project.list':
    case 'enterprise.policy.list':
    case 'enterprise.fleet.list':
    case 'enterprise.usage.list':
    case 'enterprise.audit.list':
    case 'enterprise.integration.list':
      return response.result.snapshotRevision
    default:
      throw clientFailure(
        'ENTERPRISE_MANAGEMENT_RESPONSE_MISMATCH',
        'The enterprise management source returned a non-enterprise projection.',
      )
  }
}

const ENTERPRISE_EVENT_TYPES = Object.freeze({
  organization: ControlPlaneWebSocketEventType.EnterpriseOrganizationInvalidatedV1,
  members: ControlPlaneWebSocketEventType.EnterpriseMembershipInvalidatedV1,
  projects: ControlPlaneWebSocketEventType.EnterpriseProjectInvalidatedV1,
  policy: ControlPlaneWebSocketEventType.EnterprisePolicyInvalidatedV1,
  fleet: ControlPlaneWebSocketEventType.EnterpriseFleetInvalidatedV1,
  usage: ControlPlaneWebSocketEventType.EnterpriseUsageInvalidatedV1,
  audit: ControlPlaneWebSocketEventType.EnterpriseAuditInvalidatedV1,
  integration: ControlPlaneWebSocketEventType.EnterpriseIntegrationInvalidatedV1,
} satisfies Readonly<Record<EnterpriseManagementArea, ControlPlaneWebSocketEventType>>)

/** Canonical generated query and invalidation adapters used by production enterprise pages. */
export function createEnterpriseManagementSources(): readonly EnterpriseManagementSource[] {
  return Object.freeze(ENTERPRISE_MANAGEMENT_AREAS.map(area => Object.freeze({
    area,
    eventTypes: Object.freeze([ENTERPRISE_EVENT_TYPES[area]]),
    query: (context: EnterpriseManagementQueryContext) => enterpriseQuery(area, context),
    revision: enterpriseSnapshotRevision,
  })))
}

function aggregateStatus(
  areas: Readonly<Record<EnterpriseManagementArea, EnterpriseManagementAreaState>>,
): EnterpriseManagementStatus {
  const values = ENTERPRISE_MANAGEMENT_AREAS.map(area => areas[area])
  if (values.every(value => value.status === 'ready')) return 'ready'
  if (values.every(value => value.status === 'permission-denied')) return 'authorization-denied'
  if (values.every(value => value.status === 'cancelled')) return 'cancelled'
  if (values.some(value => value.status === 'ready')) return 'partial'
  if (values.some(value => value.status === 'loading' || value.status === 'refreshing')) {
    return 'loading'
  }
  return 'error'
}

function nextCursor(
  response: QueryResultResponse,
  seen: Set<OpaqueCursor>,
): OpaqueCursor | null {
  if (!response.page.hasMore) {
    if (response.page.nextCursor !== null) throw clientFailure(
      'ENTERPRISE_MANAGEMENT_PAGE_INVALID',
      'A final enterprise management page returned a continuation cursor.',
    )
    return null
  }
  const next = response.page.nextCursor
  if (next === null || seen.has(next)) throw clientFailure(
    'ENTERPRISE_MANAGEMENT_CURSOR_INVALID',
    'An enterprise management page returned an invalid continuation cursor.',
  )
  seen.add(next)
  return next
}

function validateQuery(
  query: QueryRequest,
  context: EnterpriseManagementQueryContext,
  actor: Actor,
  scope: Scope,
): void {
  if (
    query.schemaVersion !== SCHEMA_VERSION
    || query.requestId !== context.requestId
    || query.page.cursor !== context.cursor
    || query.page.limit !== context.limit
    || !sameActor(query.actor, actor)
    || !sameScope(query.scope, scope)
  ) throw clientFailure(
    'ENTERPRISE_MANAGEMENT_QUERY_INVALID',
    'The enterprise management source returned a query for another request or scope.',
  )
}

/** Coordinate every enterprise area through the one generated Control Plane facade. */
export function createEnterpriseManagementViewModel(
  options: EnterpriseManagementViewModelOptions,
): EnterpriseManagementViewModel {
  const configuredSources = options.sources ?? createEnterpriseManagementSources()
  const sources = validateSources(configuredSources)
  const listeners = new Set<EnterpriseManagementListener>()
  const controllers = new Map<EnterpriseManagementArea, AbortController>()
  const generations = new Map(ENTERPRISE_MANAGEMENT_AREAS.map(area => [area, 0]))
  let currentState = initialState()
  let subscription: ControlPlaneSubscription | null = null
  let realtimeGeneration = 0
  let closed = false

  function publish(state: EnterpriseManagementViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<EnterpriseManagementViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function patchArea(area: EnterpriseManagementArea, value: EnterpriseManagementAreaState): void {
    const areas = Object.freeze({ ...currentState.areas, [area]: value })
    publish({ ...currentState, areas, status: aggregateStatus(areas) })
  }

  function begin(area: EnterpriseManagementArea): {
    readonly controller: AbortController
    readonly generation: number
  } {
    controllers.get(area)?.abort()
    const controller = new AbortController()
    controllers.set(area, controller)
    const generation = (generations.get(area) ?? 0) + 1
    generations.set(area, generation)
    return { controller, generation }
  }

  function isCurrent(area: EnterpriseManagementArea, generation: number): boolean {
    return !closed && generations.get(area) === generation
  }

  function release(area: EnterpriseManagementArea, controller: AbortController): void {
    if (controllers.get(area) === controller) controllers.delete(area)
  }

  async function loadArea(
    area: EnterpriseManagementArea,
    replace: boolean,
    minimumRevision: Revision | null = null,
  ): Promise<void> {
    if (closed) throw clientFailure(
      'ENTERPRISE_MANAGEMENT_CLOSED',
      'The enterprise management view is closed.',
    )
    const source = sources.get(area)
    if (source === undefined) throw clientFailure(
      'ENTERPRISE_MANAGEMENT_AREA_MISSING',
      'The enterprise management area has no query source.',
    )
    const { controller, generation } = begin(area)
    const previous = currentState.areas[area]
    patchArea(area, areaState(
      area,
      replace ? 'loading' : 'refreshing',
      previous.permission,
      replace ? Object.freeze([]) : previous.pages,
      replace ? null : previous.revision,
    ))
    try {
      const pages: QueryResultResponse[] = []
      const seen = new Set<OpaqueCursor>()
      let cursor: OpaqueCursor | null = null
      let revision: Revision | null = null
      for (let index = 0; index < MAX_PAGES_PER_AREA; index += 1) {
        const requestId = options.nextRequestId()
        const context: EnterpriseManagementQueryContext = Object.freeze({
          actor: options.actor,
          scope: options.scope,
          requestId,
          cursor,
          limit: PAGE_SIZE,
        })
        const request = source.query(context)
        validateQuery(request, context, options.actor, options.scope)
        const response = await options.client.query(request, { signal: controller.signal })
        if (response.requestId !== requestId || response.query !== request.query) throw clientFailure(
          'ENTERPRISE_MANAGEMENT_RESPONSE_MISMATCH',
          'The Control Plane returned another enterprise management query result.',
        )
        const pageRevision = source.revision(response)
        if (pageRevision !== null && (!Number.isSafeInteger(pageRevision) || pageRevision < 0)) {
          throw clientFailure(
            'ENTERPRISE_MANAGEMENT_REVISION_INVALID',
            'The enterprise management source returned an invalid revision.',
          )
        }
        if (revision !== null && pageRevision !== null && pageRevision !== revision) throw clientFailure(
          'ENTERPRISE_MANAGEMENT_SNAPSHOT_MIXED',
          'Enterprise management pages did not come from one revision.',
        )
        revision ??= pageRevision
        pages.push(response)
        cursor = nextCursor(response, seen)
        if (cursor === null) {
          if (!isCurrent(area, generation)) return
          if (
            minimumRevision !== null
            && (revision === null || revision < minimumRevision)
          ) throw clientFailure(
            'ENTERPRISE_MANAGEMENT_SNAPSHOT_STALE',
            'The enterprise management projection has not reached the completed command revision.',
          )
          patchArea(area, areaState(
            area,
            'ready',
            'allowed',
            Object.freeze(pages),
            revision,
          ))
          return
        }
      }
      throw clientFailure(
        'ENTERPRISE_MANAGEMENT_PAGE_LIMIT_EXCEEDED',
        'The enterprise management area exceeded its bounded page limit.',
      )
    } catch (error) {
      if (!isCurrent(area, generation)) return
      const normalized = normalizedError(error, controller.signal)
      if (normalized.kind === 'authentication') {
        revokeAccess(normalized)
        return
      }
      const status: EnterpriseManagementAreaStatus = isAuthorizationFailure(normalized)
        ? 'permission-denied'
        : isRevisionConflict(normalized)
          ? 'revision-conflict'
          : normalized.kind === 'cancelled'
            ? 'cancelled'
            : 'error'
      patchArea(area, areaState(
        area,
        status,
        isAuthorizationFailure(normalized) ? 'denied' : previous.permission,
        status === 'permission-denied' ? Object.freeze([]) : previous.pages,
        status === 'permission-denied' ? null : previous.revision,
        normalized,
      ))
    } finally {
      release(area, controller)
    }
  }

  async function loadAreas(
    areas: readonly EnterpriseManagementArea[],
    replace: boolean,
  ): Promise<void> {
    await Promise.all(areas.map(async area => loadArea(area, replace)))
  }

  function revokeAccess(error: ControlPlaneClientError): void {
    realtimeGeneration += 1
    for (const controller of controllers.values()) controller.abort()
    controllers.clear()
    for (const area of ENTERPRISE_MANAGEMENT_AREAS) {
      generations.set(area, (generations.get(area) ?? 0) + 1)
    }
    subscription?.close()
    subscription = null
    const denied = Object.fromEntries(ENTERPRISE_MANAGEMENT_AREAS.map(area => [
      area,
      areaState(area, 'permission-denied', 'denied', Object.freeze([]), null, error),
    ])) as Record<EnterpriseManagementArea, EnterpriseManagementAreaState>
    publish({
      status: error.kind === 'authentication'
        ? 'authentication-required'
        : 'authorization-denied',
      realtime: 'access-revoked',
      areas: Object.freeze(denied),
      interaction: interaction('error', null, null, error),
      error,
    })
  }

  function subscribeRealtime(): void {
    realtimeGeneration += 1
    subscription?.close()
    const eventTypes = Object.freeze([...new Set(
      configuredSources.flatMap(source => source.eventTypes),
    )])
    subscription = options.client.subscribe({
      subscriptionId: options.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'scope' },
        eventTypes,
      },
      async onEvent(frame) {
        const affected = configuredSources
          .filter(source => source.eventTypes.some(type => type === frame.event.type as string))
          .map(source => source.area)
        if (affected.length === 0 || closed) return
        const generation = realtimeGeneration + 1
        realtimeGeneration = generation
        patch({ realtime: 'reloading', error: null })
        await loadAreas(affected, false)
        if (
          !closed
          && realtimeGeneration === generation
          && currentState.realtime === 'reloading'
        ) {
          patch({ realtime: 'subscribed' })
        }
      },
      onAuthorizationRevoked() {
        revokeAccess(new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'Enterprise management event authorization is no longer valid.',
          requestId: null,
          retryable: false,
        }))
      },
      onError(error) {
        if (closed) return
        realtimeGeneration += 1
        if (error.kind === 'authentication' || isAuthorizationFailure(error)) {
          revokeAccess(error)
          return
        }
        patch({ realtime: 'reconnecting', error })
      },
    })
    patch({ realtime: 'subscribed' })
  }

  return {
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      if (closed) throw clientFailure(
        'ENTERPRISE_MANAGEMENT_CLOSED',
        'The enterprise management view is closed.',
      )
      patch({ status: 'loading', error: null })
      await loadAreas(ENTERPRISE_MANAGEMENT_AREAS, true)
      if (!closed && currentState.status !== 'authentication-required') subscribeRealtime()
    },
    async refresh(area) {
      invalidateClientQueryCache(options.client, {
        actor: options.actor,
        scope: options.scope,
        reason: 'manual',
      })
      const areas = area === undefined ? ENTERPRISE_MANAGEMENT_AREAS : [area]
      await loadAreas(areas, false)
      if (!closed && subscription === null && currentState.status !== 'authentication-required') {
        subscribeRealtime()
      }
    },
    async execute(area, buildCommand) {
      if (closed) throw clientFailure(
        'ENTERPRISE_MANAGEMENT_CLOSED',
        'The enterprise management view is closed.',
      )
      const areaSnapshot = currentState.areas[area]
      if (areaSnapshot.permission !== 'allowed') throw clientFailure(
        'ENTERPRISE_MANAGEMENT_PERMISSION_REQUIRED',
        'The current identity is not allowed to change this enterprise management area.',
      )
      if (areaSnapshot.revision === null) throw clientFailure(
        'ENTERPRISE_MANAGEMENT_REVISION_REQUIRED',
        'Refresh this enterprise management area before changing it.',
      )
      const { controller, generation } = begin(area)
      const requestId = options.nextRequestId()
      const request = buildCommand(Object.freeze({
        actor: options.actor,
        scope: options.scope,
        requestId,
        expectedRevision: areaSnapshot.revision,
      }))
      if (
        request.schemaVersion !== SCHEMA_VERSION
        || request.requestId !== requestId
        || request.expectedRevision !== areaSnapshot.revision
        || !sameActor(request.actor, options.actor)
        || !sameScope(request.scope, options.scope)
      ) {
        release(area, controller)
        throw clientFailure(
          'ENTERPRISE_MANAGEMENT_COMMAND_INVALID',
          'The enterprise management command targets another request, revision, or scope.',
        )
      }
      patch({ interaction: interaction('submitting', area, request.command), error: null })
      try {
        const response = await options.client.command(request, { signal: controller.signal })
        if (!isCurrent(area, generation)) return
        if (response.requestId !== requestId || response.command !== request.command) {
          throw clientFailure(
            'ENTERPRISE_MANAGEMENT_COMMAND_MISMATCH',
            'The Control Plane returned another enterprise management command result.',
          )
        }
        if (response.outcome === 'accepted') {
          patch({ interaction: interaction('waiting', area, request.command) })
          return
        }
        if (
          response.previousRevision !== areaSnapshot.revision
          || response.currentRevision < response.previousRevision
        ) throw clientFailure(
          'ENTERPRISE_MANAGEMENT_COMMAND_MISMATCH',
          'The enterprise management command result has another revision.',
        )
        patch({ interaction: interaction(), error: null })
        release(area, controller)
        await loadArea(area, false, response.currentRevision)
      } catch (error) {
        if (!isCurrent(area, generation)) return
        const normalized = normalizedError(error, controller.signal)
        if (normalized.kind === 'authentication') {
          revokeAccess(normalized)
          return
        }
        if (isAuthorizationFailure(normalized)) {
          patchArea(area, areaState(area, 'permission-denied', 'denied', Object.freeze([]), null, normalized))
          patch({ interaction: interaction('error', area, request.command, normalized) })
          return
        }
        if (isRevisionConflict(normalized)) {
          patchArea(area, areaState(
            area,
            'revision-conflict',
            areaSnapshot.permission,
            areaSnapshot.pages,
            areaSnapshot.revision,
            normalized,
          ))
          patch({
            interaction: interaction('revision-conflict', area, request.command, normalized),
            error: normalized,
          })
          return
        }
        patch({
          interaction: interaction('error', area, request.command, normalized),
          error: normalized,
        })
      } finally {
        release(area, controller)
      }
    },
    cancelPending() {
      if (closed) return
      for (const controller of controllers.values()) controller.abort()
      controllers.clear()
      for (const area of ENTERPRISE_MANAGEMENT_AREAS) {
        generations.set(area, (generations.get(area) ?? 0) + 1)
      }
      const error = new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'The enterprise management request was cancelled.',
        requestId: null,
        retryable: false,
      })
      const areas = Object.fromEntries(ENTERPRISE_MANAGEMENT_AREAS.map(area => [
        area,
        areaState(
          area,
          'cancelled',
          currentState.areas[area].permission,
          currentState.areas[area].pages,
          currentState.areas[area].revision,
          error,
        ),
      ])) as Record<EnterpriseManagementArea, EnterpriseManagementAreaState>
      publish({
        status: 'cancelled',
        realtime: subscription === null ? 'inactive' : currentState.realtime,
        areas: Object.freeze(areas),
        interaction: interaction('error', null, null, error),
        error,
      })
    },
    reconnect() {
      if (closed) throw clientFailure(
        'ENTERPRISE_MANAGEMENT_CLOSED',
        'The enterprise management view is closed.',
      )
      if (subscription === null) throw clientFailure(
        'ENTERPRISE_MANAGEMENT_SUBSCRIPTION_INACTIVE',
        'Enterprise management events are not active.',
      )
      realtimeGeneration += 1
      patch({ realtime: 'reconnecting', error: null })
      subscription.reconnect()
      const ownGeneration = realtimeGeneration
      void loadAreas(ENTERPRISE_MANAGEMENT_AREAS, false).then(() => {
        if (
          !closed
          && realtimeGeneration === ownGeneration
          && currentState.realtime === 'reconnecting'
        ) patch({ realtime: 'subscribed' })
      })
    },
    close() {
      if (closed) return
      closed = true
      invalidateClientQueryCache(options.client, {
        actor: options.actor,
        scope: options.scope,
        reason: 'manual',
        discard: true,
      })
      realtimeGeneration += 1
      for (const controller of controllers.values()) controller.abort()
      controllers.clear()
      subscription?.close()
      subscription = null
      const areas = Object.fromEntries(ENTERPRISE_MANAGEMENT_AREAS.map(area => [
        area,
        areaState(area, 'closed'),
      ])) as Record<EnterpriseManagementArea, EnterpriseManagementAreaState>
      publish({
        status: 'closed',
        realtime: 'closed',
        areas: Object.freeze(areas),
        interaction: interaction(),
        error: null,
      })
      listeners.clear()
    },
  }
}
