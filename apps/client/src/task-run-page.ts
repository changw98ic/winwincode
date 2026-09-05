// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneTaskAnchor } from './control-plane-client.js'
import type {
  TaskRunApplyFacts,
  TaskRunCandidateFacts,
  TaskRunIdentityFacts,
  TaskRunState,
  TaskRunViewModel,
  TaskRunWorkerSessionFacts,
} from './task-run-view-model.js'
import { taskRunCommitText } from './task-run-view-model.js'

export interface TaskRunPageOptions {
  readonly root: HTMLElement
  readonly model: TaskRunViewModel
  /** The My Work deep link, so the running task is never a dead end. */
  readonly homeHref?: string
}

export interface TaskRunPage {
  close(): void
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

/**
 * Mount the §16.7 run page's local identity zone: the Client, Repository,
 * Occupancy, WorkerSession, latest Candidate, and Apply rows of the running
 * task.  The module owns DOM and ARIA only; every value is projected from the
 * one run view-model snapshot, and a missing row names its gap instead of
 * inventing state.
 */
export function mountTaskRunPage(options: TaskRunPageOptions): TaskRunPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-task-run')
  const heading = element(document, 'p', 'wwc-task-run-heading')
  const description = element(document, 'p', 'wwc-task-run-description')
  const zone = element(document, 'section', 'wwc-task-run-identity')
  const zoneHeading = element(document, 'p', 'wwc-task-run-identity-heading')
  const rows = element(document, 'div', 'wwc-task-run-rows')
  const identityNotice = element(document, 'p', 'wwc-task-run-identity-notice')
  const back = element(document, 'a', 'wwc-task-run-back')

  let closed = false

  section.setAttribute('aria-label', 'Running task')
  heading.id = 'wwc-task-run-heading'
  heading.textContent = 'Running task'
  section.setAttribute('aria-labelledby', heading.id)
  description.hidden = true
  zone.setAttribute('aria-labelledby', 'wwc-task-run-identity-heading')
  zoneHeading.id = 'wwc-task-run-identity-heading'
  zoneHeading.textContent = 'Local identity'
  identityNotice.setAttribute('role', 'status')
  identityNotice.hidden = true
  back.textContent = 'Back to My Work'
  if (options.homeHref !== undefined) back.href = options.homeHref
  else back.hidden = true

  interface RowRefs {
    readonly row: HTMLElement
    readonly term: HTMLElement
    readonly value: HTMLElement
    readonly badge: HTMLElement
    readonly detail: HTMLElement
  }

  function createRow(term: string): RowRefs {
    const row = element(document, 'div', 'wwc-task-run-row')
    row.dataset.taskRunRow = term
    const termNode = element(document, 'p', 'wwc-task-run-row-term')
    termNode.textContent = term
    const value = element(document, 'p', 'wwc-task-run-row-value')
    const badge = element(document, 'span', 'wwc-task-run-row-badge')
    badge.hidden = true
    const detail = element(document, 'p', 'wwc-task-run-row-detail')
    detail.hidden = true
    value.append(badge)
    row.append(termNode, value, detail)
    return { row, term: termNode, value, badge, detail }
  }

  const clientRow = createRow('Client')
  const repositoryRow = createRow('Repository')
  const occupancyRow = createRow('Occupancy')
  const workerRow = createRow('Worker sessions')
  const candidateRow = createRow('Candidate')
  const applyRow = createRow('Apply')
  rows.append(
    clientRow.row,
    repositoryRow.row,
    occupancyRow.row,
    workerRow.row,
    candidateRow.row,
    applyRow.row,
  )
  zone.append(zoneHeading, rows, identityNotice)
  section.append(heading, description, zone, back)
  options.root.replaceChildren(section)

  function renderBadge(refs: RowRefs, text: string | null, tone: string | null): void {
    if (text === null || tone === null) {
      refs.badge.hidden = true
      refs.badge.textContent = ''
      delete refs.badge.dataset.tone
      return
    }
    refs.badge.hidden = false
    refs.badge.textContent = text
    refs.badge.dataset.tone = tone
  }

