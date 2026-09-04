// SPDX-License-Identifier: Apache-2.0

import {
  controlPlaneLoginFailure,
  type ControlPlaneClient,
  type ControlPlaneLoginFailure,
} from './control-plane-client.js'

/** Which mounted form produced the current submission or failure. */
export type LoginSubmissionSource = 'sign-in' | 'initialization'

/** Server initialization visibility for the first-time entry. */
export type LoginInitialization = 'unknown' | 'uninitialized' | 'initialized'

export interface LoginCredentials {
  readonly username: string
  readonly password: string
}

export interface LoginViewModelState {
  readonly status: 'idle' | 'submitting' | 'succeeded'
  readonly source: LoginSubmissionSource | null
  readonly failure: ControlPlaneLoginFailure | null
  readonly initialization: LoginInitialization
}

export type LoginViewModelListener = (state: LoginViewModelState) => void

export interface LoginViewModel {
  readonly state: LoginViewModelState
  subscribe(listener: LoginViewModelListener): () => void
  login(credentials: LoginCredentials): Promise<void>
  initialize(bootstrapProof: string): Promise<void>
  refreshInitialization(): Promise<void>
  /** Clear one submission outcome so the page is armed for the next session. */
  reset(): void
  dismissFailure(): void
  close(): void
}

function state(
  status: LoginViewModelState['status'],
  source: LoginSubmissionSource | null,
  failure: ControlPlaneLoginFailure | null,
  initialization: LoginInitialization,
): LoginViewModelState {
  return Object.freeze({ status, source, failure, initialization })
}

/**
 * Own the username + password sign-in lifecycle for the login page. Failure
 * reasons stay in the facade-owned presentation taxonomy, and credential
 * material is only held on the call stack of one request.
 */
export function createLoginViewModel(options: {
  readonly client: ControlPlaneClient
}): LoginViewModel {
  const client = options.client
  const listeners = new Set<LoginViewModelListener>()
  let current = state('idle', null, null, 'unknown')
  let pending: AbortController | null = null
  let initializationProbe: AbortController | null = null
  let closed = false

  function publish(next: LoginViewModelState): void {
    current = next
    for (const listener of listeners) listener(current)
  }

  async function submit(
    source: LoginSubmissionSource,
    operation: (signal: AbortSignal) => Promise<unknown>,
  ): Promise<void> {
    if (closed) return
    pending?.abort()
    const controller = new AbortController()
    pending = controller
    publish(state('submitting', source, null, current.initialization))
    try {
      await operation(controller.signal)
      if (pending !== controller || closed) return
      publish(state('succeeded', source, null, current.initialization))
    } catch (error) {
      if (pending !== controller || closed) return
      if (controller.signal.aborted) {
        publish(state('idle', null, null, current.initialization))
        return
      }
      publish(state(
        'idle',
        null,
        controlPlaneLoginFailure(error),
        current.initialization,
      ))
    } finally {
      if (pending === controller) pending = null
    }
  }

  return {
    get state() {
      return current
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
    },
    login(credentials) {
      const attempt = {
        username: credentials.username,
        password: credentials.password,
      }
      return submit('sign-in', signal => {
        const operation = client.loginWithPassword(attempt, { signal })
        // The facade consumed the material synchronously, so the retained
        // reference keeps only the username.
        attempt.password = ''
        return operation
      })
    },
    initialize(bootstrapProof) {
      let submittedProof = bootstrapProof
      return submit('initialization', signal => {
        const operation = client.login(submittedProof, { signal })
        submittedProof = ''
        return operation
      })
    },
    async refreshInitialization() {
      if (closed || initializationProbe !== null) return
      const controller = new AbortController()
      initializationProbe = controller
      try {
        const status = await client.initializationStatus({ signal: controller.signal })
        if (closed || initializationProbe !== controller) return
        publish(state(
          current.status,
          current.source,
          current.failure,
          status.initialized ? 'initialized' : 'uninitialized',
        ))
      } catch {
        // An unavailable probe only hides the first-time entry; it never
        // blocks the username + password form.
        if (closed || initializationProbe !== controller) return
        publish(state(
          current.status,
          current.source,
          current.failure,
          'unknown',
        ))
      } finally {
        if (initializationProbe === controller) initializationProbe = null
      }
    },
    reset() {
      if (closed) return
      pending?.abort()
      pending = null
      publish(state('idle', null, null, current.initialization))
    },
    dismissFailure() {
      if (closed || current.failure === null) return
      publish(state(current.status, current.source, null, current.initialization))
    },
    close() {
      if (closed) return
      closed = true
      pending?.abort()
      pending = null
      initializationProbe?.abort()
      initializationProbe = null
      listeners.clear()
    },
  }
}
