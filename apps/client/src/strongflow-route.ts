// SPDX-License-Identifier: Apache-2.0

import { scopeHash, type ScopeRouteSelection } from './core/scope-context.js'
import type { CandidateDiffViewMode } from './strongflow-diff-model.js'
import type { StrongFlowArtifactsTab } from './strongflow-layout-preferences.js'
import type {
  DeliveryDetailProjection,
  DeliveryId,
  DeliveryTaskId,
  ProductSessionId,
  RepositoryScope,
  StageRunId,
} from './generated/contracts.js'
import type { StrongFlowHistorySelection } from './strongflow-history-selection.js'

const CONTRACT_ID = '[0-9A-HJKMNP-TV-Z]{26}'
const DELIVERY_ID = new RegExp(`^dlv_${CONTRACT_ID}$`)
const PRODUCT_SESSION_ID = new RegExp(`^psn_${CONTRACT_ID}$`)
const STAGE_RUN_ID = new RegExp(`^run_${CONTRACT_ID}$`)
const DELIVERY_TASK_ID = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/
const CANDIDATE_REF = /^git-candidate:sha256:[0-9a-f]{64}$/
const KNOWN_PARAMETERS = Object.freeze([
  'delivery',
  'session',
  'stageRun',
  'task',
  'run',
  'candidate',
  'panel',
  'file',
  'view',
  'line',
])
const SCOPE_PARAMETERS = Object.freeze([
  'organizationId',
  'workspaceId',
  'projectId',
  'repositoryId',
])

export interface StrongFlowRouteTarget {
  readonly deliveryId: DeliveryId
  readonly productSessionId: ProductSessionId
  readonly stageRunId: StageRunId
  readonly historySelection: StrongFlowHistorySelection
  readonly candidateRef: string | null
  readonly panel: StrongFlowArtifactsTab
  readonly candidatePath: string | null
  readonly candidateView: CandidateDiffViewMode
  readonly candidateLine: number | null
}

export interface StrongFlowRouteHashTarget {
  readonly deliveryId: DeliveryId
  readonly productSessionId: ProductSessionId
  readonly stageRunId: StageRunId
  readonly historySelection?: StrongFlowHistorySelection
  readonly candidateRef?: string | null
  readonly panel?: StrongFlowArtifactsTab
  readonly candidatePath?: string | null
  readonly candidateView?: CandidateDiffViewMode
  readonly candidateLine?: number | null
}

export interface StrongFlowRouteRequest {
  readonly deliveryId: DeliveryId | null
  readonly productSessionId: ProductSessionId | null
  readonly stageRunId: StageRunId | null
  readonly historySelection: StrongFlowHistorySelection
  readonly candidateRef: string | null
  readonly panel: StrongFlowArtifactsTab | null
  readonly candidatePath: string | null
  readonly candidateView: CandidateDiffViewMode | null
  readonly candidateLine: number | null
}

export type StrongFlowRouteParseResult =
  | { readonly status: 'valid', readonly request: StrongFlowRouteRequest }
  | { readonly status: 'invalid', readonly reason: 'invalid-route' }

export type StrongFlowRouteResolution =
  | { readonly status: 'selected', readonly target: StrongFlowRouteTarget }
  | {
      readonly status: 'unavailable'
      readonly reason: 'invalid-route' | 'wrong-scope' | 'missing-resource'
      readonly message: string
    }

type StrongFlowRouteUnavailableReason = Extract<
  StrongFlowRouteResolution,
  { readonly status: 'unavailable' }
>['reason']

function parametersFromHash(hash: string): URLSearchParams {
  const queryIndex = hash.indexOf('?')
  return new URLSearchParams(queryIndex < 0 ? '' : hash.slice(queryIndex + 1))
}

function validPath(value: string): boolean {
  return value.length > 0
    && value.length <= 4096
    && !value.startsWith('/')
    && !value.includes('\\')
    && !new TextEncoder().encode(value).some(byte => byte <= 31 || byte === 127)
    && value.split('/').every(segment => segment.length > 0 && segment !== '.' && segment !== '..')
    && !(value[1] === ':' && /^[A-Za-z]/u.test(value))
}

function single(
  parameters: URLSearchParams,
  name: string,
): string | null | undefined {
  const values = parameters.getAll(name)
  return values.length > 1 ? undefined : values[0] ?? null
}

