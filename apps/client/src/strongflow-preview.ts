// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryCriterionResultProjection,
  DeliveryEvidenceProjection,
  EvidenceArtifactDescriptorProjection,
} from './generated/contracts.js'
import {
  EvidenceArtifactContentEncoding,
  EvidenceArtifactPreviewMode,
} from './generated/contracts.js'
import type {
  StrongFlowProjection,
  StrongFlowRealtimeStatus,
  StrongFlowViewStatus,
} from './strongflow-view-model.js'

/**
 * UI-407 keeps the Preview, screenshot, and browser Evidence surface honest.
 *
 * The closed Delivery contract carries no Preview URL, no screenshot field, and
 * no browser-run flag.  What it does carry is the Evidence a browser run leaves
 * behind (`test` and `runtime_event` records), the Criterion results joined to
 * them, and — after one authoritative read — Artifact descriptors with an exact
 * media type and preview mode.  Everything this module reports is derived from
 * those facts and nothing else, so a missing, expired, or disconnected Preview
 * is always named as such and never rendered as a pass.
 */

/**
 * How long a Preview stays reportable after its newest browser Evidence.  The
 * window is measured between the Delivery snapshot's own timestamps, never
 * against the browser clock, so the answer is reproducible for one snapshot.
 */
export const STRONGFLOW_PREVIEW_VALIDITY_MILLIS = 24 * 60 * 60 * 1000

/** Largest Artifact this workbench keeps in memory as one sandboxed image. */
export const MAX_PREVIEW_IMAGE_BYTES = 8 * 1024 * 1024

/** Default bound on the Preview rows rendered for one Delivery snapshot. */
export const DEFAULT_PREVIEW_ROW_LIMIT = 20

/** Image media types a browser decodes as pixels without executing markup. */
const RENDERABLE_IMAGE_MEDIA_TYPES: ReadonlySet<string> = new Set([
  'image/gif',
  'image/jpeg',
  'image/png',
  'image/webp',
])

export type StrongFlowPreviewMediaSupport = 'renderable' | 'scriptable' | 'unsupported'

/** Classify one Artifact media type for the only open channel this panel owns. */
export function strongFlowPreviewImageSupport(mediaType: string): StrongFlowPreviewMediaSupport {
  const bare = mediaType.split(';')[0]?.trim().toLowerCase() ?? ''
  if (bare === 'image/svg+xml') return 'scriptable'
  return RENDERABLE_IMAGE_MEDIA_TYPES.has(bare) ? 'renderable' : 'unsupported'
}

export type StrongFlowPreviewChannel = 'image' | 'text' | 'download-only'

/**
 * Decide from the descriptor alone, before one Artifact byte is read.  Returns
 * the reason the channel is closed, or `null` when the authority's inline
 * grant means the bytes must be read to learn their encoding.
 */
export function strongFlowPreviewDescriptorChannel(
  descriptor: Pick<
    EvidenceArtifactDescriptorProjection,
    'mediaType' | 'previewMode' | 'sizeBytes'
  >,
  maxBytes: number = MAX_PREVIEW_IMAGE_BYTES,
): StrongFlowPreviewChannelReason | null {
  if (descriptor.previewMode !== EvidenceArtifactPreviewMode.InlineText) {
    return 'download-only-preview'
  }
  const support = strongFlowPreviewImageSupport(descriptor.mediaType)
  if (support === 'scriptable') return 'scriptable-image'
  if (support === 'renderable' && descriptor.sizeBytes > maxBytes) return 'oversized'
  return null
}

export type StrongFlowPreviewChannelReason =
  | 'download-only-preview'
  | 'encoding'
  | 'inline-text'
  | 'oversized'
  | 'raster-image'
  | 'scriptable-image'
  | 'unsupported-media-type'

export interface StrongFlowPreviewChannelDecision {
  readonly channel: StrongFlowPreviewChannel
  readonly reason: StrongFlowPreviewChannelReason
}

/**
 * The one safe-open rule for external Artifact content.  The authority's
 * `previewMode` grants the channel; this module then narrows it to content a
 * browser renders without executing: UTF-8 text, or a bounded raster image.
 * Scriptable media (SVG), unknown media, oversized bytes, and download-only
 * Artifacts all fail closed to the download control with the exact reason.
 */
export function strongFlowPreviewChannel(
  descriptor: Pick<EvidenceArtifactDescriptorProjection, 'mediaType' | 'previewMode'>,
  contentEncoding: EvidenceArtifactContentEncoding,
  totalBytes: number,
  maxBytes: number = MAX_PREVIEW_IMAGE_BYTES,
): StrongFlowPreviewChannelDecision {
  if (descriptor.previewMode !== EvidenceArtifactPreviewMode.InlineText) {
    return { channel: 'download-only', reason: 'download-only-preview' }
  }
  const support = strongFlowPreviewImageSupport(descriptor.mediaType)
  if (support !== 'unsupported') {
    if (totalBytes > maxBytes) return { channel: 'download-only', reason: 'oversized' }
    if (support === 'renderable') {
      return contentEncoding === EvidenceArtifactContentEncoding.Binary
        ? { channel: 'image', reason: 'raster-image' }
        : { channel: 'download-only', reason: 'encoding' }
    }
    return { channel: 'download-only', reason: 'scriptable-image' }
  }
  if (contentEncoding === EvidenceArtifactContentEncoding.Utf8) {
    return { channel: 'text', reason: 'inline-text' }
  }
  return { channel: 'download-only', reason: 'unsupported-media-type' }
}

