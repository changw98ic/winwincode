// SPDX-License-Identifier: Apache-2.0

export type ClientSurfaceId =
  | 'home'
  | 'chat'
  | 'projects'
  | 'extensions'
  | 'device'
  | 'settings'
  | 'onboarding'

export interface ClientSurface {
  readonly id: ClientSurfaceId
  readonly path: `/${ClientSurfaceId}`
  readonly label: string
  readonly description: string
  readonly default: boolean
  /** Design shell: only chat, board, projects, extensions render nav links. */
  readonly nav: boolean
}

/**
 * UI-504: Home is the canonical first screen.  A start-up that has no route
 * lands on the Attention-first dashboard instead of an arbitrary Chat session
 * or the first Delivery, and every other product area stays one link away.
 */
export const CLIENT_SURFACES: readonly ClientSurface[] = Object.freeze([
  Object.freeze({
    id: 'chat',
    path: '/chat',
    label: '新对话',
    description: '对话工作区',
    default: true,
    nav: true,
  }),
  Object.freeze({
    id: 'home',
    path: '/home',
    label: '任务看板',
    description: '当前仓库的任务看板与待办',
    default: false,
    nav: true,
  }),
  Object.freeze({
    id: 'projects',
    path: '/projects',
    label: '项目',
    description: '项目与仓库列表',
    default: false,
    nav: true,
  }),
  Object.freeze({
    id: 'extensions',
    path: '/extensions',
    label: '扩展',
    description: '插件、技能与指令、MCP 连接',
    default: false,
    nav: true,
  }),
  Object.freeze({
    id: 'device',
    path: '/device',
    label: '执行设备',
    description: '执行设备连接与可访问目录',
    default: false,
    nav: false,
  }),
  Object.freeze({
    id: 'settings',
    path: '/settings',
    label: '设置',
    description: '个人与工作区设置',
    default: false,
    nav: true,
  }),
  Object.freeze({
    id: 'onboarding',
    path: '/onboarding',
    label: '首次设置',
    description: '连接执行设备与首次配置',
    default: false,
    nav: false,
  }),
])
const DEFAULT_SURFACE = (CLIENT_SURFACES.find(surface => surface.default)
  ?? CLIENT_SURFACES[0]) as ClientSurface

export function clientSurfaceFromHash(hash: string): ClientSurface {
  const path = hash.replace(/^#/u, '').replace(/\?.*$/u, '')
  return CLIENT_SURFACES.find(surface => (
    surface.path === path || path.startsWith(`${surface.path}/`)
  )) ?? DEFAULT_SURFACE
}
