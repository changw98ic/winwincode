// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './community-control-plane-client.js'
import { createQueryCacheLifecycle } from '@winwincode/browser-core/query-cache'
import type {
  Actor,
  CredentialReferenceProjection,
  DeliveryProjection,
  Instant,
  ModelRouteAvailabilityProjection,
  ProductSessionId,
  QueryRequest,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  RuntimeProjectionSnapshot,
  RuntimeSessionProjection,
  WorkerProjection,
} from './generated/contracts.js'
import {
  ModelRouteAvailabilityReason,
  ModelRouteAvailabilityStatus,
  QueryName,
  RuntimeActivityOutcome,
  RuntimeActivityStatus,
  RuntimeRecoveryState,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const LIST_PAGE_SIZE = 200
const MAX_PAGES = 10

/** A health fact older than this is presented as stale instead of current. */
export const USAGE_HEALTH_STALE_AFTER_MILLIS = 5 * 60 * 1000
/** Bounded number of ProductSessions whose runtime usage one read covers. */
export const USAGE_HEALTH_SESSION_LIMIT = 12

const CANONICAL_TOKEN_METRICS: readonly string[] = Object.freeze([
  'input_tokens',
  'cached_input_tokens',
  'output_tokens',
  'reasoning_output_tokens',
  'total_tokens',
])

export type UsageHealthStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type UsageHealthDimension = 'delivery' | 'work-run' | 'role' | 'model' | 'provider'

/**
 * Where a token total comes from. `unattributed` rows must present routing facts
 * only; the runtime publishes no per-Provider or per-Model token attribution.
 */
export type UsageAttribution = 'session' | 'unattributed'

export interface UsageMetricValue {
  readonly name: string
  readonly value: number
}

export interface UsageAggregate {
  readonly dimension: 'delivery' | 'work-run' | 'role'
  readonly attribution: 'session'
  readonly key: string
  readonly label: string
  readonly detail: string | null
  readonly sessionCount: number
  readonly inputTokens: number | null
  readonly cachedInputTokens: number | null
  readonly outputTokens: number | null
  readonly reasoningTokens: number | null
  readonly totalTokens: number | null
  readonly metrics: readonly UsageMetricValue[]
  readonly unknownMetrics: readonly string[]
  readonly tokensKnown: boolean
  /** Role rows share one WorkRun total across every Role that ran inside it. */
  readonly overlaps: boolean
  /** The runtime projection publishes no elapsed time per WorkRun or Role. */
  readonly durationMillis: null
  readonly durationKnown: false
  readonly asOf: Instant | null
  readonly asOfKnown: boolean
}

export interface ModelUsageRow {
  readonly dimension: 'model'
  readonly attribution: 'unattributed'
  readonly key: string
  readonly providerId: string
  readonly modelId: string
  readonly label: string
  readonly detail: string
  readonly isDefault: boolean
  readonly status: string
  readonly reason: string | null
  readonly tokensKnown: false
  readonly contextWindowTokens: number
  readonly asOf: null
  readonly asOfKnown: false
}

export type ProviderHealthState = 'ready' | 'disabled' | 'unavailable' | 'unknown'

export interface ProviderHealthRow {
  readonly dimension: 'provider'
  readonly attribution: 'unattributed'
  readonly key: string
  readonly providerId: string
  readonly label: string
  readonly state: ProviderHealthState
  readonly reason: string | null
  readonly reasonKnown: boolean
  readonly routeCount: number
  readonly readyRouteCount: number
  readonly isDefault: boolean
  readonly tokensKnown: false
  readonly asOf: null
  readonly asOfKnown: false
  readonly usageAttribution: 'unattributed'
}

export type WorkerHealthState =
  | 'online'
  | 'no-capacity'
  | 'draining'
  | 'offline'
  | 'heartbeat-stale'
  | 'heartbeat-unknown'

export interface WorkerHealthRow {
  readonly key: string
  readonly label: string
  readonly state: WorkerHealthState
  readonly capacity: number
  readonly lastHeartbeatAt: Instant | null
  readonly heartbeatAgeMillis: number | null
  readonly heartbeatKnown: boolean
  readonly asOf: Instant | null
  readonly asOfKnown: boolean
}

export type SessionTeamState = 'running' | 'recovering' | 'offline' | 'idle' | 'unknown'

export interface SessionTeamRow {
  readonly key: string
  readonly agentId: string | null
  readonly agentName: string | null
  readonly role: string | null
  readonly workerId: string | null
  readonly workerSessionId: string
  readonly codexThreadId: string
  readonly attempt: number
  readonly productSessionId: string
  readonly workRunId: string | null
  readonly provider: string | null
  readonly model: string | null
  readonly repositoryId: string | null
  readonly workspaceRevision: string | null
  readonly writeMode: string | null
  readonly currentActivity: string | null
  readonly recoveryMessage: string
  readonly recoveryRequiresHuman: boolean
  readonly lastFailureSourceRef: string | null
  readonly recoveryState: RuntimeSessionProjection['recovery']['state']
  readonly workerState: WorkerHealthState | null
  readonly state: SessionTeamState
}

export interface UsageCapacitySummary {
  readonly reportedCapacity: number
  readonly drainingCapacity: number
  readonly limit: number | null
  readonly sufficient: boolean | null
}

export interface CredentialHealthRow {
  readonly key: string
  readonly label: string
  readonly providerId: string
  readonly secretState: 'available' | 'missing' | 'revoked'
  readonly revokedAt: Instant | null
  readonly lastRotatedAt: Instant | null
  readonly rotationVersion: number
  readonly asOf: Instant | null
  readonly asOfKnown: boolean
}

export interface UsageHealthErrorRow {
  readonly key: string
  readonly origin: 'work-run' | 'delivery'
  readonly label: string
  readonly failureCount: number
  readonly attentionCount: number
  readonly recovered: boolean
  readonly sourceRef: string | null
  readonly asOf: Instant | null
}

export type UsageHealthSource =
  | 'delivery'
  | 'usage'
  | 'provider'
  | 'credential'
  | 'worker'
  | 'settings'

export type UsageHealthSourceState = 'ok' | 'unavailable'

export interface UsageHealthSourceFailure {
  readonly source: UsageHealthSource
  readonly code: string
}

export interface UsageHealthTimeWindow {
  readonly from: Instant | null
  readonly to: Instant | null
  readonly observedSessions: number
  readonly availableSessions: number
}

export interface UsageHealthViewModelState {
  readonly status: UsageHealthStatus
  readonly generatedAt: Instant | null
  readonly timeWindow: UsageHealthTimeWindow | null
  readonly truncated: boolean
  readonly byDelivery: readonly UsageAggregate[]
  readonly byWorkRun: readonly UsageAggregate[]
  readonly byRole: readonly UsageAggregate[]
  readonly byModel: readonly ModelUsageRow[]
  readonly byProvider: readonly ProviderHealthRow[]
  readonly sessionTeam: readonly SessionTeamRow[]
  readonly workers: readonly WorkerHealthRow[]
  readonly capacity: UsageCapacitySummary | null
  readonly credentials: readonly CredentialHealthRow[]
  readonly errors: readonly UsageHealthErrorRow[]
  readonly sources: Readonly<Record<UsageHealthSource, UsageHealthSourceState>>
  readonly unavailable: readonly UsageHealthSourceFailure[]
  readonly error: ControlPlaneClientError | null
}

export type UsageHealthListener = (state: UsageHealthViewModelState) => void

export interface UsageHealthViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
  readonly nowMillis?: () => number
  readonly staleAfterMillis?: number
  readonly sessionLimit?: number
}