export type StrongFlowEvidenceContentStatus =
  | 'download-only'
  | 'error'
  | 'idle'
  | 'image'
  | 'loading'
  | 'ready'
  | 'unavailable'

export interface StrongFlowPreviewPanelSelection {
  readonly evidenceId: string
  readonly status: StrongFlowEvidenceContentStatus | null
}

/** What the Preview panel says about the screenshot of the opened Evidence. */
export function strongFlowPreviewScreenshotNote(
  selection: StrongFlowPreviewPanelSelection | null,
): string {
  if (selection === null) {
    return 'Open a Preview record to inspect its screenshot Artifact.'
  }
  if (selection.status === 'image') {
    return `The screenshot for Evidence ${selection.evidenceId} is open in the Evidence detail viewer.`
  }
  if (selection.status === 'unavailable' || selection.status === null) {
    return 'No screenshot Artifact is linked to the opened Evidence. '
      + 'The producer retained no authoritative Artifact link, so nothing can be shown.'
  }
  if (selection.status === 'download-only') {
    return 'The opened Evidence has no safely previewable screenshot, '
      + 'so its Artifact stays behind the download control.'
  }
  if (selection.status === 'error') {
    return 'The screenshot Artifact could not be read. Retry the Evidence detail.'
  }
  return 'Loading the screenshot Artifact for the opened Evidence…'
}

export type StrongFlowPreviewEvidenceKind = 'runtime-log' | 'test-run'

export interface StrongFlowPreviewEvidenceItem {
  readonly row: DeliveryEvidenceProjection
  readonly kind: StrongFlowPreviewEvidenceKind
  readonly criterionIds: readonly string[]
  readonly failingCriterionIds: readonly string[]
}

export type StrongFlowPreviewHealthId =
  | 'degraded'
  | 'expired'
  | 'healthy'
  | 'no-candidate'
  | 'not-generated'
  | 'unverified'
  | 'unreachable'

export interface StrongFlowPreviewHealth {
  readonly id: StrongFlowPreviewHealthId
  readonly label: string
  readonly detail: string
  readonly tone: 'business-fail' | 'infra' | 'neutral' | 'pass'
  /** Only a Criterion-verified, in-window, connected Preview is a pass. */
  readonly pass: boolean
}

const PREVIEW_HEALTH: Readonly<
  Record<StrongFlowPreviewHealthId, StrongFlowPreviewHealth>
> = Object.freeze({
  degraded: Object.freeze({
    id: 'degraded',
    label: 'Preview degraded',
    detail: 'At least one Criterion joined to this Preview Evidence did not pass.',
    tone: 'business-fail',
    pass: false,
  }),
  expired: Object.freeze({
    id: 'expired',
    label: 'Preview expired',
    detail: 'The newest Preview Evidence is outside its validity window, so it is not a pass.',
    tone: 'infra',
    pass: false,
  }),
  healthy: Object.freeze({
    id: 'healthy',
    label: 'Preview healthy',
    detail: 'Every Criterion joined to the current Candidate Preview Evidence passed.',
    tone: 'pass',
    pass: true,
  }),
  'no-candidate': Object.freeze({
    id: 'no-candidate',
    label: 'No Candidate to preview',
    detail: 'This Delivery has no frozen Candidate, so no Preview was generated.',
    tone: 'neutral',
    pass: false,
  }),
  'not-generated': Object.freeze({
    id: 'not-generated',
    label: 'Preview not generated',
    detail: 'The current Candidate kept no test or runtime Evidence, so there is no Preview.',
    tone: 'neutral',
    pass: false,
  }),
  unverified: Object.freeze({
    id: 'unverified',
    label: 'Preview not verified',
    detail: 'Preview Evidence exists but no Criterion references it, so it carries no pass.',
    tone: 'neutral',
    pass: false,
  }),
  unreachable: Object.freeze({
    id: 'unreachable',
    label: 'Preview source unavailable',
    detail: 'StrongFlow is not connected to the Delivery snapshot, so no Preview state is known.',
    tone: 'infra',
    pass: false,
  }),
})

/** StrongFlow states in which the panel may not report a Preview verdict. */
const UNREACHABLE_VIEW_STATUSES: ReadonlySet<StrongFlowViewStatus> = new Set([
  'authentication-required',
  'authorization-denied',
  'cancelled',
  'closed',
  'error',
  'idle',
  'loading',
  'refreshing',
])

const UNREACHABLE_REALTIME_STATUSES: ReadonlySet<StrongFlowRealtimeStatus> = new Set([
  'access-revoked',
  'closed',
  'reconnecting',
  'reloading',
])

export interface StrongFlowPreviewConnection {
  readonly viewStatus: StrongFlowViewStatus
  readonly realtime: StrongFlowRealtimeStatus
}

