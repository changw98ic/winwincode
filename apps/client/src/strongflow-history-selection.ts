// SPDX-License-Identifier: Apache-2.0

import type { WorkItemId, WorkRunId } from './generated/contracts.js'
import type { StrongFlowHistoryTree } from './strongflow-history-tree.js'

/**
 * Presentation-only history selection model for Task and StageRun review.
 *
 * The selection never feeds Control Plane commands and never replaces the
 * StrongFlow view-model's canonical Delivery/StageRun binding: it only decides
 * which already-delivered projection rows the browser expands or renders.
 * Older attempts therefore stay read-only review targets, while the Server
 * remains the sole authority for every mutation.
 */

export const STRONGFLOW_HISTORY_TASK_PARAMETER = 'task'
export const STRONGFLOW_HISTORY_RUN_PARAMETER = 'run'

/** Browser history seam: reads the route hash and replaces it without remounting the route. */
export interface StrongFlowHistoryLocation {
  hash(): string
  replaceHash(hash: string): void
}

export interface StrongFlowHistorySelection {
  readonly taskId: WorkItemId | null
  readonly workRunId: WorkRunId | null
}

export const EMPTY_SELECTION: StrongFlowHistorySelection = Object.freeze({
  taskId: null,
  workRunId: null,
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
  const workRunId = parameters.get(STRONGFLOW_HISTORY_RUN_PARAMETER)
  return Object.freeze({
    taskId: taskId === null ? null : taskId as WorkItemId,
    workRunId: workRunId === null ? null : workRunId as WorkRunId,
  })
}

/**
 * Merge the selection into a StrongFlow route hash. Binding parameters such as
 * `delivery`, `session`, and `workRun` stay untouched because they still own
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
  if (selection.workRunId === null) parameters.delete(STRONGFLOW_HISTORY_RUN_PARAMETER)
  else parameters.set(STRONGFLOW_HISTORY_RUN_PARAMETER, selection.workRunId)
  const encoded = parameters.toString()
  return encoded.length === 0 ? base : `${base}?${encoded}`
}

export function sameHistorySelection(
  left: StrongFlowHistorySelection,
  right: StrongFlowHistorySelection,
): boolean {
  return left.taskId === right.taskId && left.workRunId === right.workRunId
}

/**
 * Keep only selection identities that exist in the current tree, and only in
 * their canonical Task association. This pure normalization result lets the
 * navigation layer detect each mismatch and fail closed without rewriting a
 * crossed or stale deep link onto another Task or StageRun.
 */
export function strongFlowHistorySelectionForTree(
  tree: StrongFlowHistoryTree,
  requested: StrongFlowHistorySelection,
): StrongFlowHistorySelection {
  const taskKnown = requested.taskId !== null
    && tree.tasks.some(node => node.task.id === requested.taskId)
  const run = requested.workRunId === null
    ? undefined
    : tree.runs.find(candidate => candidate.workRunId === requested.workRunId)
  if (taskKnown && run !== undefined && run.workItemId === requested.taskId) {
    return requested
  }
  return Object.freeze({
    // A known run owns the truth: its WorkItem decides the task. Without a
    // known run the task survives only if it still exists in the tree.
    taskId: run === undefined
      ? (taskKnown ? requested.taskId : null)
      : run.workItemId,
    workRunId: run === undefined ? null : requested.workRunId,
  })
}
