import type { SurfaceDescriptor, WorkspaceComponentDescriptor } from '@winwincode/contracts'

/** Host face kept intentionally empty; the advanced entry is registered by ./client. */
export function apply(): void {}

export const strongFlowSurface: SurfaceDescriptor = Object.freeze({
  id: 'strongflow',
  label: 'StrongFlow',
  default: false,
})

export const strongFlowComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/strongflow',
  kind: 'surface',
})
