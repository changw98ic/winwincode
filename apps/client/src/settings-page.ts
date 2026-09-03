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
import {
  createEditableDraft,
  settleDraftSubmission,
  type EditableDraft,
} from './editable-draft.js'
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

/** ADR-0029 §5: every warning also carries a non-color icon beside its text. */
function conflictWarningIcon(document: Document, className: string): HTMLElement {
  const icon = element(document, 'span', className)
  icon.setAttribute('aria-hidden', 'true')
  icon.textContent = '!'
  return icon
}

/** Mount local Provider settings and write-only Credential reference controls. */
export function mountSettingsPage(options: SettingsPageOptions): SettingsPage {
  const document = options.root.ownerDocument
  const pageDraftScope = options.model.draftScope
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
  const routeConflict = element(document, 'div', 'wwc-settings-route-conflict')
  const routeConflictIcon = conflictWarningIcon(
    document,
    'wwc-settings-route-conflict-icon',
  )
  const routeConflictText = element(document, 'p', 'wwc-settings-route-conflict-text')
  const keepRouteDraft = element(document, 'button', 'wwc-settings-route-keep-draft')
  const useServerRoute = element(document, 'button', 'wwc-settings-route-use-server')

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
  routeConflict.setAttribute('role', 'alert')
  routeConflict.hidden = true
  keepRouteDraft.type = 'button'
  keepRouteDraft.textContent = 'Keep my draft'
  useServerRoute.type = 'button'
  useServerRoute.textContent = 'Use server values'
  routeConflict.append(routeConflictIcon, routeConflictText, keepRouteDraft, useServerRoute)
  routeControls.append(saveRoute, clearRoute)
  routeForm.append(
    provider.label,
    model.label,
    credentialLabel,
    concurrency.label,
    routeConflict,
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
    readonly conflict: HTMLElement
    readonly conflictText: HTMLElement
    readonly keepDraft: HTMLButtonElement
    readonly useServer: HTMLButtonElement
    readonly draft: EditableDraft<RotateDraftValues>
    readonly onRotate: (event: SubmitEvent) => void
    readonly onRevoke: () => void
    readonly onSecretInput: () => void
    readonly onKeepDraft: () => void
    readonly onUseServer: () => void
  }
  type CreateDraftValues = {
    readonly credentialReferenceId: string
    readonly displayName: string
    readonly providerId: string
  }
  type RotateDraftValues = {
    readonly secretState: string
    readonly rotationVersion: string
  }
  const createDraft = createEditableDraft<CreateDraftValues>()
  type RouteDraftValues = {
    readonly providerId: string
    readonly modelId: string
    readonly credentialReferenceId: string
    readonly workerConcurrencyLimit: string
  }
  const routeDraft = createEditableDraft<RouteDraftValues>()
  const routeFieldLabels: Readonly<Record<keyof RouteDraftValues, string>> = Object.freeze({
    providerId: 'Provider ID',
    modelId: 'Model ID',
    credentialReferenceId: 'Credential reference',
    workerConcurrencyLimit: 'Worker concurrency',
  })
  const editProvider = () => { routeDraft.edit('providerId', provider.input.value) }
  const editModel = () => { routeDraft.edit('modelId', model.input.value) }
  const editCredential = () => {
    routeDraft.edit('credentialReferenceId', credential.value)
  }
  const editConcurrency = () => {
    routeDraft.edit('workerConcurrencyLimit', concurrency.input.value)
  }
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
      const conflict = element(document, 'div', 'wwc-settings-rotate-conflict')
      const conflictIcon = conflictWarningIcon(
        document,
        'wwc-settings-rotate-conflict-icon',
      )
      const conflictText = element(document, 'p', 'wwc-settings-rotate-conflict-text')
      const keepDraft = element(document, 'button', 'wwc-settings-rotate-keep-draft')
      const useServer = element(document, 'button', 'wwc-settings-rotate-use-server')
      const draft = createEditableDraft<RotateDraftValues>({
        revisionSensitive: true,
        redactFields: ['secretState'],
      })
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
      conflict.setAttribute('role', 'alert')
      conflict.hidden = true
      keepDraft.type = 'button'
      keepDraft.textContent = 'Keep local secret'
      useServer.type = 'button'
      useServer.textContent = 'Discard local secret'
      conflict.append(conflictIcon, conflictText, keepDraft, useServer)
      const onRotate = (event: SubmitEvent) => {
        event.preventDefault()
        if (options.readOnly === true) return
        const row = credentialRows.get(item)
        if (row === undefined) return
        const secret = row.rotateSecret.value
        row.draft.edit('secretState', secret.length === 0 ? '' : 'present')
        const submission = row.draft.beginSubmission()
        if (submission === null) return
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
      const onSecretInput = () => {
        draft.edit('secretState', rotateSecret.input.value.length === 0 ? '' : 'present')
      }
      const onKeepDraft = () => {
        draft.resolveConflicts('keep-draft')
        render(options.model.state)
      }
      const onUseServer = () => {
        draft.resolveConflicts('use-server')
        rotateSecret.input.value = ''
        render(options.model.state)
      }
      rotateForm.addEventListener('submit', onRotate)
      revoke.addEventListener('click', onRevoke)
      rotateSecret.input.addEventListener('input', onSecretInput)
      keepDraft.addEventListener('click', onKeepDraft)
      useServer.addEventListener('click', onUseServer)
      rotateForm.append(rotateSecret.label, rotate)
      item.append(title, metadata, rotateForm, conflict, revoke)
      credentialRows.set(item, {
        current: reference,
        title,
        descriptions,
        rotateForm,
        rotateSecret: rotateSecret.input,
        rotate,
        revoke,
        conflict,
        conflictText,
        keepDraft,
        useServer,
        draft,
        onRotate,
        onRevoke,
        onSecretInput,
        onKeepDraft,
        onUseServer,
      })
      return item
    },
    update(item, reference: CredentialReferenceProjection) {
      const row = credentialRows.get(item)
      if (row === undefined) return
      row.current = reference
      const state = options.model.state
      if (row.draft.state.scope !== null && row.draft.state.submission === null) {
        row.draft.edit('secretState', row.rotateSecret.value.length === 0 ? '' : 'present')
      }
      if (state.interaction.error?.kind === 'cancelled') {
        row.draft.edit('secretState', '')
        row.rotateSecret.value = ''
      }
      row.draft.synchronize({
        scope: `${pageDraftScope}:${reference.id}`,
        revision: reference.revision,
        values: {
          secretState: '',
          rotationVersion: String(reference.rotationVersion),
        },
      })
      const rotateSubmission = row.draft.state.submission
      const rotateConfirmed = rotateSubmission !== null
        && reference.rotationVersion > Number(rotateSubmission.values.rotationVersion)
      const rotateRefuted = rotateSubmission !== null
        && reference.secretState === 'revoked'
      const rotateOutcome = settleDraftSubmission(rotateSubmission, {
        busy: settingsPagePresentation(state).busy,
        failed: state.interaction.status === 'error'
          && (
            state.interaction.operation === 'credential.reference.rotate'
            || state.interaction.operation === null
        ),
        cancelled: state.interaction.error?.kind === 'cancelled',
        confirmed: rotateConfirmed,
        refuted: rotateRefuted,
      })
      if (rotateOutcome !== 'in-flight') {
        row.draft.finishSubmission(rotateOutcome)
        if (rotateOutcome === 'success') row.rotateSecret.value = ''
      }
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
      const submissionPending = row.draft.state.submission !== null
      row.rotate.disabled = disabled || submissionPending || row.draft.state.revisionConflict
      row.rotateSecret.disabled = disabled || submissionPending
      row.revoke.disabled = disabled || submissionPending
      row.conflict.hidden = !row.draft.state.revisionConflict
      row.conflictText.textContent = row.draft.state.revisionConflict
        ? `This Credential reference changed from revision ${String(
            row.draft.state.baseRevision,
          )} to revision ${String(row.draft.state.serverRevision)}.`
        : ''
      row.keepDraft.disabled = disabled || submissionPending
      row.useServer.disabled = disabled || submissionPending
    },
    remove(item) {
      const row = credentialRows.get(item)
      if (row === undefined) return
      row.rotateSecret.value = ''
      row.rotateForm.removeEventListener('submit', row.onRotate)
      row.revoke.removeEventListener('click', row.onRevoke)
      row.rotateSecret.removeEventListener('input', row.onSecretInput)
      row.keepDraft.removeEventListener('click', row.onKeepDraft)
      row.useServer.removeEventListener('click', row.onUseServer)
      row.draft.reset()
      credentialRows.delete(item)
    },
  })

  function render(state: SettingsViewModelState): void {
    if (closed) return
    const presentation = settingsPagePresentation(state)
    const route = state.settings?.defaultModelRoute ?? null
    if (createDraft.state.scope !== null && createDraft.state.submission === null) {
      createDraft.edit('credentialReferenceId', createId.input.value)
      createDraft.edit('displayName', createName.input.value)
      createDraft.edit('providerId', createProvider.input.value)
    }
    if (state.interaction.error?.kind === 'cancelled') {
      createSecret.input.value = ''
    }
    createDraft.synchronize(state.settings === null
      ? null
      : {
          scope: `${pageDraftScope}:credential-reference-create`,
          revision: 0,
          values: {
            credentialReferenceId: '',
            displayName: '',
            providerId: '',
          },
        })
    const createSubmission = createDraft.state.submission
    const submittedReference = createSubmission === null
      ? undefined
      : state.credentials.find(reference => (
          reference.id === createSubmission.values.credentialReferenceId
        ))
    const createConfirmed = createSubmission !== null
      && submittedReference !== undefined
      && (
        submittedReference.id === createSubmission.values.credentialReferenceId
        && submittedReference.displayName === createSubmission.values.displayName.trim()
        && submittedReference.providerId === createSubmission.values.providerId.trim()
      )
    const createRefuted = createSubmission !== null
      && submittedReference !== undefined
      && !createConfirmed
    const createOutcome = settleDraftSubmission(createSubmission, {
      busy: presentation.busy,
      failed: state.interaction.status === 'error'
        && (
          state.interaction.operation === 'credential.reference.create'
          || state.interaction.operation === null
      ),
      cancelled: state.interaction.error?.kind === 'cancelled',
      confirmed: createConfirmed,
      refuted: createRefuted,
    })
    if (createOutcome !== 'in-flight') {
      createDraft.finishSubmission(createOutcome)
      if (createOutcome === 'success') createSecret.input.value = ''
    }
    routeDraft.synchronize(state.settings === null
      ? null
      : {
          scope: `${pageDraftScope}:settings`,
          revision: state.settings.revision,
          values: {
            providerId: route?.providerId ?? '',
            modelId: route?.modelId ?? '',
            credentialReferenceId: route?.credentialReferenceId ?? '',
            workerConcurrencyLimit: String(state.settings.workerConcurrencyLimit),
          },
        })
    const submittedRoute = routeDraft.state.submission
    const routeSubmissionSucceeded = submittedRoute !== null
      && state.settings !== null
      && state.settings.revision > submittedRoute.revision
      && (route?.providerId ?? '') === submittedRoute.values.providerId.trim()
      && (route?.modelId ?? '') === submittedRoute.values.modelId.trim()
      && (route?.credentialReferenceId ?? '') === submittedRoute.values.credentialReferenceId
      && String(state.settings.workerConcurrencyLimit)
        === submittedRoute.values.workerConcurrencyLimit
    const routeOutcome = settleDraftSubmission(submittedRoute, {
      busy: presentation.busy,
      failed: state.interaction.status === 'error'
        && (
          state.interaction.operation === 'settings.update'
          || state.interaction.operation === null
        ),
      cancelled: state.interaction.error?.kind === 'cancelled',
      confirmed: routeSubmissionSucceeded,
      refuted: submittedRoute !== null
        && state.settings !== null
        && state.settings.revision > submittedRoute.revision
        && !routeSubmissionSucceeded,
    })
    if (routeOutcome !== 'in-flight') routeDraft.finishSubmission(routeOutcome)
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
    credentialOptions.update([
      { key: '', reference: null },
      ...state.credentials
        .filter(reference => reference.secretState === 'available')
        .map(reference => ({ key: reference.id, reference })),
    ])
    const routeValues = routeDraft.state.values
    const routeProviderId = routeValues.providerId ?? ''
    const routeModelId = routeValues.modelId ?? ''
    const routeCredentialId = routeValues.credentialReferenceId ?? ''
    const routeConcurrency = routeValues.workerConcurrencyLimit ?? ''
    if (provider.input.value !== routeProviderId) provider.input.value = routeProviderId
    if (model.input.value !== routeModelId) model.input.value = routeModelId
    if (concurrency.input.value !== routeConcurrency) {
      concurrency.input.value = routeConcurrency
    }
    if (credential.value !== routeCredentialId) {
      credential.value = routeCredentialId
    }
    const routeConflicts = routeDraft.state.conflicts
    routeConflict.hidden = routeConflicts.length === 0
    routeConflictText.textContent = routeConflicts.length === 0
      ? ''
      : `The server changed this draft. ${routeConflicts.map(conflict => (
          `${routeFieldLabels[conflict.field as keyof RouteDraftValues]}: `
          + `server “${conflict.serverValue}”; your draft “${conflict.draftValue}”.`
        )).join(' ')}`
    const mutationsDisabled = options.readOnly === true || presentation.mutationsDisabled
    const routeSubmissionPending = routeDraft.state.submission !== null
    provider.input.disabled = mutationsDisabled || routeSubmissionPending
    model.input.disabled = mutationsDisabled || routeSubmissionPending
    credential.disabled = mutationsDisabled || routeSubmissionPending
    concurrency.input.disabled = mutationsDisabled || routeSubmissionPending
    saveRoute.disabled = mutationsDisabled
      || routeSubmissionPending
      || routeDraft.state.revisionConflict
    clearRoute.disabled = mutationsDisabled
      || routeSubmissionPending
      || route === null
      || routeDraft.state.revisionConflict
    keepRouteDraft.disabled = mutationsDisabled || routeSubmissionPending
    useServerRoute.disabled = mutationsDisabled || routeSubmissionPending
    const createSubmissionPending = createDraft.state.submission !== null
    createId.input.disabled = mutationsDisabled || createSubmissionPending
    createName.input.disabled = mutationsDisabled || createSubmissionPending
    createProvider.input.disabled = mutationsDisabled || createSubmissionPending
    createSecret.input.disabled = mutationsDisabled || createSubmissionPending
    createButton.disabled = mutationsDisabled || createSubmissionPending
    const createValues = createDraft.state.values
    if (createId.input.value !== (createValues.credentialReferenceId ?? '')) {
      createId.input.value = createValues.credentialReferenceId ?? ''
    }
    if (createName.input.value !== (createValues.displayName ?? '')) {
      createName.input.value = createValues.displayName ?? ''
    }
    if (createProvider.input.value !== (createValues.providerId ?? '')) {
      createProvider.input.value = createValues.providerId ?? ''
    }
    credentialReferences.update(state.credentials)
    references.hidden = state.credentials.length === 0
    referencesEmpty.root.hidden = state.credentials.length !== 0
  }

  const onRouteSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.readOnly === true) return
    const submission = routeDraft.beginSubmission()
    if (submission === null) {
      render(options.model.state)
      return
    }
    void options.model.updateSettings({
      defaultModelRoute: {
        providerId: submission.values.providerId,
        modelId: submission.values.modelId,
        credentialReferenceId: submission.values.credentialReferenceId as CredentialReferenceId,
      },
      workerConcurrencyLimit: Number(submission.values.workerConcurrencyLimit),
    })
  }
  const onClearRoute = () => {
    if (options.readOnly === true) return
    routeDraft.edit('providerId', '')
    routeDraft.edit('modelId', '')
    routeDraft.edit('credentialReferenceId', '')
    const submission = routeDraft.beginSubmission()
    if (submission === null) {
      render(options.model.state)
      return
    }
    void options.model.updateSettings({
      defaultModelRoute: null,
      workerConcurrencyLimit: Number(submission.values.workerConcurrencyLimit),
    })
  }
  const onKeepRouteDraft = () => {
    routeDraft.resolveConflicts('keep-draft')
    render(options.model.state)
  }
  const onUseServerRoute = () => {
    routeDraft.resolveConflicts('use-server')
    render(options.model.state)
  }
  const onCreateCredential = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.readOnly === true) return
    createDraft.edit('credentialReferenceId', createId.input.value)
    createDraft.edit('displayName', createName.input.value)
    createDraft.edit('providerId', createProvider.input.value)
    const secret = createSecret.input.value
    const submission = createDraft.beginSubmission()
    if (submission === null) return
    void options.model.createCredentialReference({
      credentialReferenceId: submission.values.credentialReferenceId as CredentialReferenceId,
      displayName: submission.values.displayName,
      providerId: submission.values.providerId,
      vaultLocator: secret,
    })
  }
  const onCreateIdInput = () => {
    createDraft.edit('credentialReferenceId', createId.input.value)
  }
  const onCreateNameInput = () => { createDraft.edit('displayName', createName.input.value) }
  const onCreateProviderInput = () => {
    createDraft.edit('providerId', createProvider.input.value)
  }
  provider.input.addEventListener('input', editProvider)
  model.input.addEventListener('input', editModel)
  credential.addEventListener('change', editCredential)
  concurrency.input.addEventListener('input', editConcurrency)
  routeForm.addEventListener('submit', onRouteSubmit)
  clearRoute.addEventListener('click', onClearRoute)
  keepRouteDraft.addEventListener('click', onKeepRouteDraft)
  useServerRoute.addEventListener('click', onUseServerRoute)
  createForm.addEventListener('submit', onCreateCredential)
  createId.input.addEventListener('input', onCreateIdInput)
  createName.input.addEventListener('input', onCreateNameInput)
  createProvider.input.addEventListener('input', onCreateProviderInput)
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      provider.input.removeEventListener('input', editProvider)
      model.input.removeEventListener('input', editModel)
      credential.removeEventListener('change', editCredential)
      concurrency.input.removeEventListener('input', editConcurrency)
      routeForm.removeEventListener('submit', onRouteSubmit)
      clearRoute.removeEventListener('click', onClearRoute)
      keepRouteDraft.removeEventListener('click', onKeepRouteDraft)
      useServerRoute.removeEventListener('click', onUseServerRoute)
      createForm.removeEventListener('submit', onCreateCredential)
      createId.input.removeEventListener('input', onCreateIdInput)
      createName.input.removeEventListener('input', onCreateNameInput)
      createProvider.input.removeEventListener('input', onCreateProviderInput)
      createSecret.input.value = ''
      credentialReferences.close()
      credentialOptions.close()
      routeDraft.reset()
      createDraft.reset()
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
