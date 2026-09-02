// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSession,
} from './control-plane-client.js'

export type AuthSessionStatus =
  | 'signed-out'
  | 'restoring'
  | 'signing-in'
  | 'signed-in'
  | 'signing-out'
  | 'authentication-required'
  | 'error'
  | 'closed'

export interface AuthSessionViewModelState {
  readonly status: AuthSessionStatus
  readonly session: ControlPlaneSession | null
  readonly error: ControlPlaneClientError | null
}

export type AuthSessionViewModelListener = (state: AuthSessionViewModelState) => void

export interface AuthSessionViewModel {
  readonly state: AuthSessionViewModelState
  subscribe(listener: AuthSessionViewModelListener): () => void
  restore(): Promise<void>
  login(bootstrapProof: string): Promise<void>
  logout(): Promise<void>
  authenticationRequired(error: ControlPlaneClientError): void
  cancel(): void
  close(): void
}

function state(
  status: AuthSessionStatus,
  session: ControlPlaneSession | null = null,
  error: ControlPlaneClientError | null = null,
): AuthSessionViewModelState {
  return Object.freeze({ status, session, error })
}

function safeError(error: unknown, signal: AbortSignal): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (signal.aborted) return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The browser session request was cancelled.',
    requestId: null,
    retryable: false,
  })
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'AUTH_SESSION_VIEW_MODEL_FAILURE',
    message: 'The browser session could not be updated.',
    requestId: null,
    retryable: false,
  })
}

/** Keep browser-session status while leaving bootstrap proof material on the call stack only. */
export function createAuthSessionViewModel(client: ControlPlaneClient): AuthSessionViewModel {
  const listeners = new Set<AuthSessionViewModelListener>()
  let current = state('signed-out')
  let pending: AbortController | null = null
  let closed = false

  function publish(next: AuthSessionViewModelState): void {
    current = next
    for (const listener of listeners) listener(current)
  }

  function requireOpen(): void {
    if (!closed) return
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'AUTH_SESSION_CLOSED',
      message: 'The browser session view is closed.',
      requestId: null,
      retryable: false,
    })
  }

  return {
    get state() {
      return current
    },
    subscribe(listener) {
      requireOpen()
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
    },
    async restore() {
      requireOpen()
      pending?.abort()
      const controller = new AbortController()
      pending = controller
      publish(state('restoring'))
      try {
        const session = await client.restore({ signal: controller.signal })
        if (pending !== controller || closed) return
        publish(state('signed-in', session))
      } catch (error) {
        if (pending !== controller || closed) return
        const normalized = safeError(error, controller.signal)
        publish(state(
          normalized.kind === 'authentication' ? 'authentication-required' : 'error',
          null,
          normalized,
        ))
      } finally {
        if (pending === controller) pending = null
      }
    },
    async login(bootstrapProof) {
      requireOpen()
      pending?.abort()
      const controller = new AbortController()
      pending = controller
      publish(state('signing-in'))
      const operation = client.login(bootstrapProof, { signal: controller.signal })
      bootstrapProof = ''
      try {
        const session = await operation
        if (pending !== controller || closed) return
        publish(state('signed-in', session))
      } catch (error) {
        if (pending !== controller || closed) return
        const normalized = safeError(error, controller.signal)
        publish(state(
          normalized.kind === 'authentication' ? 'authentication-required' : 'error',
          null,
          normalized,
        ))
      } finally {
        if (pending === controller) pending = null
      }
    },
    async logout() {
      requireOpen()
      pending?.abort()
      const controller = new AbortController()
      pending = controller
      publish(state('signing-out', current.session))
      try {
        await client.logout({ signal: controller.signal })
        if (pending !== controller || closed) return
        publish(state('signed-out'))
      } catch (error) {
        if (pending !== controller || closed) return
        const normalized = safeError(error, controller.signal)
        publish(state(
          normalized.kind === 'authentication' ? 'authentication-required' : 'error',
          null,
          normalized,
        ))
      } finally {
        if (pending === controller) pending = null
      }
    },
    authenticationRequired(error) {
      if (closed || error.kind !== 'authentication') return
      pending?.abort()
      pending = null
      publish(state('authentication-required', null, error))
    },
    cancel() {
      if (closed || pending === null) return
      pending.abort()
    },
    close() {
      if (closed) return
      closed = true
      pending?.abort()
      pending = null
      current = state('closed')
      listeners.clear()
    },
  }
}
