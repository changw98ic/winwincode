// SPDX-License-Identifier: Apache-2.0

import type { DeliveryTaskId, EvidenceId } from './generated/contracts.js'
import {
  EMPTY_SELECTION,
  sameHistorySelection,
  strongFlowHistorySelectionForTree,
  type StrongFlowHistorySelection,
} from './strongflow-history-selection.js'
import type {
  StrongFlowHistoryRun,
  StrongFlowHistoryTaskNode,
  StrongFlowHistoryTree,
} from './strongflow-history-tree.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'

/** Which list a mounted run row belongs to; ArrowLeft collapse is task-scoped. */
type RunRowContext = 'task' | 'timeline'

export interface StrongFlowHistoryNavigationOptions {
  readonly document: Document
  readonly tasksParent: HTMLElement
  readonly stagesParent: HTMLElement
  readonly tasksOmitted: HTMLElement
  readonly stagesOmitted: HTMLElement
  readonly tasksEmpty: HTMLElement
  readonly stagesEmpty: HTMLElement
  readonly initialSelection?: StrongFlowHistorySelection
  readonly onSelect: (selection: StrongFlowHistorySelection) => void
  readonly onOpenEvidence: (evidenceId: EvidenceId) => void
  /** Reports an invalid deep-link selection without rewriting it to another run. */
  readonly onUnavailable?: (selection: StrongFlowHistorySelection) => void
}

export interface StrongFlowHistoryNavigationView {
  update(tree: StrongFlowHistoryTree | null): void
  selection(): StrongFlowHistorySelection
  close(): void
}

