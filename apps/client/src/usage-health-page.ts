// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import type {
  CredentialHealthRow,
  ProviderHealthRow,
  ProviderHealthState,
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
    idle: 'Not read yet',
    loading: 'Reading usage and health…',
    ready: 'Usage and health current',
    refreshing: 'Refreshing usage and health…',
    'authentication-required': 'Sign in to read usage and health',
    'authorization-denied': 'This Scope is not authorized for usage and health',
    cancelled: 'Read cancelled',
    error: 'Last read failed',
    closed: 'Summary closed',
  }),
  workerStateLabel: Object.freeze({
    online: 'Online, accepting work',
    'no-capacity': 'Online, no free capacity',
    draining: 'Draining',
    offline: 'Offline',
    'heartbeat-stale': 'Online, heartbeat stale',
    'heartbeat-unknown': 'Online, no heartbeat reported',
  }),
  providerStateLabel: Object.freeze({
    ready: 'Route ready',
    disabled: 'Provider or model disabled',
    unavailable: 'Provider unavailable',
    unknown: 'Provider state unknown',
  }),
  credentialStateLabel: Object.freeze({
    available: 'Credential available',
    missing: 'Credential missing',
    revoked: 'Credential revoked',
  }),
  dimensionHeading: Object.freeze({
    delivery: 'Usage by Delivery',
    'stage-run': 'Usage by StageRun',
    role: 'Usage by Role',
    model: 'Usage by Model',
    provider: 'Provider routing',
  }),
  unknownLabel: 'Unknown',
  unattributedLabel: 'Token usage not attributed',
  durationNote: 'The runtime projection publishes no elapsed time per StageRun or Role.',
  overlapNote: 'A StageRun total is counted for every Role that ran inside it, so Role rows overlap.',
  unattributedNote:
    'The runtime attributes token usage to the StageRun, so Provider and Model rows carry routing facts only.',
  priceSourceNote:
    'Cost is not shown: the published projections carry no price list, so unit prices are not published here.',
  coverageLabel: window => `${window.observedSessions} of ${window.availableSessions} sessions`,
  unavailableLabel: 'This section is unavailable',
  emptyLabel: 'Nothing reported in this Scope yet.',
  refreshLabel: 'Refresh',
  headingLabel: 'Usage, Provider and Worker health',
  windowLabel: 'Observed data window',
  updatedLabel: 'Last updated',
  capacityLabel: Object.freeze({
    sufficient: 'Worker capacity covers the configured concurrency limit',
    short: 'Worker capacity is below the configured concurrency limit',
    unknown: 'No concurrency limit is configured',
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

const AGGREGATE_DIMENSIONS: readonly ('delivery' | 'stage-run' | 'role')[] = Object.freeze([
  'delivery',
  'stage-run',
  'role',
])

function rowClassName(dimension: UsageHealthDimension): string {
  if (dimension === 'delivery') return 'wwc-usage-health-delivery'
  if (dimension === 'stage-run') return 'wwc-usage-health-stage-run'
  if (dimension === 'role') return 'wwc-usage-health-role'
  if (dimension === 'model') return 'wwc-usage-health-model'
  return 'wwc-usage-health-provider'
}

function asOfText(asOf: string | null, known: boolean): string {
  if (!known || asOf === null) return `${PRESENTATION.unknownLabel} observation time`
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
    detail.textContent = `${row.sessionCount} StageRun sessions`
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
    'delivery' | 'stage-run' | 'role',
    KeyedCollectionView<UsageAggregate, string, HTMLLIElement>
  >()
  const aggregateSectionRoots = new Map<'delivery' | 'stage-run' | 'role', HTMLElement>()

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
      state.textContent = `${presentation.workerStateLabel[row.state]} · capacity ${row.capacity}`
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
      routes.textContent = `${row.routeCount} routes${
        row.isDefault ? ' · default' : ''
      } · ${presentation.unattributedLabel}`
      node.replaceChildren(...withMarkers([
        label,
        state,
        routes,
        unknownMarker(false, `${presentation.unknownLabel} observation time`),
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
      } · ${row.contextWindowTokens} context tokens`
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
      state.textContent = `${presentation.credentialStateLabel[row.secretState]} · rotation ${
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
      detail.textContent = row.origin === 'stage-run'
        ? `${row.failureCount} failures${
          row.recovered ? ' · recovery in progress or complete' : ''
        }${row.sourceRef === null ? '' : ` · ${row.sourceRef}`}`
        : `${row.attentionCount} open attention items`
      node.replaceChildren(label, detail)
    },
  })

  function subSection(
    dimension: UsageHealthDimension | 'worker' | 'credential' | 'error',
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
      'provider',
      presentation.dimensionHeading.provider,
      presentation.unattributedNote,
      ['provider'],
      providerRows.root,
      modelRows.root,
    ),
    subSection(
      'worker',
      'Worker capacity and reachability',
      presentation.priceSourceNote,
      ['worker'],
      capacity,
      workerRows.root,
    ),
    subSection(
      'credential',
      'Credential lifecycle',
      presentation.unattributedNote,
      ['credential'],
      credentialRows.root,
    ),
    subSection(
      'error',
      'Recent errors',
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
      : `${presentation.capacityLabel[stateName]} · reported ${summary.reportedCapacity}${
        summary.limit === null ? '' : ` of limit ${summary.limit}`
      } · draining ${summary.drainingCapacity}`
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
    }${state.truncated ? ' · partial coverage' : ''}`
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
    aggregateCollections.get('stage-run')?.update(state.byStageRun)
    aggregateCollections.get('role')?.update(state.byRole)
    providerRows.update(state.byProvider)
    modelRows.update(state.byModel)
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
      workerRows.close()
      credentialRows.close()
      errorRows.close()
      options.root.replaceChildren()
    },
  }
}
