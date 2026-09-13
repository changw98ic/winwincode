// SPDX-License-Identifier: Apache-2.0

/**
 * WWX-DEC-03 clarification / comparison / acceptance-edit view-model.
 *
 * Constrained interaction protocol only (DEC-01): form, single/multi select,
 * text, table, comparison card, and action reference. Unknown component kinds
 * and unknown actions are rejected. Drafts autosave locally and bind to
 * user/session/version/candidate/requestId on submit (DEC-07/DEC-08).
 */

export type ClarificationComponentKind =
  | 'form'
  | 'single-select'
  | 'multi-select'
  | 'text'
  | 'table'
  | 'comparison'
  | 'action'

export type ClarificationEstimateLabel =
  | 'unverified-model-estimate'
  | 'measured'
  | 'unknown'

export interface ClarificationField {
  readonly id: string
  readonly label: string
  readonly required: boolean
  /** Progressive entry: the user may answer "unknown" or "later". */
  readonly allowUnknown: boolean
  readonly allowLater: boolean
  readonly maxLength: number
}

export interface ClarificationOption {
  readonly id: string
  readonly label: string
}

export interface ComparisonRow {
  readonly id: string
  readonly label: string
  readonly left: string
  readonly right: string
}

export interface AcceptanceCriterionDraft {
  readonly id: string
  readonly title: string
  readonly verificationMethod: string | null
  readonly required: boolean
}

export type ScopeBoundary = 'in-scope' | 'out-of-scope'

export interface ClarificationComponent {
  readonly kind: ClarificationComponentKind
  readonly id: string
  readonly title: string
  readonly fields?: readonly ClarificationField[]
  readonly options?: readonly ClarificationOption[]
  readonly rows?: readonly ComparisonRow[]
  readonly criteria?: readonly AcceptanceCriterionDraft[]
  readonly scope?: ScopeBoundary
  readonly actionId?: string
  /** DEC-03: estimates always carry a label; never bare model claims. */
  readonly estimate?: {
    readonly value: string
    readonly label: ClarificationEstimateLabel
  }
  readonly maxWidth?: number
  readonly maxDepth?: number
}

export interface ClarificationAnswerValue {
  readonly fieldId: string
  readonly text: string | null
  readonly choiceIds: readonly string[]
  readonly unknown: boolean
  readonly later: boolean
}

export interface ClarificationDraft {
  readonly componentId: string
  readonly requestId: string
  readonly answers: readonly ClarificationAnswerValue[]
  readonly offline: boolean
  readonly updatedAt: string
}

export interface ClarificationSubmitBinding {
  readonly userId: string
  readonly productSessionId: string
  readonly deliveryRevision: number
  readonly candidateRef: string | null
  readonly requestId: string
}

export type ClarificationRejectionReason =
  | 'unknown-component-kind'
  | 'unknown-action'
  | 'component-too-wide'
  | 'component-too-deep'
  | 'field-limit'
  | 'illegal-html'

export type ClarificationState =
  | { readonly status: 'empty' }
  | {
    readonly status: 'editing'
    readonly components: readonly ClarificationComponent[]
    readonly draft: ClarificationDraft
    readonly blockedHighRisk: boolean
    readonly notice: string | null
  }
  | {
    readonly status: 'submitted'
    readonly binding: ClarificationSubmitBinding
  }

export interface ClarificationViewModel {
  readonly state: ClarificationState
  subscribe(listener: (state: ClarificationState) => void): () => void
  load(components: readonly ClarificationComponent[]): void
  setAnswer(input: ClarificationAnswerValue): void
  setOffline(offline: boolean): void
  markLater(fieldId: string): void
  markUnknown(fieldId: string): void
  submit(binding: Omit<ClarificationSubmitBinding, 'requestId'> & { requestId?: string }): void
  close(): void
}

export const CLARIFICATION_MAX_COMPONENTS = 40
export const CLARIFICATION_MAX_FIELDS = 20
export const CLARIFICATION_MAX_WIDTH = 12
export const CLARIFICATION_MAX_DEPTH = 3

const ALLOWED_KINDS: ReadonlySet<ClarificationComponentKind> = new Set([
  'form',
  'single-select',
  'multi-select',
  'text',
  'table',
  'comparison',
  'action',
])

/** DEC-01: every rendered title/label is plain text; HTML never executes. */
export function containsIllegalMarkup(value: string): boolean {
  return /<\s*script|javascript:|on\w+\s*=/i.test(value)
}

export function validateClarificationComponent(
  component: ClarificationComponent,
): ClarificationRejectionReason | null {
  if (!ALLOWED_KINDS.has(component.kind)) return 'unknown-component-kind'
  if (containsIllegalMarkup(component.title)) return 'illegal-html'
  if (component.kind === 'action' && (component.actionId === undefined || component.actionId.length === 0)) {
    return 'unknown-action'
  }
  if (component.maxWidth !== undefined && component.maxWidth > CLARIFICATION_MAX_WIDTH) {
    return 'component-too-wide'
  }
  if (component.maxDepth !== undefined && component.maxDepth > CLARIFICATION_MAX_DEPTH) {
    return 'component-too-deep'
  }
  const fields = component.fields ?? []
  if (fields.length > CLARIFICATION_MAX_FIELDS) return 'field-limit'
  for (const field of fields) {
    if (containsIllegalMarkup(field.label)) return 'illegal-html'
  }
  return null
}