export interface UsageHealthViewModel {
  readonly state: UsageHealthViewModelState
  subscribe(listener: UsageHealthListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  cancelPending(): void
  close(): void
}

interface SessionObservation {
  readonly productSessionId: ProductSessionId
  readonly deliveryId: string | null
  readonly workRunId: string | null
  readonly roles: readonly (string | null)[]
  readonly workerSessionId: string
  readonly codexThreadId: string
  readonly attempt: number
  readonly runtimeContext: RuntimeSessionProjection['runtimeContext']
  readonly activities: RuntimeSessionProjection['activities']
  readonly recoveryState: RuntimeSessionProjection['recovery']['state']
  readonly metrics: readonly UsageMetricValue[]
  readonly failureCount: number
  readonly sourceRef: string | null
  readonly recovered: boolean
  readonly asOf: Instant | null
}

function emptyState(): UsageHealthViewModelState {
  return Object.freeze({
    status: 'idle',
    generatedAt: null,
    timeWindow: null,
    truncated: false,
    byDelivery: Object.freeze([]),
    byWorkRun: Object.freeze([]),
    byRole: Object.freeze([]),
    byModel: Object.freeze([]),
    byProvider: Object.freeze([]),
    sessionTeam: Object.freeze([]),
    workers: Object.freeze([]),
    capacity: null,
    credentials: Object.freeze([]),
    errors: Object.freeze([]),
    sources: Object.freeze({
      delivery: 'unavailable',
      usage: 'unavailable',
      provider: 'unavailable',
      credential: 'unavailable',
      worker: 'unavailable',
      settings: 'unavailable',
    }),
    unavailable: Object.freeze([]),
    error: null,
  })
}

interface SourceOk<T> {
  readonly state: 'ok'
  readonly value: T
}

interface SourceUnavailable {
  readonly state: 'unavailable'
  readonly code: string
}

type SourceResult<T> = SourceOk<T> | SourceUnavailable

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
    message: '用量与健康状态读取已取消。',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure('USAGE_HEALTH_FAILURE', '用量与健康状态读取失败。', error)
}

function statusForError(error: ControlPlaneClientError): UsageHealthStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

function requireQuery<T extends QueryResultResponse>(
  response: QueryResultResponse,
  query: T['query'],
): T {
  if (response.query !== query) throw clientFailure(
    'USAGE_HEALTH_QUERY_MISMATCH',
    '控制平面返回了其他用量与健康状态查询结果。',
  )
  return response as T
}

