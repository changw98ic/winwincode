import { mountSettingsPage } from '/module/settings-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const credentialId = 'crd_00000000000000000000000001'

const reference = {
  id: credentialId,
  providerId: 'server-provider-a',
  displayName: 'Browser Credential',
  secretState: 'available',
  rotationVersion: 1,
  lastRotatedAt: null,
  revokedAt: null,
  revision: 1,
  updatedAt: '2026-09-02T12:00:00.000Z',
}

function state({ revision = 4, provider = 'server-provider-a', model = 'server-model-a',
  concurrency = 2, interaction = { status: 'idle', operation: null, error: null },
  status = 'ready', credentials = [reference] } = {}) {
  return {
    status,
    realtime: 'subscribed',
    settings: {
      revision,
      defaultModelRoute: {
        providerId: provider,
        modelId: model,
        credentialReferenceId: credentialId,
      },
      workerConcurrencyLimit: concurrency,
    },
    credentials,
    interaction,
    error: null,
  }
}

class BrowserSettingsModel {
  draftScope = '["browser-settings-actor","browser-settings-scope"]'
  state = state()
  calls = []

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async start() {}
  async refresh() {}
  async updateSettings(input) {
    this.calls.push(['updateSettings', structuredClone(input)])
    this.publish(state({
      revision: 5,
      provider: 'server-provider-b',
      model: 'server-model-b',
      concurrency: 3,
      interaction: { status: 'submitting', operation: 'settings.update', error: null },
    }))
  }
  async createCredentialReference(input) {
    this.calls.push(['createCredentialReference', structuredClone(input)])
    this.publish(state({
      revision: 6,
      provider: 'server-provider-c',
      model: 'server-model-c',
      concurrency: 4,
      interaction: {
        status: 'waiting',
        operation: 'credential.reference.create',
        error: null,
      },
    }))
  }
  async rotateCredentialReference() {}
  async revokeCredentialReference() {}
  cancelPending() {}
  reconnect() {}
  close() {}
}

const model = new BrowserSettingsModel()
const mounted = mountSettingsPage({ root, model })

function input(selector, value) {
  const control = document.querySelector(selector)
  control.value = value
  control.dispatchEvent(new Event('input', { bubbles: true }))
  return control
}

globalThis.runSettingsDraftStateScenario = () => {
  const provider = input('.wwc-settings-provider', 'browser-provider')
  provider.focus()
  provider.setSelectionRange(7, 7)
  model.publish(state({
    revision: 5,
    provider: 'server-provider-b',
    model: 'server-model-b',
    concurrency: 3,
  }))
  const conflict = {
    cleanModel: document.querySelector('.wwc-settings-model').value,
    cleanConcurrency: document.querySelector('.wwc-settings-concurrency').value,
    dirtyProvider: provider.value,
    focused: document.activeElement === provider,
    icon: document.querySelector(
      '.wwc-settings-route-conflict [aria-hidden="true"]',
    ) !== null,
    message: document.querySelector('.wwc-settings-route-conflict-text').textContent,
    visible: !document.querySelector('.wwc-settings-route-conflict').hidden,
  }

  document.querySelector('.wwc-settings-route-keep-draft').click()
  document.querySelector('.wwc-settings-route-form').requestSubmit()
  const submitted = model.calls.at(-1)
  provider.value = 'late-dom-change'
  provider.dispatchEvent(new Event('input', { bubbles: true }))
  model.publish(state({
    revision: 5,
    provider: 'server-provider-b',
    model: 'server-model-b',
    concurrency: 3,
    interaction: {
      status: 'error',
      operation: 'settings.update',
      error: {
        kind: 'network',
        code: 'NETWORK_FAILURE',
        message: 'offline',
        requestId: null,
        retryable: true,
      },
    },
  }))
  const failure = { retained: provider.value }
  model.publish(state({
    revision: 6,
    provider: 'server-provider-c',
    model: 'server-model-c',
    concurrency: 4,
  }))
  document.querySelector('.wwc-settings-route-use-server').click()
  const discarded = {
    concurrency: document.querySelector('.wwc-settings-concurrency').value,
    model: document.querySelector('.wwc-settings-model').value,
    provider: provider.value,
  }

  input('.wwc-settings-create-id', 'crd_00000000000000000000000002')
  input('.wwc-settings-create-name', 'Browser new Credential')
  input('.wwc-settings-create-provider', 'server-provider-c')
  const secret = input('.wwc-settings-create-secret', 'BROWSER_ONLY_SECRET')
  document.querySelector('.wwc-settings-create-form').requestSubmit()
  const firstCreateCall = model.calls.at(-1)
  model.publish(state({
    revision: 6,
    provider: 'server-provider-c',
    model: 'server-model-c',
    concurrency: 4,
    interaction: {
      status: 'error',
      operation: 'credential.reference.create',
      error: {
        kind: 'network',
        code: 'NETWORK_FAILURE',
        message: 'offline',
        requestId: null,
        retryable: true,
      },
    },
  }))
  const secretAfterFailure = secret.value
  model.publish(state({
    revision: 6,
    provider: 'server-provider-c',
    model: 'server-model-c',
    concurrency: 4,
    status: 'cancelled',
    interaction: {
      status: 'error',
      operation: null,
      error: {
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'cancelled',
        requestId: null,
        retryable: false,
      },
    },
  }))
  const secretAfterCancel = secret.value

  input('.wwc-settings-create-id', 'crd_00000000000000000000000003')
  input('.wwc-settings-create-name', 'Browser deferred Credential')
  input('.wwc-settings-create-provider', 'server-provider-c')
  const deferredSecret = input('.wwc-settings-create-secret', 'DEFERRED_BROWSER_SECRET')
  document.querySelector('.wwc-settings-create-form').requestSubmit()
  const deferredCall = model.calls.at(-1)
  model.publish({
    status: 'refreshing',
    realtime: 'reloading',
    settings: model.state.settings,
    credentials: [reference],
    interaction: { status: 'idle', operation: null, error: null },
    error: null,
  })
  const acceptedDuringReload = {
    createDisabled: document.querySelector('.wwc-settings-create-submit').disabled,
    secretRetained: deferredSecret.value,
  }
  const deferredReference = {
    ...reference,
    id: 'crd_00000000000000000000000003',
    displayName: 'Browser deferred Credential',
    providerId: 'server-provider-c',
    revision: 2,
    updatedAt: '2026-09-02T12:00:02.000Z',
  }
  model.publish(state({
    revision: 6,
    provider: 'server-provider-c',
    model: 'server-model-c',
    concurrency: 4,
    credentials: [deferredReference, reference],
  }))
  const acceptedConfirmed = {
    createCalls: model.calls.filter(([name]) => name === 'createCredentialReference').length,
    deferredCall,
    secretCleared: deferredSecret.value === '',
    storageClean: JSON.stringify(localStorage).includes('DEFERRED_BROWSER_SECRET') === false
      && JSON.stringify(sessionStorage).includes('DEFERRED_BROWSER_SECRET') === false,
  }
  return {
    acceptedConfirmed,
    acceptedDuringReload,
    conflict,
    discarded,
    failure,
    secret: {
      afterCancel: secretAfterCancel,
      afterFailure: secretAfterFailure,
      localStorage: JSON.stringify(localStorage),
      sessionStorage: JSON.stringify(sessionStorage),
      submitted: firstCreateCall,
    },
    submitted,
  }
}

globalThis.closeSettingsDraftStateFixture = () => { mounted.close() }
