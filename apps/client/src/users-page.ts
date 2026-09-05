// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection } from './components/keyed-collection.js'
import type {
  ControlPlaneUserManagementFailure,
  ControlPlaneUserSummary,
} from './control-plane-client.js'
import {
  mountUserRowControls,
  type UserRowControls,
} from './user-management-controls.js'
import type {
  UserManagementState,
  UserManagementViewModel,
} from './user-management-view-model.js'

export interface UsersPageOptions {
  readonly root: HTMLElement
  readonly model: UserManagementViewModel
  /**
   * The signed-in account id; when present the self-service password form is
   * mounted and enabled once the account's list row carries its revision.
   */
  readonly selfUserId?: string | null
  /**
   * Deterministic clipboard seam for the one-time password; the default uses
   * the browser clipboard when the host provides one.
   */
  readonly writeText?: (text: string) => Promise<void>
}

export interface UsersPage {
  close(): void
  /** Show or hide the area; drafts survive visibility changes and re-renders. */
  setVisible(visible: boolean): void
}

interface UserRowRefs {
  readonly row: HTMLElement
  readonly username: HTMLElement
  readonly role: HTMLElement
  readonly state: HTMLElement
  readonly created: HTMLElement
  readonly controls: UserRowControls
}

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

/** Every failure copy of the area; each entry reaches the alert role too. */
function failureText(
  failure: ControlPlaneUserManagementFailure | 'username-invalid' | 'password-shape',
): string {
  switch (failure) {
    case 'username-invalid': return 'Enter a username without whitespace, at most 96 characters.'
    case 'username-conflict': return 'That username already belongs to another account.'
    case 'permission-denied': return 'Only the Owner can manage users.'
    case 'authentication-required': return 'Sign in again to continue managing users.'
    case 'invalid-request': return 'The request was rejected as invalid. Check the form and try again.'
    case 'wrong-state': return 'The request conflicts with the current accounts. Retry.'
    case 'revision-conflict': return 'The account changed while you worked. Retry.'
    case 'user-not-found': return 'This account no longer exists.'
    case 'current-password-wrong': return 'The current password is wrong. Check it and try again.'
    case 'password-shape': return 'Enter the passwords before submitting.'
    case 'unavailable': return 'The request did not go through. Check the connection and try again.'
  }
}

/** The one state copy; the text carries the meaning, the tone only supports it. */
function userStateText(state: ControlPlaneUserSummary['state']): string {
  return state === 'active' ? 'Active' : 'Disabled'
}

function userStateTone(state: ControlPlaneUserSummary['state']): 'success' | 'danger' {
  return state === 'active' ? 'success' : 'danger'
}

function userRoleText(role: ControlPlaneUserSummary['role']): string {
  return role === 'owner' ? 'Owner' : 'Member'
}

/** Deterministic date presentation of the RFC3339 creation instant. */
function createdText(createdAt: string): string {
  const date = createdAt.slice(0, 10)
  return date.length === 10 ? `Created ${date}` : 'Creation date unknown'
}

/**
 * Mount the Owner user management area: the create form with its one-time
 * temporary password presentation, the account rows, and the self-service
 * password form. The area keeps one polite secret channel and renders its
 * titles as styled paragraphs, mirroring the Clients area's shell audit
 * constraints.
 */
