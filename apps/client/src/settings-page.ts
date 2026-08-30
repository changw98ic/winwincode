// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './control-plane-client.js'
import type {
  CredentialReferenceId,
  CredentialReferenceProjection,
} from './generated/contracts.js'
import type {
  SettingsViewModel,
  SettingsViewModelState,
} from './settings-view-model.js'

export interface SettingsPageOptions {
  readonly root: HTMLElement
  readonly model: SettingsViewModel
}

export interface SettingsPage {
  close(): void
}

export interface SettingsPagePresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
  readonly mutationsDisabled: boolean
}

function knownSettingsError(error: ControlPlaneClientError): string | null {
  const labels: Readonly<Record<string, string>> = Object.freeze({
    SETTINGS_CONCURRENCY_INVALID: 'Worker concurrency must be between 1 and 10000.',
    SETTINGS_PROVIDER_REQUIRED: 'Enter a Provider ID.',
    SETTINGS_MODEL_REQUIRED: 'Enter a Model ID.',
    SETTINGS_CREDENTIAL_ROUTE_INVALID: 'Choose an available credential reference for this Provider.',
    SETTINGS_SNAPSHOT_REQUIRED: 'Refresh settings before saving a model route.',
    SETTINGS_DECISION_IN_FLIGHT: 'Wait for the current settings change to finish.',
    SETTINGS_REVISION_REQUIRED: 'Refresh settings before submitting this change.',
    CREDENTIAL_DISPLAY_NAME_REQUIRED: 'Enter a credential display name.',
    CREDENTIAL_PROVIDER_REQUIRED: 'Enter the credential Provider ID.',
    CREDENTIAL_SECRET_REQUIRED: 'Choose a local secret before submitting the credential reference.',
    CREDENTIAL_REFERENCE_STALE: 'Refresh settings and select a current credential reference.',
    INVALID_CLIENT_REQUEST: 'Check the local user identity and workspace scope configuration, then retry.',
  })
  return labels[error.code] ?? null
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  const known = knownSettingsError(error)
  if (known !== null) return known
  if (error.code === 'REVISION_CONFLICT') {
    return 'These settings changed before the update was saved. Review the current snapshot and try again.'
  }
  if (error.kind === 'authentication') return 'Sign in again to manage local Provider settings.'
  if (error.kind === 'authorization') return 'You do not have access to these Provider settings.'
  if (error.kind === 'network') return 'The settings server could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The settings update was cancelled.'
  if (error.kind === 'configuration') {
    return 'Check the local server URL and workspace scope configuration, then retry.'
  }
  return 'Provider settings could not be updated. Retry, or review the server status.'
}

