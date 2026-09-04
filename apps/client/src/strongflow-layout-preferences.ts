// SPDX-License-Identifier: Apache-2.0

import type { StrongFlowDeliveryListView } from './strongflow-delivery-list-page.js'

/** Pane sizes are a browser preference: they never enter the Delivery or Control Plane state. */
export interface StrongFlowLayoutPreferences {
  /** Navigation pane width as a percentage of the workspace, clamped to [18, 45]. */
  readonly navigationWidth: number
  /** Attention/Evidence context pane width as a percentage, clamped to [18, 45]. */
  readonly contextWidth: number
  readonly navigationCollapsed: boolean
  readonly contextCollapsed: boolean
  readonly artifactsTab: StrongFlowArtifactsTab
  readonly deliveriesView: StrongFlowDeliveryListView
}

export type StrongFlowArtifactsTab = 'solution' | 'execution' | 'candidate' | 'evidence'

export const STRONGFLOW_LAYOUT_STORAGE_KEY = 'winwincode.strongflow.layout.v1'

export const STRONGFLOW_ARTIFACTS_TABS: readonly StrongFlowArtifactsTab[] = Object.freeze([
  'solution',
  'execution',
  'candidate',
  'evidence',
])

const DELIVERIES_VIEWS: readonly StrongFlowDeliveryListView[] = Object.freeze(['list', 'kanban'])

export const DEFAULT_STRONGFLOW_LAYOUT: StrongFlowLayoutPreferences = Object.freeze({
  navigationWidth: 22,
  contextWidth: 30,
  navigationCollapsed: false,
  contextCollapsed: false,
  artifactsTab: 'solution',
  deliveriesView: 'list',
})

const MIN_PANE_WIDTH = 18
const MAX_PANE_WIDTH = 45

function clampPaneWidth(value: unknown): number | null {
  const width = typeof value === 'number' ? value : Number(value)
  if (!Number.isFinite(width)) return null
  return Math.min(MAX_PANE_WIDTH, Math.max(MIN_PANE_WIDTH, Math.round(width)))
}

/** Normalize a stored or partial value into one canonical, clamped layout. */
export function normalizeStrongFlowLayoutPreferences(
  value: unknown,
): StrongFlowLayoutPreferences {
  if (value === null || typeof value !== 'object') return DEFAULT_STRONGFLOW_LAYOUT
  const record = value as Readonly<Record<string, unknown>>
  const navigationWidth = clampPaneWidth(record.navigationWidth)
  const contextWidth = clampPaneWidth(record.contextWidth)
  const artifactsTab = STRONGFLOW_ARTIFACTS_TABS.find(tab => tab === record.artifactsTab)
  const deliveriesView = DELIVERIES_VIEWS.find(view => view === record.deliveriesView)
  return Object.freeze({
    navigationWidth: navigationWidth ?? DEFAULT_STRONGFLOW_LAYOUT.navigationWidth,
    contextWidth: contextWidth ?? DEFAULT_STRONGFLOW_LAYOUT.contextWidth,
    navigationCollapsed: record.navigationCollapsed === true
      || record.navigationCollapsed === 'yes',
    contextCollapsed: record.contextCollapsed === true
      || record.contextCollapsed === 'yes',
    artifactsTab: artifactsTab ?? DEFAULT_STRONGFLOW_LAYOUT.artifactsTab,
    deliveriesView: deliveriesView ?? DEFAULT_STRONGFLOW_LAYOUT.deliveriesView,
  })
}

/** Read the persisted browser preference; storage failures resolve to the default layout. */
export function strongFlowLayoutPreferencesFromStorage(
  storage: Pick<Storage, 'getItem'> | null,
): StrongFlowLayoutPreferences {
  if (storage === null) return DEFAULT_STRONGFLOW_LAYOUT
  let raw: string | null
  try {
    raw = storage.getItem(STRONGFLOW_LAYOUT_STORAGE_KEY)
  } catch {
    return DEFAULT_STRONGFLOW_LAYOUT
  }
  if (raw === null) return DEFAULT_STRONGFLOW_LAYOUT
  try {
    return normalizeStrongFlowLayoutPreferences(JSON.parse(raw))
  } catch {
    return DEFAULT_STRONGFLOW_LAYOUT
  }
}

/** Persist only a non-default layout; writing failures are ignored because layout is cosmetic. */
export function strongFlowLayoutPreferencesToStorage(
  storage: Pick<Storage, 'setItem' | 'removeItem'> | null,
  value: StrongFlowLayoutPreferences,
): void {
  if (storage === null) return
  const normalized = normalizeStrongFlowLayoutPreferences(value)
  try {
    if (sameLayout(normalized, DEFAULT_STRONGFLOW_LAYOUT)) {
      storage.removeItem(STRONGFLOW_LAYOUT_STORAGE_KEY)
      return
    }
    storage.setItem(STRONGFLOW_LAYOUT_STORAGE_KEY, JSON.stringify(normalized))
  } catch {
    // A full or blocked localStorage must never break the workspace render.
  }
}

function sameLayout(
  left: StrongFlowLayoutPreferences,
  right: StrongFlowLayoutPreferences,
): boolean {
  return left.navigationWidth === right.navigationWidth
    && left.contextWidth === right.contextWidth
    && left.navigationCollapsed === right.navigationCollapsed
    && left.contextCollapsed === right.contextCollapsed
    && left.artifactsTab === right.artifactsTab
    && left.deliveriesView === right.deliveriesView
}
