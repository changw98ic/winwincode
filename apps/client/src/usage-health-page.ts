// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import type {
  CredentialHealthRow,
  ProviderHealthRow,
  ProviderHealthState,
  SessionTeamRow,
  SessionTeamState,
  UsageAggregate,
  UsageCapacitySummary,
  UsageHealthDimension,
  UsageHealthErrorRow,
  UsageHealthSource,
  UsageHealthStatus,
  UsageHealthViewModel,
  UsageHealthViewModelState,
  WorkerHealthRow,
  WorkerHealthState,
} from './usage-health-view-model.js'

export interface UsageHealthPresentation {
  readonly statusLabel: Readonly<Record<UsageHealthStatus, string>>
  readonly workerStateLabel: Readonly<Record<WorkerHealthState, string>>
  readonly providerStateLabel: Readonly<Record<ProviderHealthState, string>>
  readonly credentialStateLabel: Readonly<Record<CredentialHealthRow['secretState'], string>>
  readonly dimensionHeading: Readonly<Record<UsageHealthDimension, string>>
  /** Every unknown or unreported value carries this exact word, never a blank cell. */
  readonly unknownLabel: string
  readonly unattributedLabel: string
  readonly durationNote: string
  readonly overlapNote: string
  readonly unattributedNote: string
  readonly priceSourceNote: string
  readonly coverageLabel: (window: {
    readonly observedSessions: number
    readonly availableSessions: number
  }) => string
  readonly unavailableLabel: string
  readonly emptyLabel: string
  readonly refreshLabel: string
  readonly headingLabel: string
  readonly windowLabel: string
  readonly updatedLabel: string
  readonly capacityLabel: Readonly<Record<'sufficient' | 'short' | 'unknown', string>>
}

const PRESENTATION_SPEC: UsageHealthPresentation = {
  statusLabel: Object.freeze({
    idle: '尚未读取',
    loading: '正在读取用量与健康…',
    ready: '用量与健康已更新',
    refreshing: '正在刷新用量与健康…',
    'authentication-required': '登录后查看用量与健康',
    'authorization-denied': '当前范围无权查看用量与健康',
    cancelled: '读取已取消',
    error: '上次读取失败',
    closed: '面板已关闭',
  }),
  workerStateLabel: Object.freeze({
    online: '在线 · 接受任务',
    'no-capacity': '在线 · 无空闲容量',
    draining: '正在排空',
    offline: '离线',
    'heartbeat-stale': '在线 · 心跳滞后',
    'heartbeat-unknown': '在线 · 未上报心跳',
  }),
  providerStateLabel: Object.freeze({
    ready: '路由就绪',
    disabled: '模型服务商或模型已禁用',
    unavailable: '模型服务商不可用',
    unknown: '模型服务商状态未知',
  }),
  credentialStateLabel: Object.freeze({
    available: '凭据可用',
    missing: '凭据缺失',
    revoked: '凭据已吊销',
  }),
  dimensionHeading: Object.freeze({
    delivery: '按交付用量',
    'work-run': '按工作运行用量',
    role: '按角色用量',
    model: '模型用量',
    provider: '模型服务商路由',
  }),
  unknownLabel: '未知',
  unattributedLabel: '令牌用量未归因',
  durationNote: '运行时投影未发布每个工作运行或角色的耗时。',
  overlapNote: '一个工作运行的总量会计入其中运行的每个角色，因此角色行之间存在重叠。',
  unattributedNote: '运行时把令牌用量归因到工作运行，因此模型服务商与模型行仅携带路由事实。',
  priceSourceNote: '不展示费用：已发布的投影不含价目表，因此此处不发布单价。',
  coverageLabel: window => `${String(window.availableSessions)} 个会话中的 ${
    String(window.observedSessions)} 个`,
  unavailableLabel: '此分区不可用',
  emptyLabel: '此范围暂无上报数据。',
  refreshLabel: '刷新',
  headingLabel: '用量、模型服务商与执行进程健康',
  windowLabel: '观测数据窗口',
  updatedLabel: '更新于',
  capacityLabel: Object.freeze({
    sufficient: '执行容量满足配置的并发上限',
    short: '执行容量低于配置的并发上限',
    unknown: '未配置并发上限',
  }),
}