function page(cursor: unknown): { readonly cursor: unknown; readonly limit: number } {
  return Object.freeze({ cursor, limit: LIST_PAGE_SIZE })
}

function cursorAfter(
  response: { readonly page: { readonly hasMore: boolean; readonly nextCursor: unknown } },
  seen: Set<unknown>,
): unknown {
  if (!response.page.hasMore) return null
  const next = response.page.nextCursor
  if (next === null || seen.has(next)) throw clientFailure(
    'USAGE_HEALTH_CURSOR_INVALID',
    '用量与健康状态读取返回了无效的续页游标。',
  )
  seen.add(next)
  return next
}

function instant(value: unknown): Instant | null {
  if (typeof value !== 'string' || !Number.isFinite(Date.parse(value))) return null
  return value
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : 0
}

/** Usage totals are published name-sorted; the summary keeps that canonical order. */
function metricsOf(session: RuntimeSessionProjection): readonly UsageMetricValue[] {
  return Object.freeze((session.usage?.totals ?? []).map(metric => Object.freeze({
    name: metric.name,
    value: count(metric.value),
  })))
}

function numberFor(metrics: ReadonlyMap<string, number>, name: string): number | null {
  const value = metrics.get(name)
  return value === undefined ? null : value
}

function sortedMetrics(metrics: ReadonlyMap<string, number>): readonly UsageMetricValue[] {
  return Object.freeze([...metrics.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([name, value]) => Object.freeze({ name, value })))
}

function aggregate(input: {
  readonly dimension: 'delivery' | 'work-run' | 'role'
  readonly key: string
  readonly label: string
  readonly detail: string | null
  readonly observations: readonly SessionObservation[]
  readonly overlaps: boolean
  readonly asOf: Instant | null
  readonly asOfKnown: boolean
}): UsageAggregate {
  const totals = new Map<string, number>()
  for (const observation of input.observations) {
    for (const metric of observation.metrics) {
      totals.set(metric.name, (totals.get(metric.name) ?? 0) + metric.value)
    }
  }
  const reportsTokens = input.observations.some(observation => observation.metrics.length > 0)
  const reportsKnownTokens = input.observations.some(observation => observation.metrics.some(
    metric => CANONICAL_TOKEN_METRICS.includes(metric.name),
  ))
  return Object.freeze({
    dimension: input.dimension,
    attribution: 'session' as const,
    key: input.key,
    label: input.label,
    detail: input.detail,
    sessionCount: input.observations.length,
    inputTokens: numberFor(totals, 'input_tokens'),
    cachedInputTokens: numberFor(totals, 'cached_input_tokens'),
    outputTokens: numberFor(totals, 'output_tokens'),
    reasoningTokens: numberFor(totals, 'reasoning_output_tokens'),
    totalTokens: numberFor(totals, 'total_tokens'),
    metrics: sortedMetrics(totals),
    unknownMetrics: Object.freeze(sortedMetrics(totals)
      .map(metric => metric.name)
      .filter(name => !CANONICAL_TOKEN_METRICS.includes(name))),
    tokensKnown: reportsTokens && reportsKnownTokens,
    overlaps: input.overlaps,
    durationMillis: null,
    durationKnown: false as const,
    asOf: input.asOf,
    asOfKnown: input.asOfKnown,
  })
}

function workerState(
  worker: WorkerProjection,
  nowMillis: number,
  staleAfterMillis: number,
): WorkerHealthState {
  if (worker.state === 'offline') return 'offline'
  if (worker.state === 'draining') return 'draining'
  const heartbeat = instant(worker.lastHeartbeatAt)
  if (heartbeat === null) return 'heartbeat-unknown'
  const age = nowMillis - Date.parse(heartbeat)
  if (!Number.isFinite(age) || age > staleAfterMillis) return 'heartbeat-stale'
  return worker.capacity > 0 ? 'online' : 'no-capacity'
}

const REASON_ORDER: readonly ModelRouteAvailabilityReason[] = Object.freeze([
  ModelRouteAvailabilityReason.CredentialMissingOrRevoked,
  ModelRouteAvailabilityReason.AuthenticationError,
  ModelRouteAvailabilityReason.WindowExhausted,
  ModelRouteAvailabilityReason.WeeklyExhausted,
  ModelRouteAvailabilityReason.RateLimited,
  ModelRouteAvailabilityReason.RuntimeStatusUnknown,
  ModelRouteAvailabilityReason.RequestPoolUnavailable,
  ModelRouteAvailabilityReason.ProviderOrModelDisabled,
  ModelRouteAvailabilityReason.DefaultRouteInvalid,
  ModelRouteAvailabilityReason.NoProvider,
  ModelRouteAvailabilityReason.Ready,
])

function blockingReason(routes: readonly ModelRouteAvailabilityProjection[]): string | null {
  const reasons = routes
    .map(route => route.reason)
    .filter((reason): reason is ModelRouteAvailabilityReason => reason !== null)
    .sort((left, right) => REASON_ORDER.indexOf(left) - REASON_ORDER.indexOf(right))
  return reasons[0] ?? null
}