/** Build the single canonical StrongFlow URL. Every route-owned identity is typed here. */
export function strongFlowRouteHash(
  target: StrongFlowRouteHashTarget,
  scope?: ScopeRouteSelection,
): string {
  const parameters = new URLSearchParams()
  parameters.set('delivery', target.deliveryId)
  parameters.set('session', target.productSessionId)
  parameters.set('stageRun', target.stageRunId)
  if (target.historySelection?.taskId !== null && target.historySelection?.taskId !== undefined) {
    parameters.set('task', target.historySelection.taskId)
  }
  if (
    target.historySelection?.stageRunId !== null
    && target.historySelection?.stageRunId !== undefined
  ) parameters.set('run', target.historySelection.stageRunId)
  if (target.candidateRef !== null && target.candidateRef !== undefined) {
    parameters.set('candidate', target.candidateRef)
  }
  if (target.panel !== undefined) parameters.set('panel', target.panel)
  if (target.candidatePath !== null && target.candidatePath !== undefined) {
    parameters.set('file', target.candidatePath)
  }
  parameters.set('view', target.candidateView ?? 'unified')
  if (target.candidateLine !== null && target.candidateLine !== undefined) {
    parameters.set('line', String(target.candidateLine))
  }
  const hash = `#/strongflow?${parameters.toString()}`
  return scope === undefined ? hash : scopeHash(hash, scope)
}

/** Parse and bound all route-owned identities before they reach a query or a view-model. */
export function strongFlowRouteRequestFromHash(hash: string): StrongFlowRouteParseResult {
  const parameters = parametersFromHash(hash)
  const allowed = new Set([...KNOWN_PARAMETERS, ...SCOPE_PARAMETERS])
  if (
    [...parameters.keys()].some(name => !allowed.has(name))
    || SCOPE_PARAMETERS.some(name => parameters.getAll(name).length > 1)
  ) return { status: 'invalid', reason: 'invalid-route' }
  const values = Object.fromEntries(KNOWN_PARAMETERS.map(name => [name, single(parameters, name)]))
  if (Object.values(values).includes(undefined)) return { status: 'invalid', reason: 'invalid-route' }
  const delivery = values.delivery ?? null
  const session = values.session ?? null
  const stageRun = values.stageRun ?? null
  const task = values.task ?? null
  const run = values.run ?? null
  const candidate = values.candidate ?? null
  const panel = values.panel ?? null
  const file = values.file ?? null
  const view = values.view ?? null
  const line = values.line ?? null
  if (
    (delivery !== null && !DELIVERY_ID.test(delivery))
    || (session !== null && !PRODUCT_SESSION_ID.test(session))
    || (stageRun !== null && !STAGE_RUN_ID.test(stageRun))
    || (task !== null && !DELIVERY_TASK_ID.test(task))
    || (run !== null && !STAGE_RUN_ID.test(run))
    || (candidate !== null && !CANDIDATE_REF.test(candidate))
    || (panel !== null && !['solution', 'execution', 'candidate', 'evidence'].includes(panel))
    || (file !== null && !validPath(file))
    || (view !== null && view !== 'unified' && view !== 'side-by-side')
    || (line !== null && (!/^[1-9][0-9]*$/.test(line) || Number(line) > Number.MAX_SAFE_INTEGER))
    || (line !== null && file === null)
    || ((session === null) !== (stageRun === null))
    || (
      delivery === null
      && [session, stageRun, task, run, candidate, panel, file, view, line]
        .some(value => value !== null)
    )
  ) return { status: 'invalid', reason: 'invalid-route' }
  return {
    status: 'valid',
    request: Object.freeze({
      deliveryId: delivery as DeliveryId | null,
      productSessionId: session as ProductSessionId | null,
      stageRunId: stageRun as StageRunId | null,
      historySelection: Object.freeze({
        taskId: task as DeliveryTaskId | null,
        stageRunId: run as StageRunId | null,
      }),
      candidateRef: candidate,
      panel: panel as StrongFlowArtifactsTab | null,
      candidatePath: file,
      candidateView: view as CandidateDiffViewMode | null,
      candidateLine: line === null ? null : Number(line),
    }),
  }
}

function sameOwnership(
  detail: DeliveryDetailProjection,
  scope: RepositoryScope,
): boolean {
  return detail.ownership.organizationId === scope.organizationId
    && detail.ownership.workspaceId === scope.workspaceId
    && detail.ownership.projectId === scope.projectId
    && detail.ownership.repositoryId === scope.repositoryId
}

function sameRepositoryScope(left: RepositoryScope, right: RepositoryScope): boolean {
  return left.kind === right.kind
    && left.organizationId === right.organizationId
    && left.workspaceId === right.workspaceId
    && left.projectId === right.projectId
    && left.repositoryId === right.repositoryId
}

function unavailable(
  reason: StrongFlowRouteUnavailableReason,
  message: string,
): StrongFlowRouteResolution {
  return Object.freeze({ status: 'unavailable', reason, message })
}

