// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryAttentionProjection,
  DeliveryProjection,
  DeliveryStageProjection,
  DeliveryTaskDetailProjection,
  RepositoryScope,
} from './generated/contracts.js'
import { mountButton } from './components/button.js'
import { mountDrawer } from './components/drawer.js'
import { mountEmptyState } from './components/empty-state.js'
import { mountFormField } from './components/form-field.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { mountSplitPane } from './components/split-pane.js'
import { mountTabs, type TabItem } from './components/tabs.js'
import { createEditableDraft, type EditableDraft } from './editable-draft.js'
import { mountStrongFlowCandidate } from './strongflow-candidate.js'
import { renderStrongFlowDiagrams } from './strongflow-diagrams.js'
import {
  boundedItems,
  DEFAULT_STRONGFLOW_RENDER_LIMITS,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'
import type {
  StrongFlowCreateState,
  StrongFlowCreateViewModel,
  StrongFlowProjection,
  StrongFlowViewModel,
  StrongFlowViewModelState,
} from './strongflow-view-model.js'
import { canSubmitStrongFlowVerdict } from './strongflow-view-model.js'
import {
  normalizeStrongFlowLayoutPreferences,
  STRONGFLOW_ARTIFACTS_TABS,
  strongFlowLayoutPreferencesFromStorage,
  strongFlowLayoutPreferencesToStorage,
  type StrongFlowArtifactsTab,
  type StrongFlowLayoutPreferences,
} from './strongflow-layout-preferences.js'

export const STRONGFLOW_NARROW_VIEWPORT_WIDTH = 1_024

export interface StrongFlowPageOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowViewModel
  readonly deliveries?: readonly DeliveryProjection[]
  readonly limits?: StrongFlowRenderLimits
  readonly storage?: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null
  readonly viewport?: StrongFlowLayoutViewport
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface StrongFlowLayoutViewport {
  readonly width: number
}

export interface StrongFlowPage {
  close(): void
}

export interface StrongFlowCreatePageOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowCreateViewModel
  readonly scope: RepositoryScope
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface StrongFlowCreatePage {
  close(): void
}

export interface StrongFlowPagePresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
}

export function strongFlowLayoutMode(width: number): 'wide' | 'narrow' {
  return width <= STRONGFLOW_NARROW_VIEWPORT_WIDTH ? 'narrow' : 'wide'
}

export function strongFlowPagePresentation(
  state: StrongFlowViewModelState,
): StrongFlowPagePresentation {
  const interaction = state.interaction ?? { status: 'idle', error: null }
  const visibleError = interaction.error ?? state.error
  const statusText = interaction.status === 'submitting'
    ? 'Submitting StrongFlow decision…'
    : interaction.status === 'waiting'
      ? 'Decision accepted · waiting for the current snapshot…'
      : state.status === 'loading'
    ? 'Loading StrongFlow…'
    : state.status === 'refreshing' || state.realtime === 'reloading'
      ? 'Updating StrongFlow…'
      : state.realtime === 'reconnecting'
        ? 'Reconnecting…'
        : state.status === 'authentication-required'
          ? 'Sign in required'
          : state.status === 'authorization-denied'
            ? 'Access denied'
            : state.status === 'cancelled'
              ? 'Update cancelled'
              : state.status === 'error'
                ? 'StrongFlow unavailable'
                : state.status === 'closed'
                  ? 'StrongFlow closed'
                  : state.projection === null
                    ? 'No Delivery selected'
                    : `${state.projection.delivery.status} · revision ${String(
                        state.projection.metadata.revisions.delivery,
                      )}`
  const errorText = visibleError === null
    ? null
    : visibleError.code === 'REVISION_CONFLICT'
      ? 'This Delivery changed before the decision was saved. Review the current snapshot and try again.'
      : visibleError.code === 'CANDIDATE_STALE'
        ? 'The candidate changed before the decision was saved. Review the current candidate and try again.'
        : visibleError.code === 'STRONGFLOW_ATTENTION_STALE'
          || visibleError.code === 'STRONGFLOW_REWORK_NODE_STALE'
          || visibleError.code === 'STRONGFLOW_REWORK_TASK_STALE'
          ? 'This review target is no longer current. Refresh StrongFlow and choose from the current snapshot.'
          : visibleError.kind === 'authentication'
      ? 'Sign in again to continue with StrongFlow.'
      : visibleError.kind === 'authorization'
        ? 'You do not have access to this Delivery.'
        : visibleError.kind === 'network'
          ? 'The StrongFlow server could not be reached. Check the connection and retry.'
          : visibleError.kind === 'version'
            ? 'The Client and Server versions differ. Update the Client and retry.'
            : visibleError.kind === 'cancelled'
              ? 'The StrongFlow update was cancelled.'
              : visibleError.code.startsWith('STRONGFLOW_')
                ? visibleError.message
                : 'StrongFlow could not be updated. Retry the bounded snapshot.'
  return Object.freeze({
    statusText,
    errorText,
    busy: state.status === 'loading'
      || state.status === 'refreshing'
      || state.realtime === 'reloading'
      || interaction.status === 'submitting'
      || interaction.status === 'waiting',
    retryVisible: errorText !== null && state.projection === null && interaction.error === null,
    reconnectVisible: state.realtime === 'reconnecting' && state.projection !== null,
  })
}

function strongFlowCreateError(state: StrongFlowCreateState): string | null {
  const error = state.error
  if (error === null) return null
  if (error.code.startsWith('STRONGFLOW_CREATE_')) return error.message
  if (error.kind === 'authentication') return 'Sign in again before creating this Delivery.'
  if (error.kind === 'authorization') {
    return 'You do not have permission to create a Delivery in this repository.'
  }
  if (error.kind === 'network') {
    return 'The StrongFlow server could not be reached. Your draft is still here; retry when connected.'
  }
  if (error.kind === 'cancelled') return 'Delivery creation was cancelled. Your draft is still here.'
  return 'The Delivery could not be created. Your draft is still here; review it and retry.'
}

