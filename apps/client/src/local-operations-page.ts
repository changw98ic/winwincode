// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './control-plane-client.js'
import {
  mountButton,
  mountEmptyState,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  type StatusTone,
} from './components/index.js'
import type { WorkerProjection } from './generated/contracts.js'
import type {
  FailureClassification,
  LocalOperationsViewModel,
  LocalOperationsViewModelState,
  RepositoryGitRisk,
} from './local-operations-view-model.js'

export interface LocalOperationsPageOptions {
  readonly root: HTMLElement
  readonly model: LocalOperationsViewModel
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface LocalOperationsPage {
  close(): void
}

export interface LocalOperationsPagePresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
  readonly commandsDisabled: boolean
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  if (error.code === 'REVISION_CONFLICT') {
    return 'This Worker changed before the command was saved. Review the current state and try again.'
  }
  if (error.code === 'LOCAL_OPERATIONS_WORKER_STALE') {
    return 'Refresh local Workers and select a current Worker.'
  }
  if (error.code === 'LOCAL_OPERATIONS_COMMAND_IN_FLIGHT') {
    return 'Wait for the current Worker change to finish.'
  }
  if (error.code === 'LOCAL_OPERATIONS_WORKER_OFFLINE') {
    return 'Enable the offline Worker before requesting a drain.'
  }
  if (error.kind === 'authentication') return 'Sign in again to view local operations.'
  if (error.kind === 'authorization') return 'You do not have access to local operations.'
  if (error.kind === 'network') return 'The local Control Plane could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The local operations update was cancelled.'
  if (error.kind === 'configuration' || error.code === 'INVALID_CLIENT_REQUEST') {
    return 'Check the local server URL and workspace scope configuration, then retry.'
  }
  return 'Local operations could not be updated. Retry, or review the server status.'
}

export function localOperationsPagePresentation(
  state: LocalOperationsViewModelState,
): LocalOperationsPagePresentation {
  const visibleError = state.interaction.error ?? state.error
  const statusText = state.interaction.status === 'submitting'
    ? 'Saving Worker state…'
    : state.interaction.status === 'waiting'
      ? 'Worker command accepted · waiting for the current snapshot…'
      : state.status === 'loading'
        ? 'Loading local operations…'
        : state.status === 'refreshing' || state.realtime === 'reloading'
          ? 'Updating local operations…'
          : state.realtime === 'reconnecting'
            ? 'Reconnecting…'
            : state.status === 'authentication-required'
              ? 'Sign in required'
              : state.status === 'authorization-denied'
                ? 'Access denied'
                : state.status === 'cancelled'
                  ? 'Update cancelled'
                  : state.status === 'error'
                    ? 'Local operations unavailable'
                    : state.status === 'closed'
                      ? 'Local operations closed'
                      : `Ready · ${String(state.workers.length)} local Workers`
  const busy = state.status === 'loading'
    || state.status === 'refreshing'
    || state.realtime === 'reloading'
    || state.interaction.status === 'submitting'
    || state.interaction.status === 'waiting'
  return Object.freeze({
    statusText,
    errorText: errorLabel(visibleError),
    busy,
    retryVisible: visibleError !== null && state.realtime !== 'reconnecting',
    reconnectVisible: state.realtime === 'reconnecting',
    commandsDisabled: busy
      || state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed',
  })
}

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function descriptionList(
  document: Document,
  entries: readonly (readonly [string, string])[],
  className: string,
): HTMLDListElement {
  const list = element(document, 'dl', className)
  for (const [term, description] of entries) {
    const dt = document.createElement('dt')
    const dd = document.createElement('dd')
    dt.textContent = term
    dd.textContent = description
    list.append(dt, dd)
  }
  return list
}

function gitRiskLabel(risk: RepositoryGitRisk): string {
  if (risk === 'clear') return 'No current projected risk'
  if (risk === 'attention-required') return 'Blocking Attention requires review'
  if (risk === 'code-failure') return 'Code or acceptance criteria failed'
  if (risk === 'infrastructure-failure') return 'Infrastructure failure'
  return 'No current risk projection'
}

function failureLabel(failure: FailureClassification): string {
  if (failure === 'none') return 'No current failure'
  if (failure === 'resource-shortage') return 'Resource shortage · no enabled capacity'
  if (failure === 'code-failure') return 'Code failure · acceptance criteria failed'
  if (failure === 'infrastructure-failure') return 'Infrastructure failure · not a code failure'
  return 'No current failure classification'
}

