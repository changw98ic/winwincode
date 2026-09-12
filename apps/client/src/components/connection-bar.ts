// SPDX-License-Identifier: Apache-2.0

import { formatInstant } from '../format-instant.js'

import type {
  ConnectionSnapshot,
  GlobalConnectionStatus,
} from '../core/connection-state.js'
import {
  assertMounted,
  mountButton,
  mountStatusBadge,
  removeNode,
  type ButtonView,
  type MountedView,
  type StatusBadgeView,
  type StatusTone,
} from '@winwincode/browser-ui'

export interface ConnectionBarProps {
  readonly state: ConnectionSnapshot
  readonly diagnostic: string
  readonly onRecover: () => void
  readonly onCopy: (diagnostic: string) => Promise<void> | void
}

export interface ConnectionBarMountOptions {
  readonly document: Document
  readonly props: Readonly<ConnectionBarProps>
}

export interface ConnectionBarView extends MountedView<ConnectionBarProps> {
  readonly root: HTMLElement
  readonly status: StatusBadgeView
  readonly recover: ButtonView
  readonly copy: ButtonView
}

interface ConnectionPresentation {
  readonly label: string
  readonly detail: string
  readonly tone: StatusTone
  readonly live: 'polite' | 'assertive'
  readonly recoveryLabel: string
  readonly recoverVisible: boolean
}

const PRESENTATION: Readonly<Record<GlobalConnectionStatus, ConnectionPresentation>> = Object.freeze({
  connected: Object.freeze({
    label: '客户端已连接',
    detail: '服务器请求与实时更新可用。',
    tone: 'success',
    live: 'polite',
    recoveryLabel: '重新连接',
    recoverVisible: false,
  }),
  reconnecting: Object.freeze({
    label: '重新连接中',
    detail: '当前视图与未保存内容保持不变。',
    tone: 'warning',
    live: 'polite',
    recoveryLabel: '立即重新连接',
    recoverVisible: true,
  }),
  offline: Object.freeze({
    label: '离线',
    detail: '网络恢复前保留当前视图。',
    tone: 'warning',
    live: 'assertive',
    recoveryLabel: '立即重新连接',
    recoverVisible: true,
  }),
  'refresh-required': Object.freeze({
    label: '需要完整刷新',
    detail: '实时更新出现缺口。请从服务器快照重新加载此路由。',
    tone: 'warning',
    live: 'assertive',
    recoveryLabel: '刷新当前路由',
    recoverVisible: true,
  }),
  'authentication-required': Object.freeze({
    label: '会话已过期',
    detail: '请重新登录。此浏览器视图中未保存的内容保持不变。',
    tone: 'danger',
    live: 'assertive',
    recoveryLabel: '重新登录',
    recoverVisible: true,
  }),
  'permission-denied': Object.freeze({
    label: '权限已撤销',
    detail: '当前身份已无此区域的访问权限。',
    tone: 'danger',
    live: 'assertive',
    recoveryLabel: '返回对话',
    recoverVisible: true,
  }),
  'version-mismatch': Object.freeze({
    label: '版本不匹配',
    detail: '客户端与服务器契约不一致。请先更新客户端再重试。',
    tone: 'danger',
    live: 'assertive',
    recoveryLabel: '返回对话',
    recoverVisible: true,
  }),
})

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

export function mountConnectionBar(options: ConnectionBarMountOptions): ConnectionBarView {
  const root = options.document.createElement('aside')
  const detail = options.document.createElement('p')
  const metadata = options.document.createElement('p')
  const actions = options.document.createElement('div')
  const feedback = options.document.createElement('p')
  const status = mountStatusBadge({
    document: options.document,
    props: { label: '重新连接中', tone: 'warning', live: 'polite' },
  })
  let current = options.props
  let open = true

  const recover = mountButton({
    document: options.document,
    props: {
      label: '立即重新连接',
      className: 'wwc-connection-recover',
      onActivate: () => { current.onRecover() },
    },
  })
  const copy = mountButton({
    document: options.document,
    props: {
      label: '复制诊断',
      className: 'wwc-connection-copy',
      onActivate: () => {
        feedback.textContent = '正在复制诊断摘要…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = '诊断摘要已复制。' },
          () => { feedback.textContent = '无法复制诊断摘要。' },
        )
      },
    },
  })

  root.dataset.wwcComponent = 'connection-bar'
  const deviceLink = element(options.document, 'a', 'wwc-connection-device-link')
  deviceLink.href = '#/device'
  deviceLink.setAttribute('aria-label', '执行设备')
  root.className = 'wwc-connection-bar'
  root.setAttribute('aria-label', '服务器连接')
  status.root.className = 'wwc-connection-status'
  detail.className = 'wwc-connection-detail'
  metadata.className = 'wwc-connection-metadata'
  actions.className = 'wwc-connection-actions'
  feedback.className = 'wwc-connection-copy-feedback'
  feedback.setAttribute('role', 'status')
  feedback.setAttribute('aria-live', 'polite')
  actions.append(recover.root, copy.root)
  deviceLink.append(status.root)
  root.append(deviceLink, detail, metadata, actions, feedback)

  function update(props: Readonly<ConnectionBarProps>): void {
    assertMounted(open, 'ConnectionBar')
    current = props
    const presentation = PRESENTATION[props.state.status]
    root.dataset.connectionStatus = props.state.status
    status.update({
      label: presentation.label,
      tone: presentation.tone,
      live: presentation.live,
      className: 'wwc-connection-status',
    })
    detail.textContent = presentation.detail
    metadata.textContent = `最近成功更新：${formatInstant(props.state.lastSuccessfulAt ?? 'not yet available')}`
    recover.update({
      label: presentation.recoveryLabel,
      className: 'wwc-connection-recover',
      onActivate: () => { current.onRecover() },
    })
    recover.root.hidden = !presentation.recoverVisible
    copy.update({
      label: '复制诊断',
      className: 'wwc-connection-copy',
      onActivate: () => {
        feedback.textContent = '正在复制诊断摘要…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = '诊断摘要已复制。' },
          () => { feedback.textContent = '无法复制诊断摘要。' },
        )
      },
    })
  }

  update(current)

  return {
    root,
    status,
    recover,
    copy,
    update,
    close() {
      if (!open) return
      open = false
      copy.close()
      recover.close()
      status.close()
      removeNode(root)
    },
  }
}