  function renderRow(
    refs: RowRefs,
    value: string | null,
    badge: { readonly text: string; readonly tone: string } | null,
    detail: string | null,
    pendingText: string,
  ): void {
    refs.row.hidden = false
    if (value === null) {
      // A row without a fact names the gap; it never invents state.
      refs.value.classList.add('wwc-task-run-row-pending')
      refs.value.textContent = pendingText
      refs.value.append(refs.badge)
      renderBadge(refs, null, null)
    } else {
      refs.value.classList.remove('wwc-task-run-row-pending')
      refs.value.textContent = value
      refs.value.append(refs.badge)
      renderBadge(refs, badge?.text ?? null, badge?.tone ?? null)
    }
    if (detail === null) {
      refs.detail.hidden = true
      refs.detail.textContent = ''
    } else {
      refs.detail.hidden = false
      refs.detail.textContent = detail
    }
  }

  function renderWorkerSessions(sessions: readonly TaskRunWorkerSessionFacts[]): string {
    if (sessions.length === 0) return 'No WorkerSession is running.'
    return sessions.map(session => [
      session.workerSessionId,
      session.stateText,
      session.startedAt === null ? null : `Started ${session.startedAt}`,
    ].filter(part => part !== null).join(' · ')).join('; ')
  }

  function renderCandidate(candidate: TaskRunCandidateFacts | null): string | null {
    if (candidate === null) return null
    return [candidate.candidateRef, candidate.branchName]
      .filter(part => part !== null)
      .join(' · ')
  }

  function renderApply(apply: TaskRunApplyFacts | null): { value: string; detail: string | null } | null {
    if (apply === null) return null
    const commit = taskRunCommitText(apply.resultingCommit)
    return {
      value: apply.resultText,
      detail: [
        `Strategy ${apply.strategy}`,
        `Target ${apply.targetBranch}`,
        commit === null ? null : `Resulting HEAD ${commit}`,
      ].filter(part => part !== null).join(' · '),
    }
  }

  function renderIdentity(identity: TaskRunIdentityFacts | null): void {
    renderRow(
      workerRow,
      identity === null ? null : renderWorkerSessions(identity.workerSessions),
      null,
      null,
      'WorkerSession facts are loading…',
    )
    renderRow(
      candidateRow,
      identity === null
        ? null
        : renderCandidate(identity.candidate) ?? 'No Candidate yet.',
      identity?.candidate == null
        ? null
        : { text: identity.candidate.stateText, tone: identity.candidate.tone },
      null,
      'Candidate facts are loading…',
    )
    const apply = identity === null ? null : renderApply(identity.apply)
    renderRow(
      applyRow,
      apply === null ? (identity === null ? null : 'No apply attempts yet.') : apply.value,
      null,
      apply?.detail ?? null,
      'Apply facts are loading…',
    )
  }

  function render(snapshot: TaskRunState): void {
    if (closed) return
    section.setAttribute('aria-busy', String(snapshot.status === 'loading'))
    if (snapshot.taskDescription !== null) {
      description.textContent = snapshot.taskDescription
      description.hidden = false
    } else {
      description.hidden = true
      description.textContent = ''
    }
    renderRow(
      clientRow,
      snapshot.client === null ? null : snapshot.client.displayName,
      snapshot.client === null
        ? null
        : { text: snapshot.client.stateText, tone: snapshot.client.tone },
      null,
      'Client facts are loading…',
    )
    renderRow(
      repositoryRow,
      snapshot.repository === null
        ? null
        : `${snapshot.repository.displayName} · base ${snapshot.repository.defaultBranch}`,
      null,
      null,
      'Repository facts are loading…',
    )
    renderRow(
      occupancyRow,
      snapshot.occupancy === null ? null : snapshot.occupancy.stateText,
      snapshot.occupancy === null
        ? null
        : { text: snapshot.occupancy.capacityText, tone: snapshot.occupancy.tone },
      null,
      'Occupancy facts are loading…',
    )
    renderIdentity(snapshot.identity)
    identityNotice.hidden = snapshot.identityStatus !== 'unavailable'
    identityNotice.textContent = snapshot.identityStatus === 'unavailable'
      ? 'The WorkerSession and Candidate facts are unreachable right now. The Client, Repository, and Occupancy rows keep their last known values.'
      : ''
  }

  const unsubscribe = options.model.subscribe(render)

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.root.replaceChildren()
    },
  }
}
