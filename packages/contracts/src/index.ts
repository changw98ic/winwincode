export const SUPPORTED_RELEASE_TARGETS = [
  'aarch64-apple-darwin',
  'x86_64-apple-darwin',
  'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu',
] as const

export type SupportedReleaseTarget = typeof SUPPORTED_RELEASE_TARGETS[number]

export type SurfaceMode = 'chat' | 'strongflow'

export interface SurfaceDescriptor {
  readonly id: SurfaceMode
  readonly label: string
  readonly default: boolean
}

export interface WorkspaceComponentDescriptor {
  readonly name: string
  readonly kind: 'host' | 'surface' | 'native-interface' | 'contract'
}

export * from './delivery.js'
export * from './delivery-candidate.js'
export * from './runtime-events.js'
export * from './strongflow-delivery-api.js'
export * from './strongflow-delivery-advance.js'
export * from './strongflow-plan-review.js'
export * from './strongflow-diagram-execution.js'
export * from './strongflow-runtime-execution.js'
export * from './strongflow-role.js'
export * from './strongflow-github-publication.js'
export * from './strongflow-github-review-package.js'
