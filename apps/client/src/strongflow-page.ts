// SPDX-License-Identifier: Apache-2.0

import type { DeliveryProjection } from './generated/contracts.js'
import { renderStrongFlowCandidate } from './strongflow-candidate.js'
import { renderStrongFlowDiagrams } from './strongflow-diagrams.js'
import {
  appendOmittedCount,
  boundedItems,
  DEFAULT_STRONGFLOW_RENDER_LIMITS,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'
import type {
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

function actionButton(
  document: Document,
  label: string,
  className: string,
  busy: boolean,
  action: () => void,
): HTMLButtonElement {
  const button = strongFlowElement(document, 'button', className) as HTMLButtonElement
  button.type = 'button'
  button.textContent = label
  button.disabled = busy
  button.addEventListener('click', action)
  return button
}

function reviewActions(
  document: Document,
  state: StrongFlowViewModelState,
  model: StrongFlowViewModel,
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const section = strongFlowElement(document, 'section', 'wwc-strongflow-actions')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const projection = state.projection
  const interaction = state.interaction ?? { status: 'idle', error: null }
  const busy = interaction.status === 'submitting' || interaction.status === 'waiting'
  heading.textContent = 'Review actions'
  section.setAttribute('aria-busy', String(busy))
  section.append(heading)
  if (projection === null) return section

  const review = projection.solutionReview
  if (review?.reviewStatus === 'pending') {
    const reviewGroup = strongFlowElement(document, 'div', 'wwc-strongflow-solution-actions')
    const commentsLabel = document.createElement('label')
    const comments = document.createElement('textarea')
    const changesLabel = document.createElement('label')
    const changes = document.createElement('textarea')
    commentsLabel.textContent = 'Review comments'
    commentsLabel.append(comments)
    changesLabel.textContent = 'Requested changes, one per line'
    changesLabel.append(changes)
    reviewGroup.append(
      commentsLabel,
      changesLabel,
      actionButton(document, 'Approve solution', 'wwc-strongflow-approve-solution', busy, () => {
        void model.decideSolutionReview({
          action: 'approve',
          comments: comments.value,
          requestedChanges: [],
        })
      }),
      actionButton(document, 'Request changes', 'wwc-strongflow-request-changes', busy, () => {
        void model.decideSolutionReview({
          action: 'request_changes',
          comments: comments.value,
          requestedChanges: changes.value.split(/\r?\n/u),
        })
      }),
      actionButton(document, 'Reject solution', 'wwc-strongflow-reject-solution', busy, () => {
        void model.decideSolutionReview({
          action: 'reject',
          comments: comments.value,
          requestedChanges: [],
        })
      }),
    )
    section.append(reviewGroup)
  } else if (review?.reviewStatus === 'approved' && projection.delivery.tasks.length === 0) {
    section.append(actionButton(
      document,
      'Approve task breakdown',
      'wwc-strongflow-approve-tasks',
      busy,
      () => { void model.approveTaskBreakdown() },
    ))
  }

  if (canSubmitStrongFlowVerdict(projection)) {
    section.append(actionButton(
      document,
      'Compute current verdict',
      'wwc-strongflow-submit-verdict',
      busy,
      () => { void model.submitVerdict() },
    ))
  }

  const nodes = review === null
    ? []
    : [...review.architectureDiagram.nodes, ...review.processDiagram.nodes]
  const openAttention = boundedItems(
    projection.attention.filter(item => item.status === 'open'),
    limits.attention,
  )
  for (const record of openAttention.items) {
    const group = strongFlowElement(document, 'div', 'wwc-strongflow-attention-actions')
    const title = document.createElement('strong')
    const resolutionLabel = document.createElement('label')
    const resolution = document.createElement('textarea')
    title.textContent = record.title
    resolutionLabel.textContent = 'Decision note'
    resolutionLabel.append(resolution)
    group.dataset.attentionItemId = record.id
    group.append(
      title,
      resolutionLabel,
      actionButton(document, 'Resolve', 'wwc-strongflow-resolve-attention', busy, () => {
        void model.resolveAttention({
          attentionItemId: record.id,
          decision: 'resolve',
          resolution: resolution.value,
          remediation: null,
        })
      }),
      actionButton(document, 'Dismiss', 'wwc-strongflow-dismiss-attention', busy, () => {
        void model.resolveAttention({
          attentionItemId: record.id,
          decision: 'dismiss',
          resolution: resolution.value,
          remediation: null,
        })
      }),
    )
    if (
      record.type === 'verification_blocked'
      && projection.currentCandidate !== null
      && nodes.length > 0
    ) {
      const taskLabel = document.createElement('label')
      const task = document.createElement('select')
      const nodeLabel = document.createElement('label')
      const node = document.createElement('select')
      const instructionsLabel = document.createElement('label')
      const instructions = document.createElement('textarea')
      taskLabel.textContent = 'Current task'
      task.append(...projection.delivery.tasks.map(item => {
        const option = document.createElement('option')
        option.value = item.id
        option.textContent = item.title
        return option
      }))
      taskLabel.append(task)
      nodeLabel.textContent = 'Current solution node'
      node.append(...nodes.map(item => {
        const option = document.createElement('option')
        option.value = item.id
        option.textContent = item.label
        return option
      }))
      nodeLabel.append(node)
      instructionsLabel.textContent = 'Bounded rework instructions'
      instructionsLabel.append(instructions)
      group.append(
        taskLabel,
        nodeLabel,
        instructionsLabel,
        actionButton(document, 'Approve bounded rework', 'wwc-strongflow-rework', busy, () => {
          void model.resolveAttention({
            attentionItemId: record.id,
            decision: 'resolve',
            resolution: resolution.value,
            remediation: {
              deliveryTaskId: task.value.length === 0
                ? null
                : task.value as typeof projection.delivery.tasks[number]['id'],
              nodeId: node.value,
              instructions: instructions.value,
            },
          })
        }),
      )
    }
    section.append(group)
  }
  appendOmittedCount(document, section, openAttention.omitted, 'Attention actions')

  if (projection.delivery.status === 'ready-to-deliver') {
    section.append(actionButton(
      document,
      'Approve final Delivery',
      'wwc-strongflow-advance-delivery',
      busy,
      () => { void model.advanceDelivery() },
    ))
  }
  return section
}

function deliveryList(
  document: Document,
  state: StrongFlowViewModelState,
  deliveries: readonly DeliveryProjection[],
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const root = strongFlowElement(document, 'aside', 'wwc-strongflow-deliveries')
  const heading = strongFlowElement(document, 'h2', 'wwc-strongflow-deliveries-heading')
  const list = strongFlowElement(document, 'ul', 'wwc-strongflow-delivery-list')
  const active = state.projection?.delivery ?? null
  const byId = new Map(deliveries.map(delivery => [delivery.deliveryId, delivery]))
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
  const boundedDeliveries = boundedItems([...byId.values()], limits.deliveries)
  heading.textContent = 'Deliveries'
  list.append(...boundedDeliveries.items.map(delivery => {
    const item = document.createElement('li')
    const link = document.createElement('a')
    const status = document.createElement('span')
    link.href = `#/strongflow?delivery=${encodeURIComponent(delivery.deliveryId)}`
    link.textContent = delivery.title
    link.dataset.deliveryId = delivery.deliveryId
    status.textContent = `${delivery.status} · r${String(delivery.revision)}`
    if (delivery.deliveryId === active?.deliveryId) link.setAttribute('aria-current', 'page')
    item.append(link, status)
    return item
  }))
  root.append(heading, list)
  appendOmittedCount(document, root, boundedDeliveries.omitted, 'Deliveries')
  return root
}

function deliveryDetails(
  document: Document,
  state: StrongFlowViewModelState,
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const root = strongFlowElement(document, 'div', 'wwc-strongflow-details')
  const projection = state.projection
  if (projection === null) {
    const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
    empty.textContent = state.status === 'loading' || state.status === 'refreshing'
      ? 'Loading the exact Delivery snapshot…'
      : 'Select a Delivery to open StrongFlow.'
    root.append(empty)
    return root
  }

  const overview = strongFlowElement(document, 'section', 'wwc-strongflow-overview')
  const heading = strongFlowElement(document, 'h2', 'wwc-strongflow-heading')
  const goal = strongFlowElement(document, 'p', 'wwc-strongflow-goal')
  const metadata = strongFlowElement(document, 'p', 'wwc-strongflow-metadata')
  heading.textContent = projection.delivery.requirements.title
  goal.textContent = projection.delivery.requirements.goal
  metadata.textContent = `Delivery r${String(projection.metadata.revisions.delivery)} · Runtime r${String(
    projection.metadata.revisions.runtime,
  )} · updated ${projection.metadata.updatedAt}`
  overview.append(heading, goal, metadata)

  const tasksSection = strongFlowElement(document, 'section', 'wwc-strongflow-tasks')
  const tasksHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const tasks = strongFlowElement(document, 'ul', 'wwc-strongflow-task-list')
  const boundedTasks = boundedItems(projection.delivery.tasks, limits.tasks)
  tasksHeading.textContent = 'Tasks'
  tasks.append(...boundedTasks.items.map(task => {
    const item = document.createElement('li')
    const title = document.createElement('strong')
    const status = document.createElement('span')
    title.textContent = task.title
    status.textContent = task.status
    item.dataset.status = task.status
    item.append(title, status)
    return item
  }))
  tasksSection.append(tasksHeading, tasks)
  appendOmittedCount(document, tasksSection, boundedTasks.omitted, 'tasks')

  const stagesSection = strongFlowElement(document, 'section', 'wwc-strongflow-stages')
  const stagesHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const stages = strongFlowElement(document, 'ol', 'wwc-strongflow-stage-list')
  const boundedStages = boundedItems(projection.delivery.stages, limits.stages)
  stagesHeading.textContent = 'Stages'
  stages.append(...boundedStages.items.map(stage => {
    const item = document.createElement('li')
    item.dataset.status = stage.status
    item.textContent = `${stage.stage} · ${stage.role} · ${stage.status}`
    return item
  }))
  stagesSection.append(stagesHeading, stages)
  appendOmittedCount(document, stagesSection, boundedStages.omitted, 'stages')

  const attentionSection = strongFlowElement(document, 'section', 'wwc-strongflow-attention')
  const attentionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const attention = strongFlowElement(document, 'ul', 'wwc-strongflow-attention-list')
  const boundedAttention = boundedItems(projection.attention, limits.attention)
  attentionHeading.textContent = 'Attention'
  attention.append(...boundedAttention.items.map(record => {
    const item = document.createElement('li')
    item.dataset.status = record.status
    item.textContent = `${record.title} · ${record.status}`
    return item
  }))
  attentionSection.append(attentionHeading, attention)
  appendOmittedCount(document, attentionSection, boundedAttention.omitted, 'Attention records')

  root.append(
    overview,
    tasksSection,
    stagesSection,
    attentionSection,
    renderStrongFlowDiagrams(document, projection, limits),
    renderStrongFlowCandidate(document, projection, limits),
  )
  return root
}

/** Mount the advanced StrongFlow workspace against its Control Plane view-model. */
export function mountStrongFlowPage(options: StrongFlowPageOptions): StrongFlowPage {
  const document = options.root.ownerDocument
  const limits = options.limits ?? DEFAULT_STRONGFLOW_RENDER_LIMITS
  const layout = strongFlowElement(document, 'div', 'wwc-strongflow')
  const status = strongFlowElement(document, 'p', 'wwc-strongflow-status')
  const error = strongFlowElement(document, 'div', 'wwc-strongflow-error')
  const errorText = strongFlowElement(document, 'span', 'wwc-strongflow-error-text')
  const retry = strongFlowElement(document, 'button', 'wwc-strongflow-retry')
  const reconnect = strongFlowElement(document, 'button', 'wwc-strongflow-reconnect')
  const content = strongFlowElement(document, 'div', 'wwc-strongflow-content')
  let closed = false
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  retry.type = 'button'
  retry.textContent = 'Retry snapshot'
  reconnect.type = 'button'
  reconnect.textContent = 'Reconnect events'
  error.append(errorText, retry, reconnect)
  layout.append(status, error, content)
  options.root.replaceChildren(layout)

  function render(state: StrongFlowViewModelState): void {
    if (closed) return
    const presentation = strongFlowPagePresentation(state)
    status.textContent = presentation.statusText
    content.setAttribute('aria-busy', String(presentation.busy))
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    const workspace = strongFlowElement(document, 'main', 'wwc-strongflow-workspace')
    workspace.append(
      deliveryDetails(document, state, limits),
      reviewActions(document, state, options.model, limits),
    )
    content.replaceChildren(
      deliveryList(document, state, options.deliveries ?? [], limits),
      workspace,
    )
  }

  retry.addEventListener('click', () => { void options.model.refresh() })
  reconnect.addEventListener('click', () => { options.model.reconnect() })
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
