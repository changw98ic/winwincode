// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './control-plane-client.js'
import {
  mountButton,
  mountEmptyState,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  type StatusTone,
} from './components/index.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
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
  readonly localOperationsHref?: string
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
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
  layout.dataset.wwcPage = 'management'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: 'Local Provider settings',
      eyebrow: 'Local control plane',
      description: 'Choose the default model route and manage write-only Credential references.',
      headingLevel: 1,
      className: 'wwc-settings-heading',
    },
  })
  const heading = pageHeader.root
  const localOperationsLink = element(document, 'a', 'wwc-settings-local-operations-link')
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: 'Loading Provider settings…',
      tone: 'info',
      live: 'polite',
      className: 'wwc-settings-status',
    },
  })
  const status = statusBadge.root
  const retryButton = mountButton({
    document,
    props: {
      label: 'Retry snapshot',
      className: 'wwc-settings-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: 'Reconnect events',
      className: 'wwc-settings-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: 'Provider settings unavailable',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-settings-error',
    },
  })
  const error = errorState.root
  const errorText = errorState.message
  errorText.className = 'wwc-settings-error-text'

  const routePanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-route',
      title: 'Default model route',
      description: 'Select the Provider, model, Credential reference, and local Worker limit.',
      className: 'wwc-settings-route',
    },
  })
  const routeSection = routePanel.root
  const routeHeading = routePanel.title
  routeHeading.className = 'wwc-settings-section-heading'
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

  const createPanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-create-credential',
      title: 'Add Credential reference',
      description: 'The local secret-store locator is submitted once and is not shown again.',
      className: 'wwc-settings-create-credential',
    },
  })
  const createSection = createPanel.root
  const createHeading = createPanel.title
  createHeading.className = 'wwc-settings-section-heading'
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

  const referencesPanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-credentials',
      title: 'Credential references',
      description: 'Only secret-safe lifecycle metadata is displayed.',
      className: 'wwc-settings-credentials',
    },
  })
  const referencesSection = referencesPanel.root
  const referencesHeading = referencesPanel.title
  referencesHeading.className = 'wwc-settings-section-heading'
  const referencesHelp = element(document, 'p', 'wwc-settings-credential-help')
  const references = element(document, 'ul', 'wwc-settings-credential-list')
  const referencesEmpty = mountEmptyState({
    document,
    props: {
      title: 'No Credential references',
      detail: 'Add a write-only Credential reference before choosing a default model route.',
      className: 'wwc-settings-credential-empty',
    },
  })
  let closed = false

  localOperationsLink.href = options.localOperationsHref ?? '#/settings/runtime'
  localOperationsLink.textContent = 'Open repository and local Worker operations'

  credentialLabel.htmlFor = 'wwc-settings-credential'
  credentialLabel.textContent = 'Credential reference'
  credential.id = 'wwc-settings-credential'
  credentialLabel.append(credential)
  concurrency.input.min = '1'
  concurrency.input.max = '10000'
  concurrency.input.step = '1'
  saveRoute.type = 'submit'
  saveRoute.textContent = 'Save model route'
  saveRoute.dataset.wwcComponent = 'button'
  saveRoute.dataset.variant = 'primary'
  clearRoute.type = 'button'
  clearRoute.textContent = 'Clear default route'
  clearRoute.dataset.wwcComponent = 'button'
  clearRoute.dataset.variant = 'destructive'
  routeControls.append(saveRoute, clearRoute)
  routeForm.append(
    provider.label,
    model.label,
    credentialLabel,
    concurrency.label,
    routeControls,
  )
  routePanel.content.append(routeForm)

  createHelp.textContent = 'The local secret-store locator is submitted once and is not shown again.'
  createHelp.hidden = true
  createSecret.input.autocomplete = 'new-password'
  createSecret.input.spellcheck = false
  createButton.type = 'submit'
  createButton.textContent = 'Add reference'
  createButton.dataset.wwcComponent = 'button'
  createButton.dataset.variant = 'primary'
  createForm.append(
    createId.label,
    createName.label,
    createProvider.label,
    createSecret.label,
    createButton,
  )
  createPanel.content.append(createHelp, createForm)

  referencesHelp.textContent = 'Only secret-safe lifecycle metadata is displayed.'
  referencesHelp.hidden = true
  references.setAttribute('aria-live', 'polite')
  referencesPanel.content.append(referencesHelp, references, referencesEmpty.root)
  layout.append(
    heading,
    localOperationsLink,
    status,
    error,
    routeSection,
    createSection,
    referencesSection,
  )
  options.root.replaceChildren(layout)

  interface CredentialChoice {
    readonly key: string
    readonly reference: CredentialReferenceProjection | null
  }
  interface CredentialRow {
    current: CredentialReferenceProjection
    readonly title: HTMLElement
    readonly descriptions: readonly HTMLElement[]
    readonly rotateForm: HTMLFormElement
    readonly rotateSecret: HTMLInputElement
    readonly rotate: HTMLButtonElement
    readonly revoke: HTMLButtonElement
    readonly onRotate: (event: SubmitEvent) => void
    readonly onRevoke: () => void
  }
  let routeDirty = false
  const markRouteDirty = () => { routeDirty = true }
  const credentialOptions = mountKeyedCollection<CredentialChoice, string, HTMLOptionElement>({
    parent: credential,
    key: choice => choice.key,
    create: () => document.createElement('option'),
    update(choice, item) {
      choice.value = item.key
      choice.textContent = item.reference === null
        ? 'Choose an available reference'
        : `${item.reference.displayName} · ${item.reference.providerId}`
    },
  })
  const credentialRows = new WeakMap<HTMLLIElement, CredentialRow>()
  const credentialReferences = mountKeyedCollection({
    parent: references,
    key: (reference: CredentialReferenceProjection) => reference.id,
    create(reference: CredentialReferenceProjection) {
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
      const terms = [
        'Reference ID',
        'Provider ID',
        'Secret state',
        'Rotation version',
        'Updated',
        'Last rotated',
        'Revoked',
      ] as const
      const descriptions = terms.map(term => {
        const dt = document.createElement('dt')
        const dd = document.createElement('dd')
        dt.textContent = term
        metadata.append(dt, dd)
        return dd
      })
      rotateSecret.input.autocomplete = 'new-password'
      rotateSecret.input.spellcheck = false
      rotate.type = 'submit'
      rotate.textContent = 'Rotate secret'
      rotate.dataset.wwcComponent = 'button'
      rotate.dataset.variant = 'default'
      revoke.type = 'button'
      revoke.textContent = 'Revoke reference'
      revoke.dataset.wwcComponent = 'button'
      revoke.dataset.variant = 'destructive'
      const onRotate = (event: SubmitEvent) => {
        event.preventDefault()
        if (options.readOnly === true) return
        const row = credentialRows.get(item)
        if (row === undefined) return
        const secret = row.rotateSecret.value
        row.rotateSecret.value = ''
        void options.model.rotateCredentialReference({
          credentialReferenceId: row.current.id,
          vaultLocator: secret,
        })
      }
      const onRevoke = () => {
        if (options.readOnly === true) return
        const row = credentialRows.get(item)
        if (row !== undefined) void options.model.revokeCredentialReference(row.current.id)
      }
      rotateForm.addEventListener('submit', onRotate)
      revoke.addEventListener('click', onRevoke)
      rotateForm.append(rotateSecret.label, rotate)
      item.append(title, metadata, rotateForm, revoke)
      credentialRows.set(item, {
        current: reference,
        title,
        descriptions,
        rotateForm,
        rotateSecret: rotateSecret.input,
        rotate,
        revoke,
        onRotate,
        onRevoke,
      })
      return item
    },
    update(item, reference: CredentialReferenceProjection) {
      const row = credentialRows.get(item)
      if (row === undefined) return
      row.current = reference
      row.title.textContent = reference.displayName
      const values = [
        reference.id,
        reference.providerId,
        lifecycleLabel(reference),
        String(reference.rotationVersion),
        reference.updatedAt,
        reference.lastRotatedAt ?? 'Never',
        reference.revokedAt ?? 'No',
      ] as const
      values.forEach((value, index) => {
        const description = row.descriptions[index]
        if (description !== undefined && description.textContent !== value) {
          description.textContent = value
        }
      })
      const disabled = options.readOnly === true
        || settingsPagePresentation(options.model.state).mutationsDisabled
        || reference.secretState === 'revoked'
      row.rotate.disabled = disabled
      row.rotateSecret.disabled = disabled
      row.revoke.disabled = disabled
    },
    remove(item) {
      const row = credentialRows.get(item)
      if (row === undefined) return
      row.rotateSecret.value = ''
      row.rotateForm.removeEventListener('submit', row.onRotate)
      row.revoke.removeEventListener('click', row.onRevoke)
      credentialRows.delete(item)
    },
  })

  function render(state: SettingsViewModelState): void {
    if (closed) return
    const presentation = settingsPagePresentation(state)
    const route = state.settings?.defaultModelRoute ?? null
    const tone: StatusTone = presentation.errorText !== null
      ? 'danger'
      : state.realtime === 'reconnecting'
        ? 'warning'
        : presentation.busy
          ? 'info'
          : state.status === 'ready'
            ? 'success'
            : 'neutral'
    statusBadge.update({
      label: presentation.statusText,
      tone,
      live: 'polite',
      className: 'wwc-settings-status',
    })
    layout.setAttribute('aria-busy', String(presentation.busy))
    errorState.update({
      title: 'Provider settings unavailable',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-settings-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    if (!routeDirty) {
      const providerValue = route?.providerId ?? ''
      const modelValue = route?.modelId ?? ''
      const concurrencyValue = String(state.settings?.workerConcurrencyLimit ?? 1)
      const credentialValue = route?.credentialReferenceId ?? ''
      if (provider.input.value !== providerValue) provider.input.value = providerValue
      if (model.input.value !== modelValue) model.input.value = modelValue
      if (concurrency.input.value !== concurrencyValue) concurrency.input.value = concurrencyValue
    }
    credentialOptions.update([
      { key: '', reference: null },
      ...state.credentials
        .filter(reference => reference.secretState === 'available')
        .map(reference => ({ key: reference.id, reference })),
    ])
    if (!routeDirty) {
      const credentialValue = route?.credentialReferenceId ?? ''
      if (credential.value !== credentialValue) credential.value = credentialValue
    }
    const mutationsDisabled = options.readOnly === true || presentation.mutationsDisabled
    provider.input.disabled = mutationsDisabled
    model.input.disabled = mutationsDisabled
    credential.disabled = mutationsDisabled
    concurrency.input.disabled = mutationsDisabled
    saveRoute.disabled = mutationsDisabled
    clearRoute.disabled = mutationsDisabled || route === null
    createId.input.disabled = mutationsDisabled
    createName.input.disabled = mutationsDisabled
    createProvider.input.disabled = mutationsDisabled
    createSecret.input.disabled = mutationsDisabled
    createButton.disabled = mutationsDisabled
    credentialReferences.update(state.credentials)
    references.hidden = state.credentials.length === 0
    referencesEmpty.root.hidden = state.credentials.length !== 0
  }

  const onRouteSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.readOnly === true) return
    void options.model.updateSettings({
      defaultModelRoute: {
        providerId: provider.input.value,
        modelId: model.input.value,
        credentialReferenceId: credential.value as CredentialReferenceId,
      },
      workerConcurrencyLimit: Number(concurrency.input.value),
    }).then(() => {
      if (options.model.state.interaction.status !== 'error') routeDirty = false
    })
  }
  const onClearRoute = () => {
    if (options.readOnly === true) return
    void options.model.updateSettings({
      defaultModelRoute: null,
      workerConcurrencyLimit: Number(concurrency.input.value),
    }).then(() => {
      if (options.model.state.interaction.status !== 'error') routeDirty = false
    })
  }
  const onCreateCredential = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.readOnly === true) return
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
  }
  provider.input.addEventListener('input', markRouteDirty)
  model.input.addEventListener('input', markRouteDirty)
  credential.addEventListener('change', markRouteDirty)
  concurrency.input.addEventListener('input', markRouteDirty)
  routeForm.addEventListener('submit', onRouteSubmit)
  clearRoute.addEventListener('click', onClearRoute)
  createForm.addEventListener('submit', onCreateCredential)
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      provider.input.removeEventListener('input', markRouteDirty)
      model.input.removeEventListener('input', markRouteDirty)
      credential.removeEventListener('change', markRouteDirty)
      concurrency.input.removeEventListener('input', markRouteDirty)
      routeForm.removeEventListener('submit', onRouteSubmit)
      clearRoute.removeEventListener('click', onClearRoute)
      createForm.removeEventListener('submit', onCreateCredential)
      credentialReferences.close()
      credentialOptions.close()
      options.model.close()
      retryButton.close()
      reconnectButton.close()
      errorState.close()
      referencesEmpty.close()
      referencesPanel.close()
      createPanel.close()
      routePanel.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
    },
  }
}
