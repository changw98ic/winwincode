// SPDX-License-Identifier: Apache-2.0

export type ClientSurfaceId =
  | 'chat'
  | 'strongflow'
  | 'settings'
  | 'approvals'
  | 'enterprise'

export interface ClientSurface {
  readonly id: ClientSurfaceId
  readonly path: `/${ClientSurfaceId}`
  readonly label: string
  readonly description: string
  readonly default: boolean
}

export const CLIENT_SURFACES: readonly ClientSurface[] = Object.freeze([
  Object.freeze({
    id: 'chat',
    path: '/chat',
    label: 'Chat',
    description: 'Conversation workspace',
    default: true,
  }),
  Object.freeze({
    id: 'strongflow',
    path: '/strongflow',
    label: 'StrongFlow',
    description: 'Advanced delivery workspace',
    default: false,
  }),
  Object.freeze({
    id: 'settings',
    path: '/settings',
    label: 'Settings',
    description: 'Personal and workspace settings',
    default: false,
  }),
  Object.freeze({
    id: 'approvals',
    path: '/approvals',
    label: 'Approvals',
    description: 'Human decisions awaiting review',
    default: false,
  }),
  Object.freeze({
    id: 'enterprise',
    path: '/enterprise',
    label: 'Enterprise',
    description: 'Organization administration',
    default: false,
  }),
])

const DEFAULT_SURFACE = CLIENT_SURFACES[0] as ClientSurface

export function clientSurfaceFromHash(hash: string): ClientSurface {
  const path = hash.replace(/^#/u, '').replace(/\?.*$/u, '')
  return CLIENT_SURFACES.find(surface => (
    surface.path === path || path.startsWith(`${surface.path}/`)
  )) ?? DEFAULT_SURFACE
}
