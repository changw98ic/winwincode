// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryId,
  EvidenceId,
  ProductSessionId,
  StageRunId,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'

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

function canonicalParameter<Identity extends string>(
  parameters: URLSearchParams,
  name: string,
  schema: 'DeliveryId' | 'EvidenceId' | 'ProductSessionId' | 'StageRunId',
): Identity | null {
  const values = parameters.getAll(name)
  if (values.length !== 1) return null
  const value = values[0]
  return value !== undefined && matchesCanonicalSchema(schema, value)
    ? value as Identity
    : null
}

/** Parse the complete StrongFlow browser route once at the application boundary. */
export function parseStrongFlowRouteHash(hash: string): StrongFlowRoute {
  const query = hash.indexOf('?')
  const parameters = new URLSearchParams(query < 0 ? '' : hash.slice(query + 1))
  const tab = parameters.get('tab')
  return Object.freeze({
    deliveryId: canonicalParameter<DeliveryId>(parameters, 'delivery', 'DeliveryId'),
    productSessionId: canonicalParameter<ProductSessionId>(
      parameters,
      'session',
      'ProductSessionId',
    ),
    stageRunId: canonicalParameter<StageRunId>(parameters, 'stageRun', 'StageRunId'),
    evidenceTab: tab === 'tests' || tab === 'logs' ? tab : 'evidence',
    evidenceId: canonicalParameter<EvidenceId>(parameters, 'evidence', 'EvidenceId'),
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
