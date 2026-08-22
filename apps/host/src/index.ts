import type { SurfaceDescriptor, WorkspaceComponentDescriptor } from '@winwincode/contracts'
import { chatSurface, dshProfileComponent } from '@winwincode/dsh-profile'
import { nativeComponent, resolveReleaseTarget } from '@winwincode/native'
import { strongFlowComponent, strongFlowSurface } from '@winwincode/strongflow'

export interface HostDescriptor {
  readonly target: ReturnType<typeof resolveReleaseTarget>
  readonly defaultSurface: SurfaceDescriptor
  readonly surfaces: readonly SurfaceDescriptor[]
  readonly components: readonly WorkspaceComponentDescriptor[]
}

export function describeHost(
  platform: string = process.platform,
  architecture: string = process.arch,
): HostDescriptor {
  return Object.freeze({
    target: resolveReleaseTarget(platform, architecture),
    defaultSurface: chatSurface,
    surfaces: Object.freeze([chatSurface, strongFlowSurface]),
    components: Object.freeze([dshProfileComponent, strongFlowComponent, nativeComponent]),
  })
}

export * from './strongflow-cli.js'