const PRESENTATION: UsageHealthPresentation = Object.freeze(PRESENTATION_SPEC)

export function usageHealthPresentation(): UsageHealthPresentation {
  return PRESENTATION
}

export interface UsageHealthSummaryOptions {
  readonly root: HTMLElement
  readonly model: UsageHealthViewModel
}

export interface UsageHealthSummary {
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

const WORKER_TONES: Readonly<Record<WorkerHealthState, string>> = Object.freeze({
  online: 'success',
  'no-capacity': 'warning',
  draining: 'info',
  offline: 'danger',
  'heartbeat-stale': 'warning',
  'heartbeat-unknown': 'neutral',
})

const PROVIDER_TONES: Readonly<Record<ProviderHealthState, string>> = Object.freeze({
  ready: 'success',
  disabled: 'warning',
  unavailable: 'danger',
  unknown: 'neutral',
})

const SESSION_TEAM_TONES: Readonly<Record<SessionTeamState, string>> = Object.freeze({
  running: 'success',
  recovering: 'warning',
  offline: 'danger',
  idle: 'neutral',
  unknown: 'neutral',
})

const SESSION_TEAM_STATE_LABEL: Readonly<Record<SessionTeamState, string>> = Object.freeze({
  running: '运行中',
  recovering: '恢复中',
  offline: '离线',
  idle: '空闲',
  unknown: '状态未知',
})

const RECOVERY_STATE_LABEL: Readonly<Record<SessionTeamRow['recoveryState'], string>> =
  Object.freeze({
    none: '无需恢复',
    required: '等待恢复',
    'in-progress': '恢复中',
    recovered: '已恢复',
  })

const AGGREGATE_DIMENSIONS: readonly ('delivery' | 'work-run' | 'role')[] = Object.freeze([
  'delivery',
  'work-run',
  'role',
])

function rowClassName(dimension: UsageHealthDimension): string {
  if (dimension === 'delivery') return 'wwc-usage-health-delivery'
  if (dimension === 'work-run') return 'wwc-usage-health-work-run'
  if (dimension === 'role') return 'wwc-usage-health-role'
  if (dimension === 'model') return 'wwc-usage-health-model'
  return 'wwc-usage-health-provider'
}

function asOfText(asOf: string | null, known: boolean): string {
  if (!known || asOf === null) return `${PRESENTATION.unknownLabel} 观测时间`
  return `${PRESENTATION.updatedLabel} ${asOf}`
}

export function mountUsageHealthSummary(
  options: UsageHealthSummaryOptions,
): UsageHealthSummary {
  const document = options.root.ownerDocument
  const presentation = PRESENTATION

  const section = element(document, 'section', 'wwc-usage-health')
  section.setAttribute('aria-labelledby', 'wwc-usage-health-title')
  const heading = element(document, 'h2', 'wwc-usage-health-heading')
  heading.id = 'wwc-usage-health-title'
  heading.textContent = presentation.headingLabel
  // The host page owns the single polite live region; this read-only panel never
  // opens a second announcement channel next to it.
  const updated = element(document, 'p', 'wwc-usage-health-updated')
  const refresh = element(document, 'button', 'wwc-usage-health-refresh')
  refresh.type = 'button'
  refresh.textContent = presentation.refreshLabel
  const windowNode = element(document, 'p', 'wwc-usage-health-window')
  const errorBanner = element(document, 'p', 'wwc-usage-health-error-banner')
  errorBanner.hidden = true
  const capacity = element(document, 'p', 'wwc-usage-health-capacity')
  const header = element(document, 'header', 'wwc-usage-health-header')
  header.append(heading, updated, refresh)
  section.append(header, windowNode, errorBanner, capacity)
  options.root.replaceChildren(section)
  const unavailableNodes: {
    readonly node: HTMLElement
    readonly sources: readonly UsageHealthSource[]
  }[] = []

  /** A missing fact renders an explicit named marker; known facts render no marker at all. */
  function unknownMarker(known: boolean, label: string): HTMLElement | null {
    if (known) return null
    const marker = element(document, 'span', 'wwc-usage-health-unknown')
    marker.dataset.unknown = 'true'
    marker.textContent = label
    return marker
  }

  function withMarkers(
    children: readonly (HTMLElement | null)[],
  ): readonly (HTMLElement | Text)[] {
    return children.filter((child): child is HTMLElement => child !== null)
  }

  function aggregateRow(row: UsageAggregate): HTMLLIElement {
    const node = element(document, 'li', `wwc-usage-health-row ${rowClassName(row.dimension)}`)
    return node
  }

  function fillAggregateRow(node: HTMLLIElement, row: UsageAggregate): void {
    node.dataset.key = row.key
    node.dataset.tokensKnown = row.tokensKnown ? 'true' : 'false'
    node.dataset.unknown = row.tokensKnown ? 'false' : 'true'
    const label = element(document, 'span', 'wwc-usage-health-row-label')
    label.textContent = row.label
    const usage = element(document, 'span', 'wwc-usage-health-row-usage')
    usage.textContent = row.tokensKnown
      ? row.metrics.map(metric => `${metric.name} ${metric.value}`).join(' · ')
      : presentation.unknownLabel
    const detail = element(document, 'span', 'wwc-usage-health-row-detail')
    detail.textContent = `${row.sessionCount} 个 WorkRun 会话`
    const asOf = element(document, 'span', 'wwc-usage-health-row-asof')
    asOf.textContent = asOfText(row.asOf, row.asOfKnown)
    node.replaceChildren(...withMarkers([
      label,
      usage,
      detail,
      asOf,
      unknownMarker(row.tokensKnown, presentation.unknownLabel),
    ]))
  }

  const aggregateCollections = new Map<
    'delivery' | 'work-run' | 'role',
    KeyedCollectionView<UsageAggregate, string, HTMLLIElement>
  >()
  const aggregateSectionRoots = new Map<'delivery' | 'work-run' | 'role', HTMLElement>()

  for (const dimension of AGGREGATE_DIMENSIONS) {
    const headingNode = element(document, 'h3', 'wwc-usage-health-section-heading')
    headingNode.textContent = presentation.dimensionHeading[dimension]
    const note = element(document, 'p', 'wwc-usage-health-note')
    note.textContent = dimension === 'role'
      ? `${presentation.overlapNote} ${presentation.durationNote}`
      : presentation.durationNote
    const list = element(document, 'ul', 'wwc-usage-health-rows')
    aggregateCollections.set(dimension, mountKeyedCollection<
      UsageAggregate,
      string,
      HTMLLIElement
    >({
      parent: list,
      key: row => row.key,
      create: row => aggregateRow(row),
      update: fillAggregateRow,
    }))
    const unavailable = element(document, 'p', 'wwc-usage-health-unavailable')
    unavailable.hidden = true
    unavailable.dataset.sourceState = 'unavailable'
    unavailableNodes.push({ node: unavailable, sources: ['usage'] })
    const sectionNode = element(document, 'section', 'wwc-usage-health-section')
    sectionNode.dataset.dimension = dimension
    sectionNode.append(headingNode, note, unavailable, list)
    aggregateSectionRoots.set(dimension, sectionNode)
  }

  const workerRows = mountKeyedCollection<WorkerHealthRow, string, HTMLLIElement>({
    parent: element(document, 'ul', 'wwc-usage-health-worker-list'),
    key: row => row.key,
    create: () => element(document, 'li', 'wwc-usage-health-worker'),
    update: (node, row) => {
      node.dataset.key = row.key
      node.dataset.workerState = row.state
      node.dataset.tone = WORKER_TONES[row.state]
      const label = element(document, 'span', 'wwc-usage-health-worker-label')
      label.textContent = row.label
      const state = element(document, 'span', 'wwc-usage-health-worker-state')
      state.textContent = `${presentation.workerStateLabel[row.state]} · 容量 ${row.capacity}`
      const heartbeat = element(document, 'span', 'wwc-usage-health-worker-heartbeat')
      heartbeat.textContent = asOfText(row.lastHeartbeatAt, row.heartbeatKnown)
      node.replaceChildren(...withMarkers([
        label,
        state,
        heartbeat,
        unknownMarker(row.heartbeatKnown, presentation.unknownLabel),
      ]))
    },
  })

  const sessionTeamRows = mountKeyedCollection<SessionTeamRow, string, HTMLLIElement>({
    parent: element(document, 'ul', 'wwc-usage-health-session-team-list'),
    key: row => row.key,
    create: () => element(document, 'li', 'wwc-usage-health-session-team'),
    update: (node, row) => {
      const unknown = presentation.unknownLabel
      node.dataset.key = row.key
      node.dataset.sessionState = row.state
      node.dataset.tone = SESSION_TEAM_TONES[row.state]
      const label = element(document, 'span', 'wwc-usage-health-session-team-label')
      label.textContent = row.agentName === null
        ? `AgentIdentity ${unknown}`
        : `${row.agentName} · ${row.role ?? unknown}`
      const identity = element(document, 'span', 'wwc-usage-health-session-team-identity')
      identity.textContent = `AgentIdentity ${row.agentId ?? unknown} · Worker ${
        row.workerId ?? unknown
      }`
      const session = element(document, 'span', 'wwc-usage-health-session-team-session')
      session.textContent = `Session ${row.workerSessionId} · ${row.codexThreadId} · attempt ${
        String(row.attempt)
      }`
      const provider = element(document, 'span', 'wwc-usage-health-session-team-provider')
      provider.textContent = `Provider ${row.provider ?? unknown} · Model ${row.model ?? unknown}`
      const workspace = element(document, 'span', 'wwc-usage-health-session-team-workspace')
      workspace.textContent = `Workspace ${row.repositoryId ?? unknown} · ${
        row.workspaceRevision ?? unknown
      } · ${row.writeMode ?? unknown}`
      const activity = element(document, 'span', 'wwc-usage-health-session-team-activity')
      activity.textContent = `当前活动 ${row.currentActivity ?? '无运行活动'}`
      const state = element(document, 'span', 'wwc-usage-health-session-team-state')
      state.textContent = `${SESSION_TEAM_STATE_LABEL[row.state]} · ${
        RECOVERY_STATE_LABEL[row.recoveryState]
      }`
      const recovery = element(document, 'span', 'wwc-usage-health-session-team-recovery')
      recovery.dataset.requiresHuman = row.recoveryRequiresHuman ? 'true' : 'false'
      recovery.textContent = `${row.recoveryMessage}${
        row.lastFailureSourceRef === null ? '' : ` · 异常来源 ${row.lastFailureSourceRef}`
      }`
      node.replaceChildren(label, identity, session, provider, workspace, activity, state, recovery)
    },
  })

  const providerRows = mountKeyedCollection<ProviderHealthRow, string, HTMLLIElement>({
    parent: element(document, 'ul', 'wwc-usage-health-providers'),
    key: row => row.key,
    create: () => element(document, 'li', 'wwc-usage-health-provider'),
    update: (node, row) => {
      node.dataset.key = row.key
      node.dataset.providerState = row.state
      node.dataset.tone = PROVIDER_TONES[row.state]
      const label = element(document, 'span', 'wwc-usage-health-provider-label')
      label.textContent = row.label
      const state = element(document, 'span', 'wwc-usage-health-provider-state')
      state.textContent = `${presentation.providerStateLabel[row.state]}${
        row.state === 'ready' ? '' : row.reason === null ? '' : ` · ${row.reason}`
      }`
      const routes = element(document, 'span', 'wwc-usage-health-provider-routes')
      routes.textContent = `${String(row.routeCount)} 条路由${
        row.isDefault ? ' · 默认' : ''
      } · ${presentation.unattributedLabel}`
      node.replaceChildren(...withMarkers([
        label,
        state,
        routes,
        unknownMarker(false, `${presentation.unknownLabel} 观测时间`),
      ]))
    },
  })

  const modelRows = mountKeyedCollection<
    UsageHealthViewModelState['byModel'][number],
    string,
    HTMLLIElement
  >({
    parent: element(document, 'ul', 'wwc-usage-health-models'),
    key: row => row.key,
    create: () => element(document, 'li', 'wwc-usage-health-model'),
    update: (node, row) => {
      node.dataset.key = row.key
      const label = element(document, 'span', 'wwc-usage-health-model-label')
      label.textContent = row.label
      const detail = element(document, 'span', 'wwc-usage-health-model-detail')
      detail.textContent = `${row.detail} · ${row.status}${
        row.reason === null ? '' : ` · ${row.reason}`
      } · 上下文 ${row.contextWindowTokens} tokens`
      node.replaceChildren(...withMarkers([
        label,
        detail,
        unknownMarker(false, `${presentation.unattributedLabel} · ${presentation.unknownLabel}`),
      ]))
    },
  })

  const credentialRows = mountKeyedCollection<CredentialHealthRow, string, HTMLLIElement>({
    parent: element(document, 'ul', 'wwc-usage-health-credentials'),
    key: row => row.key,
    create: () => element(document, 'li', 'wwc-usage-health-credential'),
    update: (node, row) => {
      node.dataset.key = row.key
      node.dataset.credentialState = row.secretState
      const label = element(document, 'span', 'wwc-usage-health-credential-label')
      label.textContent = row.label
      const state = element(document, 'span', 'wwc-usage-health-credential-state')
      state.textContent = `${presentation.credentialStateLabel[row.secretState]} · 轮换 ${
        row.rotationVersion
      }`
      const asOf = element(document, 'span', 'wwc-usage-health-credential-asof')
      asOf.textContent = asOfText(row.asOf, row.asOfKnown)
      node.replaceChildren(label, state, asOf)
    },
  })

  const errorRows = mountKeyedCollection<UsageHealthErrorRow, string, HTMLLIElement>({
    parent: element(document, 'ul', 'wwc-usage-health-errors'),
    key: row => row.key,
    create: () => element(document, 'li', 'wwc-usage-health-error'),
    update: (node, row) => {
      node.dataset.key = row.key
      const label = element(document, 'span', 'wwc-usage-health-error-label')
      label.textContent = row.label
      const detail = element(document, 'span', 'wwc-usage-health-error-detail')
      detail.textContent = row.origin === 'work-run'
        ? `${row.failureCount} 次失败${
          row.recovered ? ' · 恢复进行中或已完成' : ''
        }${row.sourceRef === null ? '' : ` · ${row.sourceRef}`}`
        : `${row.attentionCount} 个未关闭注意点`
      node.replaceChildren(label, detail)
    },
  })

  function subSection(
    dimension: UsageHealthDimension | 'session-team' | 'worker' | 'credential' | 'error',
    headingText: string,
    note: string,
    sources: readonly UsageHealthSource[],
    ...children: readonly HTMLElement[]
  ): HTMLElement {
    const headingNode = element(document, 'h3', 'wwc-usage-health-section-heading')
    headingNode.textContent = headingText
    const noteNode = element(document, 'p', 'wwc-usage-health-note')
    noteNode.textContent = note
    const unavailable = element(document, 'p', 'wwc-usage-health-unavailable')
    unavailable.hidden = true
    unavailable.dataset.sourceState = 'unavailable'
    unavailableNodes.push({ node: unavailable, sources })
    const sectionNode = element(document, 'section', 'wwc-usage-health-section')
    sectionNode.dataset.dimension = dimension
    sectionNode.append(headingNode, noteNode, unavailable, ...children)
    return sectionNode
  }

  const sections = element(document, 'div', 'wwc-usage-health-sections')
  sections.append(
    ...AGGREGATE_DIMENSIONS.map(dimension => aggregateSectionRoots.get(dimension)!),
    subSection(
      'session-team',
      '会话与 Agent 团队',
      '展示会话打开时冻结的身份、模型服务商与工作区事实，以及当前运行和恢复状态。',
      ['usage', 'worker'],
      sessionTeamRows.root,
    ),
    subSection(
      'provider',
      presentation.dimensionHeading.provider,
      presentation.unattributedNote,
      ['provider'],
      providerRows.root,
      modelRows.root,
    ),
    subSection(
      'worker',
      '执行容量与可达性',
      presentation.priceSourceNote,
      ['worker'],
      capacity,
      workerRows.root,
    ),
    subSection(
      'credential',
      '凭据生命周期',
      presentation.unattributedNote,
      ['credential'],
      credentialRows.root,
    ),
    subSection(
      'error',
      '近期错误',
      presentation.durationNote,
      ['delivery', 'usage'],
      errorRows.root,
    ),
  )
  section.append(sections)

  let closed = false

  function renderCapacity(state: UsageHealthViewModelState): void {
    const summary = state.capacity
    const sufficient = summary === null ? null : summary.sufficient
    const stateName = sufficient === null ? 'unknown' : sufficient ? 'sufficient' : 'short'
    capacity.dataset.capacityState = stateName
    capacity.textContent = summary === null
      ? presentation.emptyLabel
      : `${presentation.capacityLabel[stateName]} · 上报容量 ${summary.reportedCapacity}${
        summary.limit === null ? '' : ` / 上限 ${summary.limit}`
      } · 排空中 ${summary.drainingCapacity}`
  }

  function render(state: UsageHealthViewModelState): void {
    if (closed) return
    updated.textContent = `${presentation.statusLabel[state.status]} · ${
      presentation.updatedLabel
    } ${state.generatedAt ?? presentation.unknownLabel}`
    windowNode.textContent = `${presentation.windowLabel} ${
      state.timeWindow?.from ?? presentation.unknownLabel
    } … ${state.timeWindow?.to ?? presentation.unknownLabel} · ${
      state.timeWindow === null
        ? presentation.emptyLabel
        : presentation.coverageLabel(state.timeWindow)
    }${state.truncated ? ' · 覆盖不完整' : ''}`
    errorBanner.hidden = state.error === null
    errorBanner.textContent = state.error === null
      ? ''
      : `${presentation.statusLabel[state.status]} · ${state.error.code}`
    renderCapacity(state)
    const unavailable = new Map(state.unavailable.map(entry => [entry.source, entry.code]))
    for (const entry of unavailableNodes) {
      const failing = entry.sources.filter(source => unavailable.has(source))
      entry.node.hidden = failing.length === 0
      entry.node.textContent = failing.length === 0
        ? ''
        : `${presentation.unavailableLabel} · ${
          failing.map(source => unavailable.get(source)).join(' · ')
        }`
    }
    aggregateCollections.get('delivery')?.update(state.byDelivery)
    aggregateCollections.get('work-run')?.update(state.byWorkRun)
    aggregateCollections.get('role')?.update(state.byRole)
    providerRows.update(state.byProvider)
    modelRows.update(state.byModel)
    sessionTeamRows.update(state.sessionTeam)
    workerRows.update(state.workers)
    credentialRows.update(state.credentials)
    errorRows.update(state.errors)
  }

  const unsubscribe = options.model.subscribe(render)
  const onRefresh = () => { void options.model.refresh() }
  refresh.addEventListener('click', onRefresh)

  return {
    close() {
      if (closed) return
      closed = true
      refresh.removeEventListener('click', onRefresh)
      unsubscribe()
      for (const collection of aggregateCollections.values()) collection.close()
      providerRows.close()
      modelRows.close()
      sessionTeamRows.close()
      workerRows.close()
      credentialRows.close()
      errorRows.close()
      options.root.replaceChildren()
    },
  }
}