/**
 * DEC-02: only missing required fields on high-risk stages block. Unrelated
 * fields never block unrelated edits.
 */
export function missingRequiredFields(
  component: ClarificationComponent,
  answers: readonly ClarificationAnswerValue[],
): readonly string[] {
  const byField = new Map(answers.map(answer => [answer.fieldId, answer]))
  const missing: string[] = []
  for (const field of component.fields ?? []) {
    if (!field.required) continue
    const answer = byField.get(field.id)
    if (answer === undefined) {
      missing.push(field.id)
      continue
    }
    if (answer.unknown || answer.later) continue
    const emptyText = answer.text === null || answer.text.trim() === ''
    const emptyChoices = answer.choiceIds.length === 0
    if (emptyText && emptyChoices) missing.push(field.id)
  }
  return missing
}

/** DEC-03: model estimates never render as verified facts. */
export function estimateDisplayText(
  estimate: { readonly value: string; readonly label: ClarificationEstimateLabel },
): string {
  if (estimate.label === 'unverified-model-estimate') {
    return `${estimate.value}（模型估算，未验证）`
  }
  if (estimate.label === 'measured') {
    return `${estimate.value}（已测量）`
  }
  return `${estimate.value}（未知来源）`
}

function emptyDraft(componentId: string, requestId: string): ClarificationDraft {
  return Object.freeze({
    componentId,
    requestId,
    answers: Object.freeze([]) as readonly ClarificationAnswerValue[],
    offline: false,
    updatedAt: '1970-01-01T00:00:00.000Z',
  })
}

export interface ClarificationViewModelOptions {
  readonly now?: () => string
  readonly nextRequestId?: () => string
  /** High-risk stage ids whose required fields must be complete before submit. */
  readonly highRiskComponentIds?: readonly string[]
}

export function createClarificationViewModel(
  options: ClarificationViewModelOptions = {},
): ClarificationViewModel {
  const now = options.now ?? (() => new Date().toISOString())
  const nextRequestId = options.nextRequestId ?? (() => `req_${String(Date.now())}`)
  const highRisk = new Set(options.highRiskComponentIds ?? [])
  let state: ClarificationState = Object.freeze({ status: 'empty' })
  const listeners = new Set<(next: ClarificationState) => void>()

  function emit(next: ClarificationState): void {
    state = Object.freeze(next)
    for (const listener of listeners) listener(state)
  }

  return {
    get state() {
      return state
    },
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    load(components) {
      if (components.length > CLARIFICATION_MAX_COMPONENTS) {
        emit({
          status: 'editing',
          components: Object.freeze([]),
          draft: emptyDraft('', nextRequestId()),
          blockedHighRisk: true,
          notice: '澄清组件数量超过限制，已拒绝加载。',
        })
        return
      }
      const accepted: ClarificationComponent[] = []
      for (const component of components) {
        const reason = validateClarificationComponent(component)
        if (reason === null) accepted.push(Object.freeze(component))
      }
      emit({
        status: 'editing',
        components: Object.freeze(accepted),
        draft: emptyDraft(accepted[0]?.id ?? '', nextRequestId()),
        blockedHighRisk: false,
        notice: accepted.length < components.length
          ? '部分非法组件已被拒绝，不会渲染。'
          : null,
      })
    },
    setAnswer(input) {
      if (state.status !== 'editing') return
      const answers = [
        ...state.draft.answers.filter(answer => answer.fieldId !== input.fieldId),
        Object.freeze(input),
      ]
      const draft = Object.freeze({
        ...state.draft,
        answers: Object.freeze(answers),
        updatedAt: now(),
      })
      emit({ ...state, draft, notice: null })
    },
    setOffline(offline) {
      if (state.status !== 'editing') return
      const draft = Object.freeze({ ...state.draft, offline, updatedAt: now() })
      emit({
        ...state,
        draft,
        notice: offline ? '离线草稿已标记，提交前不会当作已授权。' : null,
      })
    },
    markLater(fieldId) {
      this.setAnswer({ fieldId, text: null, choiceIds: [], unknown: false, later: true })
    },
    markUnknown(fieldId) {
      this.setAnswer({ fieldId, text: null, choiceIds: [], unknown: true, later: false })
    },
    submit(binding) {
      if (state.status !== 'editing') return
      const missing = state.components.flatMap(component => {
        if (!highRisk.has(component.id)) return []
        return missingRequiredFields(component, state.status === 'editing' ? state.draft.answers : [])
      })
      if (missing.length > 0) {
        emit({
          ...state,
          blockedHighRisk: true,
          notice: `高风险阶段仍有必填信息未完成：${missing.join('、')}`,
        })
        return
      }
      if (state.status === 'editing' && state.draft.offline) {
        emit({
          ...state,
          blockedHighRisk: true,
          notice: '离线草稿不能直接提交为已授权变更。',
        })
        return
      }
      emit({
        status: 'submitted',
        binding: Object.freeze({
          ...binding,
          requestId: binding.requestId ?? nextRequestId(),
        }),
      })
    },
    close() {
      listeners.clear()
      state = Object.freeze({ status: 'empty' })
    },
  }
}
