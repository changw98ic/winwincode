// SPDX-License-Identifier: Apache-2.0

import type { AuthSessionViewModelState } from './auth-view-model.js'
import { CLIENT_SURFACES, type ClientSurface } from './client-surface.js'

/**
 * Read-only navigation capability projection.
 *
 * One of four user-visible states, derived from the current AuthSession,
 * its authorized Scopes, and optional read-only query facts:
 * - `hidden`: the deployment or session shape can never use the entry.
 * - `disabled`: the entry exists for this deployment but the current
 *   identity is known to lack permission.
 * - `read-only`: the identity may enter but mutation remains unavailable.
 * - `available`: the identity may enter; page and Server authorization
 *   still decide what is readable or writable inside.
 *
 * This module owns no business state. It never mutates the session and
 * never replaces Server-side authorization; hiding an entry is an
 * experience decision, not an access decision.
 */
export type NavigationCapability = 'hidden' | 'disabled' | 'read-only' | 'available'

export type NavigationDeployment = 'unknown' | 'personal'

export type NavigationSurfaceAccess = 'denied' | 'read-only' | 'write'

/**
 * Optional read-only facts from deployment configuration or Control Plane query projections.
 * Missing facts never grant access; Scope remains the minimum route requirement.
 */
export interface NavigationCapabilityFacts {
  readonly deployment?: Exclude<NavigationDeployment, 'unknown'>
  readonly surfaceAccess?: Readonly<Partial<Record<ClientSurface['id'], NavigationSurfaceAccess>>>
}

export type NavigationCapabilityReason =
  | 'no-session'
  | 'authorized-scope'
  | 'no-repository-scope'
  | 'capability-denied'
  | 'read-only-capability'
  | 'writable-capability'

export interface SurfaceCapability {
  readonly surface: ClientSurface
  readonly capability: NavigationCapability
  readonly reason: NavigationCapabilityReason
}

export interface NavigationCapabilityProjection {
  readonly deployment: NavigationDeployment
  readonly surfaces: readonly SurfaceCapability[]
}

/**
 * Project the current browser session onto the canonical surface list.
 * Scope is always the minimum route requirement. Optional facts refine a
 * scoped entry into denied, read-only, or writable presentation without
 * becoming a second authorization boundary.
 */
export function projectionForSession(
  state: Pick<AuthSessionViewModelState, 'status' | 'session'>,
  facts: Readonly<NavigationCapabilityFacts> = {},
): NavigationCapabilityProjection {
  const session = state.status === 'signed-in' ? state.session : null
  if (session === null) {
    return Object.freeze({
      deployment: 'unknown',
      surfaces: Object.freeze(CLIENT_SURFACES.map(surface => Object.freeze({
        surface,
        capability: 'hidden',
        reason: 'no-session',
      }))),
    })
  }
  const scopes = session.authorizedScopes
  const hasRepositoryScope = scopes.some(scope => scope.kind === 'repository')
  const deployment: NavigationDeployment = facts.deployment
    ?? (hasRepositoryScope ? 'personal' : 'unknown')
  return Object.freeze({
    deployment,
    surfaces: Object.freeze(CLIENT_SURFACES.map(surface => Object.freeze({
      surface,
      capability: capabilityForSurface(
        surface,
        deployment,
        hasRepositoryScope,
        facts.surfaceAccess?.[surface.id],
      ),
      reason: reasonForSurface(
        surface,
        deployment,
        hasRepositoryScope,
        facts.surfaceAccess?.[surface.id],
      ),
    }))),
  })
}

function capabilityForSurface(
  surface: ClientSurface,
  deployment: NavigationDeployment,
  hasRepositoryScope: boolean,
  access: NavigationSurfaceAccess | undefined,
): NavigationCapability {
  if (access === 'denied') return 'disabled'
  if (!hasRepositoryScope) return deployment === 'unknown' ? 'hidden' : 'disabled'
  return access === 'read-only' ? 'read-only' : 'available'
}

function reasonForSurface(
  surface: ClientSurface,
  deployment: NavigationDeployment,
  hasRepositoryScope: boolean,
  access: NavigationSurfaceAccess | undefined,
): NavigationCapabilityReason {
  if (access === 'denied') return 'capability-denied'
  if (!hasRepositoryScope) return 'no-repository-scope'
  if (access === 'read-only') return 'read-only-capability'
  if (access === 'write') return 'writable-capability'
  return 'authorized-scope'
}

/** Resolve the surface a hash URL will enter and its capability together. */
export function surfaceCapabilityForHash(
  hash: string,
  state: Pick<AuthSessionViewModelState, 'status' | 'session'>,
  facts: Readonly<NavigationCapabilityFacts> = {},
): SurfaceCapability {
  const path = hash.replace(/^#/u, '').replace(/\?.*$/u, '')
  const fallback = CLIENT_SURFACES[0] as ClientSurface
  const surface = CLIENT_SURFACES.find(candidate => (
    candidate.path === path || path.startsWith(`${candidate.path}/`)
  )) ?? fallback
  return projectionForSession(state, facts).surfaces.find(candidate => (
    candidate.surface.id === surface.id
  )) as SurfaceCapability
}
