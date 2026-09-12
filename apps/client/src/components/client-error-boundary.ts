// SPDX-License-Identifier: Apache-2.0

import type { ClientFailure } from '../core/connection-state.js'
import {
  assertMounted,
  mountButton,
  mountErrorState,
  removeNode,
  type ButtonView,
  type ErrorStateView,
  type MountedView,
} from '@winwincode/browser-ui'

export interface ClientErrorBoundaryProps {
  readonly failure: ClientFailure | null
  readonly diagnostic: string
  readonly onRetry: () => void
  readonly onSafeEntry: () => void
  readonly onCopy: (diagnostic: string) => Promise<void> | void
}

export interface ClientErrorBoundaryMountOptions {
  readonly document: Document
  readonly props: Readonly<ClientErrorBoundaryProps>
}

export interface ClientErrorBoundaryView extends MountedView<ClientErrorBoundaryProps> {
  readonly root: HTMLElement
  readonly errorState: ErrorStateView
  readonly retry: ButtonView
  readonly safeEntry: ButtonView
  readonly copy: ButtonView
}

export function mountClientErrorBoundary(
  options: ClientErrorBoundaryMountOptions,
): ClientErrorBoundaryView {
  let current = options.props
  let open = true
  const feedback = options.document.createElement('p')
  const retry = mountButton({
    document: options.document,
    props: {
      label: '重试当前页面',
      variant: 'primary',
      className: 'wwc-client-error-retry',
      onActivate: () => { current.onRetry() },
    },
  })
  const safeEntry = mountButton({
    document: options.document,
    props: {
      label: '返回对话',
      className: 'wwc-client-error-safe-entry',
      onActivate: () => { current.onSafeEntry() },
    },
  })
  const copy = mountButton({
    document: options.document,
    props: {
      label: '复制诊断',
      className: 'wwc-client-error-copy',
      onActivate: () => {
        feedback.textContent = '正在复制诊断摘要…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = '诊断摘要已复制。' },
          () => { feedback.textContent = '无法复制诊断摘要。' },
        )
      },
    },
  })
  const errorState = mountErrorState({
    document: options.document,
    props: {
      title: '此区域意外停止',
      message: '请重试当前页面或返回对话。',
      actions: [retry.root, safeEntry.root, copy.root],
      visible: false,
      className: 'wwc-client-error-boundary',
    },
  })
  const root = errorState.root

  feedback.className = 'wwc-client-error-copy-feedback'
  feedback.setAttribute('role', 'status')
  feedback.setAttribute('aria-live', 'polite')
  root.append(feedback)

  function update(props: Readonly<ClientErrorBoundaryProps>): void {
    assertMounted(open, 'ClientErrorBoundary')
    current = props
    const failure = props.failure
    errorState.update({
      title: failure?.title ?? '此区域意外停止',
      message: failure?.message ?? '请重试当前页面或返回对话。',
      ...(failure === null
        ? {}
        : { detail: `错误代码：${failure.code} · 请求 ID：${failure.requestId ?? '不可用'}` }),
      actions: [retry.root, safeEntry.root, copy.root],
      visible: failure !== null,
      className: 'wwc-client-error-boundary',
    })
    retry.update({
      label: failure?.recoveryLabel ?? '重试当前页面',
      variant: 'primary',
      className: 'wwc-client-error-retry',
      onActivate: () => { current.onRetry() },
    })
    safeEntry.update({
      label: '返回对话',
      className: 'wwc-client-error-safe-entry',
      onActivate: () => { current.onSafeEntry() },
    })
    copy.update({
      label: '复制诊断',
      className: 'wwc-client-error-copy',
      onActivate: () => {
        feedback.textContent = '正在复制诊断摘要…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = '诊断摘要已复制。' },
          () => { feedback.textContent = '无法复制诊断摘要。' },
        )
      },
    })
    if (failure === null) feedback.textContent = ''
  }

  update(current)

  return {
    root,
    errorState,
    retry,
    safeEntry,
    copy,
    update,
    close() {
      if (!open) return
      open = false
      copy.close()
      safeEntry.close()
      retry.close()
      errorState.close()
      removeNode(root)
    },
  }
}
