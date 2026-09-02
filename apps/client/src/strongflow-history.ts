// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryTaskDetailProjection,
  DeliveryTaskId,
  StageRunId,
} from './generated/contracts.js'
import type { StrongFlowProjection } from './strongflow-view-model.js'
import {
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'
import {
  mountKeyedCollection,
  type KeyedCollectionView,
} from './components/keyed-collection.js'

/**
 * Presentation-only history navigation for Task, StageRun, and Attempt review.
 *
 * The selection below never feeds Control Plane commands and never replaces the
 * StrongFlow view-model's canonical Delivery/StageRun binding: it only decides
 * which already-delivered projection rows the browser expands or renders.
 * Older attempts therefore stay read-only review targets, while the Server
 * remains the sole authority for every mutation.
 */

export const STRONGFLOW_HISTORY_TASK_PARAMETER = 'task'
export const STRONGFLOW_HISTORY_RUN_PARAMETER = 'run'

export interface StrongFlowHistorySelection {
  readonly taskId: DeliveryTaskId | null
  readonly stageRunId: StageRunId | null
}

export interface StrongFlowHistoryBinding {
  readonly productSessionId: string
  readonly executionJobId: string
  readonly workerId: string | null
  readonly workerSessionId: string | null
  readonly codexThreadId: string | null
}

export interface StrongFlowHistoryRun {
  readonly stageRunId: StageRunId
  readonly deliveryTaskId: DeliveryTaskId | null
  readonly stage: string
  readonly role: string
  readonly actorType: 'codex' | 'human'
  readonly attempt: number | null
  readonly status: string
  readonly startedAt: string
  readonly finishedAt: string | null
  readonly isCurrent: boolean
  readonly producedCurrentCandidate: boolean
  readonly evidenceCount: number
  readonly candidateRefs: readonly string[]
  readonly binding: StrongFlowHistoryBinding | null
}

export interface StrongFlowHistoryTaskNode {
  readonly task: DeliveryTaskDetailProjection
  readonly runs: readonly StrongFlowHistoryRun[]
}

export interface StrongFlowHistoryTree {
  readonly tasks: readonly StrongFlowHistoryTaskNode[]
  readonly deliveryRuns: readonly StrongFlowHistoryRun[]
  readonly runs: readonly StrongFlowHistoryRun[]
  readonly currentStageRunId: StageRunId | null
  readonly omittedTasks: number
  readonly omittedRuns: number
}

/** Browser history seam: reads the route hash and replaces it without remounting the route. */
export interface StrongFlowHistoryLocation {
  hash(): string
  replaceHash(hash: string): void
}

const EMPTY_SELECTION: StrongFlowHistorySelection = Object.freeze({
  taskId: null,
  stageRunId: null,
})

function routeParameters(hash: string): URLSearchParams {
  const query = hash.indexOf('?')
  return new URLSearchParams(query < 0 ? '' : hash.slice(query + 1))
}

/** Read the presentation-only selection from a StrongFlow route hash. */
export function strongFlowHistorySelectionFromHash(
  hash: string,
): StrongFlowHistorySelection {
  const parameters = routeParameters(hash)
  const taskId = parameters.get(STRONGFLOW_HISTORY_TASK_PARAMETER)
  const stageRunId = parameters.get(STRONGFLOW_HISTORY_RUN_PARAMETER)
  return Object.freeze({
    taskId: taskId === null ? null : taskId as DeliveryTaskId,
    stageRunId: stageRunId === null ? null : stageRunId as StageRunId,
  })
}

/**
 * Merge the selection into a StrongFlow route hash. Binding parameters such as
 * `delivery`, `session`, and `stageRun` stay untouched because they still own
 * the view-model identity.
 */
export function strongFlowHistoryHashWithSelection(
  hash: string,
  selection: StrongFlowHistorySelection,
): string {
  const queryIndex = hash.indexOf('?')
  const base = queryIndex < 0 ? hash : hash.slice(0, queryIndex)
  const parameters = routeParameters(hash)
  if (selection.taskId === null) parameters.delete(STRONGFLOW_HISTORY_TASK_PARAMETER)
  else parameters.set(STRONGFLOW_HISTORY_TASK_PARAMETER, selection.taskId)
  if (selection.stageRunId === null) parameters.delete(STRONGFLOW_HISTORY_RUN_PARAMETER)
  else parameters.set(STRONGFLOW_HISTORY_RUN_PARAMETER, selection.stageRunId)
  const encoded = parameters.toString()
  return encoded.length === 0 ? base : `${base}?${encoded}`
}

function historyRun(
  stage: StrongFlowProjection['delivery']['stages'][number],
  currentStageRunId: StageRunId | null,
  currentCandidateRef: string | null,
  evidence: StrongFlowProjection['evidence'],
): StrongFlowHistoryRun {
  const runEvidence = evidence.filter(item => item.stageRunId === stage.id)
  const candidateRefs = [...new Set(runEvidence.map(item => item.candidateRef))]
  const binding = stage.sessionBinding ?? null
  return Object.freeze({
    stageRunId: stage.id,
    deliveryTaskId: stage.deliveryTaskId ?? null,
    stage: stage.stage,
    role: stage.role,
    actorType: stage.actorType === 'human' ? 'human' : 'codex',
    attempt: typeof stage.attempt === 'number' ? stage.attempt : null,
    status: stage.status,
    startedAt: stage.startedAt,
    finishedAt: stage.finishedAt ?? null,
    isCurrent: stage.id === currentStageRunId,
    producedCurrentCandidate: currentCandidateRef !== null
      && candidateRefs.includes(currentCandidateRef),
    evidenceCount: runEvidence.length,
    candidateRefs: Object.freeze(candidateRefs),
    binding: binding === null ? null : Object.freeze({
      productSessionId: binding.productSessionId,
      executionJobId: binding.executionJobId,
      workerId: binding.workerId ?? null,
      workerSessionId: binding.workerSessionId ?? null,
      codexThreadId: binding.codexThreadId ?? null,
    }),
  })
}

/**
 * Project one bounded Delivery snapshot onto the navigable history tree.
 * The association truth is the StageRun's own `deliveryTaskId`, so Task nodes,
 * the Delivery-level stage list, and attempt numbering all derive from the same
 * canonical snapshot instead of a second state machine.
 */
export function strongFlowHistoryTree(
  projection: StrongFlowProjection,
  limits: StrongFlowRenderLimits,
): StrongFlowHistoryTree {
  const currentStageRunId = projection.stage?.id ?? null
  const currentCandidateRef = projection.currentCandidate?.candidateRef ?? null
  const boundedStages = boundedItems(projection.delivery.stages, limits.stages)
  const runs = boundedStages.items.map(stage => historyRun(
    stage,
    currentStageRunId,
    currentCandidateRef,
    projection.evidence,
  ))
  const runsByTask = new Map<string, StrongFlowHistoryRun[]>()
  const deliveryRuns: StrongFlowHistoryRun[] = []
  const runById = new Map<StageRunId, StrongFlowHistoryRun>()
  for (const run of runs) {
    runById.set(run.stageRunId, run)
    if (run.deliveryTaskId === null) {
      deliveryRuns.push(run)
      continue
    }
    const owned = runsByTask.get(run.deliveryTaskId) ?? []
    owned.push(run)
    runsByTask.set(run.deliveryTaskId, owned)
  }
  const boundedTasks = boundedItems(projection.delivery.tasks, limits.tasks)
  const tasks = boundedTasks.items.map(task => Object.freeze({
    task,
    runs: Object.freeze(runsByTask.get(task.id) ?? []),
  }))
  return Object.freeze({
    tasks: Object.freeze(tasks),
    deliveryRuns: Object.freeze(deliveryRuns),
    runs: Object.freeze(runs),
    currentStageRunId,
    omittedTasks: boundedTasks.omitted,
    omittedRuns: boundedStages.omitted,
  })
}

/**
 * Keep only selection identities that exist in the current tree. Stale deep
 * links collapse to the live view instead of pinning a vanished run.
 */
export function strongFlowHistorySelectionForTree(
  tree: StrongFlowHistoryTree,
  requested: StrongFlowHistorySelection,
): StrongFlowHistorySelection {
  const taskKnown = requested.taskId !== null
    && tree.tasks.some(node => node.task.id === requested.taskId)
  const runKnown = requested.stageRunId !== null
    && tree.runs.some(run => run.stageRunId === requested.stageRunId)
  if (taskKnown && runKnown) return requested
  return Object.freeze({
    taskId: taskKnown ? requested.taskId : null,
    stageRunId: runKnown ? requested.stageRunId : null,
  })
}

function runLabel(run: StrongFlowHistoryRun): string {
  const attempt = run.attempt === null ? '' : ` · attempt ${String(run.attempt)}`
  return `${run.stage} · ${run.role}${attempt} · ${run.status}`
}

function runKey(run: StrongFlowHistoryRun): string {
  return run.stageRunId
}

interface RunRowState {
  run: StrongFlowHistoryRun
  button: HTMLButtonElement
  onClick: () => void
  onKeydown: (event: KeyboardEvent) => void
}

interface TaskRowState {
  node: StrongFlowHistoryTaskNode
  toggle: HTMLButtonElement
  status: HTMLElement
  runList: HTMLElement
  runs: KeyedCollectionView<StrongFlowHistoryRun, string, HTMLLIElement>
  onToggle: () => void
  onKeydown: (event: KeyboardEvent) => void
}

export interface StrongFlowHistoryNavigationOptions {
  readonly document: Document
  readonly tasksParent: HTMLElement
  readonly stagesParent: HTMLElement
  readonly tasksOmitted: HTMLElement
  readonly stagesOmitted: HTMLElement
  readonly tasksEmpty: HTMLElement
  readonly stagesEmpty: HTMLElement
  readonly limits: StrongFlowRenderLimits
  readonly initialSelection?: StrongFlowHistorySelection
  readonly onSelect: (selection: StrongFlowHistorySelection) => void
}

export interface StrongFlowHistoryNavigationView {
  update(projection: StrongFlowProjection | null): void
  selection(): StrongFlowHistorySelection
  close(): void
}

interface MountedRunRow {
  readonly toggle: HTMLButtonElement
  readonly item: HTMLLIElement
}

/**
 * Mount the clickable Task → StageRun → Attempt navigation into the existing
 * StrongFlow task and stage lists. Rows stay keyed by business identity so
 * repeated snapshots never rebuild unchanged DOM.
 */
export function mountStrongFlowHistoryNavigation(
  options: StrongFlowHistoryNavigationOptions,
): StrongFlowHistoryNavigationView {
  const document = options.document
  let closed = false
  let selection = options.initialSelection ?? EMPTY_SELECTION
  let projection: StrongFlowProjection | null = null
  let currentTree: StrongFlowHistoryTree | null = null
  const expandedTasks = new Set<DeliveryTaskId>()
  const taskRows = new WeakMap<HTMLLIElement, TaskRowState>()
  const runRows = new WeakMap<HTMLLIElement, RunRowState>()
  let taskCollection: ReturnType<typeof mountTaskCollection>
  let timelineCollection: KeyedCollectionView<StrongFlowHistoryRun, string, HTMLLIElement>

  function applySelection(next: StrongFlowHistorySelection): void {
    selection = next
    update(projection)
    options.onSelect(selection)
  }

  function activateRun(state: RunRowState): void {
    if (state.run.isCurrent) {
      applySelection(EMPTY_SELECTION)
      return
    }
    applySelection(Object.freeze({
      taskId: state.run.deliveryTaskId,
      stageRunId: state.run.stageRunId,
    }))
  }

  function createRunRow(): HTMLLIElement {
    const item = document.createElement('li')
    const button = document.createElement('button')
    button.type = 'button'
    item.append(button)
    const state: RunRowState = {
      run: undefined as unknown as StrongFlowHistoryRun,
      button,
      onClick: () => activateRun(state),
      onKeydown: (event: KeyboardEvent) => handleRunKeydown(event, state),
    }
    button.addEventListener('click', state.onClick)
    button.addEventListener('keydown', state.onKeydown)
    runRows.set(item, state)
    return item
  }

  function updateRunRow(item: HTMLLIElement, run: StrongFlowHistoryRun): void {
    const state = runRows.get(item)
    if (state === undefined) return
    state.run = run
    item.dataset.status = run.status
    if (run.attempt !== null) item.dataset.attempt = String(run.attempt)
    else delete item.dataset.attempt
    state.button.dataset.stageRunId = run.stageRunId
    if (run.attempt !== null) state.button.dataset.attempt = String(run.attempt)
    else delete state.button.dataset.attempt
    const label = runLabel(run)
    if (run.isCurrent) {
      state.button.className = 'wwc-strongflow-current-run'
      state.button.setAttribute('aria-current', 'true')
      state.button.removeAttribute('aria-pressed')
      state.button.textContent = `${label} · current`
    } else {
      state.button.className = 'wwc-strongflow-run-button'
      state.button.removeAttribute('aria-current')
      state.button.setAttribute(
        'aria-pressed',
        String(selection.stageRunId === run.stageRunId),
      )
      state.button.textContent = label
    }
  }

  function mountRunCollection(
    parent: HTMLElement,
  ): KeyedCollectionView<StrongFlowHistoryRun, string, HTMLLIElement> {
    return mountKeyedCollection<StrongFlowHistoryRun, string, HTMLLIElement>({
      parent,
      key: runKey,
      create: createRunRow,
      update: updateRunRow,
      remove(item) {
        const state = runRows.get(item)
        if (state === undefined) return
        state.button.removeEventListener('click', state.onClick)
        state.button.removeEventListener('keydown', state.onKeydown)
        runRows.delete(item)
      },
    })
  }

  function createTaskRow(node: StrongFlowHistoryTaskNode): HTMLLIElement {
    const item = document.createElement('li')
    const toggle = document.createElement('button')
    const status = document.createElement('span')
    const runList = document.createElement('ul')
    toggle.type = 'button'
    toggle.className = 'wwc-strongflow-history-toggle'
    status.className = 'wwc-strongflow-task-status'
    runList.className = 'wwc-strongflow-run-list'
    runList.id = `wwc-strongflow-runs-${node.task.id}`
    toggle.setAttribute('aria-controls', runList.id)
    item.append(toggle, status, runList)
    const state: TaskRowState = {
      node,
      toggle,
      status,
      runList,
      runs: mountRunCollection(runList),
      onToggle: () => setExpanded(
        state.node.task.id,
        !expandedTasks.has(state.node.task.id),
      ),
      onKeydown: (event: KeyboardEvent) => handleTaskKeydown(event, state),
    }
    toggle.addEventListener('click', state.onToggle)
    toggle.addEventListener('keydown', state.onKeydown)
    taskRows.set(item, state)
    return item
  }

  function updateTaskRow(item: HTMLLIElement, node: StrongFlowHistoryTaskNode): void {
    const state = taskRows.get(item)
    if (state === undefined) return
    state.node = node
    item.dataset.status = node.task.status
    state.toggle.textContent = node.task.title
    state.status.textContent = node.task.status
    const expanded = expandedTasks.has(node.task.id) || selection.taskId === node.task.id
    state.toggle.setAttribute('aria-expanded', String(expanded))
    state.runList.hidden = !expanded
    state.runs.update(expanded ? node.runs : [])
  }

  function mountTaskCollection(): KeyedCollectionView<
    StrongFlowHistoryTaskNode,
    string,
    HTMLLIElement
  > {
    return mountKeyedCollection<StrongFlowHistoryTaskNode, string, HTMLLIElement>({
      parent: options.tasksParent,
      key: node => node.task.id,
      create: createTaskRow,
      update: updateTaskRow,
      remove(item) {
        const state = taskRows.get(item)
        if (state === undefined) return
        state.toggle.removeEventListener('click', state.onToggle)
        state.toggle.removeEventListener('keydown', state.onKeydown)
        state.runs.close()
        taskRows.delete(item)
      },
    })
  }

  taskCollection = mountTaskCollection()
  timelineCollection = mountRunCollection(options.stagesParent)

  function taskToggleOf(taskId: DeliveryTaskId): MountedRunRow | null {
    const item = taskCollection.node(taskId)
    if (item === null) return null
    const state = taskRows.get(item)
    return state === undefined ? null : { toggle: state.toggle, item }
  }

  function rovingOrder(): HTMLButtonElement[] {
    const order: HTMLButtonElement[] = []
    for (const node of currentTree?.tasks ?? []) {
      const state = taskToggleOf(node.task.id)
      if (state === null) continue
      order.push(state.toggle)
      if (state.toggle.getAttribute('aria-expanded') !== 'true') continue
      const taskState = taskRows.get(state.item)
      if (taskState === undefined) continue
      for (const run of node.runs) {
        const runItem = taskState.runs.node(run.stageRunId)
        const runState = runItem === null ? undefined : runRows.get(runItem)
        if (runState !== undefined) order.push(runState.button)
      }
    }
    for (const run of currentTree?.runs ?? []) {
      const item = timelineCollection.node(run.stageRunId)
      const state = item === null ? undefined : runRows.get(item)
      if (state !== undefined) order.push(state.button)
    }
    return order
  }

  function handleRunKeydown(event: KeyboardEvent, state: RunRowState): void {
    const ownerTaskId = state.run.deliveryTaskId
    if (event.key === 'ArrowLeft' && ownerTaskId !== null) {
      event.preventDefault()
      expandedTasks.delete(ownerTaskId)
      update(projection)
      taskToggleOf(ownerTaskId)?.toggle.focus()
    } else if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.preventDefault()
      moveRovingFocus(event.key, state.button)
    }
  }

  function handleTaskKeydown(event: KeyboardEvent, state: TaskRowState): void {
    if (event.key === 'ArrowRight') {
      event.preventDefault()
      if (!expandedTasks.has(state.node.task.id)) {
        expandedTasks.add(state.node.task.id)
        update(projection)
      }
      const firstRun = state.node.runs[0]
      if (firstRun !== undefined) {
        const runItem = state.runs.node(firstRun.stageRunId)
        const runState = runItem === null ? undefined : runRows.get(runItem)
        runState?.button.focus()
      }
    } else if (event.key === 'ArrowLeft') {
      event.preventDefault()
      expandedTasks.delete(state.node.task.id)
      update(projection)
      state.toggle.focus()
    } else if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.preventDefault()
      moveRovingFocus(event.key, state.toggle)
    }
  }

  function moveRovingFocus(key: string, from: HTMLButtonElement): void {
    const order = rovingOrder()
    const index = order.indexOf(from)
    if (index < 0) return
    const target = key === 'ArrowDown'
      ? order[index + 1]
      : key === 'ArrowUp'
        ? order[index - 1]
        : key === 'Home'
          ? order[0]
          : order[order.length - 1]
    target?.focus()
  }

  function updateTabIndices(): void {
    const order = rovingOrder()
    if (order.length === 0) return
    const active = order.find(button => button === document.activeElement)
    const preferred = active
      ?? (selection.stageRunId === null
        ? undefined
        : order.find(button => button.dataset.stageRunId === selection.stageRunId))
      ?? order[0]
    for (const button of order) {
      button.tabIndex = button === preferred ? 0 : -1
    }
  }

  function setExpanded(taskId: DeliveryTaskId, expanded: boolean): void {
    if (expanded) {
      expandedTasks.add(taskId)
      update(projection)
      return
    }
    expandedTasks.delete(taskId)
    if (selection.taskId === taskId) {
      // Collapsing a Task closes its history review instead of keeping a
      // hidden selection alive.
      applySelection(EMPTY_SELECTION)
      return
    }
    update(projection)
  }

  function noteOmitted(node: HTMLElement, count: number, label: string): void {
    node.hidden = count === 0
    const text = `${String(count)} more ${label} not shown.`
    if (node.textContent !== text) node.textContent = text
  }

  function update(next: StrongFlowProjection | null): void {
    if (closed) return
    projection = next
    if (next === null) {
      currentTree = null
      selection = EMPTY_SELECTION
      taskCollection.update([])
      timelineCollection.update([])
      noteOmitted(options.tasksOmitted, 0, 'tasks')
      noteOmitted(options.stagesOmitted, 0, 'stages')
      options.tasksEmpty.hidden = true
      options.stagesEmpty.hidden = true
      return
    }
    const tree = strongFlowHistoryTree(next, options.limits)
    currentTree = tree
    selection = strongFlowHistorySelectionForTree(tree, selection)
    taskCollection.update(tree.tasks)
    timelineCollection.update(tree.runs)
    noteOmitted(options.tasksOmitted, tree.omittedTasks, 'tasks')
    noteOmitted(options.stagesOmitted, tree.omittedRuns, 'stages')
    if (options.tasksEmpty.textContent !== 'No DeliveryTasks yet.') {
      options.tasksEmpty.textContent = 'No DeliveryTasks yet.'
    }
    if (options.stagesEmpty.textContent !== 'No StageRuns yet.') {
      options.stagesEmpty.textContent = 'No StageRuns yet.'
    }
    options.tasksEmpty.hidden = tree.tasks.length !== 0
    options.stagesEmpty.hidden = tree.runs.length !== 0
    updateTabIndices()
  }

  return {
    update,
    selection() {
      return selection
    },
    close() {
      if (closed) return
      closed = true
      taskCollection.close()
      timelineCollection.close()
    },
  }
}