export function mountUsersPage(options: UsersPageOptions): UsersPage {
  const document = options.root.ownerDocument
  const writeText = options.writeText
    ?? (typeof navigator !== 'undefined' && navigator.clipboard !== undefined
      ? (text: string) => navigator.clipboard.writeText(text)
      : null)
  const region = element(document, 'section', 'wwc-users')
  const heading = element(document, 'p', 'wwc-users-heading')
  const createForm = element(document, 'form', 'wwc-users-create-form')
  const createHeading = element(document, 'p', 'wwc-users-create-heading')
  const usernameLabel = element(document, 'label', 'wwc-users-label')
  const usernameInput = element(document, 'input', 'wwc-users-control wwc-users-username-input')
  const roleLabel = element(document, 'label', 'wwc-users-label')
  const roleSelect = element(document, 'select', 'wwc-users-control wwc-users-role-select')
  const memberOption = element(document, 'option', 'wwc-users-role-option')
  const ownerOption = element(document, 'option', 'wwc-users-role-option')
  const createSubmit = element(document, 'button', 'wwc-users-create-submit')
  const createStatus = element(document, 'p', 'wwc-users-create-status')
  const createError = element(document, 'p', 'wwc-users-create-error')
  const oneTime = element(document, 'div', 'wwc-users-one-time')
  const oneTimeTitle = element(document, 'p', 'wwc-users-one-time-title')
  const oneTimeSecret = element(document, 'code', 'wwc-users-one-time-secret')
  const oneTimeHint = element(document, 'p', 'wwc-users-one-time-hint')
  const oneTimeCopy = element(document, 'button', 'wwc-users-one-time-copy')
  const oneTimeDone = element(document, 'button', 'wwc-users-one-time-done')
  const oneTimeCopied = element(document, 'p', 'wwc-users-one-time-copied')
  const createListHeading = element(document, 'p', 'wwc-users-list-heading')
  const empty = element(document, 'p', 'wwc-users-empty')
  const list = element(document, 'div', 'wwc-users-list')
  const selfForm = element(document, 'form', 'wwc-users-self-form')
  const selfHeading = element(document, 'p', 'wwc-users-self-heading')
  const currentLabel = element(document, 'label', 'wwc-users-label')
  const currentInput = element(document, 'input', 'wwc-users-control wwc-users-current-input')
  const newLabel = element(document, 'label', 'wwc-users-label')
  const newInput = element(document, 'input', 'wwc-users-control wwc-users-new-input')
  const selfSubmit = element(document, 'button', 'wwc-users-self-submit')
  const selfStatus = element(document, 'p', 'wwc-users-self-status')
  const selfError = element(document, 'p', 'wwc-users-self-error')
  const rows = new WeakMap<HTMLElement, UserRowRefs>()
  let closed = false
  let clearedAfterCreation = false
  let shownSecret: string | null = null

  region.setAttribute('aria-label', 'Users')
  region.hidden = true
  heading.textContent = 'Users'
  heading.id = 'wwc-users-heading'
  region.setAttribute('aria-labelledby', heading.id)

  createHeading.textContent = 'Create a user'
  createHeading.id = 'wwc-users-create-heading'
  createForm.setAttribute('aria-labelledby', createHeading.id)

  usernameInput.id = 'wwc-users-username-input'
  usernameInput.name = 'username'
  usernameInput.type = 'text'
  usernameInput.autocomplete = 'off'
  usernameInput.spellcheck = false
  usernameInput.setAttribute('autocapitalize', 'none')
  usernameInput.maxLength = 96
  usernameInput.required = true
  usernameLabel.htmlFor = usernameInput.id
  usernameLabel.textContent = 'Username'

  roleSelect.id = 'wwc-users-role-select'
  roleSelect.name = 'role'
  memberOption.value = 'member'
  memberOption.textContent = 'Member'
  ownerOption.value = 'owner'
  ownerOption.textContent = 'Owner'
  roleSelect.append(memberOption, ownerOption)
  roleSelect.value = 'member'
  roleLabel.htmlFor = roleSelect.id
  roleLabel.textContent = 'Role'

  createSubmit.type = 'submit'
  createSubmit.textContent = 'Create user'
  createStatus.hidden = true
  createError.setAttribute('role', 'alert')
  createError.hidden = true
  createError.id = 'wwc-users-create-error'
  createForm.append(
    createHeading,
    usernameLabel,
    usernameInput,
    roleLabel,
    roleSelect,
    createSubmit,
    createStatus,
    createError,
  )  // The one-time secret region: the only place the password was ever shown.
  oneTime.setAttribute('role', 'status')
  oneTime.hidden = true
  oneTimeSecret.id = 'wwc-users-one-time-secret'
  oneTimeHint.textContent =
    'Copy it now. It is shown only once and disappears when you hide it or reload the page.'
  oneTimeCopy.type = 'button'
  oneTimeCopy.textContent = 'Copy password'
  oneTimeDone.type = 'button'
  oneTimeDone.textContent = 'Done — hide it'
  oneTimeCopied.hidden = true
  oneTime.append(
    oneTimeTitle,
    oneTimeSecret,
    oneTimeHint,
    oneTimeCopy,
    oneTimeDone,
    oneTimeCopied,
  )

  createListHeading.textContent = 'Accounts'
  createListHeading.id = 'wwc-users-list-heading'
  empty.textContent = 'No accounts yet.'
  empty.hidden = true

  const selfPasswordAvailable = typeof options.selfUserId === 'string'
    && options.selfUserId.length > 0
  selfHeading.textContent = 'Change your password'
  selfHeading.id = 'wwc-users-self-heading'
  selfForm.setAttribute('aria-labelledby', selfHeading.id)
  currentInput.id = 'wwc-users-current-input'
  currentInput.name = 'currentPassword'
  currentInput.type = 'password'
  currentInput.autocomplete = 'current-password'
  currentInput.maxLength = 256
  currentInput.required = true
  currentLabel.htmlFor = currentInput.id
  currentLabel.textContent = 'Current password'
  newInput.id = 'wwc-users-new-input'
  newInput.name = 'newPassword'
  newInput.type = 'password'
  newInput.autocomplete = 'new-password'
  newInput.maxLength = 256
  newInput.required = true
  newLabel.htmlFor = newInput.id
  newLabel.textContent = 'New password'
  selfSubmit.type = 'submit'
  selfSubmit.textContent = 'Change password'
  selfStatus.hidden = true
  selfError.setAttribute('role', 'alert')
  selfError.hidden = true
  selfError.id = 'wwc-users-self-error'
  selfForm.append(
    selfHeading,
    currentLabel,
    currentInput,
    newLabel,
    newInput,
    selfSubmit,
    selfStatus,
    selfError,
  )
  selfForm.hidden = !selfPasswordAvailable

  region.append(
    heading,
    createForm,
    oneTime,
    createListHeading,
    list,
    empty,
    selfForm,
  )
  options.root.replaceChildren(region)

  const rowCollection = mountKeyedCollection<ControlPlaneUserSummary, string, HTMLElement>({
    parent: list,
    key: user => user.userId,
    create(user) {
      const row = element(document, 'article', 'wwc-users-row')
      const username = element(document, 'p', 'wwc-users-row-username')
      const role = element(document, 'span', 'wwc-users-row-role')
      const state = element(document, 'span', 'wwc-users-row-state')
      const created = element(document, 'p', 'wwc-users-row-created')
      const actions = element(document, 'div', 'wwc-users-row-actions')
      const notice = element(document, 'div', 'wwc-users-row-notice')
      const controls = mountUserRowControls({
        document,
        actions,
        notice,
        model: options.model,
      })
      row.append(username, role, state, created, actions, notice)
      const refs: UserRowRefs = {
        row, username, role, state, created, controls,
      }
      rows.set(row, refs)
      updateRow(refs, user)
      return row
    },
    update(node, user) {
      const refs = rows.get(node)
      if (refs === undefined) return
      updateRow(refs, user)
    },
    remove(node) {
      rows.get(node)?.controls.close()
    },
  })

  function updateRow(refs: UserRowRefs, user: ControlPlaneUserSummary): void {
    refs.row.setAttribute('aria-label', `${user.username}: ${userStateText(user.state)}`)
    refs.username.textContent = user.username
    refs.role.textContent = userRoleText(user.role)
    refs.state.textContent = userStateText(user.state)
    refs.state.dataset.tone = userStateTone(user.state)
    refs.created.textContent = createdText(user.createdAt)
    refs.controls.update(user)
  }

  function renderOneTime(): void {
    const secret = options.model.state.oneTime
    if (secret === null) {
      // Hiding is final: the dismissed secret is dropped, never re-shown.
      shownSecret = null
      oneTime.hidden = true
      oneTimeTitle.textContent = ''
      oneTimeSecret.textContent = ''
      oneTimeCopied.hidden = true
      oneTimeCopied.textContent = ''
      return
    }
    if (shownSecret !== secret.password) {
      oneTimeCopied.hidden = true
      oneTimeCopied.textContent = ''
    }
    shownSecret = secret.password
    oneTimeTitle.textContent = secret.reason === 'created'
      ? `Account created. One-time password for ${secret.username}`
      : `Password reset. One-time password for ${secret.username}`
    oneTimeSecret.textContent = secret.password
    oneTime.hidden = false
  }

  function render(snapshot: UserManagementState): void {
    if (closed) return
    const busy = snapshot.status === 'creating'
    if (
      snapshot.oneTime?.reason === 'created' && !clearedAfterCreation
    ) {
      clearedAfterCreation = true
      usernameInput.value = ''
      roleSelect.value = 'member'
    }
    if (snapshot.oneTime?.reason !== 'created') clearedAfterCreation = false
    createStatus.textContent = busy ? 'Creating the user…' : ''
    createStatus.hidden = !busy
    const nextFailure = snapshot.status === 'idle' && snapshot.failure !== null
      ? failureText(snapshot.failure)
      : null
    createError.textContent = nextFailure ?? ''
    createError.hidden = nextFailure === null
    usernameInput.disabled = busy
    roleSelect.disabled = busy
    createSubmit.disabled = busy
    createForm.setAttribute('aria-busy', busy ? 'true' : 'false')
    renderOneTime()
    rowCollection.update(snapshot.users)
    empty.hidden = snapshot.users.length !== 0 || snapshot.usersStatus === 'loading'

    const selfBusy = options.model.selfPassword.kind === 'submitting'
    selfStatus.textContent = options.model.selfPassword.kind === 'succeeded'
      ? 'Your password is updated.'
      : ''
    selfStatus.hidden = options.model.selfPassword.kind !== 'succeeded'
    const selfFailure = options.model.selfPassword.kind === 'failed'
      ? failureText(options.model.selfPassword.failure)
      : null
    selfError.textContent = selfFailure ?? ''
    selfError.hidden = selfFailure === null
    currentInput.disabled = selfBusy
    newInput.disabled = selfBusy
    selfSubmit.disabled = selfBusy || !selfPasswordAvailable
    selfForm.setAttribute('aria-busy', selfBusy ? 'true' : 'false')
  }

  const onCreateSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.model.state.status === 'creating') return
    void options.model.createUser({
      username: usernameInput.value,
      role: roleSelect.value === 'owner' ? 'owner' : 'member',
    })
  }
  const onUsernameEdit = () => {
    if (options.model.state.failure !== null) options.model.dismissCreateFailure()
  }
  const onOneTimeCopy = () => {
    const secret = options.model.state.oneTime
    if (secret === null || writeText === null) return
    // The secret reaches the clipboard through the one seam; the text node
    // itself stays the only other copy.
    void writeText(secret.password).then(() => {
      oneTimeCopied.textContent = 'Copied to the clipboard.'
      oneTimeCopied.hidden = false
    }, () => {
      oneTimeCopied.textContent = 'Copy failed. Select the password text to copy it.'
      oneTimeCopied.hidden = false
    })
  }
  const onOneTimeDone = () => {
    options.model.dismissOneTime()
  }
  const onSelfSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.model.selfPassword.kind === 'submitting') return
    // Secret-safe submission: the passwords leave the DOM before the await,
    // mirroring the sign-in page's password draft.
    const submittedCurrent = currentInput.value
    const submittedNew = newInput.value
    currentInput.value = ''
    newInput.value = ''
    void options.model.changeOwnPassword({
      currentPassword: submittedCurrent,
      newPassword: submittedNew,
    })
  }
  const onSelfEdit = () => {
    if (options.model.selfPassword.kind === 'failed') {
      selfError.hidden = true
      selfError.textContent = ''
    }
  }

  createForm.addEventListener('submit', onCreateSubmit)
  usernameInput.addEventListener('input', onUsernameEdit)
  oneTimeCopy.addEventListener('click', onOneTimeCopy)
  oneTimeDone.addEventListener('click', onOneTimeDone)
  selfForm.addEventListener('submit', onSelfSubmit)
  currentInput.addEventListener('input', onSelfEdit)
  newInput.addEventListener('input', onSelfEdit)

  const unsubscribe = options.model.subscribe(() => {
    render(options.model.state)
  })

  return {
    setVisible(visible) {
      if (closed) return
      region.hidden = !visible
    },
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      rowCollection.close()
      usernameInput.value = ''
      currentInput.value = ''
      newInput.value = ''
      createForm.removeEventListener('submit', onCreateSubmit)
      usernameInput.removeEventListener('input', onUsernameEdit)
      oneTimeCopy.removeEventListener('click', onOneTimeCopy)
      oneTimeDone.removeEventListener('click', onOneTimeDone)
      selfForm.removeEventListener('submit', onSelfSubmit)
      currentInput.removeEventListener('input', onSelfEdit)
      newInput.removeEventListener('input', onSelfEdit)
      options.root.replaceChildren()
    },
  }
}