interface RunRowState {
  run: StrongFlowHistoryRun
  context: RunRowContext
  button: HTMLButtonElement
  evidence: HTMLElement
  evidenceButtons: KeyedCollectionView<EvidenceId, string, HTMLButtonElement>
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

interface MountedTaskToggle {
  readonly toggle: HTMLButtonElement
  readonly item: HTMLLIElement
}

function runLabel(run: StrongFlowHistoryRun): string {
  const attempt = run.attempt === null ? '' : ` · attempt ${String(run.attempt)}`
  return `${run.stage} · ${run.role}${attempt} · ${run.status}`
}

/**
 * Mount the clickable Task → StageRun → Attempt navigation for one shared
 * history tree. Rows stay keyed by business identity so repeated snapshots
 * never rebuild unchanged DOM. Invalid Task→StageRun associations remain
 * explicit route failures instead of being rewritten onto another run.
 */
export function mountStrongFlowHistoryNavigation(
  options: StrongFlowHistoryNavigationOptions,
): StrongFlowHistoryNavigationView {
  const document = options.document
  let closed = false
  let selection = options.initialSelection ?? EMPTY_SELECTION
  let selectionUnavailable = false
  let currentTree: StrongFlowHistoryTree | null = null
  const expandedTasks = new Set<DeliveryTaskId>()
  const taskRows = new WeakMap<HTMLLIElement, TaskRowState>()
  const runRows = new WeakMap<HTMLLIElement, RunRowState>()
  const evidenceButtonListeners = new WeakMap<HTMLButtonElement, () => void>()
  let taskCollection: ReturnType<typeof mountTaskCollection>
  let timelineCollection: KeyedCollectionView<StrongFlowHistoryRun, string, HTMLLIElement>

  function applySelection(next: StrongFlowHistorySelection): void {
    selection = next
    selectionUnavailable = false
    update(currentTree)
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

  function createRunRow(context: RunRowContext): () => HTMLLIElement {
    return () => {
      const item = document.createElement('li')
      const button = document.createElement('button')
      const evidence = document.createElement('span')
      button.type = 'button'
      evidence.className = 'wwc-strongflow-stage-evidence'
      item.append(button, evidence)
      const evidenceButtons = mountKeyedCollection<EvidenceId, string, HTMLButtonElement>({
        parent: evidence,
        key: evidenceId => evidenceId,
        create() {
          const opener = document.createElement('button')
          opener.type = 'button'
          opener.className = 'wwc-strongflow-stage-evidence-open'
          const onOpen = () => {
            const evidenceId = opener.dataset.evidenceId
            if (evidenceId !== undefined) options.onOpenEvidence(evidenceId as EvidenceId)
          }
          opener.addEventListener('click', onOpen)
          evidenceButtonListeners.set(opener, onOpen)
          return opener
        },
        update(opener, evidenceId) {
          opener.dataset.evidenceId = evidenceId
          opener.textContent = `Open Evidence ${evidenceId}`
        },
        remove(opener) {
          const onOpen = evidenceButtonListeners.get(opener)
          if (onOpen !== undefined) opener.removeEventListener('click', onOpen)
          evidenceButtonListeners.delete(opener)
        },
      })
      const state: RunRowState = {
        run: undefined as unknown as StrongFlowHistoryRun,
        context,
        button,
        evidence,
        evidenceButtons,
        onClick: () => activateRun(state),
        onKeydown: (event: KeyboardEvent) => handleRunKeydown(event, state),
      }
      button.addEventListener('click', state.onClick)
      button.addEventListener('keydown', state.onKeydown)
      runRows.set(item, state)
      return item
    }
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
    const evidenceIds = run.evidence.map(row => row.id)
    state.evidenceButtons.update(evidenceIds)
    state.evidence.hidden = evidenceIds.length === 0
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
    context: RunRowContext,
  ): KeyedCollectionView<StrongFlowHistoryRun, string, HTMLLIElement> {
    return mountKeyedCollection<StrongFlowHistoryRun, string, HTMLLIElement>({
      parent,
      key: run => run.stageRunId,
      create: createRunRow(context),
      update: updateRunRow,
      remove(item) {
        const state = runRows.get(item)
        if (state === undefined) return
        state.button.removeEventListener('click', state.onClick)
        state.button.removeEventListener('keydown', state.onKeydown)
        state.evidenceButtons.close()
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
      runs: mountRunCollection(runList, 'task'),
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
    item.dataset.deliveryTaskId = node.task.id
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
  timelineCollection = mountRunCollection(options.stagesParent, 'timeline')

  function taskToggleOf(taskId: DeliveryTaskId): MountedTaskToggle | null {
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
    if (event.key === 'ArrowLeft' && state.context === 'task') {
      // ArrowLeft collapses only the Task list that owns this row. Timeline
      // rows must never reach into the separate Task tree.
      const ownerTaskId = state.run.deliveryTaskId
      event.preventDefault()
      if (ownerTaskId === null) return
      expandedTasks.delete(ownerTaskId)
      update(currentTree)
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
        update(currentTree)
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
      update(currentTree)
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
      update(currentTree)
      return
    }
    expandedTasks.delete(taskId)
    if (selection.taskId === taskId) {
      // Collapsing a Task closes its history review instead of keeping a
      // hidden selection alive.
      applySelection(EMPTY_SELECTION)
      return
    }
    update(currentTree)
  }

  function noteOmitted(node: HTMLElement, count: number, label: string): void {
    node.hidden = count === 0
    const text = `${String(count)} more ${label} not shown.`
    if (node.textContent !== text) node.textContent = text
  }

  function update(tree: StrongFlowHistoryTree | null): void {
    if (closed) return
    currentTree = tree
    if (tree === null) {
      const hadSelection = !sameHistorySelection(selection, EMPTY_SELECTION)
      taskCollection.update([])
      timelineCollection.update([])
      noteOmitted(options.tasksOmitted, 0, 'tasks')
      noteOmitted(options.stagesOmitted, 0, 'stages')
      options.tasksEmpty.hidden = true
      options.stagesEmpty.hidden = true
      if (hadSelection) {
        selectionUnavailable = true
        options.onUnavailable?.(selection)
      }
      return
    }
    const requested = selection
    const normalized = strongFlowHistorySelectionForTree(tree, selection)
    const unavailable = !sameHistorySelection(normalized, requested)
    selectionUnavailable = unavailable
    if (!unavailable) selection = normalized
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
    if (unavailable) {
      options.onUnavailable?.(requested)
    }
  }

  return {
    update,
    selection() {
      return selectionUnavailable ? EMPTY_SELECTION : selection
    },
    close() {
      if (closed) return
      closed = true
      taskCollection.close()
      timelineCollection.close()
      // Release the last snapshot so a closed view keeps no Delivery state.
      currentTree = null
      selection = EMPTY_SELECTION
    },
  }
}
