// SPDX-License-Identifier: Apache-2.0

import type { ClientFailure } from '../core/connection-state.js'
import { mountButton, type ButtonView } from './button.js'
import { mountErrorState, type ErrorStateView } from './error-state.js'
import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

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
      label: 'Retry route',
      variant: 'primary',
      className: 'wwc-client-error-retry',
      onActivate: () => { current.onRetry() },
    },
  })
  const safeEntry = mountButton({
    document: options.document,
    props: {
      label: 'Return to Chat',
      className: 'wwc-client-error-safe-entry',
      onActivate: () => { current.onSafeEntry() },
    },
  })
  const copy = mountButton({
    document: options.document,
    props: {
      label: 'Copy diagnostic',
      className: 'wwc-client-error-copy',
      onActivate: () => {
        feedback.textContent = 'Copying diagnostic summary…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = 'Diagnostic summary copied.' },
          () => { feedback.textContent = 'Diagnostic copy is unavailable.' },
        )
      },
    },
  })
  const errorState = mountErrorState({
    document: options.document,
    props: {
      title: 'This area stopped unexpectedly',
      message: 'Retry this route or return to Chat.',
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
      title: failure?.title ?? 'This area stopped unexpectedly',
      message: failure?.message ?? 'Retry this route or return to Chat.',
      ...(failure === null
        ? {}
        : { detail: `Error code: ${failure.code} · Request ID: ${failure.requestId ?? 'not available'}` }),
      actions: [retry.root, safeEntry.root, copy.root],
      visible: failure !== null,
      className: 'wwc-client-error-boundary',
    })
    retry.update({
      label: failure?.recoveryLabel ?? 'Retry route',
      variant: 'primary',
      className: 'wwc-client-error-retry',
      onActivate: () => { current.onRetry() },
    })
    safeEntry.update({
      label: 'Return to Chat',
      className: 'wwc-client-error-safe-entry',
      onActivate: () => { current.onSafeEntry() },
    })
    copy.update({
      label: 'Copy diagnostic',
      className: 'wwc-client-error-copy',
      onActivate: () => {
        feedback.textContent = 'Copying diagnostic summary…'
        void Promise.resolve(current.onCopy(current.diagnostic)).then(
          () => { feedback.textContent = 'Diagnostic summary copied.' },
          () => { feedback.textContent = 'Diagnostic copy is unavailable.' },
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