function providerState(
  routes: readonly ModelRouteAvailabilityProjection[],
): { readonly state: ProviderHealthState; readonly reason: string | null } {
  if (routes.length === 0) return { state: 'unknown', reason: null }
  const ready = routes.find(route => route.status === ModelRouteAvailabilityStatus.Available
    && route.reason === ModelRouteAvailabilityReason.Ready)
  if (ready !== undefined) return { state: 'ready', reason: null }
  const blocking = routes.find(route => route.status === ModelRouteAvailabilityStatus.Disabled)
    ?? routes[0]
  return {
    state: blocking?.status === ModelRouteAvailabilityStatus.Disabled
      ? 'disabled'
      : 'unavailable',
    reason: blockingReason(routes),
  }
}

function isReadyRoute(route: ModelRouteAvailabilityProjection): boolean {
  return route.status === ModelRouteAvailabilityStatus.Available
    && route.reason === ModelRouteAvailabilityReason.Ready
}

function buildState(input: {
  readonly deliveries: readonly DeliveryProjection[]
  readonly sessions: readonly SessionObservation[]
  readonly observedSessions: number
  readonly availableSessions: number
  readonly routes: readonly ModelRouteAvailabilityProjection[]
  readonly isDefaultRoute: (route: ModelRouteAvailabilityProjection) => boolean
  readonly credentials: readonly CredentialReferenceProjection[]
  readonly workers: readonly WorkerProjection[]
  readonly settingsConcurrencyLimit: number | null
  readonly nowMillis: number
  readonly staleAfterMillis: number
  readonly generatedAt: Instant
  readonly sources: Readonly<Record<UsageHealthSource, UsageHealthSourceState>>
  readonly unavailable: readonly UsageHealthSourceFailure[]
}): UsageHealthViewModelState {
  const deliveryTitles = new Map<string, string>(
    input.deliveries.map(entry => [entry.deliveryId as string, entry.title]),
  )

  function groupBy(
    dimension: 'delivery' | 'work-run' | 'role',
    keyOf: (observation: SessionObservation) => readonly (readonly [string, string])[],
    overlaps: boolean,
  ): readonly UsageAggregate[] {
    const grouped = new Map<string, { readonly label: string; readonly rows: SessionObservation[] }>()
    for (const observation of input.sessions) {
      for (const [key, label] of keyOf(observation)) {
        const existing = grouped.get(key)
        grouped.set(key, existing === undefined
          ? { label, rows: [observation] }
          : { label: existing.label, rows: [...existing.rows, observation] })
      }
    }
    return Object.freeze([...grouped.entries()]
      .map(([key, group]) => aggregate({
        dimension,
        key,
        label: group.label,
        detail: null,
        observations: group.rows,
        overlaps,
        asOf: group.rows
          .map(row => row.asOf)
          .filter((value): value is Instant => value !== null)
          .sort()
          .at(-1) ?? null,
        asOfKnown: group.rows.some(row => row.asOf !== null),
      }))
      .sort((left, right) => left.key.localeCompare(right.key)))
  }

  const byDelivery = groupBy(
    'delivery',
    observation => [[
      observation.deliveryId ?? 'delivery-unassigned',
      observation.deliveryId === null
        ? '未报告交付'
        : deliveryTitles.get(observation.deliveryId) ?? observation.deliveryId,
    ] as const],
    false,
  )
  const byWorkRun = groupBy(
    'work-run',
    observation => [[
      observation.workRunId ?? `${observation.productSessionId}/unbound`,
      observation.workRunId ?? '未报告工作运行',
    ] as const],
    false,
  )
  const byRole = groupBy(
    'role',
    observation => observation.roles.map(role => [
      role ?? 'role-unknown',
      role ?? '未报告角色',
    ] as const),
    true,
  )

  const byProviderRoutes = new Map<string, ModelRouteAvailabilityProjection[]>()
  for (const route of input.routes) {
    const existing = byProviderRoutes.get(route.route.providerId)
    byProviderRoutes.set(
      route.route.providerId,
      existing === undefined ? [route] : [...existing, route],
    )
  }
  const byProvider = Object.freeze([...byProviderRoutes.entries()]
    .map(([providerId, routes]) => {
      const health = providerState(routes)
      return Object.freeze({
        dimension: 'provider' as const,
        attribution: 'unattributed' as const,
        key: providerId,
        providerId,
        label: routes[0]?.providerDisplayName ?? providerId,
        state: health.state,
        reason: health.reason ?? blockingReason(routes),
        reasonKnown: routes.length > 0,
        routeCount: routes.length,
        readyRouteCount: routes.filter(isReadyRoute).length,
        isDefault: routes.some(route => input.isDefaultRoute(route)),
        tokensKnown: false as const,
        asOf: null,
        asOfKnown: false as const,
        usageAttribution: 'unattributed' as const,
      })
    })
    .sort((left, right) => left.key.localeCompare(right.key)))

  const byModel = Object.freeze(input.routes
    .map(route => Object.freeze({
      dimension: 'model' as const,
      attribution: 'unattributed' as const,
      key: `${route.route.providerId}/${route.route.modelId}`,
      providerId: route.route.providerId,
      modelId: route.route.modelId,
      label: route.modelDisplayName,
      detail: route.providerDisplayName,
      isDefault: input.isDefaultRoute(route),
      status: route.status,
      reason: route.reason,
      tokensKnown: false as const,
      contextWindowTokens: route.contextWindowTokens,
      asOf: null,
      asOfKnown: false as const,
    }))
    .sort((left, right) => left.key.localeCompare(right.key)))

  const workers = Object.freeze(input.workers
    .map(worker => {
      const state = workerState(worker, input.nowMillis, input.staleAfterMillis)
      const heartbeat = instant(worker.lastHeartbeatAt)
      return Object.freeze({
        key: worker.id,
        label: worker.id,
        state,
        capacity: worker.capacity,
        lastHeartbeatAt: heartbeat,
        heartbeatAgeMillis: heartbeat === null ? null : input.nowMillis - Date.parse(heartbeat),
        heartbeatKnown: heartbeat !== null,
        asOf: heartbeat,
        asOfKnown: heartbeat !== null,
      })
    })
    .sort((left, right) => left.key.localeCompare(right.key)))

  const workersById = new Map(workers.map(worker => [worker.key, worker]))
  const sessionTeam = Object.freeze(input.sessions
    .map(observation => {
      const context = observation.runtimeContext
      const worker = context === null
        ? undefined
        : workersById.get(context.agentIdentity.workerId)
      const runningActivity = observation.activities.find(
        activity => activity.status === RuntimeActivityStatus.Running,
      )
      const recovering = observation.recoveryState === RuntimeRecoveryState.Required
        || observation.recoveryState === RuntimeRecoveryState.InProgress
      const recoveryRequiresHuman = observation.activities.some(
        activity => activity.outcome === RuntimeActivityOutcome.PolicyDenied,
      )
      const recoveryMessages: string[] = []
      if (recoveryRequiresHuman) {
        recoveryMessages.push('安全策略已暂停自动继续，需要人工检查')
      } else {
        if (observation.recoveryState === RuntimeRecoveryState.Required) {
          recoveryMessages.push('连接中断，等待自动恢复')
        } else if (observation.recoveryState === RuntimeRecoveryState.InProgress) {
          recoveryMessages.push('连接中断，正在自动恢复')
        } else if (observation.recoveryState === RuntimeRecoveryState.Recovered) {
          recoveryMessages.push('连接中断后自动恢复成功')
        }
        if (observation.attempt > 1) {
          recoveryMessages.push(`新 Session 已接手（attempt ${String(observation.attempt)}）`)
        }
        if (recoveryMessages.length === 0) recoveryMessages.push('无需恢复')
      }
      const state: SessionTeamState = worker?.state === 'offline'
        || worker?.state === 'heartbeat-stale'
        ? 'offline'
        : recovering
          ? 'recovering'
          : runningActivity !== undefined
            ? 'running'
            : context === null
              ? 'unknown'
              : 'idle'
      return Object.freeze({
        key: observation.workerSessionId,
        agentId: context?.agentIdentity.id ?? null,
        agentName: context?.agentIdentity.name ?? null,
        role: context?.agentIdentity.role ?? null,
        workerId: context?.agentIdentity.workerId ?? null,
        workerSessionId: observation.workerSessionId,
        codexThreadId: observation.codexThreadId,
        attempt: observation.attempt,
        productSessionId: observation.productSessionId,
        workRunId: observation.workRunId,
        provider: context?.provider ?? null,
        model: context?.model ?? null,
        repositoryId: context?.workspace.repositoryId ?? null,
        workspaceRevision: context?.workspace.revision ?? null,
        writeMode: context?.workspace.writeMode ?? null,
        currentActivity: runningActivity?.command ?? runningActivity?.activityType ?? null,
        recoveryMessage: recoveryMessages.join(' · '),
        recoveryRequiresHuman,
        lastFailureSourceRef: observation.sourceRef,
        recoveryState: observation.recoveryState,
        workerState: worker?.state ?? null,
        state,
      })
    })
    .sort((left, right) => left.key.localeCompare(right.key)))

  // Capacity derived from a Worker projection that could not be read must not be
  // reported as a shortage, so an unavailable Worker read yields no capacity claim.
  const enabledCapacity = input.workers
    .filter(worker => worker.state === 'enabled')
    .reduce((total, worker) => total + worker.capacity, 0)
  const capacity: UsageCapacitySummary | null = input.sources.worker !== 'ok'
    ? null
    : Object.freeze({
      reportedCapacity: enabledCapacity,
      drainingCapacity: input.workers
        .filter(worker => worker.state === 'draining')
        .reduce((total, worker) => total + worker.capacity, 0),
      limit: input.settingsConcurrencyLimit,
      sufficient: input.settingsConcurrencyLimit === null
        ? null
        : enabledCapacity >= input.settingsConcurrencyLimit,
    })

  const credentials = Object.freeze(input.credentials
    .map(reference => Object.freeze({
      key: reference.id,
      label: reference.displayName,
      providerId: reference.providerId,
      secretState: reference.secretState,
      revokedAt: instant(reference.revokedAt),
      lastRotatedAt: instant(reference.lastRotatedAt),
      rotationVersion: reference.rotationVersion,
      asOf: instant(reference.updatedAt),
      asOfKnown: instant(reference.updatedAt) !== null,
    }))
    .sort((left, right) => left.key.localeCompare(right.key)))

  const errors = Object.freeze([
    ...input.sessions
      .filter(observation => observation.failureCount > 0)
      .map(observation => Object.freeze({
        key: `${observation.deliveryId ?? 'unassigned'}/${observation.workRunId ?? 'unbound'}`,
        origin: 'work-run' as const,
        label: observation.workRunId ?? observation.productSessionId,
        failureCount: observation.failureCount,
        attentionCount: 0,
        recovered: observation.recovered,
        sourceRef: observation.sourceRef,
        asOf: observation.asOf,
      })),
    ...input.deliveries
      .filter(entry => entry.openAttentionCount > 0)
      .map(entry => Object.freeze({
        key: entry.deliveryId,
        origin: 'delivery' as const,
        label: entry.title,
        failureCount: 0,
        attentionCount: entry.openAttentionCount,
        recovered: false,
        sourceRef: null,
        asOf: instant(entry.updatedAt),
      })),
  ].sort((left, right) => left.key.localeCompare(right.key)))

  const observed = [
    ...input.deliveries.map(entry => instant(entry.updatedAt)),
    ...input.workers.map(worker => instant(worker.lastHeartbeatAt)),
    ...input.credentials.map(reference => instant(reference.updatedAt)),
    ...input.sessions.map(observation => observation.asOf),
  ].filter((value): value is Instant => value !== null).sort()
  const timeWindow: UsageHealthTimeWindow = Object.freeze({
    from: observed[0] ?? null,
    to: observed.at(-1) ?? null,
    observedSessions: input.observedSessions,
    availableSessions: input.availableSessions,
  })

  return Object.freeze({
    status: 'ready',
    generatedAt: input.generatedAt,
    timeWindow,
    truncated: input.observedSessions < input.availableSessions,
    byDelivery,
    byWorkRun,
    byRole,
    byModel,
    byProvider,
    sessionTeam,
    workers,
    capacity,
    credentials,
    errors,
    sources: input.sources,
    unavailable: input.unavailable,
    error: null,
  })
}