export interface StrongFlowPreviewSnapshotOptions {
  readonly connection?: StrongFlowPreviewConnection
  readonly validityMillis?: number
  readonly limit?: number
}

export interface StrongFlowPreviewSnapshot {
  readonly health: StrongFlowPreviewHealth
  /** Rows of a superseded Candidate never enter `items`; they are counted only. */
  readonly candidateState: 'current' | 'none'
  readonly items: readonly StrongFlowPreviewEvidenceItem[]
  readonly omitted: number
  readonly supersededCount: number
  readonly newestEvidenceAt: string | null
  readonly validityMillis: number
}

/**
 * The Preview tab keeps exactly the Evidence kinds a browser run leaves behind:
 * recorded test runs and runtime events (the console and network stream).
 */
export function strongFlowPreviewRowsForTab(
  rows: readonly DeliveryEvidenceProjection[],
): readonly DeliveryEvidenceProjection[] {
  return rows.filter(row => row.type === 'test' || row.type === 'runtime_event')
}

function criterionJoins(
  criteria: readonly DeliveryCriterionResultProjection[],
  row: DeliveryEvidenceProjection,
): { readonly criterionIds: readonly string[]; readonly failingCriterionIds: readonly string[] } {
  const joined = criteria.filter(criterion => criterion.evidenceRefs.includes(row.id))
  return {
    criterionIds: Object.freeze(joined.map(criterion => criterion.criterionId)),
    failingCriterionIds: Object.freeze(
      joined
        .filter(criterion => criterion.verdict !== 'pass')
        .map(criterion => criterion.criterionId),
    ),
  }
}

function instant(value: string): number | null {
  const parsed = Date.parse(value)
  return Number.isFinite(parsed) ? parsed : null
}

/**
 * Derive the complete Preview panel read model from one Delivery snapshot.
 * The result is a pure function of that snapshot and the given connection, so
 * an expired, unverified, or disconnected Preview can never look like a pass.
 */
export function strongFlowPreviewSnapshot(
  projection: StrongFlowProjection | null,
  options: StrongFlowPreviewSnapshotOptions = {},
): StrongFlowPreviewSnapshot {
  const validityMillis = options.validityMillis ?? STRONGFLOW_PREVIEW_VALIDITY_MILLIS
  const limit = options.limit ?? DEFAULT_PREVIEW_ROW_LIMIT
  const connection = options.connection ?? { viewStatus: 'ready', realtime: 'subscribed' }
  const rows = strongFlowPreviewRowsForTab(projection?.evidence ?? [])
  const candidateRef = projection?.currentCandidate?.candidateRef ?? null
  const current = candidateRef === null
    ? []
    : rows.filter(row => row.candidateRef === candidateRef)
  const supersededCount = candidateRef === null ? rows.length : rows.length - current.length
  const newestFirst = Object.freeze([...current].sort((left, right) => {
    const leftAt = instant(left.createdAt)
    const rightAt = instant(right.createdAt)
    if (leftAt !== rightAt) {
      return (rightAt ?? Number.MAX_SAFE_INTEGER) - (leftAt ?? Number.MAX_SAFE_INTEGER)
    }
    // Equal instants keep one deterministic order: the later Evidence id first.
    return right.id > left.id ? 1 : right.id < left.id ? -1 : 0
  }))
  const criteria = projection?.verdict?.criteria ?? []
  const items = Object.freeze(newestFirst.map(row => Object.freeze({
    row,
    kind: row.type === 'test' ? 'test-run' : 'runtime-log',
    ...criterionJoins(criteria, row),
  })))
  const bounded = items.slice(0, Math.max(1, limit))
  const newestEvidenceAt = newestFirst[0]?.createdAt ?? null

  let health = PREVIEW_HEALTH.healthy
  if (projection === null
    || UNREACHABLE_VIEW_STATUSES.has(connection.viewStatus)
    || UNREACHABLE_REALTIME_STATUSES.has(connection.realtime)) {
    health = PREVIEW_HEALTH.unreachable
  } else if (candidateRef === null) {
    health = PREVIEW_HEALTH['no-candidate']
  } else if (current.length === 0) {
    health = PREVIEW_HEALTH['not-generated']
  } else {
    const updatedAt = instant(projection.metadata.updatedAt)
    const newestAt = instant(newestEvidenceAt ?? '')
    if (updatedAt === null || newestAt === null || newestAt + validityMillis < updatedAt) {
      health = PREVIEW_HEALTH.expired
    } else {
      const failing = items.some(item => item.failingCriterionIds.length > 0)
      const joined = items.some(item => item.criterionIds.length > 0)
      health = failing
        ? PREVIEW_HEALTH.degraded
        : joined
          ? PREVIEW_HEALTH.healthy
          : PREVIEW_HEALTH.unverified
    }
  }

  return Object.freeze({
    health,
    candidateState: candidateRef === null ? 'none' : 'current',
    items: Object.freeze(bounded),
    omitted: Math.max(0, items.length - bounded.length),
    supersededCount,
    newestEvidenceAt,
    validityMillis,
  })
}