/** Resolve a parsed route only against the authorized, current Delivery projection. */
export function resolveStrongFlowRoute(
  request: StrongFlowRouteRequest,
  detail: DeliveryDetailProjection,
  scope: RepositoryScope,
): StrongFlowRouteResolution {
  if (!sameOwnership(detail, scope) || !sameRepositoryScope(detail.readCursor.scope, scope)) {
    return unavailable('wrong-scope', 'This StrongFlow link is not available in the selected repository Scope.')
  }
  if (request.deliveryId !== null && request.deliveryId !== detail.deliveryId) {
    return unavailable('missing-resource', 'This StrongFlow link no longer names an available Delivery.')
  }
  if (
    new Set(detail.tasks.map(task => task.id)).size !== detail.tasks.length
    || new Set(detail.stages.map(stage => stage.id)).size !== detail.stages.length
  ) return unavailable(
    'invalid-route',
    'This StrongFlow snapshot contains repeated Task or StageRun identities.',
  )
  const stage = detail.stages.findLast(candidate => (
    candidate.actorType === 'codex' && candidate.sessionBinding !== null
  ))
  if (stage === undefined || stage.actorType !== 'codex' || stage.sessionBinding === null) {
    return unavailable('missing-resource', 'This Delivery has no executable StrongFlow StageRun.')
  }
  if (
    stage.sessionBinding.stageRunId !== null
    && stage.sessionBinding.stageRunId !== stage.id
  ) return unavailable(
    'invalid-route',
    'This StrongFlow link encountered an inconsistent StageRun binding.',
  )
  if (
    (request.stageRunId !== null && request.stageRunId !== stage.id)
    || (
      request.productSessionId !== null
      && request.productSessionId !== stage.sessionBinding.productSessionId
    )
  ) return unavailable(
    'missing-resource',
    'This StrongFlow link no longer names the current StageRun and ProductSession.',
  )

  const requestedTask = request.historySelection.taskId
  const requestedRun = request.historySelection.stageRunId
  const task = requestedTask === null
    ? undefined
    : detail.tasks.find(candidate => candidate.id === requestedTask)
  if (requestedTask !== null && task === undefined) {
    return unavailable('missing-resource', 'This StrongFlow link names a Task that is no longer available.')
  }
  const historicalStage = requestedRun === null
    ? undefined
    : detail.stages.find(candidate => candidate.id === requestedRun)
  if (requestedRun !== null && historicalStage === undefined) {
    return unavailable('missing-resource', 'This StrongFlow link names an Attempt that is no longer available.')
  }
  if (
    historicalStage !== undefined
    && requestedTask !== null
    && historicalStage.deliveryTaskId !== requestedTask
  ) return unavailable(
    'invalid-route',
    'This StrongFlow link combines a Task with an unrelated Attempt.',
  )
  const canonicalTaskId = historicalStage?.deliveryTaskId ?? requestedTask
  if (
    canonicalTaskId !== null
    && canonicalTaskId !== undefined
    && !detail.tasks.some(candidate => candidate.id === canonicalTaskId)
  ) return unavailable('missing-resource', 'This StrongFlow link names an unavailable Task association.')
  if (
    requestedRun !== null
    && canonicalTaskId !== null
    && canonicalTaskId !== undefined
    && !detail.tasks.find(candidate => candidate.id === canonicalTaskId)?.stageRunIds.includes(
      requestedRun,
    )
  ) return unavailable(
    'invalid-route',
    'This StrongFlow link names an Attempt outside its canonical Task history.',
  )

  const currentCandidateRef = detail.currentCandidate?.candidateRef ?? null
  if (request.candidateRef !== null && request.candidateRef !== currentCandidateRef) {
    return unavailable('missing-resource', 'This StrongFlow Candidate has expired or is no longer available.')
  }
  const panel = request.panel ?? (request.candidatePath === null ? 'solution' : 'candidate')
  if ((panel === 'candidate' || request.candidatePath !== null) && currentCandidateRef === null) {
    return unavailable('missing-resource', 'This Delivery does not have a current Candidate.')
  }

  return Object.freeze({
    status: 'selected',
    target: Object.freeze({
      deliveryId: detail.deliveryId,
      productSessionId: stage.sessionBinding.productSessionId,
      stageRunId: stage.id,
      historySelection: Object.freeze({
        taskId: canonicalTaskId ?? null,
        stageRunId: requestedRun,
      }),
      candidateRef: panel === 'candidate' || request.candidateRef !== null
        ? currentCandidateRef
        : null,
      panel,
      candidatePath: request.candidatePath,
      candidateView: request.candidateView ?? 'unified',
      candidateLine: request.candidateLine,
    }),
  })
}
