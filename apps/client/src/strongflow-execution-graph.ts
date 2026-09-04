// SPDX-License-Identifier: Apache-2.0

import type { EvidenceId, RuntimeSessionProjection } from './generated/contracts.js'
import {
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'

/**
 * One Evidence record that a timeline activity may jump to. It is a safe
 * subset of the delivered DeliveryEvidenceProjection: an activity row only
 * links to Evidence from its own StageRun and session binding, and only when
 * the Control Plane actually delivered that record.
 */
export interface StrongFlowExecutionEvidenceLink {
  readonly id: EvidenceId
  readonly type: string
  readonly stageRunId: string | null
  readonly sessionBindingId: string
}

/** Projects one delivered Evidence record onto the timeline jump target. */
export function strongFlowExecutionEvidenceLink(
  evidence: {
    readonly id: EvidenceId
    readonly type: string
    readonly stageRunId: string | null
    readonly sessionBindingId: string
  },
): StrongFlowExecutionEvidenceLink {
  return Object.freeze({
    id: evidence.id,
    type: evidence.type,
    stageRunId: evidence.stageRunId,
    sessionBindingId: evidence.sessionBindingId,
  })
}

/** One display-only gating decision (approval/attention) tied to the run. */
export interface StrongFlowExecutionApprovalRow {
  readonly id: string
  readonly title: string
  readonly type: string
  readonly status: string
  readonly blocking: boolean
}

export interface StrongFlowExecutionGraphState {
  readonly heading: string
  readonly emptyText: string
  readonly sessions: readonly RuntimeSessionProjection[]
  readonly evidence: readonly StrongFlowExecutionEvidenceLink[]
  readonly approvals: readonly StrongFlowExecutionApprovalRow[]
  readonly readOnly: boolean
}

export interface StrongFlowExecutionGraphOptions {
  readonly document: Document
  readonly limits: StrongFlowRenderLimits
  /** Invoked only for Evidence ids that the caller actually delivered. */
  readonly onOpenEvidence: (evidenceId: EvidenceId) => void
}

export interface StrongFlowExecutionGraphView {
  readonly root: HTMLElement
  update(state: StrongFlowExecutionGraphState): void
  close(): void
}

/** Largest number of collapsible activity type groups rendered per session. */
const MAX_ACTIVITY_GROUPS = 8
/** Safety cap for remembered fold/filter choices; unreachable in practice. */
const MAX_REMEMBERED_TYPES = 64

type RuntimeActivity = RuntimeSessionProjection['activities'][number]
type RuntimeAgent = RuntimeSessionProjection['agents'][number]

function sessionKey(session: RuntimeSessionProjection): string {
  return `${session.productSessionId}:${session.stageRunId ?? 'none'}:${session.sessionBindingId}`
}

function setText(node: HTMLElement, text: string): void {
  if (node.textContent !== text) node.textContent = text
}

/**
 * Deterministic depth-first order of the agent graph: roots (no parent or an
 * unknown parent) first, children after their parent, siblings ordered by
 * thread id. The result is a flat bounded list; `data-depth` carries the
 * parent-child nesting for presentation.
 */
export function strongFlowAgentGraphOrder(
  agents: readonly RuntimeAgent[],
): readonly { readonly agent: RuntimeAgent; readonly depth: number }[] {
  const byId = new Map(agents.map(agent => [agent.threadId, agent]))
  const childrenOf = new Map<string, RuntimeAgent[]>()
  const roots: RuntimeAgent[] = []
  for (const agent of agents) {
    const parent = agent.parentThreadId === null ? undefined : byId.get(agent.parentThreadId)
    if (parent === undefined) {
      roots.push(agent)
      continue
    }
    const children = childrenOf.get(parent.threadId) ?? []
    children.push(agent)
    childrenOf.set(parent.threadId, children)
  }
  const byThreadId = (left: RuntimeAgent, right: RuntimeAgent): number =>
    left.threadId < right.threadId ? -1 : left.threadId > right.threadId ? 1 : 0
  roots.sort(byThreadId)
  for (const children of childrenOf.values()) children.sort(byThreadId)
  const ordered: { agent: RuntimeAgent; depth: number }[] = []
  const visited = new Set<string>()
  const visit = (agent: RuntimeAgent, depth: number): void => {
    if (visited.has(agent.threadId)) return
    visited.add(agent.threadId)
    ordered.push({ agent, depth })
    for (const child of childrenOf.get(agent.threadId) ?? []) visit(child, depth + 1)
  }
  for (const root of roots) visit(root, 0)
  return ordered
}

interface ActivityTimelineInput {
  readonly session: RuntimeSessionProjection
  readonly evidence: readonly StrongFlowExecutionEvidenceLink[]
  readonly readOnly: boolean
}

interface ActivityTimelineView {
  readonly root: HTMLElement
  update(input: ActivityTimelineInput): void
  close(): void
}

interface ActivityRowState {
  readonly text: HTMLElement
  evidenceId: EvidenceId | null
  jump: HTMLButtonElement | null
  onJump: () => void
}

interface ActivityGroupState {
  readonly toggle: HTMLButtonElement
  readonly rows: HTMLElement
  readonly rowCollection: KeyedCollectionView<RuntimeActivity, string, HTMLLIElement>
  onToggle: () => void
}

/**
 * One collapsible, type-filterable activity timeline for exactly one runtime
 * session. Rows are keyed by call id, the rendered window is bounded by the
 * shared render limits, and fold/filter choices live in this view so
 * equivalent snapshots never touch the DOM (and never drop focus, scroll, or
 * context). An activity row only renders an Evidence jump when the Control
 * Plane actually delivered a matching Evidence record.
 *
 * This is the one canonical timeline presentation: the live execution graph
 * and the read-only historical run detail both mount it per runtime session.
 */
export function mountStrongFlowActivityTimeline(options: {
  document: Document
  limits: StrongFlowRenderLimits
  onOpenEvidence: (evidenceId: EvidenceId) => void
}): ActivityTimelineView {
  const document = options.document
  const root = strongFlowElement(document, 'div', 'wwc-strongflow-activity-timeline')
  const filters = strongFlowElement(document, 'div', 'wwc-strongflow-activity-filters')
  filters.setAttribute('role', 'group')
  filters.setAttribute('aria-label', 'Filter activities by type')
  const groups = strongFlowElement(document, 'div', 'wwc-strongflow-activity-groups')
  const omitted = strongFlowElement(document, 'p', 'wwc-strongflow-activity-omitted')
  omitted.hidden = true
  root.append(filters, groups, omitted)

  const collapsedTypes = new Set<string>()
  const hiddenTypes = new Set<string>()
  const rowStates = new WeakMap<HTMLLIElement, ActivityRowState>()
  const groupStates = new WeakMap<HTMLLIElement, ActivityGroupState>()
  let closed = false
  let lastFingerprint: string | null = null
  let timelineSession: RuntimeSessionProjection | null = null
  let timelineSessionKey = ''
  let evidenceIndex = new Map<string, EvidenceId>()

  function evidenceIdFor(activity: RuntimeActivity): EvidenceId | null {
    const session = timelineSession
    if (session === null) return null
    const key = `${String(activity.activityType)}:${session.sessionBindingId}:${session.stageRunId ?? 'none'}`
    return evidenceIndex.get(key) ?? null
  }

  function createRow(): HTMLLIElement {
    const item = document.createElement('li')
    const text = document.createElement('span')
    item.className = 'wwc-strongflow-activity-row'
    text.className = 'wwc-strongflow-activity-text'
    const state: ActivityRowState = {
      text,
      evidenceId: null,
      jump: null,
      onJump: () => {
        if (state.evidenceId !== null) options.onOpenEvidence(state.evidenceId)
      },
    }
    item.append(text)
    rowStates.set(item, state)
    return item
  }

  function updateRow(item: HTMLLIElement, activity: RuntimeActivity): void {
    const state = rowStates.get(item)
    if (state === undefined || timelineSession === null) return
    item.dataset.callId = activity.callId
    item.dataset.activityType = String(activity.activityType)
    item.dataset.status = String(activity.status)
    item.dataset.outcome = String(activity.outcome)
    const command = activity.command ?? activity.callId
    const exit = activity.exitCode === null ? '' : ` · exit ${String(activity.exitCode)}`
    setText(
      state.text,
      `${String(activity.activityType)}: ${command} · ${String(activity.status)} · ${String(activity.outcome)}${exit}`,
    )
    const evidenceId = evidenceIdFor(activity)
    state.evidenceId = evidenceId
    if (evidenceId === null) {
      state.jump?.removeEventListener('click', state.onJump)
      state.jump?.remove()
      state.jump = null
      return
    }
    if (state.jump === null) {
      const jump = document.createElement('button') as HTMLButtonElement
      jump.type = 'button'
      jump.className = 'wwc-strongflow-activity-evidence'
      jump.addEventListener('click', state.onJump)
      state.jump = jump
      item.append(jump)
    }
    state.jump.dataset.evidenceId = evidenceId
    setText(state.jump, 'Open evidence')
  }

  function removeRow(item: HTMLLIElement): void {
    const state = rowStates.get(item)
    if (state === undefined) return
    if (state.jump !== null) {
      state.jump.removeEventListener('click', state.onJump)
      state.jump = null
    }
    rowStates.delete(item)
  }

  function createGroup(): HTMLLIElement {
    const item = document.createElement('li')
    const toggle = document.createElement('button') as HTMLButtonElement
    const rows = document.createElement('ul')
    item.className = 'wwc-strongflow-activity-group'
    toggle.type = 'button'
    toggle.className = 'wwc-strongflow-activity-group-toggle'
    toggle.setAttribute('aria-expanded', 'true')
    rows.className = 'wwc-strongflow-activity-rows'
    const state: ActivityGroupState = {
      toggle,
      rows,
      rowCollection: mountKeyedCollection<RuntimeActivity, string, HTMLLIElement>({
        parent: rows,
        key: activity => activity.callId,
        create: createRow,
        update: updateRow,
        remove: removeRow,
      }),
      onToggle: () => {
        const type = item.dataset.activityType ?? ''
        if (collapsedTypes.has(type)) collapsedTypes.delete(type)
        else collapsedTypes.add(type)
        applyFold(state, type)
      },
    }
    toggle.addEventListener('click', state.onToggle)
    item.append(toggle, rows)
    groupStates.set(item, state)
    return item
  }

  function applyFold(state: ActivityGroupState, type: string): void {
    const expanded = !collapsedTypes.has(type)
    state.toggle.setAttribute('aria-expanded', String(expanded))
    state.rows.hidden = !expanded
  }

  function updateGroup(
    item: HTMLLIElement,
    group: { readonly type: string; readonly activities: readonly RuntimeActivity[] },
  ): void {
    const state = groupStates.get(item)
    if (state === undefined) return
    item.dataset.activityType = group.type
    state.rows.id = `wwc-strongflow-activity-rows-${group.type}-${timelineSessionKey}`
    state.toggle.setAttribute('aria-controls', state.rows.id)
    setText(state.toggle, `${group.type} · ${String(group.activities.length)}`)
    state.rowCollection.update([...group.activities])
    applyFold(state, group.type)
    item.hidden = hiddenTypes.has(group.type)
  }

  function removeGroup(item: HTMLLIElement): void {
    const state = groupStates.get(item)
    if (state === undefined) return
    state.toggle.removeEventListener('click', state.onToggle)
    state.rowCollection.close()
    groupStates.delete(item)
  }

  const groupCollection = mountKeyedCollection<{
    readonly type: string
    readonly activities: readonly RuntimeActivity[]
  }, string, HTMLLIElement>({
    parent: groups,
    key: group => group.type,
    create: createGroup,
    update: updateGroup,
    remove: removeGroup,
  })

  const chipStates = new WeakMap<HTMLButtonElement, { onToggle: () => void }>()
  const chipCollection = mountKeyedCollection<{
    readonly type: string
    readonly count: number
  }, string, HTMLButtonElement>({
    parent: filters,
    key: chip => chip.type,
    create: () => {
      const chip = document.createElement('button') as HTMLButtonElement
      chip.type = 'button'
      chip.className = 'wwc-strongflow-activity-filter'
      const onToggle = () => {
        const type = chip.dataset.activityType ?? ''
        if (hiddenTypes.has(type)) hiddenTypes.delete(type)
        else if (hiddenTypes.size < MAX_REMEMBERED_TYPES) hiddenTypes.add(type)
        chip.setAttribute('aria-pressed', String(!hiddenTypes.has(type)))
        for (const item of [...groups.children] as HTMLLIElement[]) {
          const groupType = item.dataset.activityType ?? ''
          if (groupType !== '') item.hidden = hiddenTypes.has(groupType)
        }
      }
      chip.addEventListener('click', onToggle)
      chipStates.set(chip, { onToggle })
      return chip
    },
    update(chip, filter) {
      chip.dataset.activityType = filter.type
      chip.setAttribute('aria-pressed', String(!hiddenTypes.has(filter.type)))
      setText(chip, `${filter.type} · ${String(filter.count)}`)
    },
    remove(chip) {
      const state = chipStates.get(chip)
      if (state !== undefined) {
        chip.removeEventListener('click', state.onToggle)
        chipStates.delete(chip)
      }
    },
  })

  return {
    root,
    update(input) {
      if (closed) return
      const fingerprint = JSON.stringify([
        input.session,
        input.evidence,
        input.readOnly,
      ])
      if (fingerprint === lastFingerprint) return
      lastFingerprint = fingerprint
      timelineSession = input.session
      timelineSessionKey = sessionKey(input.session)
      evidenceIndex = new Map()
      for (const record of input.evidence) {
        const key = `${record.type}:${record.sessionBindingId}:${record.stageRunId ?? 'none'}`
        const current = evidenceIndex.get(key)
        if (current === undefined || record.id < current) {
          evidenceIndex.set(key, record.id)
        }
      }
      const bounded = boundedItems([...input.session.activities], options.limits.activities)
      const seen = new Set<string>()
      const unique = bounded.items.filter(activity => {
        if (seen.has(activity.callId)) return false
        seen.add(activity.callId)
        return true
      })
      const typeOrder: string[] = []
      const byType = new Map<string, RuntimeActivity[]>()
      for (const activity of unique) {
        const type = String(activity.activityType)
        if (!byType.has(type)) {
          byType.set(type, [])
          typeOrder.push(type)
        }
        byType.get(type)?.push(activity)
      }
      const renderedTypes = typeOrder.slice(0, MAX_ACTIVITY_GROUPS)
      const omittedGroups = Math.max(0, typeOrder.length - MAX_ACTIVITY_GROUPS)
      if (hiddenTypes.size > MAX_REMEMBERED_TYPES) hiddenTypes.clear()
      if (collapsedTypes.size > MAX_REMEMBERED_TYPES) collapsedTypes.clear()
      chipCollection.update(renderedTypes.map(type => ({
        type,
        count: byType.get(type)?.length ?? 0,
      })))
      groupCollection.update(renderedTypes.map(type => ({
        type,
        activities: byType.get(type) ?? [],
      })))
      omitted.hidden = bounded.omitted === 0 && omittedGroups === 0
      setText(omitted, `${String(bounded.omitted)} more runtime activities not shown.`
        + (omittedGroups > 0
          ? ` ${String(omittedGroups)} more activity types not shown.`
          : ''))
    },
    close() {
      if (closed) return
      closed = true
      chipCollection.close()
      groupCollection.close()
      timelineSession = null
      lastFingerprint = null
      evidenceIndex = new Map()
      root.remove?.()
    },
  }
}

interface SessionViewState {
  readonly section: HTMLElement
  readonly heading: HTMLElement
  readonly metadata: HTMLElement
  readonly outcomeValues: readonly HTMLElement[]
  readonly agentList: HTMLElement
  readonly agentNodes: Map<string, HTMLLIElement>
  readonly timeline: ActivityTimelineView
  fingerprint: string | null
}

/**
 * Mount the execution graph and timeline for one StageRun's runtime sessions.
 *
 * The view is a pure client-side projection of already-delivered typed
 * snapshots: every collection is keyed by business identity, every window is
 * bounded by the shared render limits, and equivalent snapshots never rebuild
 * DOM, so high-frequency deltas cannot jitter the page or grow it without
 * bound. Read-only mode (historical attempts) renders the same projection
 * without the approvals surface and marks itself read-only.
 */
export function mountStrongFlowExecutionGraph(
  options: StrongFlowExecutionGraphOptions,
): StrongFlowExecutionGraphView {
  const document = options.document
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-execution-graph')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const empty = strongFlowElement(document, 'p', 'wwc-strongflow-execution-empty')
  const approvalsHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const approvals = strongFlowElement(document, 'ul', 'wwc-strongflow-execution-approvals')
  const sessions = strongFlowElement(document, 'div', 'wwc-strongflow-execution-sessions')
  const omitted = strongFlowElement(document, 'p', 'wwc-strongflow-execution-omitted')
  empty.hidden = true
  approvals.hidden = true
  approvals.setAttribute('aria-label', 'Approvals and attention')
  approvalsHeading.textContent = 'Approvals and attention'
  omitted.hidden = true
  root.append(heading, empty, approvalsHeading, approvals, sessions, omitted)

  const approvalChips = mountKeyedCollection<StrongFlowExecutionApprovalRow, string, HTMLLIElement>({
    parent: approvals,
    key: approval => approval.id,
    create: () => {
      const chip = document.createElement('li')
      chip.className = 'wwc-strongflow-approval-chip'
      return chip
    },
    update(chip, approval) {
      chip.dataset.approvalId = approval.id
      chip.dataset.blocking = String(approval.blocking)
      setText(chip, `${approval.title} · ${approval.type} · ${approval.status}${approval.blocking ? ' · blocking' : ''}`)
    },
  })

  const sessionViews = new Map<string, SessionViewState>()
  let currentEvidence: readonly StrongFlowExecutionEvidenceLink[] = []
  let currentReadOnly = false

  function createSessionView(): SessionViewState {
    const section = strongFlowElement(document, 'section', 'wwc-strongflow-execution-session')
    const sessionHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-execution-heading')
    const metadata = strongFlowElement(document, 'p', 'wwc-strongflow-execution-metadata')
    const outcome = strongFlowElement(document, 'dl', 'wwc-strongflow-execution-outcome')
    const outcomeValues = [
      'Agents',
      'Agent links',
      'Current activity',
      'Diff',
      'Usage',
      'Failures',
    ].map(term => {
      const termNode = document.createElement('dt')
      const valueNode = document.createElement('dd')
      termNode.textContent = term
      outcome.append(termNode, valueNode)
      return valueNode
    })
    const agentList = strongFlowElement(document, 'ul', 'wwc-strongflow-agent-graph')
    agentList.setAttribute('aria-label', 'Agent graph')
    const timeline = mountStrongFlowActivityTimeline({
      document,
      limits: options.limits,
      onOpenEvidence: options.onOpenEvidence,
    })
    section.append(sessionHeading, metadata, outcome, agentList, timeline.root)
    return {
      section,
      heading: sessionHeading,
      metadata,
      outcomeValues,
      agentList,
      agentNodes: new Map(),
      timeline,
      fingerprint: null,
    }
  }

  function updateSessionAgents(view: SessionViewState, session: RuntimeSessionProjection): void {
    const boundedAgents = boundedItems(session.agents, options.limits.graphNodes)
    const ordered = strongFlowAgentGraphOrder(boundedAgents.items)
    const retained = new Set<string>(ordered.map(entry => entry.agent.threadId))
    for (const [threadId, node] of view.agentNodes) {
      if (retained.has(threadId)) continue
      node.remove()
      view.agentNodes.delete(threadId)
    }
    ordered.forEach((entry, index) => {
      let node = view.agentNodes.get(entry.agent.threadId)
      if (node === undefined) {
        node = document.createElement('li')
        node.className = 'wwc-strongflow-agent-node'
        const label = document.createElement('strong')
        const detail = document.createElement('span')
        label.className = 'wwc-strongflow-agent-node-label'
        detail.className = 'wwc-strongflow-agent-node-detail'
        node.append(label, detail)
        view.agentNodes.set(entry.agent.threadId, node)
      }
      node.dataset.threadId = entry.agent.threadId
      node.dataset.depth = String(entry.depth)
      node.dataset.status = String(entry.agent.status)
      setText(node.children[0] as HTMLElement, entry.agent.nickname ?? entry.agent.threadId)
      setText(
        node.children[1] as HTMLElement,
        `${entry.agent.role ?? 'agent'} · ${String(entry.agent.status)}`,
      )
      const current = view.agentList.childNodes[index] ?? null
      if (current !== node) view.agentList.insertBefore(node, current)
    })
  }

  function updateSessionView(
    view: SessionViewState,
    session: RuntimeSessionProjection,
  ): void {
    const key = sessionKey(session)
    view.section.dataset.sessionKey = key
    view.section.dataset.attempt = String(session.attempt)
    view.section.dataset.readOnly = currentReadOnly ? 'true' : 'false'
    setText(
      view.heading,
      session.deliveryTaskId === null
        ? 'Delivery execution'
        : `Task ${session.deliveryTaskId}`,
    )
    setText(
      view.metadata,
      `Attempt ${String(session.attempt)} · thread ${session.codexThreadId}`
        + ` · as-of sequence ${String(session.asOfSequence)}`
        + (currentReadOnly ? ' · Read-only historical projection' : ''),
    )
    const running = session.activities.find(activity => activity.status === 'running')
    const currentActivity = running === undefined
      ? 'None running'
      : `${String(running.activityType)}: ${running.command ?? running.callId} · ${String(running.status)}`
    // Nullable projection facts (usage, recovery, diff summary) may be absent
    // from an older delivered snapshot, so absence and null read the same.
    const usageTotals = session.usage?.totals ?? []
    const usage = usageTotals.length === 0
      ? '—'
      : usageTotals.map(total => `${total.name} ${String(total.value)}`).join(' · ')
    const recoveryState = session.recovery?.state ?? 'none'
    const recoveryFailures = session.recovery?.failureCount ?? 0
    const recoveryCount = session.recovery?.recoveryCount ?? 0
    const failures = recoveryState === 'none' && recoveryFailures === 0
      ? 'none'
      : `${recoveryState} · ${String(recoveryFailures)} failures · ${String(recoveryCount)} recoveries`
    const diffSummary = session.diffSummary ?? null
    const outcomeValues = [
      String(session.agents.length),
      String(session.agentEdges.length),
      currentActivity,
      diffSummary === null
        ? '—'
        : `${String(diffSummary.changedFileCount)} files · +${String(diffSummary.additions)} / −${String(diffSummary.deletions)} (counts only)`,
      usage,
      failures,
    ]
    view.outcomeValues.forEach((value, index) => {
      setText(value, outcomeValues[index] ?? '—')
    })
    updateSessionAgents(view, session)
    view.timeline.update({
      session,
      evidence: currentEvidence,
      readOnly: currentReadOnly,
    })
    view.fingerprint = JSON.stringify([session, currentEvidence, currentReadOnly])
  }

  const sessionsCollection = mountKeyedCollection<
    RuntimeSessionProjection,
    string,
    HTMLElement
  >({
    parent: sessions,
    key: sessionKey,
    create: session => {
      const view = createSessionView()
      sessionViews.set(sessionKey(session), view)
      updateSessionView(view, session)
      return view.section
    },
    update(section, session) {
      const view = sessionViews.get(sessionKey(session))
      if (view === undefined) return
      const fingerprint = JSON.stringify([session, currentEvidence, currentReadOnly])
      if (fingerprint === view.fingerprint) {
        // Equivalent snapshot for this session: keep DOM identity untouched.
        return
      }
      updateSessionView(view, session)
    },
    remove(section) {
      const key = section.dataset.sessionKey ?? ''
      const view = sessionViews.get(key)
      if (view === undefined) return
      view.timeline.close()
      sessionViews.delete(key)
    },
  })

  let closed = false

  return {
    root,
    update(state) {
      if (closed) return
      setText(heading, state.heading)
      setText(empty, state.emptyText)
      empty.hidden = state.sessions.length !== 0
      const approvalsVisible = state.approvals.length !== 0 && !state.readOnly
      approvals.hidden = !approvalsVisible
      approvalsHeading.hidden = !approvalsVisible
      approvalChips.update(approvalsVisible ? [...state.approvals] : [])
      currentEvidence = state.evidence
      currentReadOnly = state.readOnly
      const bounded = boundedItems(state.sessions, options.limits.runtimeSessions)
      sessionsCollection.update([...bounded.items])
      omitted.hidden = bounded.omitted === 0
      setText(omitted, `${String(bounded.omitted)} more runtime sessions not shown.`)
    },
    close() {
      if (closed) return
      closed = true
      approvalChips.close()
      sessionsCollection.close()
      sessionViews.clear()
      currentEvidence = []
      root.remove?.()
    },
  }
}