function worktreeLabel(value: 'candidate-frozen' | 'no-candidate' | 'not-reported'): string {
  if (value === 'candidate-frozen') return 'Frozen candidate reported · path hidden'
  if (value === 'no-candidate') return 'No frozen candidate · path hidden'
  return 'Not reported by Control Plane'
}

function workerIdentity(worker: WorkerProjection): string {
  return `Worker …${worker.id.slice(-6)}`
}

function workerStateLabel(worker: WorkerProjection): string {
  if (worker.state === 'enabled') return 'Enabled'
  if (worker.state === 'draining') return 'Draining'
  return 'Offline'
}

/** Mount repository diagnostics and local Worker controls against the read/write view-model only. */
export function mountLocalOperationsPage(options: LocalOperationsPageOptions): LocalOperationsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'main', 'wwc-local-operations')
  layout.dataset.wwcPage = 'management'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: 'Repository and local Worker operations',
      eyebrow: 'Local diagnostics',
      description: 'Inspect secret-safe repository signals, reported capacity, and local Worker state.',
      headingLevel: 1,
      className: 'wwc-local-operations-heading',
    },
  })
  const heading = pageHeader.root
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: 'Loading local operations…',
      tone: 'info',
      live: 'polite',
      className: 'wwc-local-operations-status',
    },
  })
  const status = statusBadge.root
  const retryButton = mountButton({
    document,
    props: {
      label: 'Retry snapshot',
      className: 'wwc-local-operations-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: 'Reconnect events',
      className: 'wwc-local-operations-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: 'Local operations unavailable',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-local-operations-error',
    },
  })
  const error = errorState.root
  errorState.message.className = 'wwc-local-operations-error-text'
  const repositoryPanel = mountPanel({
    document,
    props: {
      id: 'wwc-local-repository',
      title: 'Repository diagnostics',
      description: 'Repository paths stay hidden; only bounded identity and Git-risk signals are shown.',
      className: 'wwc-local-repository',
    },
  })
  const repositorySection = repositoryPanel.root
  const repositoryHeading = repositoryPanel.title
  repositoryHeading.className = 'wwc-local-operations-section-heading'
  const repositoryContent = element(document, 'div', 'wwc-local-repository-content')
  const resourcesPanel = mountPanel({
    document,
    props: {
      id: 'wwc-local-resources',
      title: 'Local resources',
      description: 'Reported capacity is kept separate from code and infrastructure failure signals.',
      className: 'wwc-local-resources',
    },
  })
  const resourcesSection = resourcesPanel.root
  const resourcesHeading = resourcesPanel.title
  resourcesHeading.className = 'wwc-local-operations-section-heading'
  const resourcesContent = element(document, 'div', 'wwc-local-resources-content')
  const resourceStatusBadge = mountStatusBadge({
    document,
    props: {
      label: 'No current failure signal',
      tone: 'success',
      className: 'wwc-local-resource-status',
    },
  })
  const workersPanel = mountPanel({
    document,
    props: {
      id: 'wwc-local-workers',
      title: 'Local Workers',
      description: 'Drain or enable a reported Worker without exposing host or repository paths.',
      className: 'wwc-local-workers',
    },
  })
  const workersSection = workersPanel.root
  const workersHeading = workersPanel.title
  workersHeading.className = 'wwc-local-operations-section-heading'
  const workers = element(document, 'ul', 'wwc-local-worker-list')
  const workersEmpty = mountEmptyState({
    document,
    props: {
      title: 'No local Workers reported',
      detail: 'A local Worker will appear here after the Control Plane reports it.',
      className: 'wwc-local-worker-empty',
    },
  })
  let closed = false

  repositoryPanel.content.append(repositoryContent)
  resourcesPanel.content.append(resourceStatusBadge.root, resourcesContent)
  workers.setAttribute('aria-live', 'polite')
  workersPanel.content.append(workers, workersEmpty.root)
  layout.append(heading, status, error, repositorySection, resourcesSection, workersSection)
  options.root.replaceChildren(layout)

  function renderWorker(worker: WorkerProjection, commandsDisabled: boolean): HTMLLIElement {
    const item = element(document, 'li', 'wwc-local-worker')
    const title = element(document, 'h3', 'wwc-local-worker-heading')
    const details = descriptionList(document, [
      ['State', workerStateLabel(worker)],
      ['Reported capacity slots', String(worker.capacity)],
      ['Last heartbeat', worker.lastHeartbeatAt ?? 'Never reported'],
      ['Revision', String(worker.revision)],
    ], 'wwc-local-worker-details')
    const controls = element(document, 'div', 'wwc-local-worker-controls')
    const drain = element(document, 'button', 'wwc-local-worker-drain')
    const enable = element(document, 'button', 'wwc-local-worker-enable')
    title.textContent = workerIdentity(worker)
    item.dataset.state = worker.state
    drain.type = 'button'
    drain.textContent = 'Drain Worker'
    drain.dataset.wwcComponent = 'button'
    drain.dataset.variant = 'destructive'
    drain.disabled = commandsDisabled || worker.state !== 'enabled'
    enable.type = 'button'
    enable.textContent = 'Enable Worker'
    enable.dataset.wwcComponent = 'button'
    enable.dataset.variant = 'default'
    enable.disabled = commandsDisabled || worker.state === 'enabled'
    drain.addEventListener('click', () => {
      if (options.readOnly !== true) void options.model.drainWorker(worker.id)
    })
    enable.addEventListener('click', () => {
      if (options.readOnly !== true) void options.model.enableWorker(worker.id)
    })
    controls.append(drain, enable)
    item.append(title, details, controls)
    return item
  }

  function render(state: LocalOperationsViewModelState): void {
    if (closed) return
    const presentation = localOperationsPagePresentation(state)
    const tone: StatusTone = presentation.errorText !== null
      ? 'danger'
      : state.realtime === 'reconnecting'
        ? 'warning'
        : presentation.busy
          ? 'info'
          : state.status === 'ready'
            ? 'success'
            : 'neutral'
    statusBadge.update({
      label: presentation.statusText,
      tone,
      live: 'polite',
      className: 'wwc-local-operations-status',
    })
    layout.setAttribute('aria-busy', String(presentation.busy))
    errorState.update({
      title: 'Local operations unavailable',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-local-operations-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    repositoryContent.replaceChildren(descriptionList(document, [
      ['Repository', state.repository.repositoryIdentity ?? 'No repository projection'],
      ['Kind', state.repository.repositoryKind ?? 'Not reported'],
      ['Latest Delivery baseline', state.repository.baselineRevision ?? 'Not reported'],
      ['Candidate/worktree', worktreeLabel(state.repository.worktreeState)],
      ['Latest Delivery Git risk', gitRiskLabel(state.repository.gitRisk)],
      ['Open Attention', String(state.repository.openAttentionCount)],
      ['Path policy', state.repository.pathsHidden ? 'Repository paths hidden' : 'Hidden'],
    ], 'wwc-local-repository-details'))
    resourcesContent.dataset.failureClassification = state.resources.failureClassification
    resourceStatusBadge.update({
      label: failureLabel(state.resources.failureClassification),
      tone: state.resources.failureClassification === 'none'
        ? 'success'
        : state.resources.failureClassification === 'resource-shortage'
          ? 'warning'
          : 'danger',
      live: state.resources.failureClassification === 'none' ? 'off' : 'polite',
      className: 'wwc-local-resource-status',
    })
    resourcesContent.replaceChildren(descriptionList(document, [
      ['Failure classification', failureLabel(state.resources.failureClassification)],
      ['Reported Workers', String(state.resources.reportedWorkerCount)],
      ['Enabled Workers', String(state.resources.enabledWorkerCount)],
      ['Reported capacity slots', String(state.resources.reportedCapacitySlots)],
      ['CPU', 'Not reported by Control Plane'],
      ['Memory', 'Not reported by Control Plane'],
      ['Disk', 'Not reported by Control Plane'],
      ['Cleanup', 'Not reported by Control Plane'],
    ], 'wwc-local-resource-details'))
    workers.replaceChildren(...state.workers.map(worker => (
      renderWorker(worker, options.readOnly === true || presentation.commandsDisabled)
    )))
    workers.hidden = state.workers.length === 0
    workersEmpty.root.hidden = state.workers.length !== 0
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.model.close()
      retryButton.close()
      reconnectButton.close()
      errorState.close()
      workersEmpty.close()
      workersPanel.close()
      resourceStatusBadge.close()
      resourcesPanel.close()
      repositoryPanel.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
    },
  }
}
