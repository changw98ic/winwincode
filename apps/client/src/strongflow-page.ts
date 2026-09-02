// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryAttentionProjection,
  DeliveryProjection,
  DeliveryStageProjection,
  DeliveryTaskDetailProjection,
  RepositoryScope,
} from './generated/contracts.js'
import { mountButton } from './components/button.js'
import { mountEmptyState } from './components/empty-state.js'
import { mountFormField } from './components/form-field.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
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

export interface StrongFlowPageOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowViewModel
  readonly deliveries?: readonly DeliveryProjection[]
  readonly limits?: StrongFlowRenderLimits
}

export interface StrongFlowPage {
  close(): void
}

export interface StrongFlowCreatePageOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowCreateViewModel
  readonly scope: RepositoryScope
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
  const document = options.root.ownerDocument
  const layout = strongFlowElement(document, 'div', 'wwc-strongflow wwc-strongflow-create')
  const status = strongFlowElement(document, 'p', 'wwc-strongflow-create-status')
  const content = strongFlowElement(document, 'main', 'wwc-strongflow-create-content')
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
  goal.required = true
  repository.type = 'text'
  repository.readOnly = true
  repository.value = options.scope.repositoryId
  baseline.type = 'text'
  baseline.required = true
  repositoryScope.type = 'text'
  repositoryScope.readOnly = true
  repositoryScope.value = [
    options.scope.organizationId,
    options.scope.workspaceId,
    options.scope.projectId,
    options.scope.repositoryId,
  ].join(' / ')
  deliveryScope.required = true
  criteria.required = true

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
      onActivate() { options.model.cancelPending() },
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
      disabled: state.status === 'created' || state.status === 'closed',
      type: 'submit',
      variant: 'primary',
    })
    cancel.root.hidden = !busy
    cancel.update({
      className: 'wwc-strongflow-create-cancel',
      label: 'Cancel pending creation',
      type: 'button',
      onActivate() { options.model.cancelPending() },
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
      criteriaField.close()
      submit.close()
      cancel.close()
      empty.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}

/** Mount the advanced StrongFlow workspace against its Control Plane view-model. */
export function mountStrongFlowPage(options: StrongFlowPageOptions): StrongFlowPage {
  const document = options.root.ownerDocument
  const limits = options.limits ?? DEFAULT_STRONGFLOW_RENDER_LIMITS
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
  const deliveriesRoot = strongFlowElement(document, 'aside', 'wwc-strongflow-deliveries')
  const deliveriesHeading = strongFlowElement(
    document,
    'h2',
    'wwc-strongflow-deliveries-heading',
  )
  const deliveries = strongFlowElement(document, 'ul', 'wwc-strongflow-delivery-list')
  const deliveriesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const workspace = strongFlowElement(document, 'main', 'wwc-strongflow-workspace')
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
  const diagramsHost = strongFlowElement(document, 'div', 'wwc-strongflow-diagrams-host')
  const candidateHost = strongFlowElement(document, 'div', 'wwc-strongflow-candidate-host')
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
  let diagramsNode: HTMLElement | null = null
  let lastSolutionReview: StrongFlowProjection['solutionReview'] | null = null
  let lastRuntime: StrongFlowProjection['runtime'] | null = null
  let solutionDraftKey: string | null = null

  function updateOmitted(node: HTMLElement, count: number, label: string): void {
    node.hidden = count === 0
    const text = `${String(count)} more ${label} not shown.`
    if (node.textContent !== text) node.textContent = text
  }

  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  retry.type = 'button'
  retry.textContent = 'Retry snapshot'
  reconnect.type = 'button'
  reconnect.textContent = 'Reconnect events'
  deliveriesHeading.textContent = 'Deliveries'
  tasksHeading.textContent = 'Tasks'
  stagesHeading.textContent = 'Stages'
  attentionHeading.textContent = 'Attention'
  actionsHeading.textContent = 'Review actions'
  commentsLabel.textContent = 'Review comments'
  commentsLabel.append(comments)
  changesLabel.textContent = 'Requested changes, one per line'
  changesLabel.append(changes)
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
  solutionActions.append(
    commentsLabel,
    changesLabel,
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
  details.append(
    empty,
    overview,
    tasksSection,
    stagesSection,
    attentionSection,
    diagramsHost,
    candidateHost,
  )
  actions.append(
    actionsHeading,
    solutionActions,
    approveTasks,
    submitVerdict,
    attentionActions,
    actionsOmitted,
    advanceDelivery,
  )
  workspace.append(details, actions)
  content.append(deliveriesRoot, workspace)
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
      if (delivery.deliveryId === options.model.state.projection?.delivery.deliveryId) {
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
    readonly taskOptions: KeyedCollectionView<
      DeliveryTaskDetailProjection,
      string,
      HTMLOptionElement
    >
    readonly nodeOptions: KeyedCollectionView<ReviewNode, string, HTMLOptionElement>
    readonly onResolve: () => void
    readonly onDismiss: () => void
    readonly onRework: () => void
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
        const row = attentionActionRows.get(group)
        if (row === undefined) return
        void options.model.resolveAttention({
          attentionItemId: row.current.record.id,
          decision,
          resolution: row.resolution.value,
          remediation: remediation
            ? {
                deliveryTaskId: row.task.value.length === 0
                  ? null
                  : row.task.value as DeliveryTaskDetailProjection['id'],
                nodeId: row.node.value,
                instructions: row.instructions.value,
              }
            : null,
        })
      }
      const onResolve = () => { decide('resolve', false) }
      const onDismiss = () => { decide('dismiss', false) }
      const onRework = () => { decide('resolve', true) }
      resolve.addEventListener('click', onResolve)
      dismiss.addEventListener('click', onDismiss)
      rework.addEventListener('click', onRework)
      reworkFields.append(taskLabel, nodeLabel, instructionsLabel, rework)
      group.append(title, resolutionLabel, resolve, dismiss, reworkFields)
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
        taskOptions,
        nodeOptions,
        onResolve,
        onDismiss,
        onRework,
      })
      return group
    },
    update(group, item: AttentionActionItem) {
      const row = attentionActionRows.get(group)
      if (row === undefined) return
      row.current = item
      row.title.textContent = item.record.title
      group.dataset.attentionItemId = item.record.id
      row.resolve.disabled = item.busy
      row.dismiss.disabled = item.busy
      row.rework.disabled = item.busy
      const reworkVisible = item.record.type === 'verification_blocked'
        && item.candidateAvailable
        && item.nodes.length > 0
      row.reworkFields.hidden = !reworkVisible
      row.taskOptions.update(reworkVisible ? item.tasks : [])
      row.nodeOptions.update(reworkVisible ? item.nodes : [])
    },
    remove(group) {
      const row = attentionActionRows.get(group)
      if (row === undefined) return
      row.resolution.value = ''
      row.instructions.value = ''
      row.resolve.removeEventListener('click', row.onResolve)
      row.dismiss.removeEventListener('click', row.onDismiss)
      row.rework.removeEventListener('click', row.onRework)
      row.taskOptions.close()
      row.nodeOptions.close()
      attentionActionRows.delete(group)
    },
  })

  const onApproveSolution = () => {
    void options.model.decideSolutionReview({
      action: 'approve',
      comments: comments.value,
      requestedChanges: [],
    })
  }
  const onRequestChanges = () => {
    void options.model.decideSolutionReview({
      action: 'request_changes',
      comments: comments.value,
      requestedChanges: changes.value.split(/\r?\n/u),
    })
  }
  const onRejectSolution = () => {
    void options.model.decideSolutionReview({
      action: 'reject',
      comments: comments.value,
      requestedChanges: [],
    })
  }
  const onApproveTasks = () => { void options.model.approveTaskBreakdown() }
  const onSubmitVerdict = () => { void options.model.submitVerdict() }
  const onAdvanceDelivery = () => { void options.model.advanceDelivery() }
  const onRetry = () => { void options.model.refresh() }
  const onReconnect = () => { options.model.reconnect() }
  approveSolution.addEventListener('click', onApproveSolution)
  requestChanges.addEventListener('click', onRequestChanges)
  rejectSolution.addEventListener('click', onRejectSolution)
  approveTasks.addEventListener('click', onApproveTasks)
  submitVerdict.addEventListener('click', onSubmitVerdict)
  advanceDelivery.addEventListener('click', onAdvanceDelivery)
  retry.addEventListener('click', onRetry)
  reconnect.addEventListener('click', onReconnect)

  function renderDeliveries(state: StrongFlowViewModelState): void {
    const active = state.projection?.delivery ?? null
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
      diagramsNode = null
      lastSolutionReview = null
      lastRuntime = null
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

    if (
      diagramsNode === null
      || lastSolutionReview !== projection.solutionReview
      || lastRuntime !== projection.runtime
    ) {
      diagramsNode?.remove()
      diagramsNode = renderStrongFlowDiagrams(document, projection, limits)
      diagramsHost.append(diagramsNode)
      lastSolutionReview = projection.solutionReview
      lastRuntime = projection.runtime
    }
    candidateView.update({ projection, candidateFiles })
  }

  function renderActions(state: StrongFlowViewModelState): void {
    const projection = state.projection
    const interaction = state.interaction ?? { status: 'idle', error: null }
    const busy = interaction.status === 'submitting' || interaction.status === 'waiting'
    actions.setAttribute('aria-busy', String(busy))
    const review = projection?.solutionReview ?? null
    const pendingReview = review?.reviewStatus === 'pending'
    const nextDraftKey = pendingReview
      ? projection?.currentCandidate?.diffSha256 ?? projection?.delivery.deliveryId ?? null
      : null
    if (solutionDraftKey !== nextDraftKey) {
      comments.value = ''
      changes.value = ''
      solutionDraftKey = nextDraftKey
    }
    solutionActions.hidden = !pendingReview
    approveSolution.disabled = busy
    requestChanges.disabled = busy
    rejectSolution.disabled = busy
    approveTasks.hidden = review?.reviewStatus !== 'approved'
      || (projection?.delivery.tasks.length ?? 0) > 0
    approveTasks.disabled = busy
    const verdictVisible = projection !== null && canSubmitStrongFlowVerdict(projection)
    if (verdictVisible && submitVerdict.parentNode === null) {
      actions.insertBefore(submitVerdict, attentionActions)
    } else if (!verdictVisible) {
      submitVerdict.remove()
    }
    submitVerdict.disabled = busy
    advanceDelivery.hidden = projection?.delivery.status !== 'ready-to-deliver'
    advanceDelivery.disabled = busy
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
      tasks: reviewTasks,
      nodes,
      candidateAvailable: projection?.currentCandidate !== null,
    })))
    updateOmitted(actionsOmitted, openAttention.omitted, 'Attention actions')
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
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      retry.removeEventListener('click', onRetry)
      reconnect.removeEventListener('click', onReconnect)
      approveSolution.removeEventListener('click', onApproveSolution)
      requestChanges.removeEventListener('click', onRequestChanges)
      rejectSolution.removeEventListener('click', onRejectSolution)
      approveTasks.removeEventListener('click', onApproveTasks)
      submitVerdict.removeEventListener('click', onSubmitVerdict)
      advanceDelivery.removeEventListener('click', onAdvanceDelivery)
      comments.value = ''
      changes.value = ''
      attentionActionCollection.close()
      attentionCollection.close()
      stageCollection.close()
      taskCollection.close()
      deliveryCollection.close()
      diagramsNode?.remove()
      candidateView.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
