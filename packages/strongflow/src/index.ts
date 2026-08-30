import type {
  SurfaceDescriptor,
  WorkspaceComponentDescriptor,
} from '@winwincode/contracts'

export * from './delivery-service.js'
export * from './delivery-store.js'
export * from './delivery-authenticator.js'
export * from './credential-boundary.js'
export * from './delivery-invoker.js'
export * from './delivery-runtime-projection.js'
export * from './runtime-execution-projection.js'
export * from './execution-source.js'
export * from './evaluation-measures.js'
export * from './diagram-execution-projection.js'
export * from './acceptance-verification.js'
export * from './candidate-evidence.js'
export * from './independent-verification.js'
export * from './delivery-verdict.js'
export * from './delivery-attention.js'
export * from './plan-review.js'
export * from './local-git-delivery-workspace.js'
export * from './delivery-stage-coordinator.js'
export * from './github-publication.js'

export const strongFlowSurface: SurfaceDescriptor = Object.freeze({
  id: 'strongflow',
  label: 'StrongFlow',
  default: false,
})

export const strongFlowComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/strongflow',
  kind: 'surface',
})