/** Mount the complete first-Delivery form without introducing browser-owned Delivery state. */
export function mountStrongFlowCreatePage(
  options: StrongFlowCreatePageOptions,
): StrongFlowCreatePage {
  const readOnly = options.readOnly === true
  const document = options.root.ownerDocument
  const layout = strongFlowElement(document, 'div', 'wwc-strongflow wwc-strongflow-create')
  const status = strongFlowElement(document, 'p', 'wwc-strongflow-create-status')
  const content = strongFlowElement(document, 'section', 'wwc-strongflow-create-content')
  const form = strongFlowElement(document, 'form', 'wwc-strongflow-create-form') as HTMLFormElement
  const error = strongFlowElement(document, 'p', 'wwc-strongflow-create-error')
  const title = strongFlowElement(
    document,
    'input',
    'wwc-strongflow-create-title',
  ) as HTMLInputElement
  const goal = strongFlowElement(
    document,
    'textarea',
    'wwc-strongflow-create-goal',
  ) as HTMLTextAreaElement
  const repository = strongFlowElement(
    document,
    'input',
    'wwc-strongflow-create-repository',
  ) as HTMLInputElement
  const baseline = strongFlowElement(
    document,
    'input',
    'wwc-strongflow-create-baseline',
  ) as HTMLInputElement
  const repositoryScope = strongFlowElement(
    document,
    'input',
    'wwc-strongflow-create-scope',
  ) as HTMLInputElement
  const deliveryScope = strongFlowElement(
    document,
    'textarea',
    'wwc-strongflow-create-delivery-scope',
  ) as HTMLTextAreaElement
  const outOfScope = strongFlowElement(
    document,
    'textarea',
    'wwc-strongflow-create-out-of-scope',
  ) as HTMLTextAreaElement
  const constraints = strongFlowElement(
    document,
    'textarea',
    'wwc-strongflow-create-constraints',
  ) as HTMLTextAreaElement
  const criteria = strongFlowElement(
    document,
    'textarea',
    'wwc-strongflow-create-criteria',
  ) as HTMLTextAreaElement
  let closed = false

  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  title.type = 'text'
  title.required = true
  title.disabled = readOnly
  goal.required = true
  goal.disabled = readOnly
  repository.type = 'text'
  repository.readOnly = true
  repository.value = options.scope.repositoryId
  baseline.type = 'text'
  baseline.required = true
  baseline.disabled = readOnly
  repositoryScope.type = 'text'
  repositoryScope.readOnly = true
  repositoryScope.value = [
    options.scope.organizationId,
    options.scope.workspaceId,
    options.scope.projectId,
    options.scope.repositoryId,
  ].join(' / ')
  deliveryScope.required = true
  deliveryScope.disabled = readOnly
  outOfScope.disabled = readOnly
  constraints.disabled = readOnly
  criteria.required = true
  criteria.disabled = readOnly

  const titleField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-title',
      label: 'Delivery title',
      control: title,
      required: true,
    },
  })
  const goalField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-goal',
      label: 'Goal',
      help: 'Describe the concrete result this Delivery must reach.',
      control: goal,
      required: true,
    },
  })
  const repositoryField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-repository',
      label: 'Repository',
      help: 'This value comes from your exact authorized Repository Scope.',
      control: repository,
    },
  })
  const baselineField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-baseline',
      label: 'Baseline revision',
      help: 'Enter the commit or repository revision StrongFlow should start from.',
      control: baseline,
      required: true,
    },
  })
  const scopeField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-scope',
      label: 'Repository Scope',
      help: 'Organization / workspace / project / repository',
      control: repositoryScope,
    },
  })
  const deliveryScopeField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-delivery-scope',
      label: 'In scope',
      help: 'Enter one result that this Delivery includes per line.',
      control: deliveryScope,
      required: true,
    },
  })
  const outOfScopeField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-out-of-scope',
      label: 'Out of scope',
      help: 'Enter one explicit exclusion per line.',
      control: outOfScope,
    },
  })
  const constraintsField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-constraints',
      label: 'Constraints',
      help: 'Enter one implementation or operating constraint per line.',
      control: constraints,
    },
  })
  const criteriaField = mountFormField({
    document,
    props: {
      id: 'strongflow-create-criteria',
      label: 'Initial acceptance criteria',
      help: 'Enter one required result per line.',
      control: criteria,
      required: true,
    },
  })
  const submit = mountButton({
    document,
    props: {
      className: 'wwc-strongflow-create-submit',
      label: 'Create Delivery and open StrongFlow',
      type: 'submit',
      variant: 'primary',
    },
  })
  const cancel = mountButton({
    document,
    props: {
      className: 'wwc-strongflow-create-cancel',
      label: 'Cancel pending creation',
      type: 'button',
      disabled: readOnly,
      onActivate() { if (!readOnly) options.model.cancelPending() },
    },
  })
  cancel.root.hidden = true
  const empty = mountEmptyState({
    document,
    props: {
      className: 'wwc-strongflow-create-empty',
      title: 'Create the first Delivery',
      detail: 'Define the goal, repository baseline, exact Scope, and initial acceptance criteria.',
    },
  })
  form.append(
    titleField.root,
    goalField.root,
    repositoryField.root,
    baselineField.root,
    scopeField.root,
    deliveryScopeField.root,
    outOfScopeField.root,
    constraintsField.root,
    criteriaField.root,
    error,
    submit.root,
    cancel.root,
  )
  content.append(empty.root, form)
  layout.append(status, content)
  options.root.replaceChildren(layout)

  const onSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (readOnly) return
    void options.model.create({
      title: title.value,
      goal: goal.value,
      baseRevision: baseline.value,
      scope: deliveryScope.value.split(/\r?\n/u),
      outOfScope: outOfScope.value.split(/\r?\n/u),
      constraints: constraints.value.split(/\r?\n/u),
      sourceProductSessionId: null,
      acceptanceCriteria: criteria.value.split(/\r?\n/u),
    })
  }
  form.addEventListener('submit', onSubmit)

  function render(state: StrongFlowCreateState): void {
    if (closed) return
    const busy = state.status === 'submitting' || state.status === 'waiting'
    status.textContent = state.status === 'submitting'
      ? 'Creating Delivery…'
      : state.status === 'waiting'
        ? 'Delivery accepted · waiting for its executable stage…'
        : state.status === 'created'
          ? 'Delivery created · opening StrongFlow…'
          : 'No Delivery exists in this repository yet'
    form.setAttribute('aria-busy', String(busy))
    const visibleError = strongFlowCreateError(state)
    error.hidden = visibleError === null
    error.textContent = visibleError ?? ''
    submit.update({
      className: 'wwc-strongflow-create-submit',
      label: 'Create Delivery and open StrongFlow',
      busy,
      busyLabel: state.status === 'waiting' ? 'Waiting for Delivery…' : 'Creating Delivery…',
      disabled: readOnly || state.status === 'created' || state.status === 'closed',
      type: 'submit',
      variant: 'primary',
    })
    cancel.root.hidden = !busy
    cancel.update({
      className: 'wwc-strongflow-create-cancel',
      label: 'Cancel pending creation',
      type: 'button',
      disabled: readOnly,
      onActivate() { if (!readOnly) options.model.cancelPending() },
    })
  }

  const unsubscribe = options.model.subscribe(render)
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      form.removeEventListener?.('submit', onSubmit)
      titleField.close()
      goalField.close()
      repositoryField.close()
      baselineField.close()
      scopeField.close()
      deliveryScopeField.close()
      outOfScopeField.close()
      constraintsField.close()
      criteriaField.close()
      submit.close()
      cancel.close()
      empty.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}

const ARTIFACT_TAB_LABELS: Readonly<Record<StrongFlowArtifactsTab, string>> = Object.freeze({
  solution: 'Solution',
  execution: 'Execution',
  candidate: 'Candidate',
  evidence: 'Evidence',
})

function renderEvidencePanel(
  document: Document,
  projection: StrongFlowProjection,
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const section = strongFlowElement(document, 'section', 'wwc-strongflow-view-evidence')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const evidence = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence')
  const bounded = boundedItems(projection.evidence, limits.evidence)
  heading.textContent = 'Evidence'
  evidence.setAttribute('aria-label', 'Delivery evidence')
  evidence.append(...bounded.items.map(item => {
    const row = document.createElement('li')
    const title = document.createElement('strong')
    const source = document.createElement('p')
    title.textContent = `${item.type} · ${item.id}`
    source.textContent = item.sourceRef
    row.dataset.candidateRef = item.candidateRef
    row.append(title, source)
    return row
  }))
  section.append(heading, evidence)
  const omitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  omitted.hidden = bounded.omitted === 0
  omitted.textContent = `${String(bounded.omitted)} more evidence records not shown.`
  section.append(omitted)
  return section
}

interface StrongFlowResizeHandle {
  readonly root: HTMLElement
  update(value: number): void
  close(): void
}

function mountStrongFlowResizeHandle(
  document: Document,
  options: {
    readonly className: string
    readonly label: string
    readonly controls: string
    readonly direction: 1 | -1
    readonly value: () => number
    readonly workspaceWidth: () => number
    readonly onChange: (value: number) => void
  },
): StrongFlowResizeHandle {
  const root = strongFlowElement(document, 'div', options.className)
  let open = true
  let dragging = false
  let pointerId: number | null = null
  let startX = 0
  let startValue = 0
  let startWorkspaceWidth = 0

  root.tabIndex = 0
  root.setAttribute('role', 'separator')
  root.setAttribute('aria-label', options.label)
  root.setAttribute('aria-orientation', 'vertical')
  root.setAttribute('aria-controls', options.controls)
  root.setAttribute('aria-valuemin', '18')
  root.setAttribute('aria-valuemax', '45')

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
    event.preventDefault()
    const step = event.shiftKey ? 5 : 2
    options.onChange(options.value() + (event.key === 'ArrowRight' ? step : -step))
  }
  const stopDragging = (event: PointerEvent) => {
    if (!dragging || pointerId !== event.pointerId) return
    dragging = false
    root.releasePointerCapture?.(event.pointerId)
    pointerId = null
  }
  const onPointerDown = (event: PointerEvent) => {
    if (event.button !== 0) return
    const width = options.workspaceWidth()
    if (!Number.isFinite(width) || width <= 0) return
    event.preventDefault()
    dragging = true
    pointerId = event.pointerId
    startX = event.clientX
    startValue = options.value()
    startWorkspaceWidth = width
    root.setPointerCapture?.(event.pointerId)
  }
  const onPointerMove = (event: PointerEvent) => {
    if (!dragging || pointerId !== event.pointerId) return
    event.preventDefault()
    const delta = ((event.clientX - startX) / startWorkspaceWidth) * 100
    options.onChange(startValue + (options.direction * delta))
  }

  root.addEventListener('keydown', onKeyDown)
  root.addEventListener('pointerdown', onPointerDown)
  root.addEventListener('pointermove', onPointerMove)
  root.addEventListener('pointerup', stopDragging)
  root.addEventListener('pointercancel', stopDragging)

  return {
    root,
    update(value) {
      if (!open) return
      root.setAttribute('aria-valuenow', String(value))
      root.setAttribute('aria-valuetext', `${String(value)} percent`)
    },
    close() {
      if (!open) return
      open = false
      root.removeEventListener('keydown', onKeyDown)
      root.removeEventListener('pointerdown', onPointerDown)
      root.removeEventListener('pointermove', onPointerMove)
      root.removeEventListener('pointerup', stopDragging)
      root.removeEventListener('pointercancel', stopDragging)
      root.remove?.()
    },
  }
}

