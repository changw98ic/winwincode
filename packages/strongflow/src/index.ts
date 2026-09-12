import type {
  SurfaceDescriptor,
  WorkspaceComponentDescriptor,
} from '@winwincode/contracts'

export * from './credential-boundary.js'

export const strongFlowSurface: SurfaceDescriptor = Object.freeze({
  id: 'strongflow',
  label: 'StrongFlow',
  default: false,
})

export const strongFlowComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/strongflow',
  kind: 'surface',
})
