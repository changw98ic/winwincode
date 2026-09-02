// SPDX-License-Identifier: Apache-2.0

import type {
  ConnectionSnapshot,
  GlobalConnectionStatus,
} from '../core/connection-state.js'
import { mountButton, type ButtonView } from './button.js'
import { assertMounted, removeNode, type MountedView } from './mounted-view.js'
import { mountStatusBadge, type StatusBadgeView, type StatusTone } from './status-badge.js'

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
    label: 'Connected',
    detail: 'Server requests and live updates are available.',
    tone: 'success',
    live: 'polite',
    recoveryLabel: 'Reconnect',
    recoverVisible: false,
  }),
  reconnecting: Object.freeze({
    label: 'Reconnecting',
    detail: 'The current view and unsaved fields remain in place.',
    tone: 'warning',
    live: 'polite',
    recoveryLabel: 'Reconnect now',
    recoverVisible: true,
  }),
  offline: Object.freeze({
    label: 'Offline',
    detail: 'The current view is preserved until the network returns.',
    tone: 'warning',
    live: 'assertive',
    recoveryLabel: 'Try reconnecting',
    recoverVisible: true,
  }),
  'refresh-required': Object.freeze({
    label: 'Full refresh required',
    detail: 'Live updates have a gap. Reload this route from the Server snapshot.',
    tone: 'warning',
    live: 'assertive',
    recoveryLabel: 'Refresh route',
    recoverVisible: true,
  }),
  'authentication-required': Object.freeze({
    label: 'Session expired',
    detail: 'Sign in again. Unsaved fields remain in this browser view.',
    tone: 'danger',
    live: 'assertive',
    recoveryLabel: 'Sign in again',
    recoverVisible: true,
  }),
  'permission-denied': Object.freeze({
    label: 'Permission revoked',
    detail: 'The current identity no longer has access to this area.',
    tone: 'danger',
    live: 'assertive',
    recoveryLabel: 'Return to Chat',
    recoverVisible: true,
  }),
  'version-mismatch': Object.freeze({
    label: 'Version mismatch',
    detail: 'The Client and Server contracts differ. Update the Client before retrying.',
    tone: 'danger',
    live: 'assertive',
    recoveryLabel: 'Return to Chat',
    recoverVisible: true,
  }),
})

export function mountConnectionBar(options: ConnectionBarMountOptions): ConnectionBarView {
  const root = options.document.createElement('aside')
  const detail = options.document.createElement('p')
  const metadata = options.document.createElement('p')
  const actions = options.document.createElement('div')
  const feedback = options.document.createElement('p')
  const status = mountStatusBadge({
    document: options.document,
    props: { label: 'Reconnecting', tone: 'warning', live: 'polite' },
  })
  let current = options.props
  let open = true

  const recover = mountButton({
    document: options.document,
    props: {
      label: 'Reconnect now',
      className: 'wwc-connection-recover',
      onActivate: () => { current.onRecover() },
    },
  })
  const copy = mountButton({
    document: options.document,
    props: {
      label: 'Copy diagnostic',
      className: 'wwc-connection-copy',
      onActivate: () => {
        feedback.textContent = 'Copying diagnostic summary…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = 'Diagnostic summary copied.' },
          () => { feedback.textContent = 'Diagnostic copy is unavailable.' },
        )
      },
    },
  })

  root.dataset.wwcComponent = 'connection-bar'
  root.className = 'wwc-connection-bar'
  root.setAttribute('aria-label', 'Server connection')
  status.root.className = 'wwc-connection-status'
  detail.className = 'wwc-connection-detail'
  metadata.className = 'wwc-connection-metadata'
  actions.className = 'wwc-connection-actions'
  feedback.className = 'wwc-connection-copy-feedback'
  feedback.setAttribute('role', 'status')
  feedback.setAttribute('aria-live', 'polite')
  actions.append(recover.root, copy.root)
  root.append(status.root, detail, metadata, actions, feedback)

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
    metadata.textContent = `Last successful update: ${props.state.lastSuccessfulAt ?? 'not yet available'}`
    recover.update({
      label: presentation.recoveryLabel,
      className: 'wwc-connection-recover',
      onActivate: () => { current.onRecover() },
    })
    recover.root.hidden = !presentation.recoverVisible
    copy.update({
      label: 'Copy diagnostic',
      className: 'wwc-connection-copy',
      onActivate: () => {
        feedback.textContent = 'Copying diagnostic summary…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = 'Diagnostic summary copied.' },
          () => { feedback.textContent = 'Diagnostic copy is unavailable.' },
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