function safeWindowStorage(
  browserWindow: (Window & typeof globalThis) | null,
): Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null {
  try {
    return browserWindow?.localStorage ?? null
  } catch {
    return null
  }
}

/** Mount the advanced StrongFlow workspace against its Control Plane view-model. */
export function mountStrongFlowPage(options: StrongFlowPageOptions): StrongFlowPage {
  const readOnly = options.readOnly === true
  const document = options.root.ownerDocument
  const pageDraftScope = options.model.draftScope ?? 'mounted-strongflow-scope'
  const limits = options.limits ?? DEFAULT_STRONGFLOW_RENDER_LIMITS
  const browserWindow = document.defaultView
    ?? (typeof window === 'undefined' ? null : window)
  const storage = options.storage !== undefined
    ? options.storage
    : safeWindowStorage(browserWindow)
  const viewport = options.viewport ?? {
    get width() {
      return browserWindow?.innerWidth ?? STRONGFLOW_NARROW_VIEWPORT_WIDTH + 1
    },
  }
  const layout = strongFlowElement(document, 'div', 'wwc-strongflow')
  const status = strongFlowElement(document, 'p', 'wwc-strongflow-status')
  const error = strongFlowElement(document, 'div', 'wwc-strongflow-error')
  const errorText = strongFlowElement(document, 'span', 'wwc-strongflow-error-text')
  const retry = strongFlowElement(document, 'button', 'wwc-strongflow-retry') as HTMLButtonElement
  const reconnect = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-reconnect',
  ) as HTMLButtonElement
  const content = strongFlowElement(document, 'div', 'wwc-strongflow-content')
  const workspace = strongFlowElement(document, 'section', 'wwc-strongflow-workspace')
  const desktopControls = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-workspace-controls',
  )
  const narrowBar = strongFlowElement(document, 'div', 'wwc-strongflow-narrow-bar')
  const navigation = strongFlowElement(document, 'nav', 'wwc-strongflow-navigation')
  const mainRegion = strongFlowElement(document, 'section', 'wwc-strongflow-main-region')
  const context = strongFlowElement(document, 'aside', 'wwc-strongflow-context')
  const artifacts = strongFlowElement(document, 'section', 'wwc-strongflow-artifacts')
  const artifactsHeading = strongFlowElement(
    document,
    'h3',
    'wwc-strongflow-section-heading',
  )
  const navigationDrawerContent = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-navigation-drawer-content',
  )
  const contextDrawerContent = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-context-drawer-content',
  )
  const deliveriesRoot = strongFlowElement(document, 'aside', 'wwc-strongflow-deliveries')
  const deliveriesHeading = strongFlowElement(
    document,
    'h2',
    'wwc-strongflow-deliveries-heading',
  )
  const deliveries = strongFlowElement(document, 'ul', 'wwc-strongflow-delivery-list')
  const deliveriesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const details = strongFlowElement(document, 'div', 'wwc-strongflow-details')
  const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const overview = strongFlowElement(document, 'section', 'wwc-strongflow-overview')
  const heading = strongFlowElement(document, 'h2', 'wwc-strongflow-heading')
  const goal = strongFlowElement(document, 'p', 'wwc-strongflow-goal')
  const metadata = strongFlowElement(document, 'p', 'wwc-strongflow-metadata')
  const tasksSection = strongFlowElement(document, 'section', 'wwc-strongflow-tasks')
  const tasksHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const tasks = strongFlowElement(document, 'ul', 'wwc-strongflow-task-list')
  const tasksOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const stagesSection = strongFlowElement(document, 'section', 'wwc-strongflow-stages')
  const stagesHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const stages = strongFlowElement(document, 'ol', 'wwc-strongflow-stage-list')
  const stagesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const attentionSection = strongFlowElement(document, 'section', 'wwc-strongflow-attention')
  const attentionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const attention = strongFlowElement(document, 'ul', 'wwc-strongflow-attention-list')
  const attentionOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const contextEvidenceHost = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-context-evidence-host',
  )
  const diagramsHost = strongFlowElement(document, 'div', 'wwc-strongflow-diagrams-host')
  const candidateHost = strongFlowElement(document, 'div', 'wwc-strongflow-candidate-host')
  const artifactEvidenceHost = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-artifact-evidence-host',
  )
  const actions = strongFlowElement(document, 'section', 'wwc-strongflow-actions')
  const actionsHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const solutionActions = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-solution-actions',
  )
  const commentsLabel = document.createElement('label')
  const comments = document.createElement('textarea')
  const changesLabel = document.createElement('label')
  const changes = document.createElement('textarea')
  const reviewConflict = strongFlowElement(document, 'div', 'wwc-strongflow-review-conflict')
  const reviewConflictText = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-conflict-text',
  )
  const keepReviewDraft = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-review-keep-draft',
  ) as HTMLButtonElement
  const useServerReview = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-review-use-server',
  ) as HTMLButtonElement
  const approveSolution = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-approve-solution',
  ) as HTMLButtonElement
  const requestChanges = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-request-changes',
  ) as HTMLButtonElement
  const rejectSolution = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-reject-solution',
  ) as HTMLButtonElement
  const approveTasks = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-approve-tasks',
  ) as HTMLButtonElement
  const submitVerdict = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-submit-verdict',
  ) as HTMLButtonElement
  const attentionActions = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-attention-actions-list',
  )
  const actionsOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const advanceDelivery = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-advance-delivery',
  ) as HTMLButtonElement
  let closed = false
  let preferences = strongFlowLayoutPreferencesFromStorage(storage)
  if (options.model.state.candidateFiles.selectedPath !== null) {
    preferences = normalizeStrongFlowLayoutPreferences({
      ...preferences,
      artifactsTab: 'candidate',
    })
  }
  let navigationDrawerOpen = false
  let contextDrawerOpen = false
  let diagramsNode: HTMLElement | null = null
  let contextEvidenceNode: HTMLElement | null = null
  let artifactEvidenceNode: HTMLElement | null = null
  let diagramsFingerprint: string | null = null
  let lastEvidenceKey: string | null = null
  let lastLayoutKey: string | null = null
  let activeDeliveryId: string | null = null
  type ReviewDraftValues = {
    readonly comments: string
    readonly requestedChanges: string
  }
  const reviewDraft = createEditableDraft<ReviewDraftValues>({ revisionSensitive: true })

  function updateOmitted(node: HTMLElement, count: number, label: string): void {
    node.hidden = count === 0
    const text = `${String(count)} more ${label} not shown.`
    if (node.textContent !== text) node.textContent = text
  }

  function persist(next: StrongFlowLayoutPreferences): void {
    preferences = normalizeStrongFlowLayoutPreferences(next)
    strongFlowLayoutPreferencesToStorage(storage, preferences)
    render(options.model.state)
  }

  function selectArtifactTab(id: string): void {
    if (!STRONGFLOW_ARTIFACTS_TABS.includes(id as StrongFlowArtifactsTab)) return
    persist({ ...preferences, artifactsTab: id as StrongFlowArtifactsTab })
  }

  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  retry.type = 'button'
  retry.textContent = 'Retry snapshot'
  reconnect.type = 'button'
  reconnect.textContent = 'Reconnect events'
  workspace.setAttribute('aria-label', 'StrongFlow workbench')
  navigation.id = 'wwc-strongflow-navigation'
  navigation.setAttribute('aria-label', 'Delivery and Task navigation')
  mainRegion.id = 'wwc-strongflow-main-region'
  mainRegion.setAttribute('aria-label', 'Delivery main content')
  context.id = 'wwc-strongflow-context'
  context.setAttribute('aria-label', 'Attention and Evidence context')
  artifacts.id = 'wwc-strongflow-artifacts'
  artifacts.setAttribute('aria-label', 'Delivery artifacts')
  artifactsHeading.textContent = 'Delivery artifacts'
  deliveriesHeading.textContent = 'Deliveries'
  tasksHeading.textContent = 'Tasks'
  stagesHeading.textContent = 'Stages'
  attentionHeading.textContent = 'Attention'
  actionsHeading.textContent = 'Review actions'
  commentsLabel.textContent = 'Review comments'
  commentsLabel.append(comments)
  changesLabel.textContent = 'Requested changes, one per line'
  changesLabel.append(changes)
  reviewConflict.setAttribute('role', 'alert')
  reviewConflict.hidden = true
  keepReviewDraft.type = 'button'
  keepReviewDraft.textContent = 'Keep my draft'
  useServerReview.type = 'button'
  useServerReview.textContent = 'Use current server review'
  reviewConflict.append(reviewConflictText, keepReviewDraft, useServerReview)
  approveSolution.type = 'button'
  approveSolution.textContent = 'Approve solution'
  requestChanges.type = 'button'
  requestChanges.textContent = 'Request changes'
  rejectSolution.type = 'button'
  rejectSolution.textContent = 'Reject solution'
  approveTasks.type = 'button'
  approveTasks.textContent = 'Approve task breakdown'
  submitVerdict.type = 'button'
  submitVerdict.textContent = 'Compute current verdict'
  advanceDelivery.type = 'button'
  advanceDelivery.textContent = 'Approve final Delivery'

  const tabItems: readonly TabItem[] = STRONGFLOW_ARTIFACTS_TABS.map(tab => ({
    id: tab,
    label: ARTIFACT_TAB_LABELS[tab],
    panelId: `wwc-strongflow-artifact-panel-${tab}`,
  }))
  const artifactTabs = mountTabs({
    document,
    props: {
      id: 'wwc-strongflow-artifact-tab',
      label: 'Delivery artifacts',
      tabs: tabItems,
      selectedId: preferences.artifactsTab,
      className: 'wwc-strongflow-artifact-tabs',
      onSelect: selectArtifactTab,
    },
  })
  const artifactPanels = new Map<StrongFlowArtifactsTab, HTMLElement>()
  for (const tab of STRONGFLOW_ARTIFACTS_TABS) {
    const panel = strongFlowElement(document, 'section', 'wwc-strongflow-artifact-panel')
    panel.id = `wwc-strongflow-artifact-panel-${tab}`
    panel.dataset.artifactTab = tab
    panel.setAttribute('role', 'tabpanel')
    panel.setAttribute('aria-labelledby', `wwc-strongflow-artifact-tab-${tab}`)
    artifactPanels.set(tab, panel)
  }
  artifactPanels.get('solution')?.append(diagramsHost)
  artifactPanels.get('candidate')?.append(candidateHost)
  artifactPanels.get('evidence')?.append(artifactEvidenceHost)
  artifacts.append(artifactsHeading, artifactTabs.root, ...artifactPanels.values())

  const innerSplit = mountSplitPane({
    document,
    props: {
      primary: mainRegion,
      primaryLabel: 'Delivery main content',
      secondary: context,
      secondaryLabel: 'Attention and Evidence context',
      className: 'wwc-strongflow-main-context-split',
    },
  })
  const outerSplit = mountSplitPane({
    document,
    props: {
      primary: navigation,
      primaryLabel: 'Delivery and Task navigation',
      secondary: innerSplit.root,
      secondaryLabel: 'Delivery review workspace',
      className: 'wwc-strongflow-navigation-split',
    },
  })
  const collapseNavigation = mountButton({
    document,
    props: {
      className: 'wwc-strongflow-collapse-navigation',
      label: 'Collapse navigation pane',
      accessibleName: 'Collapse navigation pane',
      variant: 'ghost',
      onActivate: () => {
        persist({
          ...preferences,
          navigationCollapsed: !preferences.navigationCollapsed,
        })
      },
    },
  })
  const collapseContext = mountButton({
    document,
    props: {
      className: 'wwc-strongflow-collapse-context',
      label: 'Collapse context pane',
      accessibleName: 'Collapse context pane',
      variant: 'ghost',
      onActivate: () => {
        persist({ ...preferences, contextCollapsed: !preferences.contextCollapsed })
      },
    },
  })
  const openNavigation = mountButton({
    document,
    props: {
      className: 'wwc-strongflow-open-navigation',
      label: 'Deliveries and tasks',
      accessibleName: 'Open Delivery and Task navigation',
      onActivate: () => {
        navigationDrawerOpen = !navigationDrawerOpen
        render(options.model.state)
      },
    },
  })
  const openContext = mountButton({
    document,
    props: {
      className: 'wwc-strongflow-open-context',
      label: 'Attention and Evidence',
      accessibleName: 'Open Attention and Evidence context',
      onActivate: () => {
        contextDrawerOpen = !contextDrawerOpen
        render(options.model.state)
      },
    },
  })
  const navigationDrawer = mountDrawer({
    document,
    props: {
      id: 'wwc-strongflow-navigation-drawer',
      title: 'Delivery and Task navigation',
      open: false,
      content: navigationDrawerContent,
      closeLabel: 'Close Delivery and Task navigation',
      className: 'wwc-strongflow-navigation-drawer',
      onClose: () => {
        navigationDrawerOpen = false
        render(options.model.state)
      },
    },
  })
  const contextDrawer = mountDrawer({
    document,
    props: {
      id: 'wwc-strongflow-context-drawer',
      title: 'Attention and Evidence',
      open: false,
      content: contextDrawerContent,
      closeLabel: 'Close Attention and Evidence context',
      className: 'wwc-strongflow-context-drawer',
      onClose: () => {
        contextDrawerOpen = false
        render(options.model.state)
      },
    },
  })
  const navigationResize = mountStrongFlowResizeHandle(document, {
    className: 'wwc-strongflow-resize-navigation',
    label: 'Resize navigation width',
    controls: navigation.id,
    direction: 1,
    value: () => preferences.navigationWidth,
    workspaceWidth: () => workspace.getBoundingClientRect?.().width ?? 0,
    onChange: value => { persist({ ...preferences, navigationWidth: value }) },
  })
  const contextResize = mountStrongFlowResizeHandle(document, {
    className: 'wwc-strongflow-resize-context',
    label: 'Resize context pane width',
    controls: context.id,
    direction: -1,
    value: () => preferences.contextWidth,
    workspaceWidth: () => workspace.getBoundingClientRect?.().width ?? 0,
    onChange: value => { persist({ ...preferences, contextWidth: value }) },
  })
  solutionActions.append(
    commentsLabel,
    changesLabel,
    reviewConflict,
    approveSolution,
    requestChanges,
    rejectSolution,
  )
  error.append(errorText, retry, reconnect)
  deliveriesRoot.append(deliveriesHeading, deliveries, deliveriesOmitted)
  overview.append(heading, goal, metadata)
  tasksSection.append(tasksHeading, tasks, tasksOmitted)
  stagesSection.append(stagesHeading, stages, stagesOmitted)
  attentionSection.append(attentionHeading, attention, attentionOmitted)
  details.append(empty, overview)
  actions.append(
    actionsHeading,
    solutionActions,
    approveTasks,
    submitVerdict,
    attentionActions,
    actionsOmitted,
    advanceDelivery,
  )
  navigation.append(deliveriesRoot, tasksSection, stagesSection)
  mainRegion.append(details, actions)
  context.append(attentionSection, contextEvidenceHost)
  outerSplit.root.replaceChildren(
    outerSplit.primary,
    navigationResize.root,
    outerSplit.secondary,
  )
  innerSplit.root.replaceChildren(
    innerSplit.primary,
    contextResize.root,
    innerSplit.secondary,
  )
  desktopControls.append(collapseNavigation.root, collapseContext.root)
  narrowBar.append(openNavigation.root, openContext.root)
  workspace.append(desktopControls, narrowBar, outerSplit.root, artifacts)
  content.append(workspace, navigationDrawer.root, contextDrawer.root)
  layout.append(status, error, content)
  options.root.replaceChildren(layout)

  const candidateView = mountStrongFlowCandidate({
    document,
    limits,
    onLoadFiles() { void options.model.loadCandidateFiles() },
    onLoadMoreFiles() { void options.model.loadMoreCandidateFiles() },
    onSelectFile(path) { void options.model.selectCandidateFile(path) },
    onLoadMoreDiff() { void options.model.loadMoreCandidateDiff() },
  })
  candidateHost.append(candidateView.root)

  const deliveryRows = new WeakMap<HTMLLIElement, {
    readonly link: HTMLAnchorElement
    readonly status: HTMLElement
  }>()
  const deliveryCollection = mountKeyedCollection({
    parent: deliveries,
    key: (delivery: DeliveryProjection) => delivery.deliveryId,
    create() {
      const item = document.createElement('li')
      const link = document.createElement('a')
      const deliveryStatus = document.createElement('span')
      item.append(link, deliveryStatus)
      deliveryRows.set(item, { link, status: deliveryStatus })
      return item
    },
    update(item, delivery: DeliveryProjection) {
      const row = deliveryRows.get(item)
      if (row === undefined) return
      row.link.href = `#/strongflow?delivery=${encodeURIComponent(delivery.deliveryId)}`
      row.link.textContent = delivery.title
      row.link.dataset.deliveryId = delivery.deliveryId
      row.status.textContent = `${delivery.status} · r${String(delivery.revision)}`
      if (delivery.deliveryId === activeDeliveryId) {
        row.link.setAttribute('aria-current', 'page')
      } else {
        row.link.removeAttribute('aria-current')
      }
    },
    remove(item) { deliveryRows.delete(item) },
  })

  const taskRows = new WeakMap<HTMLLIElement, {
    readonly title: HTMLElement
    readonly status: HTMLElement
  }>()
  const taskCollection = mountKeyedCollection({
    parent: tasks,
    key: (task: DeliveryTaskDetailProjection) => task.id,
    create() {
      const item = document.createElement('li')
      const title = document.createElement('strong')
      const taskStatus = document.createElement('span')
      item.append(title, taskStatus)
      taskRows.set(item, { title, status: taskStatus })
      return item
    },
    update(item, task: DeliveryTaskDetailProjection) {
      const row = taskRows.get(item)
      if (row === undefined) return
      item.dataset.status = task.status
      row.title.textContent = task.title
      row.status.textContent = task.status
    },
    remove(item) { taskRows.delete(item) },
  })

  const stageCollection = mountKeyedCollection({
    parent: stages,
    key: (stage: DeliveryStageProjection) => stage.id,
    create: () => document.createElement('li'),
    update(item, stage: DeliveryStageProjection) {
      item.dataset.status = stage.status
      item.textContent = `${stage.stage} · ${stage.role} · ${stage.status}`
    },
  })

  const attentionCollection = mountKeyedCollection({
    parent: attention,
    key: (record: DeliveryAttentionProjection) => record.id,
    create: () => document.createElement('li'),
    update(item, record: DeliveryAttentionProjection) {
      item.dataset.status = record.status
      item.textContent = `${record.title} · ${record.status}`
    },
  })

  type ReviewNode = { readonly id: string; readonly label: string }
  interface AttentionActionItem {
    readonly record: DeliveryAttentionProjection
    readonly busy: boolean
    readonly interactionSettled: boolean
    readonly interactionCancelled: boolean
    readonly deliveryId: string
    readonly deliveryRevision: number
    readonly candidateDigest: string
    readonly tasks: readonly DeliveryTaskDetailProjection[]
    readonly nodes: readonly ReviewNode[]
    readonly candidateAvailable: boolean
  }
  interface AttentionActionRow {
    current: AttentionActionItem
    readonly title: HTMLElement
    readonly resolution: HTMLTextAreaElement
    readonly task: HTMLSelectElement
    readonly node: HTMLSelectElement
    readonly instructions: HTMLTextAreaElement
    readonly reworkFields: HTMLElement
    readonly resolve: HTMLButtonElement
    readonly dismiss: HTMLButtonElement
    readonly rework: HTMLButtonElement
    readonly conflict: HTMLElement
    readonly conflictText: HTMLElement
    readonly keepDraft: HTMLButtonElement
    readonly useServer: HTMLButtonElement
    readonly draft: EditableDraft<AttentionDraftValues>
    readonly taskOptions: KeyedCollectionView<
      DeliveryTaskDetailProjection,
      string,
      HTMLOptionElement
    >
    readonly nodeOptions: KeyedCollectionView<ReviewNode, string, HTMLOptionElement>
    readonly onResolve: () => void
    readonly onDismiss: () => void
    readonly onRework: () => void
    readonly onResolutionInput: () => void
    readonly onTaskChange: () => void
    readonly onNodeChange: () => void
    readonly onInstructionsInput: () => void
    readonly onKeepDraft: () => void
    readonly onUseServer: () => void
  }
  type AttentionDraftValues = {
    readonly resolution: string
    readonly taskId: string
    readonly nodeId: string
    readonly instructions: string
  }
  const attentionActionRows = new WeakMap<HTMLElement, AttentionActionRow>()
  const attentionActionCollection = mountKeyedCollection({
    parent: attentionActions,
    key: (item: AttentionActionItem) => item.record.id,
    create(item: AttentionActionItem) {
      const group = strongFlowElement(document, 'div', 'wwc-strongflow-attention-actions')
      const title = document.createElement('strong')
      const resolutionLabel = document.createElement('label')
      const resolution = document.createElement('textarea')
      const resolve = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-resolve-attention',
      ) as HTMLButtonElement
      const dismiss = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-dismiss-attention',
      ) as HTMLButtonElement
      const reworkFields = strongFlowElement(document, 'div', 'wwc-strongflow-rework-fields')
      const taskLabel = document.createElement('label')
      const task = document.createElement('select')
      const nodeLabel = document.createElement('label')
      const node = document.createElement('select')
      const instructionsLabel = document.createElement('label')
      const instructions = document.createElement('textarea')
      const rework = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-rework',
      ) as HTMLButtonElement
      const conflict = strongFlowElement(document, 'div', 'wwc-strongflow-attention-conflict')
      const conflictText = strongFlowElement(
        document,
        'p',
        'wwc-strongflow-attention-conflict-text',
      )
      const keepDraft = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-attention-keep-draft',
      ) as HTMLButtonElement
      const useServer = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-attention-use-server',
      ) as HTMLButtonElement
      const draft = createEditableDraft<AttentionDraftValues>({ revisionSensitive: true })
      resolutionLabel.textContent = 'Decision note'
      resolutionLabel.append(resolution)
      resolve.type = 'button'
      resolve.textContent = 'Resolve'
      dismiss.type = 'button'
      dismiss.textContent = 'Dismiss'
      taskLabel.textContent = 'Current task'
      taskLabel.append(task)
      nodeLabel.textContent = 'Current solution node'
      nodeLabel.append(node)
      instructionsLabel.textContent = 'Bounded rework instructions'
      instructionsLabel.append(instructions)
      rework.type = 'button'
      rework.textContent = 'Approve bounded rework'
      conflict.setAttribute('role', 'alert')
      conflict.hidden = true
      keepDraft.type = 'button'
      keepDraft.textContent = 'Keep my draft'
      useServer.type = 'button'
      useServer.textContent = 'Use current server target'
      conflict.append(conflictText, keepDraft, useServer)
      const taskOptions = mountKeyedCollection<
        DeliveryTaskDetailProjection,
        string,
        HTMLOptionElement
      >({
        parent: task,
        key: option => option.id,
        create: () => document.createElement('option'),
        update(option, currentTask) {
          option.value = currentTask.id
          option.textContent = currentTask.title
        },
      })
      const nodeOptions = mountKeyedCollection<ReviewNode, string, HTMLOptionElement>({
        parent: node,
        key: option => option.id,
        create: () => document.createElement('option'),
        update(option, currentNode) {
          option.value = currentNode.id
          option.textContent = currentNode.label
        },
      })
      const decide = (decision: 'resolve' | 'dismiss', remediation: boolean) => {
        if (readOnly) return
        const row = attentionActionRows.get(group)
        if (row === undefined) return
        row.draft.edit('resolution', row.resolution.value)
        row.draft.edit('taskId', row.task.value)
        row.draft.edit('nodeId', row.node.value)
        row.draft.edit('instructions', row.instructions.value)
        const submission = row.draft.beginSubmission()
        if (submission === null) return
        void options.model.resolveAttention({
          attentionItemId: row.current.record.id,
          decision,
          resolution: submission.values.resolution,
          remediation: remediation
            ? {
                deliveryTaskId: submission.values.taskId.length === 0
                  ? null
                  : submission.values.taskId as DeliveryTaskDetailProjection['id'],
                nodeId: submission.values.nodeId,
                instructions: submission.values.instructions,
              }
            : null,
        })
      }
      const onResolve = () => { decide('resolve', false) }
      const onDismiss = () => { decide('dismiss', false) }
      const onRework = () => { decide('resolve', true) }
      const onResolutionInput = () => { draft.edit('resolution', resolution.value) }
      const onTaskChange = () => { draft.edit('taskId', task.value) }
      const onNodeChange = () => { draft.edit('nodeId', node.value) }
      const onInstructionsInput = () => { draft.edit('instructions', instructions.value) }
      const onKeepDraft = () => {
        draft.resolveConflicts('keep-draft')
        renderActions(options.model.state)
      }
      const onUseServer = () => {
        draft.resolveConflicts('use-server')
        renderActions(options.model.state)
      }
      resolve.addEventListener('click', onResolve)
      dismiss.addEventListener('click', onDismiss)
      rework.addEventListener('click', onRework)
      resolution.addEventListener('input', onResolutionInput)
      task.addEventListener('change', onTaskChange)
      node.addEventListener('change', onNodeChange)
      instructions.addEventListener('input', onInstructionsInput)
      keepDraft.addEventListener('click', onKeepDraft)
      useServer.addEventListener('click', onUseServer)
      reworkFields.append(taskLabel, nodeLabel, instructionsLabel, rework)
      group.append(title, resolutionLabel, conflict, resolve, dismiss, reworkFields)
      attentionActionRows.set(group, {
        current: item,
        title,
        resolution,
        task,
        node,
        instructions,
        reworkFields,
        resolve,
        dismiss,
        rework,
        conflict,
        conflictText,
        keepDraft,
        useServer,
        draft,
        taskOptions,
        nodeOptions,
        onResolve,
        onDismiss,
        onRework,
        onResolutionInput,
        onTaskChange,
        onNodeChange,
        onInstructionsInput,
        onKeepDraft,
        onUseServer,
      })
      return group
    },
    update(group, item: AttentionActionItem) {
      const row = attentionActionRows.get(group)
      if (row === undefined) return
      row.current = item
      if (row.draft.state.scope !== null && row.draft.state.submission === null) {
        row.draft.edit('resolution', row.resolution.value)
        row.draft.edit('taskId', row.task.value)
        row.draft.edit('nodeId', row.node.value)
        row.draft.edit('instructions', row.instructions.value)
      }
      if (item.interactionSettled && row.draft.state.submission !== null) {
        row.draft.finishSubmission(item.interactionCancelled ? 'cancelled' : 'failure')
      }
      row.draft.synchronize({
        scope: `${pageDraftScope}:${item.deliveryId}:${item.record.id}:${item.candidateDigest}`,
        revision: item.deliveryRevision,
        values: {
          resolution: '',
          taskId: item.tasks[0]?.id ?? '',
          nodeId: item.nodes[0]?.id ?? '',
          instructions: '',
        },
      })
      row.title.textContent = item.record.title
      group.dataset.attentionItemId = item.record.id
      const decisionDisabled = readOnly || item.busy || row.draft.state.revisionConflict
      row.resolve.disabled = decisionDisabled
      row.dismiss.disabled = decisionDisabled
      row.rework.disabled = decisionDisabled
      const reworkVisible = item.record.type === 'verification_blocked'
        && item.candidateAvailable
        && item.nodes.length > 0
      row.reworkFields.hidden = !reworkVisible
      row.taskOptions.update(reworkVisible ? item.tasks : [])
      row.nodeOptions.update(reworkVisible ? item.nodes : [])
      const draftValues = row.draft.state.values
      if (row.resolution.value !== draftValues.resolution) row.resolution.value = draftValues.resolution
      if (row.task.value !== draftValues.taskId) row.task.value = draftValues.taskId
      if (row.node.value !== draftValues.nodeId) row.node.value = draftValues.nodeId
      if (row.instructions.value !== draftValues.instructions) {
        row.instructions.value = draftValues.instructions
      }
      row.resolution.disabled = readOnly || item.busy
      row.task.disabled = readOnly || item.busy
      row.node.disabled = readOnly || item.busy
      row.instructions.disabled = readOnly || item.busy
      row.conflict.hidden = !row.draft.state.revisionConflict
      row.conflictText.textContent = row.draft.state.revisionConflict
        ? `This Attention draft started at Delivery revision ${String(
            row.draft.state.baseRevision,
          )}; the server is now at revision ${String(row.draft.state.serverRevision)}.`
        : ''
      row.keepDraft.disabled = readOnly || item.busy
      row.useServer.disabled = readOnly || item.busy
    },
    remove(group) {
      const row = attentionActionRows.get(group)
      if (row === undefined) return
      row.resolution.value = ''
      row.instructions.value = ''
      row.resolve.removeEventListener('click', row.onResolve)
      row.dismiss.removeEventListener('click', row.onDismiss)
      row.rework.removeEventListener('click', row.onRework)
      row.resolution.removeEventListener('input', row.onResolutionInput)
      row.task.removeEventListener('change', row.onTaskChange)
      row.node.removeEventListener('change', row.onNodeChange)
      row.instructions.removeEventListener('input', row.onInstructionsInput)
      row.keepDraft.removeEventListener('click', row.onKeepDraft)
      row.useServer.removeEventListener('click', row.onUseServer)
      row.taskOptions.close()
      row.nodeOptions.close()
      row.draft.reset()
      attentionActionRows.delete(group)
    },
  })

  const onApproveSolution = () => {
    if (readOnly) return
    reviewDraft.edit('comments', comments.value)
    reviewDraft.edit('requestedChanges', changes.value)
    const submission = reviewDraft.beginSubmission()
    if (submission === null) return
    void options.model.decideSolutionReview({
      action: 'approve',
      comments: submission.values.comments,
      requestedChanges: [],
    })
  }
  const onRequestChanges = () => {
    if (readOnly) return
    reviewDraft.edit('comments', comments.value)
    reviewDraft.edit('requestedChanges', changes.value)
    const submission = reviewDraft.beginSubmission()
    if (submission === null) return
    void options.model.decideSolutionReview({
      action: 'request_changes',
      comments: submission.values.comments,
      requestedChanges: submission.values.requestedChanges.split(/\r?\n/u),
    })
  }
  const onRejectSolution = () => {
    if (readOnly) return
    reviewDraft.edit('comments', comments.value)
    reviewDraft.edit('requestedChanges', changes.value)
    const submission = reviewDraft.beginSubmission()
    if (submission === null) return
    void options.model.decideSolutionReview({
      action: 'reject',
      comments: submission.values.comments,
      requestedChanges: [],
    })
  }
  const onApproveTasks = () => { if (!readOnly) void options.model.approveTaskBreakdown() }
  const onSubmitVerdict = () => { if (!readOnly) void options.model.submitVerdict() }
  const onAdvanceDelivery = () => { if (!readOnly) void options.model.advanceDelivery() }
  const onRetry = () => { void options.model.refresh() }
  const onReconnect = () => { options.model.reconnect() }
  const onReviewCommentsInput = () => { reviewDraft.edit('comments', comments.value) }
  const onReviewChangesInput = () => {
    reviewDraft.edit('requestedChanges', changes.value)
  }
  const onKeepReviewDraft = () => {
    reviewDraft.resolveConflicts('keep-draft')
    renderActions(options.model.state)
  }
  const onUseServerReview = () => {
    reviewDraft.resolveConflicts('use-server')
    renderActions(options.model.state)
  }
  approveSolution.addEventListener('click', onApproveSolution)
  requestChanges.addEventListener('click', onRequestChanges)
  rejectSolution.addEventListener('click', onRejectSolution)
  approveTasks.addEventListener('click', onApproveTasks)
  submitVerdict.addEventListener('click', onSubmitVerdict)
  advanceDelivery.addEventListener('click', onAdvanceDelivery)
  retry.addEventListener('click', onRetry)
  reconnect.addEventListener('click', onReconnect)
  comments.addEventListener('input', onReviewCommentsInput)
  changes.addEventListener('input', onReviewChangesInput)
  keepReviewDraft.addEventListener('click', onKeepReviewDraft)
  useServerReview.addEventListener('click', onUseServerReview)

  function renderDeliveries(state: StrongFlowViewModelState): void {
    const active = state.projection?.delivery ?? null
    activeDeliveryId = active?.deliveryId ?? null
    const byId = new Map(
      (options.deliveries ?? []).map(delivery => [delivery.deliveryId, delivery]),
    )
    if (active !== null && !byId.has(active.deliveryId)) {
      byId.set(active.deliveryId, {
        schemaVersion: active.schemaVersion,
        deliveryId: active.deliveryId,
        revision: active.deliveryRevision,
        status: active.status,
        title: active.requirements.title,
        updatedAt: state.projection?.metadata.updatedAt ?? '',
        ownership: active.ownership,
        activeStageRunId: state.projection?.stage.id ?? null,
        openAttentionCount: active.attention.filter(item => item.status === 'open').length,
        taskCounts: {
          total: active.tasks.length,
          pending: active.tasks.filter(item => item.status === 'pending').length,
          active: active.tasks.filter(item => item.status === 'active').length,
          blocked: active.tasks.filter(item => item.status === 'blocked').length,
          verifying: active.tasks.filter(item => item.status === 'verifying').length,
          completed: active.tasks.filter(item => item.status === 'completed').length,
          failed: active.tasks.filter(item => item.status === 'failed').length,
        },
      })
    }
    const bounded = boundedItems([...byId.values()], limits.deliveries)
    deliveryCollection.update(bounded.items)
    updateOmitted(deliveriesOmitted, bounded.omitted, 'Deliveries')
  }

  function renderProjection(
    projection: StrongFlowProjection | null,
    stateStatus: string,
    candidateFiles: StrongFlowViewModelState['candidateFiles'],
  ): void {
    empty.hidden = projection !== null
    overview.hidden = projection === null
    tasksSection.hidden = projection === null
    stagesSection.hidden = projection === null
    attentionSection.hidden = projection === null
    diagramsHost.hidden = projection === null
    candidateHost.hidden = projection === null
    if (projection === null) {
      empty.textContent = stateStatus === 'loading' || stateStatus === 'refreshing'
        ? 'Loading the exact Delivery snapshot…'
        : 'Select a Delivery to open StrongFlow.'
      taskCollection.update([])
      stageCollection.update([])
      attentionCollection.update([])
      updateOmitted(tasksOmitted, 0, 'tasks')
      updateOmitted(stagesOmitted, 0, 'stages')
      updateOmitted(attentionOmitted, 0, 'Attention records')
      if (diagramsNode !== null) diagramsNode.remove()
      if (contextEvidenceNode !== null) contextEvidenceNode.remove()
      if (artifactEvidenceNode !== null) artifactEvidenceNode.remove()
      diagramsNode = null
      contextEvidenceNode = null
      artifactEvidenceNode = null
      diagramsFingerprint = null
      lastEvidenceKey = null
      candidateView.update({ projection: null, candidateFiles })
      return
    }

    heading.textContent = projection.delivery.requirements.title
    goal.textContent = projection.delivery.requirements.goal
    metadata.textContent = `Delivery r${String(
      projection.metadata.revisions.delivery,
    )} · Runtime r${String(projection.metadata.revisions.runtime)} · updated ${projection.metadata.updatedAt}`
    const boundedTasks = boundedItems(projection.delivery.tasks, limits.tasks)
    const boundedStages = boundedItems(projection.delivery.stages, limits.stages)
    const boundedAttention = boundedItems(projection.attention, limits.attention)
    taskCollection.update(boundedTasks.items)
    stageCollection.update(boundedStages.items)
    attentionCollection.update(boundedAttention.items)
    updateOmitted(tasksOmitted, boundedTasks.omitted, 'tasks')
    updateOmitted(stagesOmitted, boundedStages.omitted, 'stages')
    updateOmitted(attentionOmitted, boundedAttention.omitted, 'Attention records')

    const evidenceKey = JSON.stringify(projection.evidence)
    const nextDiagramsFingerprint = JSON.stringify([
      projection.solutionReview?.architectureDiagram ?? null,
      projection.solutionReview?.processDiagram ?? null,
      projection.runtime.sessions,
    ])
    if (diagramsNode === null || diagramsFingerprint !== nextDiagramsFingerprint) {
      diagramsNode?.remove()
      diagramsNode = renderStrongFlowDiagrams(document, projection, limits)
      diagramsHost.append(diagramsNode)
      diagramsFingerprint = nextDiagramsFingerprint
    }
    candidateView.update({ projection, candidateFiles })
    if (
      contextEvidenceNode === null
      || artifactEvidenceNode === null
      || lastEvidenceKey !== evidenceKey
    ) {
      contextEvidenceNode?.remove()
      artifactEvidenceNode?.remove()
      contextEvidenceNode = renderEvidencePanel(document, projection, limits)
      artifactEvidenceNode = renderEvidencePanel(document, projection, limits)
      contextEvidenceHost.append(contextEvidenceNode)
      artifactEvidenceHost.append(artifactEvidenceNode)
    }
    lastEvidenceKey = evidenceKey
  }

  function renderActions(state: StrongFlowViewModelState): void {
    const projection = state.projection
    const interaction = state.interaction ?? { status: 'idle', error: null }
    const busy = interaction.status === 'submitting'
      || interaction.status === 'waiting'
      || state.status === 'loading'
      || state.status === 'refreshing'
      || state.realtime === 'reloading'
    actions.setAttribute('aria-busy', String(busy))
    const review = projection?.solutionReview ?? null
    const pendingReview = review?.reviewStatus === 'pending'
    if (reviewDraft.state.scope !== null && reviewDraft.state.submission === null) {
      reviewDraft.edit('comments', comments.value)
      reviewDraft.edit('requestedChanges', changes.value)
    }
    if (
      (interaction.status === 'error' || interaction.status === 'idle')
      && reviewDraft.state.submission !== null
    ) {
      reviewDraft.finishSubmission(
        interaction.error?.kind === 'cancelled' ? 'cancelled' : 'failure',
      )
    }
    reviewDraft.synchronize(pendingReview && projection !== null
      ? {
          scope: `${pageDraftScope}:${projection.delivery.deliveryId}:${String(
            review?.attentionItemId ?? '',
          )}:${projection.currentCandidate?.diffSha256 ?? ''}`,
          revision: projection.metadata.revisions.delivery,
          values: { comments: '', requestedChanges: '' },
        }
      : null)
    const reviewValues = reviewDraft.state.values
    if (comments.value !== (reviewValues.comments ?? '')) {
      comments.value = reviewValues.comments ?? ''
    }
    if (changes.value !== (reviewValues.requestedChanges ?? '')) {
      changes.value = reviewValues.requestedChanges ?? ''
    }
    reviewConflict.hidden = !reviewDraft.state.revisionConflict
    reviewConflictText.textContent = reviewDraft.state.revisionConflict
      ? `This review draft started at Delivery revision ${String(
          reviewDraft.state.baseRevision,
        )}; the server is now at revision ${String(reviewDraft.state.serverRevision)}.`
      : ''
    solutionActions.hidden = !pendingReview
    const reviewDisabled = readOnly || busy || reviewDraft.state.revisionConflict
    comments.disabled = readOnly || busy
    changes.disabled = readOnly || busy
    approveSolution.disabled = reviewDisabled
    requestChanges.disabled = reviewDisabled
    rejectSolution.disabled = reviewDisabled
    keepReviewDraft.disabled = readOnly || busy
    useServerReview.disabled = readOnly || busy
    approveTasks.hidden = review?.reviewStatus !== 'approved'
      || (projection?.delivery.tasks.length ?? 0) > 0
    approveTasks.disabled = readOnly || busy
    const verdictVisible = projection !== null && canSubmitStrongFlowVerdict(projection)
    if (verdictVisible && submitVerdict.parentNode === null) {
      actions.insertBefore(submitVerdict, attentionActions)
    } else if (!verdictVisible) {
      submitVerdict.remove()
    }
    submitVerdict.disabled = readOnly || busy
    advanceDelivery.hidden = projection?.delivery.status !== 'ready-to-deliver'
    advanceDelivery.disabled = readOnly || busy
    const nodes: readonly ReviewNode[] = review === null
      ? []
      : boundedItems(
          [...review.architectureDiagram.nodes, ...review.processDiagram.nodes],
          limits.graphNodes,
        ).items
    const reviewTasks = projection === null
      ? []
      : boundedItems(projection.delivery.tasks, limits.tasks).items
    const openAttention = boundedItems(
      projection?.attention.filter(item => item.status === 'open') ?? [],
      limits.attention,
    )
    attentionActionCollection.update(openAttention.items.map(record => ({
      record,
      busy,
      interactionSettled: interaction.status === 'error' || interaction.status === 'idle',
      interactionCancelled: interaction.error?.kind === 'cancelled',
      deliveryId: projection?.delivery.deliveryId ?? '',
      deliveryRevision: projection?.metadata.revisions.delivery ?? 0,
      candidateDigest: projection?.currentCandidate?.diffSha256 ?? '',
      tasks: reviewTasks,
      nodes,
      candidateAvailable: projection?.currentCandidate !== null,
    })))
    updateOmitted(actionsOmitted, openAttention.omitted, 'Attention actions')
  }

  function mountRegion(target: HTMLElement, nodes: readonly HTMLElement[]): void {
    if (
      target.children.length === nodes.length
      && nodes.every((node, index) => target.children[index] === node)
    ) return
    target.replaceChildren(...nodes)
  }

  function renderLayout(mode: 'wide' | 'narrow'): void {
    const narrow = mode === 'narrow'
    const layoutKey = [
      mode,
      preferences.navigationWidth,
      preferences.contextWidth,
      preferences.navigationCollapsed,
      preferences.contextCollapsed,
      preferences.artifactsTab,
      navigationDrawerOpen,
      contextDrawerOpen,
    ].join(':')

    for (const tab of STRONGFLOW_ARTIFACTS_TABS) {
      const panel = artifactPanels.get(tab)
      if (panel !== undefined) panel.hidden = tab !== preferences.artifactsTab
    }
    const diagramPanel = artifactPanels.get(
      preferences.artifactsTab === 'execution' ? 'execution' : 'solution',
    )
    if (diagramPanel !== undefined) mountRegion(diagramPanel, [diagramsHost])
    if (diagramsNode !== null) {
      for (const child of [...diagramsNode.children] as HTMLElement[]) {
        if (child.className === 'wwc-strongflow-view-solution') {
          child.hidden = preferences.artifactsTab === 'execution'
        } else if (child.className === 'wwc-strongflow-view-execution') {
          child.hidden = preferences.artifactsTab !== 'execution'
        }
      }
    }
    if (lastLayoutKey === layoutKey) return
    lastLayoutKey = layoutKey

    workspace.dataset.viewport = mode
    workspace.dataset.navigationWidth = String(preferences.navigationWidth)
    workspace.dataset.contextWidth = String(preferences.contextWidth)
    workspace.dataset.navigationCollapsed = String(preferences.navigationCollapsed)
    workspace.dataset.contextCollapsed = String(preferences.contextCollapsed)
    workspace.setAttribute('data-viewport', mode)
    workspace.setAttribute('data-navigation-width', String(preferences.navigationWidth))
    workspace.setAttribute('data-context-width', String(preferences.contextWidth))
    workspace.setAttribute(
      'data-navigation-collapsed',
      String(preferences.navigationCollapsed),
    )
    workspace.setAttribute(
      'data-context-collapsed',
      String(preferences.contextCollapsed),
    )

    mountRegion(
      narrow ? navigationDrawerContent : navigation,
      [deliveriesRoot, tasksSection, stagesSection],
    )
    mountRegion(
      narrow ? contextDrawerContent : context,
      [attentionSection, contextEvidenceHost],
    )

    desktopControls.hidden = narrow
    narrowBar.hidden = !narrow
    outerSplit.primary.hidden = narrow || preferences.navigationCollapsed
    navigationResize.root.hidden = narrow || preferences.navigationCollapsed
    contextResize.root.hidden = narrow || preferences.contextCollapsed
    innerSplit.update({
      primary: mainRegion,
      primaryLabel: 'Delivery main content',
      secondary: context,
      secondaryLabel: 'Attention and Evidence context',
      secondaryHidden: narrow || preferences.contextCollapsed,
      className: 'wwc-strongflow-main-context-split',
    })
    outerSplit.update({
      primary: navigation,
      primaryLabel: 'Delivery and Task navigation',
      secondary: innerSplit.root,
      secondaryLabel: 'Delivery review workspace',
      className: 'wwc-strongflow-navigation-split',
    })
    outerSplit.root.dataset.primaryHidden = String(
      narrow || preferences.navigationCollapsed,
    )
    innerSplit.root.dataset.secondaryHidden = String(
      narrow || preferences.contextCollapsed,
    )
    navigationResize.update(preferences.navigationWidth)
    contextResize.update(preferences.contextWidth)

    const navigationExpanded = !preferences.navigationCollapsed
    collapseNavigation.update({
      className: 'wwc-strongflow-collapse-navigation',
      label: navigationExpanded ? 'Collapse navigation pane' : 'Expand navigation pane',
      accessibleName: navigationExpanded
        ? 'Collapse navigation pane'
        : 'Expand navigation pane',
      variant: 'ghost',
      onActivate: () => {
        persist({
          ...preferences,
          navigationCollapsed: !preferences.navigationCollapsed,
        })
      },
    })
    collapseNavigation.root.setAttribute('aria-controls', navigation.id)
    collapseNavigation.root.setAttribute('aria-expanded', String(navigationExpanded))
    const contextExpanded = !preferences.contextCollapsed
    collapseContext.update({
      className: 'wwc-strongflow-collapse-context',
      label: contextExpanded ? 'Collapse context pane' : 'Expand context pane',
      accessibleName: contextExpanded ? 'Collapse context pane' : 'Expand context pane',
      variant: 'ghost',
      onActivate: () => {
        persist({ ...preferences, contextCollapsed: !preferences.contextCollapsed })
      },
    })
    collapseContext.root.setAttribute('aria-controls', context.id)
    collapseContext.root.setAttribute('aria-expanded', String(contextExpanded))

    const restoreArtifactTabFocus = STRONGFLOW_ARTIFACTS_TABS.some(
      tab => artifactTabs.tab(tab) === document.activeElement,
    )
    artifactTabs.update({
      id: 'wwc-strongflow-artifact-tab',
      label: 'Delivery artifacts',
      tabs: tabItems,
      selectedId: preferences.artifactsTab,
      className: 'wwc-strongflow-artifact-tabs',
      onSelect: selectArtifactTab,
    })
    for (const tab of STRONGFLOW_ARTIFACTS_TABS) {
      const tabButton = artifactTabs.tab(tab)
      tabButton.className = 'wwc-strongflow-artifact-tab'
      tabButton.dataset.artifactTab = tab
    }
    if (restoreArtifactTabFocus) artifactTabs.tab(preferences.artifactsTab).focus()

    openNavigation.update({
      className: 'wwc-strongflow-open-navigation',
      label: 'Deliveries and tasks',
      accessibleName: 'Open Delivery and Task navigation',
      onActivate: () => {
        navigationDrawerOpen = !navigationDrawerOpen
        render(options.model.state)
      },
    })
    openNavigation.root.setAttribute('aria-controls', navigationDrawer.root.id)
    openNavigation.root.setAttribute(
      'aria-expanded',
      String(narrow && navigationDrawerOpen),
    )
    openContext.update({
      className: 'wwc-strongflow-open-context',
      label: 'Attention and Evidence',
      accessibleName: 'Open Attention and Evidence context',
      onActivate: () => {
        contextDrawerOpen = !contextDrawerOpen
        render(options.model.state)
      },
    })
    openContext.root.setAttribute('aria-controls', contextDrawer.root.id)
    openContext.root.setAttribute('aria-expanded', String(narrow && contextDrawerOpen))
    navigationDrawer.update({
      id: 'wwc-strongflow-navigation-drawer',
      title: 'Delivery and Task navigation',
      open: narrow && navigationDrawerOpen,
      content: navigationDrawerContent,
      closeLabel: 'Close Delivery and Task navigation',
      className: 'wwc-strongflow-navigation-drawer',
      onClose: () => {
        navigationDrawerOpen = false
        render(options.model.state)
      },
    })
    contextDrawer.update({
      id: 'wwc-strongflow-context-drawer',
      title: 'Attention and Evidence',
      open: narrow && contextDrawerOpen,
      content: contextDrawerContent,
      closeLabel: 'Close Attention and Evidence context',
      className: 'wwc-strongflow-context-drawer',
      onClose: () => {
        contextDrawerOpen = false
        render(options.model.state)
      },
    })
  }

  function render(state: StrongFlowViewModelState): void {
    if (closed) return
    const presentation = strongFlowPagePresentation(state)
    status.textContent = presentation.statusText
    content.setAttribute('aria-busy', String(presentation.busy))
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    renderDeliveries(state)
    renderProjection(state.projection, state.status, state.candidateFiles)
    renderActions(state)
    renderLayout(strongFlowLayoutMode(viewport.width))
  }

  const onWindowResize = () => { render(options.model.state) }
  browserWindow?.addEventListener('resize', onWindowResize)
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      browserWindow?.removeEventListener('resize', onWindowResize)
      retry.removeEventListener('click', onRetry)
      reconnect.removeEventListener('click', onReconnect)
      comments.removeEventListener('input', onReviewCommentsInput)
      changes.removeEventListener('input', onReviewChangesInput)
      keepReviewDraft.removeEventListener('click', onKeepReviewDraft)
      useServerReview.removeEventListener('click', onUseServerReview)
      approveSolution.removeEventListener('click', onApproveSolution)
      requestChanges.removeEventListener('click', onRequestChanges)
      rejectSolution.removeEventListener('click', onRejectSolution)
      approveTasks.removeEventListener('click', onApproveTasks)
      submitVerdict.removeEventListener('click', onSubmitVerdict)
      advanceDelivery.removeEventListener('click', onAdvanceDelivery)
      comments.value = ''
      changes.value = ''
      reviewDraft.reset()
      attentionActionCollection.close()
      attentionCollection.close()
      stageCollection.close()
      taskCollection.close()
      deliveryCollection.close()
      diagramsNode?.remove()
      candidateView.close()
      contextEvidenceNode?.remove()
      artifactEvidenceNode?.remove()
      navigationResize.close()
      contextResize.close()
      navigationDrawer.close()
      contextDrawer.close()
      openNavigation.close()
      openContext.close()
      collapseNavigation.close()
      collapseContext.close()
      artifactTabs.close()
      outerSplit.close()
      innerSplit.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
