// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  controlPlaneUserCreateFailure,
  controlPlaneUserPasswordFailure,
  controlPlaneUserStateFailure,
  type ControlPlaneUserAccount,
  type ControlPlaneUserAccountState,
  type ControlPlaneUserCreateOutcome,
  type ControlPlaneUserManagementFailure,
  type ControlPlaneUserPasswordResetOutcome,
  type ControlPlaneUserRole,
  type ControlPlaneUserSummary,
} from './control-plane-client.js'

/**
 * The user-management seam the page consumes, derived from the frozen
 * create/state/password facade (UI-100.1). Every method resolves when the
 * Server accepted the request and rejects with the one
 * `ControlPlaneClientError` identity; the durable account list always arrives
 * through `listUsers`, never from a write return value.
 */
export interface UserManagementPort {
  listUsers(): Promise<readonly ControlPlaneUserSummary[]>
  create(input: {
    readonly username: string
    readonly role: ControlPlaneUserRole
  }): Promise<ControlPlaneUserCreateOutcome>
  setState(input: {
    readonly userId: string
    readonly expectedRevision: number
    readonly state: ControlPlaneUserAccountState
  }): Promise<ControlPlaneUserAccount>
  resetPassword(input: {
    readonly userId: string
    readonly expectedRevision: number
    readonly currentPassword?: string
    readonly newPassword?: string
  }): Promise<ControlPlaneUserPasswordResetOutcome>
}

/** The create-form taxonomy: the facade taxonomy plus the shape reason. */
export type UserCreateFailure = ControlPlaneUserManagementFailure | 'username-invalid'

/** The row state-change taxonomy; the wire codes are already the page copy. */
export type UserStateFailure = ControlPlaneUserManagementFailure

/** The password-write taxonomy plus the pre-request shape reason. */
export type UserResetFailure = ControlPlaneUserManagementFailure | 'password-shape'

/**
 * The one one-time temporary password presentation. The Server issues the
 * secret exactly once, so the browser holds it in this one field only: it is
 * never cached, never persisted, and a page reload or dismiss loses it
 * forever.
 */
export interface OneTimeUserSecret {
  readonly username: string
  readonly password: string
  readonly reason: 'created' | 'reset'
}

export type UsersLoadStatus = 'unloaded' | 'loading' | 'loaded' | 'unavailable'

export interface UserManagementState {
  readonly status: 'idle' | 'creating'
  readonly failure: UserCreateFailure | null
  readonly users: readonly ControlPlaneUserSummary[]
  readonly usersStatus: UsersLoadStatus
  readonly oneTime: OneTimeUserSecret | null
}

/** The two directions of an account state change. */
export type UserStateAction = 'disable' | 'enable'

/** The one state-change interaction a user row can be in. */
export type UserRowInteraction =
  | { readonly kind: 'rest' }
  | { readonly kind: 'confirming'; readonly action: UserStateAction }
  | { readonly kind: 'submitting'; readonly action: UserStateAction }
  | {
    readonly kind: 'failed'
    readonly action: UserStateAction
    readonly failure: UserStateFailure
  }

/** The one Owner reset interaction a user row can be in. */
export type UserResetInteraction =
  | { readonly kind: 'rest' }
  | { readonly kind: 'submitting' }
  | { readonly kind: 'failed'; readonly failure: UserResetFailure }

/** The one self-service password form interaction. */
export type SelfPasswordInteraction =
  | { readonly kind: 'rest' }
  | { readonly kind: 'submitting' }
  | { readonly kind: 'succeeded' }
  | { readonly kind: 'failed'; readonly failure: UserResetFailure }

const REST_ROW: UserRowInteraction = Object.freeze({ kind: 'rest' })
const REST_RESET: UserResetInteraction = Object.freeze({ kind: 'rest' })

export type UserManagementViewModelListener = () => void

function rowInteractionKey(interaction: UserRowInteraction): string {
  if (interaction.kind === 'rest') return 'rest'
  if (interaction.kind === 'failed') {
    return `failed:${interaction.action}:${interaction.failure}`
  }
  return `${interaction.kind}:${interaction.action}`
}

function resetInteractionKey(interaction: UserResetInteraction): string {
  if (interaction.kind === 'rest') return 'rest'
  if (interaction.kind === 'failed') return `failed:${interaction.failure}`
  return interaction.kind
}

/**
 * The provisional wire-code translation of the facade validation rejections;
 * every Server code already reaches the page through the facade-owned context
 * classifiers. An Owner reset shares the password wire path with the
 * self-service form, but its 401 means the browser session expired, not a
 * wrong current password — so the two forms classify that code differently.
 */
