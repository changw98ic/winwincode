// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClient } from './community-control-plane-client.js'
import { queryDeliveryDetail } from './community-control-plane-client.js'
import { createEditableDraft, type DraftFieldConflict } from './editable-draft.js'
import type {
  Actor,
  DeliveryDetailProjection,
  DeliveryId,
  DeliveryUpdateSpecCommand,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'

export type ClarificationField =
  | 'title'
  | 'goal'
  | 'scope'
  | 'outOfScope'
  | 'constraints'
  | 'acceptanceCriteria'

export type ClarificationValues = Readonly<Record<ClarificationField, string>>

export interface ClarificationCriterion {
  readonly id: string
  readonly title: string
  readonly required: boolean
  /** Verification is selected by the trusted repository adapter, not the browser. */
  readonly verificationMethod: string | null
}

export interface ClarificationSnapshot {
  readonly deliveryId: DeliveryId
  readonly deliveryRevision: number
  readonly deliverySpecRevision: number
  readonly actorId: string
  readonly productSessionId: DeliveryDetailProjection['requirements']['sourceProductSessionId']
  readonly candidateRef: string | null
  readonly baseRevision: string
  readonly publicationTarget: DeliveryDetailProjection['requirements']['publicationTarget']
  readonly values: ClarificationValues
}

export interface ClarificationSaveInput {
  readonly source: ClarificationSnapshot
  readonly expectedRevision: number
  readonly requestId: RequestId
  readonly values: ClarificationValues
}

export interface ClarificationPort {
  load(): Promise<ClarificationSnapshot>
  save(input: ClarificationSaveInput): Promise<ClarificationSnapshot>
}

export interface ClarificationChange {
  readonly field: ClarificationField
  readonly label: string
  readonly before: string
  readonly after: string
}

export interface ClarificationState {
  readonly status: 'loading' | 'editing' | 'failed'
  readonly snapshot: ClarificationSnapshot | null
  readonly values: ClarificationValues
  readonly criteria: readonly ClarificationCriterion[]
  readonly changes: readonly ClarificationChange[]
  readonly dirty: boolean
  readonly busy: boolean
  readonly offline: boolean
  readonly conflicts: readonly DraftFieldConflict[]
  readonly notice: string | null
  readonly lastRequestId: RequestId | null
  /** Changes only when controls must be rebuilt, so typing never loses focus. */
  readonly formRevision: number
}

export interface ClarificationViewModel {
  readonly state: ClarificationState
  subscribe(listener: (state: ClarificationState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  edit(field: Exclude<ClarificationField, 'acceptanceCriteria'>, value: string): void
  markUnknown(field: Exclude<ClarificationField, 'acceptanceCriteria'>): void
  markLater(field: Exclude<ClarificationField, 'acceptanceCriteria'>): void
  addCriterion(): void
  updateCriterion(id: string, patch: Readonly<Partial<Pick<ClarificationCriterion, 'title' | 'required'>>>): void
  removeCriterion(id: string): void
  setOffline(offline: boolean): void
  resolveConflicts(resolution: 'keep-draft' | 'use-server'): void
  submit(): Promise<void>
  close(): void
}

export interface ClarificationViewModelOptions {
  readonly port: ClarificationPort
  readonly nextRequestId: () => RequestId
  readonly nextCriterionId: () => string
}

const EMPTY_VALUES: ClarificationValues = Object.freeze({
  title: '',
  goal: '',
  scope: '',
  outOfScope: '',
  constraints: '',
  acceptanceCriteria: '[]',
})

const FIELD_LABELS: Readonly<Record<ClarificationField, string>> = Object.freeze({
  title: '标题',
  goal: '目标',
  scope: '范围内',
  outOfScope: '范围外',
  constraints: '约束',
  acceptanceCriteria: '验收条件',
})

const UNKNOWN = '【不知道】'
const LATER = '【稍后补充】'
const MAX_CRITERIA = 1_000

function criteriaJson(criteria: readonly ClarificationCriterion[]): string {
  return JSON.stringify(criteria)
}

function criteriaFrom(value: string): readonly ClarificationCriterion[] {
  try {
    const parsed: unknown = JSON.parse(value)
    if (!Array.isArray(parsed)) return Object.freeze([])
    return Object.freeze(parsed.flatMap(item => {
      if (item === null || typeof item !== 'object') return []
      const source = item as Readonly<Record<string, unknown>>
      if (typeof source.id !== 'string' || typeof source.title !== 'string'
        || typeof source.required !== 'boolean'
        || (source.verificationMethod !== null && typeof source.verificationMethod !== 'string')) return []
      return [Object.freeze({
        id: source.id,
        title: source.title,
        required: source.required,
        verificationMethod: source.verificationMethod,
      })]
    }))
  } catch {
    return Object.freeze([])
  }
}

function lines(value: string): readonly string[] {
  return [...new Set(value.split('\n').map(item => item.trim()).filter(Boolean))]
}

function valuesFrom(detail: DeliveryDetailProjection): ClarificationValues {
  const requirements = detail.requirements
  return Object.freeze({
    title: requirements.title,
    goal: requirements.goal,
    scope: requirements.scope.join('\n'),
    outOfScope: requirements.outOfScope.join('\n'),
    constraints: requirements.constraints.join('\n'),
    acceptanceCriteria: criteriaJson(requirements.acceptanceCriteria.map(criterion => ({
      id: criterion.id,
      title: criterion.description,
      required: criterion.required,
      verificationMethod: criterion.verificationMethod,
    }))),
  })
}

function snapshotFrom(
  detail: DeliveryDetailProjection,
  actor: Actor,
): ClarificationSnapshot {
  return Object.freeze({
    deliveryId: detail.deliveryId,
    deliveryRevision: detail.deliveryRevision,
    deliverySpecRevision: detail.requirements.deliverySpecRevision,
    actorId: actor.id,
    productSessionId: detail.requirements.sourceProductSessionId,
    candidateRef: detail.currentCandidate?.candidateRef ?? null,
    baseRevision: detail.requirements.baseRevision,
    publicationTarget: detail.requirements.publicationTarget,
    values: valuesFrom(detail),
  })
}

/** The existing delivery.get/update_spec contract is the only persistence path. */
export function clarificationUpdateCommand(
  input: ClarificationSaveInput,
  actor: Actor,
  scope: RepositoryScope,
): DeliveryUpdateSpecCommand {
  return {
    schemaVersion: 'winwincode/v1',
    command: 'delivery.update_spec',
    actor,
    scope,
    requestId: input.requestId,
    expectedRevision: input.expectedRevision,
    payload: {
      deliveryId: input.source.deliveryId,
      spec: {
        title: input.values.title.trim(),
        goal: input.values.goal.trim(),
        repositoryId: scope.repositoryId,
        baseRevision: input.source.baseRevision,
        scope: lines(input.values.scope),
        outOfScope: lines(input.values.outOfScope),
        constraints: lines(input.values.constraints),
        sourceProductSessionId: input.source.productSessionId,
        acceptanceCriteria: criteriaFrom(input.values.acceptanceCriteria).map(criterion => ({
          id: criterion.id,
          title: criterion.title.trim(),
          required: criterion.required,
        })),
        publicationTarget: input.source.publicationTarget,
      },
    },
  }
}

export function createControlPlaneClarificationPort(options: {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly deliveryId: DeliveryId
  readonly nextRequestId: () => RequestId
}): ClarificationPort {
  async function load(): Promise<ClarificationSnapshot> {
    return snapshotFrom(await queryDeliveryDetail(options.client, {
      actor: options.actor,
      scope: options.scope,
      deliveryId: options.deliveryId,
      requestId: options.nextRequestId(),
    }), options.actor)
  }

  return Object.freeze({
    load,
    async save(input: ClarificationSaveInput) {
      const command = clarificationUpdateCommand(input, options.actor, options.scope)
      const response = await options.client.command(command)
      if (response.outcome !== 'completed' || response.command !== 'delivery.update_spec') {
        throw new Error('控制平面已接收更新，但尚未确认完成；请使用同一草稿重试。')
      }
      return load()
    },
  })
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : '需求更新失败。'
}

function isRevisionConflict(error: unknown): boolean {
  return error !== null && typeof error === 'object'
    && Reflect.get(error, 'code') === 'REVISION_CONFLICT'
}

function unresolved(value: string): boolean {
  return value.includes(UNKNOWN) || value.includes(LATER)
}

function validationMessage(values: ClarificationValues): string | null {
  if (values.title.trim() === '') return '标题不能为空。'
  if (values.goal.trim() === '') return '目标不能为空。'
  if (Object.values(values).some(unresolved)) return '仍有“不知道”或“稍后补充”的信息，不能提交高风险规范变更。'
  const criteria = criteriaFrom(values.acceptanceCriteria)
  if (criteria.length === 0) return '至少需要一项验收条件。'
  if (criteria.some(criterion => criterion.title.trim() === '')) return '验收条件不能为空。'
  if (!criteria.some(criterion => criterion.required)) return '至少需要一项必须验收条件。'
  return null
}

function displayValue(field: ClarificationField, value: string): string {
  if (field !== 'acceptanceCriteria') return value
  return criteriaFrom(value).map(criterion => (
    `${criterion.required ? '必须' : '可选'}：${criterion.title}`
  )).join('\n')
}

export function createClarificationViewModel(
  options: ClarificationViewModelOptions,
): ClarificationViewModel {
  const draft = createEditableDraft<ClarificationValues>({ revisionSensitive: true })
  const listeners = new Set<(state: ClarificationState) => void>()
  let snapshot: ClarificationSnapshot | null = null
  let status: ClarificationState['status'] = 'loading'
  let notice: string | null = null
  let offline = false
  let closed = false
  let pendingRequestId: RequestId | null = null
  let lastRequestId: RequestId | null = null
  let formRevision = 0
  let loadGeneration = 0

  function currentState(): ClarificationState {
    const draftState = draft.state
    const values = draftState.scope === null ? EMPTY_VALUES : draftState.values
    const changes: ClarificationChange[] = []
    if (snapshot !== null) {
      for (const field of Object.keys(FIELD_LABELS) as ClarificationField[]) {
        if (snapshot.values[field] === values[field]) continue
        changes.push(Object.freeze({
          field,
          label: FIELD_LABELS[field],
          before: displayValue(field, snapshot.values[field]),
          after: displayValue(field, values[field]),
        }))
      }
    }
    return Object.freeze({
      status,
      snapshot,
      values,
      criteria: criteriaFrom(values.acceptanceCriteria),
      changes: Object.freeze(changes),
      dirty: draftState.dirtyFields.length > 0,
      busy: draftState.submission !== null,
      offline,
      conflicts: draftState.conflicts,
      notice,
      lastRequestId,
      formRevision,
    })
  }

  function emit(): void {
    if (closed) return
    const next = currentState()
    for (const listener of listeners) listener(next)
  }

  async function synchronize(message: string | null): Promise<void> {
    const generation = ++loadGeneration
    const loaded = await options.port.load()
    if (closed || generation !== loadGeneration) return
    snapshot = loaded
    draft.synchronize({
      scope: loaded.deliveryId,
      revision: loaded.deliveryRevision,
      values: loaded.values,
    })
    status = 'editing'
    notice = message
    formRevision += 1
    emit()
  }

  function editCriteria(criteria: readonly ClarificationCriterion[], rebuild: boolean): void {
    if (status !== 'editing' || draft.state.submission !== null) return
    draft.edit('acceptanceCriteria', criteriaJson(criteria))
    pendingRequestId = null
    notice = null
    if (rebuild) formRevision += 1
    emit()
  }

  const model: ClarificationViewModel = {
    get state() { return currentState() },
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    async start() {
      try {
        await synchronize(null)
      } catch (error) {
        if (closed) return
        status = 'failed'
        notice = errorMessage(error)
        emit()
      }
    },
    async refresh() {
      if (draft.state.submission !== null) return
      try {
        await synchronize('已读取最新规范。')
      } catch (error) {
        if (closed) return
        notice = errorMessage(error)
        emit()
      }
    },
    edit(field, value) {
      if (status !== 'editing' || draft.state.submission !== null) return
      draft.edit(field, value)
      pendingRequestId = null
      notice = null
      emit()
    },
    markUnknown(field) { this.edit(field, UNKNOWN) },
    markLater(field) { this.edit(field, LATER) },
    addCriterion() {
      const criteria = criteriaFrom(draft.state.values.acceptanceCriteria)
      if (criteria.length >= MAX_CRITERIA) {
        notice = '验收条件数量已达到上限。'
        emit()
        return
      }
      editCriteria([...criteria, Object.freeze({
        id: options.nextCriterionId(),
        title: '',
        required: false,
        verificationMethod: null,
      })], true)
    },
    updateCriterion(id, patch) {
      editCriteria(criteriaFrom(draft.state.values.acceptanceCriteria).map(criterion => (
        criterion.id === id ? Object.freeze({ ...criterion, ...patch }) : criterion
      )), false)
    },
    removeCriterion(id) {
      editCriteria(criteriaFrom(draft.state.values.acceptanceCriteria).filter(criterion => (
        criterion.id !== id
      )), true)
    },
    setOffline(value) {
      offline = value
      if (value) notice = '当前离线：草稿仍保留在本页，但不会显示为已授权。'
      else if (notice?.startsWith('当前离线：') === true) notice = null
      emit()
    },
    resolveConflicts(resolution) {
      draft.resolveConflicts(resolution)
      pendingRequestId = null
      notice = resolution === 'use-server' ? '已采用服务端最新值。' : '已保留本页草稿，请重新提交。'
      formRevision += 1
      emit()
    },
    async submit() {
      if (status !== 'editing' || snapshot === null || offline) {
        if (offline) {
          notice = '离线草稿不能提交为已授权变更。'
          emit()
        }
        return
      }
      const problem = validationMessage(draft.state.values)
      if (problem !== null) {
        notice = problem
        emit()
        return
      }
      if (draft.state.dirtyFields.length === 0) return
      const submission = draft.beginSubmission()
      if (submission === null) {
        if (draft.state.revisionConflict) notice = '请先解决并发修改冲突。'
        emit()
        return
      }
      const requestId = pendingRequestId ?? options.nextRequestId()
      loadGeneration += 1
      pendingRequestId = requestId
      notice = '正在提交规范修订…'
      emit()
      try {
        const saved = await options.port.save({
          source: snapshot,
          expectedRevision: submission.revision,
          requestId,
          values: submission.values,
        })
        if (closed) return
        snapshot = saved
        draft.synchronize({ scope: saved.deliveryId, revision: saved.deliveryRevision, values: saved.values })
        draft.finishSubmission('success')
        lastRequestId = requestId
        pendingRequestId = null
        notice = `已保存为规范修订 ${String(saved.deliverySpecRevision)}。`
        formRevision += 1
        emit()
      } catch (error) {
        if (closed) return
        if (isRevisionConflict(error)) {
          try {
            const latest = await options.port.load()
            if (closed) return
            snapshot = latest
            draft.synchronize({ scope: latest.deliveryId, revision: latest.deliveryRevision, values: latest.values })
            pendingRequestId = null
            formRevision += 1
            notice = '服务端规范已被其他窗口修改，请选择保留草稿或采用服务端值。'
          } catch (reloadError) {
            notice = `检测到修订冲突，但读取最新规范失败：${errorMessage(reloadError)}`
          }
        } else {
          notice = errorMessage(error)
        }
        draft.finishSubmission('failure')
        emit()
      }
    },
    close() {
      closed = true
      listeners.clear()
      draft.reset()
    },
  }
  return model
}
