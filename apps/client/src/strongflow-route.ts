// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryId,
  EvidenceId,
  ProductSessionId,
  StageRunId,
} from './generated/contracts.js'

export type StrongFlowEvidenceTabId = 'evidence' | 'tests' | 'logs'

export interface StrongFlowEvidenceRouteState {
  readonly tab: StrongFlowEvidenceTabId
  readonly evidenceId: EvidenceId | null
}

export interface StrongFlowRoute {
  readonly deliveryId: DeliveryId | null
  readonly productSessionId: ProductSessionId | null
  readonly stageRunId: StageRunId | null
  readonly evidenceTab: StrongFlowEvidenceTabId
  readonly evidenceId: EvidenceId | null
}

function boundedParameter(parameters: URLSearchParams, name: string): string | null {
  const value = parameters.get(name)
  return value === null || value.length === 0 || value.length > 4096 ? null : value
}

/** Parse the complete StrongFlow browser route once at the application boundary. */
export function parseStrongFlowRouteHash(hash: string): StrongFlowRoute {
  const query = hash.indexOf('?')
  const parameters = new URLSearchParams(query < 0 ? '' : hash.slice(query + 1))
  const tab = parameters.get('tab')
  return Object.freeze({
    deliveryId: boundedParameter(parameters, 'delivery') as DeliveryId | null,
    productSessionId: boundedParameter(parameters, 'session') as ProductSessionId | null,
    stageRunId: boundedParameter(parameters, 'stageRun') as StageRunId | null,
    evidenceTab: tab === 'tests' || tab === 'logs' ? tab : 'evidence',
    evidenceId: boundedParameter(parameters, 'evidence') as EvidenceId | null,
  })
}

/** Format every StrongFlow route field through the same typed route boundary. */
export function strongFlowRouteHash(route: StrongFlowRoute): string {
  const parameters = new URLSearchParams()
  if (route.deliveryId !== null) parameters.set('delivery', route.deliveryId)
  if (route.productSessionId !== null) parameters.set('session', route.productSessionId)
  if (route.stageRunId !== null) parameters.set('stageRun', route.stageRunId)
  if (route.evidenceTab !== 'evidence' || route.evidenceId !== null) {
    parameters.set('tab', route.evidenceTab)
  }
  if (route.evidenceId !== null) parameters.set('evidence', route.evidenceId)
  const query = parameters.toString()
  return query.length === 0 ? '#/strongflow' : `#/strongflow?${query}`
}