function createFailure(error: unknown): UserCreateFailure {
  if (error instanceof ControlPlaneClientError) {
    if (error.code === 'USERS_CREATE_INPUT_INVALID') return 'username-invalid'
  }
  return controlPlaneUserCreateFailure(error)
}

function ownerResetFailure(error: unknown): UserResetFailure {
  if (error instanceof ControlPlaneClientError) {
    if (error.code === 'USERS_PASSWORD_INPUT_INVALID') return 'password-shape'
  }
  return controlPlaneUserStateFailure(error)
}

function selfResetFailure(error: unknown): UserResetFailure {
  if (error instanceof ControlPlaneClientError) {
    if (error.code === 'USERS_PASSWORD_INPUT_INVALID') return 'password-shape'
  }
  return controlPlaneUserPasswordFailure(error)
}

/**
 * Own the user management interactions: the account list read, the create
 * form, the per-row state changes with their explicit confirmation, the Owner
 * password resets, and the self-service rotation. The account list stays the
 * single durable authority; the one-time temporary password lives in exactly
 * one browser field and survives only until it is dismissed or the page
 * closes.
 */
export interface UserManagementViewModel {
  readonly state: UserManagementState
  /** The current state-change interaction of one row; unknown rows rest. */
  rowInteraction(userId: string): UserRowInteraction
  /** The current Owner reset interaction of one row; unknown rows rest. */
  resetInteraction(userId: string): UserResetInteraction
  /** The current interaction of the self-service password form. */
  readonly selfPassword: SelfPasswordInteraction
  /** Submit one account creation; the secret arrives through the state. */
  createUser(input: { readonly username: string; readonly role: ControlPlaneUserRole }): Promise<void>
  /** Arm the explicit confirmation of one account state change. */
  requestStateChange(userId: string, action: UserStateAction): void
  /** Submit the armed state change; only an armed or failed draft submits. */
  confirmStateChange(userId: string): void
  /** Drop the armed draft or the shown failure of one row. */
  dismissStateChange(userId: string): void
  /** Submit one Owner reset; the fresh secret arrives through the state. */
  requestOwnerReset(userId: string): void
  /** Submit the self-service rotation for the signed-in account. */
  changeOwnPassword(input: {
    readonly currentPassword: string
    readonly newPassword: string
  }): Promise<void>
  /** Drop the one-time secret forever; it can never be shown again. */
  dismissOneTime(): void
  /** Re-read the account list; a failed read never discards the shown rows. */
  refresh(): Promise<void>
  dismissCreateFailure(): void
  subscribe(listener: UserManagementViewModelListener): () => void
  close(): void
}

/**
 * Create the user management model. `port` stays null until the facade-backed
 * adapter is composed; a null port submits nothing and every action resolves
 * to the honest unavailable failure.
 */