interface DetailSection {
  readonly term: string
  readonly value: string
}

function detailList(
  document: Document,
  className: string,
  sections: readonly DetailSection[],
): HTMLElement {
  const list = strongFlowElement(document, 'dl', className)
  for (const section of sections) {
    const term = document.createElement('dt')
    const value = document.createElement('dd')
    term.textContent = section.term
    value.textContent = section.value
    list.append(term, value)
  }
  return list
}

export interface StrongFlowRunDetailOptions {
  readonly document: Document
  readonly limits: StrongFlowRenderLimits
}

export interface StrongFlowRunDetailView {
  readonly root: HTMLElement
  update(
    projection: StrongFlowProjection | null,
    selection: StrongFlowHistorySelection,
  ): void
  close(): void
}

/**
 * Mount the read-only historical run review panel. It renders identity,
 * runtime binding, Evidence, produced candidates, and the run conclusion for
 * one selected non-current StageRun and exposes no mutating control.
 */
export function mountStrongFlowRunDetail(
  options: StrongFlowRunDetailOptions,
): StrongFlowRunDetailView {
  const document = options.document
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-history')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const note = strongFlowElement(document, 'p', 'wwc-strongflow-history-note')
  const identityHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-identity-host')
  const runtimeHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const bindingHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-binding-host')
  const evidenceHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const evidence = strongFlowElement(document, 'ul', 'wwc-strongflow-history-evidence')
  const candidatesHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const candidates = strongFlowElement(document, 'ul', 'wwc-strongflow-history-candidates')
  const conclusion = strongFlowElement(
    document,
    'section',
    'wwc-strongflow-history-conclusion',
  )
  const conclusionStatus = document.createElement('strong')
  const conclusionText = document.createElement('p')
  const evidenceOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  root.hidden = true
  root.setAttribute('aria-label', 'Historical StageRun review')
  heading.textContent = 'Historical StageRun review'
  note.textContent = 'Read-only history: this StageRun is not the current run.'
  runtimeHeading.textContent = 'Runtime binding'
  evidenceHeading.textContent = 'Evidence from this run'
  candidatesHeading.textContent = 'Candidates from this run'
  evidence.setAttribute('aria-label', 'StageRun evidence')
  conclusion.append(conclusionStatus, conclusionText)
  root.append(
    heading,
    note,
    identityHost,
    runtimeHeading,
    bindingHost,
    evidenceHeading,
    evidence,
    evidenceOmitted,
    candidatesHeading,
    candidates,
    conclusion,
  )

  function update(
    projection: StrongFlowProjection | null,
    selection: StrongFlowHistorySelection,
  ): void {
    const tree = projection === null
      ? null
      : strongFlowHistoryTree(projection, options.limits)
    const run = tree === null || selection.stageRunId === null
      ? null
      : tree.runs.find(candidate => candidate.stageRunId === selection.stageRunId) ?? null
    if (run === null || run.isCurrent) {
      root.hidden = true
      return
    }
    root.hidden = false
    root.dataset.stageRunId = run.stageRunId
    if (run.attempt !== null) root.dataset.attempt = String(run.attempt)
    else delete root.dataset.attempt

    const taskTitle = tree?.tasks.find(
      node => node.task.id === run.deliveryTaskId,
    )?.task.title ?? run.deliveryTaskId
    identityHost.replaceChildren(detailList(document, 'wwc-strongflow-history-identity', [
      { term: 'StageRun', value: run.stageRunId },
      { term: 'Attempt', value: run.attempt === null ? '—' : String(run.attempt) },
      { term: 'Stage', value: run.stage },
      { term: 'Role', value: run.role },
      { term: 'Actor', value: run.actorType },
      { term: 'Task', value: taskTitle ?? 'Delivery-level' },
      { term: 'Status', value: run.status },
      { term: 'Started', value: run.startedAt },
      { term: 'Finished', value: run.finishedAt ?? 'Not finished' },
    ]))

    if (run.binding === null) {
      const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
      empty.textContent = 'Human review StageRun — no runtime binding.'
      bindingHost.replaceChildren(empty)
    } else {
      bindingHost.replaceChildren(detailList(document, 'wwc-strongflow-history-binding', [
        { term: 'ProductSession', value: run.binding.productSessionId },
        { term: 'ExecutionJob', value: run.binding.executionJobId },
        { term: 'Worker', value: run.binding.workerId ?? '—' },
        { term: 'WorkerSession', value: run.binding.workerSessionId ?? '—' },
        { term: 'CodexThread', value: run.binding.codexThreadId ?? '—' },
      ]))
    }

    const runEvidence = projection?.evidence.filter(
      item => item.stageRunId === run.stageRunId,
    ) ?? []
    const boundedEvidence = boundedItems(runEvidence, options.limits.evidence)
    evidence.replaceChildren(...boundedEvidence.items.map(item => {
      const row = document.createElement('li')
      row.textContent = `${item.type} · ${item.id} · ${item.sourceRef}`
      return row
    }))
    evidenceOmitted.hidden = boundedEvidence.omitted === 0
    evidenceOmitted.textContent = `${String(boundedEvidence.omitted)} more evidence records not shown.`

    const currentCandidateRef = projection?.currentCandidate?.candidateRef ?? null
    candidates.replaceChildren(...run.candidateRefs.map(candidateRef => {
      const row = document.createElement('li')
      row.dataset.candidateRef = candidateRef
      row.textContent = candidateRef
      if (candidateRef === currentCandidateRef) row.dataset.current = 'true'
      return row
    }))
    if (candidates.children.length === 0) {
      const empty = document.createElement('li')
      empty.textContent = 'No candidates were produced by this run.'
      candidates.append(empty)
    }

    conclusion.dataset.status = run.status
    conclusionStatus.textContent = run.status
    conclusionText.textContent = run.finishedAt === null
      ? 'This run has not finished yet.'
      : `Finished ${run.finishedAt}`
  }

  return {
    root,
    update,
    close() {
      root.remove?.()
    },
  }
}
