import type { SurfaceDescriptor, WorkspaceComponentDescriptor } from '@winwincode/contracts'

export * from './agent-factory.js'
export * from './model-port.js'
export * from './runtime-events.js'
export * from './runtime-projection.js'
export * from './session-ledger.js'
export * from './strongflow-approval.js'

export const chatSurface: SurfaceDescriptor = Object.freeze({
  id: 'chat',
  label: 'Chat',
  default: true,
})

export const dshProfileComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/dsh-profile',
  kind: 'surface',
})

export const dshExecutionRowsOwnedByCodex = Object.freeze([
  'winwincode-agent-factory',
] as const)