export function settingsPagePresentation(
  state: SettingsViewModelState,
): SettingsPagePresentation {
  const visibleError = state.interaction.error ?? state.error
  const statusText = state.interaction.status === 'submitting'
    ? 'Saving Provider settings…'
    : state.interaction.status === 'waiting'
      ? 'Change accepted · waiting for the current snapshot…'
      : state.status === 'loading'
        ? 'Loading Provider settings…'
        : state.status === 'refreshing' || state.realtime === 'reloading'
          ? 'Updating Provider settings…'
          : state.realtime === 'reconnecting'
            ? 'Reconnecting…'
            : state.status === 'authentication-required'
              ? 'Sign in required'
              : state.status === 'authorization-denied'
                ? 'Access denied'
                : state.status === 'cancelled'
                  ? 'Update cancelled'
                  : state.status === 'error'
                    ? 'Provider settings unavailable'
                    : state.status === 'closed'
                      ? 'Provider settings closed'
                      : state.settings === null
                        ? 'No settings snapshot'
                        : `Ready · revision ${String(state.settings.revision)}`
  const busy = state.status === 'loading'
    || state.status === 'refreshing'
    || state.realtime === 'reloading'
    || state.interaction.status === 'submitting'
    || state.interaction.status === 'waiting'
  const mutationsDisabled = busy
    || state.settings === null
    || state.status === 'authentication-required'
    || state.status === 'authorization-denied'
    || state.status === 'closed'
  return Object.freeze({
    statusText,
    errorText: errorLabel(visibleError),
    busy,
    retryVisible: visibleError !== null && state.realtime !== 'reconnecting',
    reconnectVisible: state.realtime === 'reconnecting',
    mutationsDisabled,
  })
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

function labelledInput(
  document: Document,
  id: string,
  labelText: string,
  className: string,
  type = 'text',
): { readonly label: HTMLLabelElement; readonly input: HTMLInputElement } {
  const label = element(document, 'label', `${className}-label`)
  const input = element(document, 'input', className)
  label.htmlFor = id
  label.textContent = labelText
  input.id = id
  input.type = type
  label.append(input)
  return Object.freeze({ label, input })
}

function lifecycleLabel(reference: CredentialReferenceProjection): string {
  if (reference.secretState === 'revoked') return 'Revoked'
  if (reference.secretState === 'missing') return 'Secret missing'
  return 'Available'
}

/** Mount local Provider settings and write-only Credential reference controls. */
export function mountSettingsPage(options: SettingsPageOptions): SettingsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'main', 'wwc-settings')
  const heading = element(document, 'h1', 'wwc-settings-heading')
  const status = element(document, 'p', 'wwc-settings-status')
  const error = element(document, 'div', 'wwc-settings-error')
  const errorText = element(document, 'span', 'wwc-settings-error-text')
  const retry = element(document, 'button', 'wwc-settings-retry')
  const reconnect = element(document, 'button', 'wwc-settings-reconnect')

  const routeSection = element(document, 'section', 'wwc-settings-route')
  const routeHeading = element(document, 'h2', 'wwc-settings-section-heading')
  const routeForm = element(document, 'form', 'wwc-settings-route-form')
  const provider = labelledInput(document, 'wwc-settings-provider', 'Provider ID', 'wwc-settings-provider')
  const model = labelledInput(document, 'wwc-settings-model', 'Model ID', 'wwc-settings-model')
  const credentialLabel = element(document, 'label', 'wwc-settings-credential-label')
  const credential = element(document, 'select', 'wwc-settings-credential')
  const concurrency = labelledInput(
    document,
    'wwc-settings-concurrency',
    'Worker concurrency',
    'wwc-settings-concurrency',
    'number',
  )
  const routeControls = element(document, 'div', 'wwc-settings-route-controls')
  const saveRoute = element(document, 'button', 'wwc-settings-save-route')
  const clearRoute = element(document, 'button', 'wwc-settings-clear-route')

  const createSection = element(document, 'section', 'wwc-settings-create-credential')
  const createHeading = element(document, 'h2', 'wwc-settings-section-heading')
  const createHelp = element(document, 'p', 'wwc-settings-secret-help')
  const createForm = element(document, 'form', 'wwc-settings-create-form')
  const createId = labelledInput(document, 'wwc-settings-create-id', 'Reference ID', 'wwc-settings-create-id')
  const createName = labelledInput(document, 'wwc-settings-create-name', 'Display name', 'wwc-settings-create-name')
  const createProvider = labelledInput(
    document,
    'wwc-settings-create-provider',
    'Provider ID',
    'wwc-settings-create-provider',
  )
  const createSecret = labelledInput(
    document,
    'wwc-settings-create-secret',
    'Local secret-store locator',
    'wwc-settings-create-secret',
    'password',
  )
  const createButton = element(document, 'button', 'wwc-settings-create-submit')

  const referencesSection = element(document, 'section', 'wwc-settings-credentials')
  const referencesHeading = element(document, 'h2', 'wwc-settings-section-heading')
  const referencesHelp = element(document, 'p', 'wwc-settings-credential-help')
  const references = element(document, 'ul', 'wwc-settings-credential-list')
  let closed = false

  heading.textContent = 'Local Provider settings'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  retry.type = 'button'
  retry.textContent = 'Retry snapshot'
  reconnect.type = 'button'
  reconnect.textContent = 'Reconnect events'
  error.append(errorText, retry, reconnect)

  routeHeading.textContent = 'Default model route'
  credentialLabel.htmlFor = 'wwc-settings-credential'
  credentialLabel.textContent = 'Credential reference'
  credential.id = 'wwc-settings-credential'
  credentialLabel.append(credential)
  concurrency.input.min = '1'
  concurrency.input.max = '10000'
  concurrency.input.step = '1'
  saveRoute.type = 'submit'
  saveRoute.textContent = 'Save model route'
  clearRoute.type = 'button'
  clearRoute.textContent = 'Clear default route'
  routeControls.append(saveRoute, clearRoute)
  routeForm.append(
    provider.label,
    model.label,
    credentialLabel,
    concurrency.label,
    routeControls,
  )
  routeSection.append(routeHeading, routeForm)

  createHeading.textContent = 'Add Credential reference'
  createHelp.textContent = 'The local secret-store locator is submitted once and is not shown again.'
  createSecret.input.autocomplete = 'new-password'
  createSecret.input.spellcheck = false
  createButton.type = 'submit'
  createButton.textContent = 'Add reference'
  createForm.append(
    createId.label,
    createName.label,
    createProvider.label,
    createSecret.label,
    createButton,
  )
  createSection.append(createHeading, createHelp, createForm)

  referencesHeading.textContent = 'Credential references'
  referencesHelp.textContent = 'Only secret-safe lifecycle metadata is displayed.'
  references.setAttribute('aria-live', 'polite')
  referencesSection.append(referencesHeading, referencesHelp, references)
  layout.append(heading, status, error, routeSection, createSection, referencesSection)
  options.root.replaceChildren(layout)

  function renderReference(
    reference: CredentialReferenceProjection,
    disabled: boolean,
  ): HTMLLIElement {
    const item = element(document, 'li', 'wwc-settings-credential-item')
    const title = element(document, 'h3', 'wwc-settings-credential-title')
    const metadata = element(document, 'dl', 'wwc-settings-credential-metadata')
    const rotateForm = element(document, 'form', 'wwc-settings-rotate-form')
    const rotateSecret = labelledInput(
      document,
      `wwc-settings-rotate-${reference.id}`,
      `New local secret for ${reference.displayName}`,
      'wwc-settings-rotate-secret',
      'password',
    )
    const rotate = element(document, 'button', 'wwc-settings-rotate')
    const revoke = element(document, 'button', 'wwc-settings-revoke')
    const entries: readonly [string, string][] = Object.freeze([
      ['Reference ID', reference.id],
      ['Provider ID', reference.providerId],
      ['Secret state', lifecycleLabel(reference)],
      ['Rotation version', String(reference.rotationVersion)],
      ['Updated', reference.updatedAt],
      ['Last rotated', reference.lastRotatedAt ?? 'Never'],
      ['Revoked', reference.revokedAt ?? 'No'],
    ])
    title.textContent = reference.displayName
    for (const [term, description] of entries) {
      const dt = document.createElement('dt')
      const dd = document.createElement('dd')
      dt.textContent = term
      dd.textContent = description
      metadata.append(dt, dd)
    }
    rotateSecret.input.autocomplete = 'new-password'
    rotateSecret.input.spellcheck = false
    rotate.type = 'submit'
    rotate.textContent = 'Rotate secret'
    revoke.type = 'button'
    revoke.textContent = 'Revoke reference'
    rotate.disabled = disabled || reference.secretState === 'revoked'
    rotateSecret.input.disabled = rotate.disabled
    revoke.disabled = disabled || reference.secretState === 'revoked'
    rotateForm.addEventListener('submit', event => {
      event.preventDefault()
      const secret = rotateSecret.input.value
      rotateSecret.input.value = ''
      void options.model.rotateCredentialReference({
        credentialReferenceId: reference.id,
        vaultLocator: secret,
      })
    })
    revoke.addEventListener('click', () => {
      void options.model.revokeCredentialReference(reference.id)
    })
    rotateForm.append(rotateSecret.label, rotate)
    item.append(title, metadata, rotateForm, revoke)
    return item
  }

  function render(state: SettingsViewModelState): void {
    if (closed) return
    const presentation = settingsPagePresentation(state)
    const route = state.settings?.defaultModelRoute ?? null
    status.textContent = presentation.statusText
    layout.setAttribute('aria-busy', String(presentation.busy))
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    provider.input.value = route?.providerId ?? ''
    model.input.value = route?.modelId ?? ''
    concurrency.input.value = String(state.settings?.workerConcurrencyLimit ?? 1)
    credential.replaceChildren()
    const noCredential = document.createElement('option')
    noCredential.value = ''
    noCredential.textContent = 'Choose an available reference'
    credential.append(noCredential)
    for (const reference of state.credentials) {
      if (reference.secretState !== 'available') continue
      const choice = document.createElement('option')
      choice.value = reference.id
      choice.textContent = `${reference.displayName} · ${reference.providerId}`
      choice.selected = reference.id === route?.credentialReferenceId
      credential.append(choice)
    }
    provider.input.disabled = presentation.mutationsDisabled
    model.input.disabled = presentation.mutationsDisabled
    credential.disabled = presentation.mutationsDisabled
    concurrency.input.disabled = presentation.mutationsDisabled
    saveRoute.disabled = presentation.mutationsDisabled
    clearRoute.disabled = presentation.mutationsDisabled || route === null
    createId.input.disabled = presentation.mutationsDisabled
    createName.input.disabled = presentation.mutationsDisabled
    createProvider.input.disabled = presentation.mutationsDisabled
    createSecret.input.disabled = presentation.mutationsDisabled
    createButton.disabled = presentation.mutationsDisabled
    references.replaceChildren(...state.credentials.map(reference => (
      renderReference(reference, presentation.mutationsDisabled)
    )))
  }

  routeForm.addEventListener('submit', event => {
    event.preventDefault()
    void options.model.updateSettings({
      defaultModelRoute: {
        providerId: provider.input.value,
        modelId: model.input.value,
        credentialReferenceId: credential.value as CredentialReferenceId,
      },
      workerConcurrencyLimit: Number(concurrency.input.value),
    })
  })
  clearRoute.addEventListener('click', () => {
    void options.model.updateSettings({
      defaultModelRoute: null,
      workerConcurrencyLimit: Number(concurrency.input.value),
    })
  })
  createForm.addEventListener('submit', event => {
    event.preventDefault()
    const secret = createSecret.input.value
    createSecret.input.value = ''
    void options.model.createCredentialReference({
      credentialReferenceId: createId.input.value as CredentialReferenceId,
      displayName: createName.input.value,
      providerId: createProvider.input.value,
      vaultLocator: secret,
    }).then(() => {
      if (options.model.state.interaction.status !== 'error') {
        createId.input.value = ''
        createName.input.value = ''
        createProvider.input.value = ''
      }
    })
  })
  retry.addEventListener('click', () => { void options.model.refresh() })
  reconnect.addEventListener('click', () => { options.model.reconnect() })
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