export function createUserManagementViewModel(options: {
  readonly port: UserManagementPort | null
  /** The signed-in account id behind the self-service form; null disables it. */
  readonly selfUserId?: string | null
}): UserManagementViewModel {
  const port = options.port
  const selfUserId = options.selfUserId ?? null
  const listeners = new Set<UserManagementViewModelListener>()
  let current: UserManagementState = Object.freeze({
    status: 'idle',
    failure: null,
    users: [],
    usersStatus: 'unloaded',
    oneTime: null,
  })
  const rowInteractions = new Map<string, UserRowInteraction>()
  const resetInteractions = new Map<string, UserResetInteraction>()
  let selfPasswordInteraction: SelfPasswordInteraction = { kind: 'rest' }
  let refreshEpoch = 0
  let closed = false

  function publish(): void {
    for (const listener of listeners) listener()
  }

  function setState(next: UserManagementState): void {
    current = next
    publish()
  }

  function users(): readonly ControlPlaneUserSummary[] {
    return current.users
  }

  function findUser(userId: string): ControlPlaneUserSummary | undefined {
    return current.users.find(user => user.userId === userId)
  }

  function rowStateAllows(action: UserStateAction, user: ControlPlaneUserSummary): boolean {
    return action === 'disable' ? user.state === 'active' : user.state === 'disabled'
  }

  /**
   * Replace one row with the fresh Server account a write returned, so the
   * next compare-and-swap uses the revision the Server just durableized even
   * before the list re-read lands.
   */
  function applyAccount(account: ControlPlaneUserAccount): void {
    const summary: ControlPlaneUserSummary = Object.freeze({
      userId: account.userId,
      username: account.username,
      role: account.role,
      state: account.state,
      createdAt: account.createdAt,
      revision: account.revision,
    })
    const next = users().map(user => (user.userId === account.userId ? summary : user))
    setState(Object.freeze({ ...current, users: next }))
  }

  /**
   * A snapshot that moved past an armed draft or a shown failure drops it:
   * the row vanished, or its durable state already matches the intent.
   */
  function pruneStaleRows(): void {
    let changed = false
    for (const [userId, interaction] of rowInteractions) {
      if (interaction.kind === 'rest' || interaction.kind === 'submitting') continue
      const user = findUser(userId)
      if (user === undefined || !rowStateAllows(interaction.action, user)) {
        rowInteractions.set(userId, REST_ROW)
        changed = true
      }
    }
    for (const userId of resetInteractions.keys()) {
      if (findUser(userId) === undefined) {
        resetInteractions.delete(userId)
        changed = true
      }
    }
    if (changed) publish()
  }

  async function refreshUsers(): Promise<void> {
    if (port === null) {
      setState(Object.freeze({ ...current, usersStatus: 'unavailable' }))
      return
    }
    const epoch = ++refreshEpoch
    if (current.usersStatus === 'unloaded') {
      setState(Object.freeze({ ...current, usersStatus: 'loading' }))
    }
    try {
      const loaded = await port.listUsers()
      if (closed || epoch !== refreshEpoch) return
      setState(Object.freeze({ ...current, users: loaded, usersStatus: 'loaded' }))
    } catch {
      // An unavailable read only marks the list; the shown rows survive so a
      // transient failure never erases the Server snapshot already displayed.
      if (closed || epoch !== refreshEpoch) return
      setState(Object.freeze({ ...current, usersStatus: 'unavailable' }))
    }
    pruneStaleRows()
  }

  async function submitCreate(input: {
    readonly username: string
    readonly role: ControlPlaneUserRole
  }): Promise<void> {
    if (port === null) {
      setState(Object.freeze({ ...current, status: 'idle', failure: 'unavailable' }))
      return
    }
    setState(Object.freeze({ ...current, status: 'creating', failure: null }))
    try {
      // The view model owns the username shape the port receives, so an
      // injected port gets the same trimmed input the wire path would send.
      const outcome = await port.create({
        username: input.username.trim(),
        role: input.role,
      })
      if (closed) return
      // The one-time secret is held exactly here; nothing else consumes the
      // outcome, so the page is the only place the password was ever shown.
      setState(Object.freeze({
        ...current,
        status: 'idle',
        oneTime: Object.freeze({
          username: outcome.user.username,
          password: outcome.temporaryPassword,
          reason: 'created',
        }),
      }))
      applyAccount(outcome.user)
      await refreshUsers()
    } catch (error) {
      if (closed) return
      setState(Object.freeze({
        ...current,
        status: 'idle',
        failure: createFailure(error),
      }))
    }
  }

  async function submitStateChange(userId: string, action: UserStateAction): Promise<void> {
    if (port === null) {
      rowInteractions.set(userId, {
        kind: 'failed',
        action,
        failure: 'unavailable',
      })
      publish()
      return
    }
    const user = findUser(userId)
    if (user === undefined) {
      rowInteractions.set(userId, REST_ROW)
      publish()
      return
    }
    rowInteractions.set(userId, { kind: 'submitting', action })
    publish()
    try {
      const account = await port.setState({
        userId,
        expectedRevision: user.revision,
        state: action === 'disable' ? 'disabled' : 'active',
      })
      if (closed) return
      rowInteractions.set(userId, REST_ROW)
      applyAccount(account)
      publish()
      await refreshUsers()
    } catch (error) {
      if (closed) return
      // A failed state change keeps its armed draft: the same explicit accept
      // retries the request after the failure is read.
      rowInteractions.set(userId, {
        kind: 'failed',
        action,
        failure: controlPlaneUserStateFailure(error),
      })
      publish()
    }
  }

  async function submitOwnerReset(userId: string): Promise<void> {
    if (port === null) {
      resetInteractions.set(userId, { kind: 'failed', failure: 'unavailable' })
      publish()
      return
    }
    const user = findUser(userId)
    if (user === undefined) {
      resetInteractions.set(userId, REST_RESET)
      publish()
      return
    }
    resetInteractions.set(userId, { kind: 'submitting' })
    publish()
    try {
      const outcome = await port.resetPassword({
        userId,
        expectedRevision: user.revision,
      })
      if (closed) return
      resetInteractions.set(userId, REST_RESET)
      setState(Object.freeze({
        ...current,
        oneTime: Object.freeze({
          username: outcome.user.username,
          password: outcome.temporaryPassword as string,
          reason: 'reset',
        }),
      }))
      applyAccount(outcome.user)
      publish()
      await refreshUsers()
    } catch (error) {
      if (closed) return
      resetInteractions.set(userId, { kind: 'failed', failure: ownerResetFailure(error) })
      publish()
    }
  }

  return {
    get state() {
      return current
    },
    rowInteraction(userId) {
      return rowInteractions.get(userId) ?? REST_ROW
    },
    resetInteraction(userId) {
      return resetInteractions.get(userId) ?? REST_RESET
    },
    get selfPassword() {
      return selfPasswordInteraction
    },
    createUser(input) {
      if (closed || current.status === 'creating') return Promise.resolve()
      return submitCreate(input)
    },
    requestStateChange(userId, action) {
      if (closed) return
      const currentInteraction = rowInteractions.get(userId) ?? REST_ROW
      if (currentInteraction.kind === 'submitting') return
      const user = findUser(userId)
      if (user === undefined || !rowStateAllows(action, user)) return
      rowInteractions.set(userId, { kind: 'confirming', action })
      publish()
    },
    confirmStateChange(userId) {
      if (closed) return
      const currentInteraction = rowInteractions.get(userId) ?? REST_ROW
      if (currentInteraction.kind === 'confirming') {
        void submitStateChange(userId, currentInteraction.action)
        return
      }
      // A failed state change keeps its armed draft: the same explicit accept
      // retries it.
      if (currentInteraction.kind === 'failed') {
        void submitStateChange(userId, currentInteraction.action)
      }
    },
    dismissStateChange(userId) {
      if (closed) return
      const currentInteraction = rowInteractions.get(userId) ?? REST_ROW
      if (currentInteraction.kind === 'rest') return
      rowInteractions.set(userId, REST_ROW)
      publish()
    },
    requestOwnerReset(userId) {
      if (closed) return
      const currentInteraction = resetInteractions.get(userId) ?? REST_RESET
      if (currentInteraction.kind === 'submitting') return
      void submitOwnerReset(userId)
    },
    async changeOwnPassword(input) {
      if (closed) return
      if (selfPasswordInteraction.kind === 'submitting') return
      if (port === null || selfUserId === null) {
        selfPasswordInteraction = { kind: 'failed', failure: 'unavailable' }
        publish()
        return
      }
      const self = findUser(selfUserId)
      if (self === undefined) {
        selfPasswordInteraction = { kind: 'failed', failure: 'user-not-found' }
        publish()
        return
      }
      selfPasswordInteraction = { kind: 'submitting' }
      publish()
      try {
        const outcome = await port.resetPassword({
          userId: selfUserId,
          expectedRevision: self.revision,
          currentPassword: input.currentPassword,
          newPassword: input.newPassword,
        })
        if (closed) return
        selfPasswordInteraction = { kind: 'succeeded' }
        applyAccount(outcome.user)
        publish()
        await refreshUsers()
      } catch (error) {
        if (closed) return
        selfPasswordInteraction = { kind: 'failed', failure: selfResetFailure(error) }
        publish()
      }
    },
    dismissOneTime() {
      if (closed || current.oneTime === null) return
      setState(Object.freeze({ ...current, oneTime: null }))
    },
    refresh() {
      if (closed) return Promise.resolve()
      return refreshUsers()
    },
    dismissCreateFailure() {
      if (closed || current.failure === null) return
      setState(Object.freeze({ ...current, failure: null }))
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener()
      return () => {
        listeners.delete(listener)
      }
    },
    close() {
      if (closed) return
      closed = true
      refreshEpoch += 1
      listeners.clear()
      rowInteractions.clear()
      resetInteractions.clear()
      // The one-time secret never survives the page.
      setState(Object.freeze({ ...current, oneTime: null }))
    },
  }
}

type FacadeMethod = (input: Record<string, unknown>) => Promise<unknown>

function facadeMethod(value: unknown): FacadeMethod | null {
  if (typeof value !== 'function') return null
  return value as FacadeMethod
}

/**
 * Adapt the frozen users facade to the port the page consumes. Returns null
 * while the facade methods have not landed, so hosts compose the port only
 * when the seam exists.
 */
export function userManagementPortFromFacade(facade: object): UserManagementPort | null {
  const methods = facade as Record<string, unknown>
  const listUsers = facadeMethod(methods['listUsers'])
  const create = facadeMethod(methods['createUser'])
  const setState = facadeMethod(methods['setUserState'])
  const resetPassword = facadeMethod(methods['resetUserPassword'])
  if (listUsers === null || create === null || setState === null || resetPassword === null) {
    return null
  }
  return {
    listUsers: () => listUsers({}) as Promise<readonly ControlPlaneUserSummary[]>,
    create: input => create({ ...input }) as Promise<ControlPlaneUserCreateOutcome>,
    setState: input => setState({ ...input }) as Promise<ControlPlaneUserAccount>,
    resetPassword: input =>
      resetPassword({ ...input }) as Promise<ControlPlaneUserPasswordResetOutcome>,
  }
}