/** Read-only Usage, Provider routing, Credential and Worker health summary for one repository Scope. */
export function createUsageHealthViewModel(
  options: UsageHealthViewModelOptions,
): UsageHealthViewModel {
  const queryCache = createQueryCacheLifecycle(options)
  const listeners = new Set<UsageHealthListener>()
  const controllers = new Set<AbortController>()
  const nowMillis = options.nowMillis ?? Date.now
  const staleAfterMillis = options.staleAfterMillis ?? USAGE_HEALTH_STALE_AFTER_MILLIS
  const sessionLimit = options.sessionLimit ?? USAGE_HEALTH_SESSION_LIMIT
  let currentState = emptyState()
  let generation = 0
  let closed = false

  function publish(state: UsageHealthViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<UsageHealthViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function requireOpen(): void {
    if (closed) throw clientFailure('USAGE_HEALTH_CLOSED', '用量与健康状态摘要已关闭。')
  }

  function abortRequests(): void {
    for (const active of controllers) active.abort()
    controllers.clear()
  }

  function requestBase() {
    return {
      schemaVersion: SCHEMA_VERSION,
      actor: options.actor,
      scope: options.scope,
    }
  }

  async function collect<T>(
    build: (cursor: unknown) => QueryRequest,
    query: QueryRequest['query'],
    signal: AbortSignal,
  ): Promise<readonly T[]> {
    const items: unknown[] = []
    const seen = new Set<unknown>()
    let cursor: unknown = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response = await options.client.query(build(cursor), { signal })
      if (response.query !== query) throw clientFailure(
        'USAGE_HEALTH_QUERY_MISMATCH',
        '控制平面返回了其他用量与健康状态查询结果。',
      )
      const pageItems = (response as { readonly result?: { readonly items?: unknown } })
        .result?.items
      if (!Array.isArray(pageItems)) throw clientFailure(
        'USAGE_HEALTH_PROJECTION_INVALID',
        '用量与健康状态读取返回的页面没有列表项。',
      )
      items.push(...pageItems)
      const next = cursorAfter(response, seen)
      if (next === null) return Object.freeze(items) as readonly T[]
      cursor = next
    }
    throw clientFailure(
      'USAGE_HEALTH_PAGE_LIMIT_EXCEEDED',
      '用量与健康状态读取超过了分页上限。',
    )
  }

  function listRequest(
    query: QueryName.DeliveryList | QueryName.CredentialReferenceList | QueryName.WorkerList
      | QueryName.SessionList,
    parameters: Record<string, unknown>,
  ): (cursor: unknown) => QueryRequest {
    return cursor => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query,
      parameters,
      page: page(cursor),
    } as QueryRequest)
  }

  async function readRuntimeProjection(
    productSessionId: ProductSessionId,
    signal: AbortSignal,
  ): Promise<RuntimeProjectionSnapshot> {
    return requireQuery<RuntimeProjectionGetResult>(await options.client.query({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.RuntimeProjectionGet,
      parameters: { kind: 'product-session', productSessionId },
      page: page(null),
    } as QueryRequest, { signal }), QueryName.RuntimeProjectionGet).result
  }

  async function readRoutes(signal: AbortSignal) {
    const result = requireQuery<ModelRouteAvailabilityListResult>(await options.client.query({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.ModelRouteAvailabilityList,
      parameters: {},
      page: page(null),
    } as QueryRequest, { signal }), QueryName.ModelRouteAvailabilityList).result
    return {
      routes: result.items,
      isDefault: (route: ModelRouteAvailabilityProjection) => route.isDefault
        || (result.defaultProviderId === route.route.providerId
          && result.defaultModelId === route.route.modelId),
    }
  }

  async function readConcurrencyLimit(signal: AbortSignal): Promise<number | null> {
    const result = requireQuery<SettingsGetResult>(await options.client.query({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.SettingsGet,
      parameters: {},
      page: page(null),
    } as QueryRequest, { signal }), QueryName.SettingsGet).result
    return result.workerConcurrencyLimit
  }

  function observationsOf(snapshot: RuntimeProjectionSnapshot): readonly SessionObservation[] {
    return Object.freeze(snapshot.sessions.map(session => {
      const roles = new Set(session.agents.map(agent => agent.role))
      if (session.runtimeContext !== null) roles.add(session.runtimeContext.agentIdentity.role)
      return Object.freeze({
        productSessionId: snapshot.productSessionId,
        deliveryId: snapshot.deliveryId,
        workRunId: session.workRunId,
        roles: Object.freeze([...roles]),
        workerSessionId: session.workerSessionId,
        codexThreadId: session.codexThreadId,
        attempt: session.attempt,
        runtimeContext: session.runtimeContext,
        activities: session.activities,
        recoveryState: session.recovery.state,
        metrics: metricsOf(session),
        failureCount: session.recovery.failureCount,
        sourceRef: session.recovery.lastFailureSourceRef,
        recovered: session.recovery.state === RuntimeRecoveryState.Recovered
          || session.recovery.state === RuntimeRecoveryState.InProgress,
        asOf: instant(snapshot.rebuiltAt),
      })
    }))
  }

  /**
   * One projection that cannot be read marks only its own section. A health
   * summary must stay readable when a single Server projection is unavailable,
   * and it must name that fact instead of showing an empty list.
   */
  async function settle<T>(
    source: UsageHealthSource,
    read: () => Promise<T>,
    signal: AbortSignal,
  ): Promise<SourceResult<T>> {
    try {
      return { state: 'ok', value: await read() }
    } catch (error) {
      if (signal.aborted === true) throw error
      if (error instanceof ControlPlaneClientError && (
        error.kind === 'cancelled'
          || error.kind === 'authentication'
          || error.kind === 'authorization'
      )) throw error
      return {
        state: 'unavailable',
        code: error instanceof ControlPlaneClientError
          ? error.code
          : 'USAGE_HEALTH_FAILURE',
      }
    }
  }

  /** Resolves to the runtime usage observations, or the code of the read that failed. */
  async function usageProjections(
    selected: readonly ProductSessionSummary[],
    signal: AbortSignal,
  ): Promise<{ readonly code: string | null
    readonly observations: readonly SessionObservation[] }> {
    const outcomes = await Promise.all(selected.map(sessionProjection => settle(
      'usage',
      () => readRuntimeProjection(sessionProjection.id, signal),
      signal,
    )))
    const observations: SessionObservation[] = []
    for (const outcome of outcomes) {
      if (outcome.state === 'unavailable') return { code: outcome.code, observations: [] }
      observations.push(...observationsOf(outcome.value))
    }
    return { code: null, observations }
  }

  async function snapshot(signal: AbortSignal): Promise<UsageHealthViewModelState> {
    const clock = nowMillis()
    const [delivery, worker, credential, session, provider, settings] = await Promise.all([
      settle('delivery', () => collect<DeliveryProjection>(
        listRequest(QueryName.DeliveryList, { states: [] }),
        QueryName.DeliveryList,
        signal,
      ), signal),
      settle('worker', () => collect<WorkerProjection>(
        listRequest(QueryName.WorkerList, { states: ['enabled', 'draining', 'offline'] }),
        QueryName.WorkerList,
        signal,
      ), signal),
      settle('credential', () => collect<CredentialReferenceProjection>(
        listRequest(QueryName.CredentialReferenceList, { providerId: null }),
        QueryName.CredentialReferenceList,
        signal,
      ), signal),
      settle('usage', () => collect<ProductSessionSummary>(
        listRequest(QueryName.SessionList, { states: [] }),
        QueryName.SessionList,
        signal,
      ), signal),
      settle('provider', () => readRoutes(signal), signal),
      settle('settings', () => readConcurrencyLimit(signal), signal),
    ])
    const results: Readonly<Record<UsageHealthSource, SourceResult<unknown>>> = Object.freeze({
      delivery,
      worker,
      credential,
      usage: session,
      provider,
      settings,
    })
    const firstFailure = Object.values(results).find(result => result.state === 'unavailable')
    if (Object.values(results).every(result => result.state === 'unavailable')) {
      throw clientFailure(
        firstFailure === undefined ? 'USAGE_HEALTH_FAILURE' : (
          (firstFailure as SourceUnavailable).code
        ),
        '所有用量与健康状态投影均不可用。',
      )
    }

    const deliveryItems = delivery.state === 'ok' ? delivery.value : []
    const workerItems = worker.state === 'ok' ? worker.value : []
    const credentialItems = credential.state === 'ok' ? credential.value : []
    const sessionItems = session.state === 'ok' ? session.value : []
    const routes = provider.state === 'ok'
      ? provider.value
      : { routes: [], isDefault: () => false }
    const concurrencyLimit = settings.state === 'ok' ? settings.value : null

    // The runtime usage read belongs to the same source as the session list, so a
    // failure there marks the usage sections unavailable instead of failing the read.
    const scopedSessions = sessionItems
      .filter(sessionProjection => sessionProjection.projectId === options.scope.projectId
        && sessionProjection.repositoryId === options.scope.repositoryId)
      .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt))
    const selected = scopedSessions.slice(0, sessionLimit)
    const usage = session.state === 'unavailable'
      ? { code: session.code as string | null, observations: [] as const }
      : await usageProjections(selected, signal)
    const sourceStates = Object.fromEntries(Object.entries(results).map(
      ([source, result]) => [source, result.state === 'ok' ? 'ok' : 'unavailable'],
    )) as Record<UsageHealthSource, UsageHealthSourceState>
    if (usage.code !== null) sourceStates.usage = 'unavailable'
    const sources = Object.freeze(sourceStates)
    const unavailable = Object.freeze(Object.entries(sourceStates)
      .filter(([, stateName]) => stateName === 'unavailable')
      .map(([source]) => Object.freeze({
        source: source as UsageHealthSource,
        code: source === 'usage' && usage.code !== null
          ? usage.code
          : (results[source as UsageHealthSource] as SourceUnavailable).code,
      })))

    return buildState({
      deliveries: deliveryItems,
      sessions: usage.observations,
      observedSessions: selected.length,
      availableSessions: scopedSessions.length,
      routes: routes.routes,
      isDefaultRoute: routes.isDefault,
      credentials: credentialItems,
      workers: workerItems,
      settingsConcurrencyLimit: concurrencyLimit,
      nowMillis: clock,
      staleAfterMillis,
      generatedAt: new Date(clock).toISOString(),
      sources,
      unavailable,
    })
  }

  async function load(replace: boolean): Promise<void> {
    requireOpen()
    generation += 1
    const ownGeneration = generation
    abortRequests()
    const active = new AbortController()
    controllers.add(active)
    patch({ status: replace ? 'loading' : 'refreshing', error: null })
    try {
      const next = await snapshot(active.signal)
      if (closed || generation !== ownGeneration) return
      publish(next)
    } catch (error) {
      if (closed || generation !== ownGeneration) return
      const normalized = normalizedError(error, active.signal)
      if (normalized.kind === 'authentication' || normalized.kind === 'authorization') {
        patch({
          ...emptyState(),
          status: statusForError(normalized),
          error: normalized,
        })
        return
      }
      // Previously published facts stay on screen next to the failure so an
      // operator always knows which numbers are the last ones the Server gave.
      patch({ status: statusForError(normalized), error: normalized })
    } finally {
      controllers.delete(active)
    }
  }

  return {
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      await load(true)
    },
    async refresh() {
      requireOpen()
      if (currentState.status === 'authentication-required'
        || currentState.status === 'authorization-denied') return
      queryCache.refresh([
        QueryName.DeliveryList,
        QueryName.WorkerList,
        QueryName.CredentialReferenceList,
        QueryName.SessionList,
        QueryName.RuntimeProjectionGet,
        QueryName.ModelRouteAvailabilityList,
        QueryName.SettingsGet,
      ])
      await load(false)
    },
    cancelPending() {
      if (closed) return
      generation += 1
      abortRequests()
      patch({
        status: 'cancelled',
        error: new ControlPlaneClientError({
          kind: 'cancelled',
          code: 'REQUEST_CANCELLED',
          message: '用量与健康状态读取已取消。',
          requestId: null,
          retryable: false,
        }),
      })
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      abortRequests()
      queryCache.close()
      publish({ ...emptyState(), status: 'closed' })
      listeners.clear()
    },
  }
}

type RuntimeProjectionGetResult = Extract<
  QueryResultResponse,
  { readonly query: 'runtime.projection.get' }
>
type ModelRouteAvailabilityListResult = Extract<
  QueryResultResponse,
  { readonly query: 'model.route.availability.list' }
>
type SettingsGetResult = Extract<QueryResultResponse, { readonly query: 'settings.get' }>

interface ProductSessionSummary {
  readonly id: ProductSessionId
  readonly projectId: string
  readonly repositoryId: string
  readonly updatedAt: Instant
}
