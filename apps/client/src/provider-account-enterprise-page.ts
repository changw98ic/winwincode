// SPDX-License-Identifier: Apache-2.0

import type {
  ProviderAccountConnectionId,
  ProviderAccountPoolId,
} from './generated/contracts.js'
import type {
  ProviderAccountViewModel,
  ProviderAccountViewModelState,
} from './provider-account-view-model.js'

const DEFAULT_MODELS = 'gpt-5.6-sol,gpt-5.6-terra,gpt-5.6-luna,gpt-5.5,gpt-5.4,gpt-5.4-mini,gpt-5.2'

export interface EnterpriseProviderAccountPageOptions {
  readonly root: HTMLElement
  readonly model: ProviderAccountViewModel
  readonly nextConnectionId: () => ProviderAccountConnectionId
  readonly nextPoolId: () => ProviderAccountPoolId
}

export interface EnterpriseProviderAccountPage { close(): void }

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function input(document: Document, labelText: string, type = 'text') {
  const label = element(document, 'label', 'wwc-enterprise-provider-label')
  const value = element(document, 'input', 'wwc-enterprise-provider-input')
  label.textContent = labelText
  value.type = type
  value.required = true
  label.append(value)
  return { label, value }
}

/** Mounts organization-owned ChatGPT accounts and deterministic account pools. */
export function mountEnterpriseProviderAccountPage(
  options: EnterpriseProviderAccountPageOptions,
): EnterpriseProviderAccountPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-enterprise-provider-accounts')
  const heading = element(document, 'h2', 'wwc-enterprise-provider-heading')
  const status = element(document, 'p', 'wwc-enterprise-provider-status')
  const error = element(document, 'p', 'wwc-enterprise-provider-error')
  const accountForm = element(document, 'form', 'wwc-enterprise-provider-account-form')
  const accountName = input(document, 'Organization account name')
  const addAccount = element(document, 'button', 'wwc-enterprise-provider-add-account')
  const accountList = element(document, 'ul', 'wwc-enterprise-provider-account-list')
  const poolHeading = element(document, 'h3', 'wwc-enterprise-provider-pool-heading')
  const poolForm = element(document, 'form', 'wwc-enterprise-provider-pool-form')
  const poolName = input(document, 'Pool name')
  const modelIds = input(document, 'Allowed model IDs (comma-separated)')
  const concurrency = input(document, 'Concurrent requests per account', 'number')
  const tokenLimit = input(document, 'Monthly tokens per account', 'number')
  const sourcePolicyLabel = element(document, 'label', 'wwc-enterprise-provider-label')
  const sourcePolicy = element(document, 'select', 'wwc-enterprise-provider-input')
  const accountChoices = element(document, 'fieldset', 'wwc-enterprise-provider-account-choices')
  const savePool = element(document, 'button', 'wwc-enterprise-provider-save-pool')
  const poolList = element(document, 'ul', 'wwc-enterprise-provider-pool-list')
  let closed = false
  let editingPool: ProviderAccountViewModelState['pools'][number] | null = null

  heading.textContent = 'Enterprise ChatGPT account pool'
  poolHeading.textContent = 'Account pools'
  status.setAttribute('role', 'status')
  error.setAttribute('role', 'alert')
  addAccount.type = 'submit'
  addAccount.textContent = 'Connect organization account'
  savePool.type = 'submit'
  savePool.textContent = 'Create pool'
  modelIds.value.value = DEFAULT_MODELS
  concurrency.value.min = '1'
  concurrency.value.value = '2'
  tokenLimit.value.min = '1'
  tokenLimit.value.value = '10000000'
  sourcePolicyLabel.textContent = 'Account source policy'
  for (const [value, text] of [
    ['enterprise_only', 'Require enterprise pool'],
    ['allow_personal_default_personal', 'Allow personal · prefer personal'],
    ['allow_personal_default_pool', 'Allow personal · prefer enterprise pool'],
  ] as const) {
    const option = element(document, 'option', '')
    option.value = value
    option.textContent = text
    sourcePolicy.append(option)
  }
  sourcePolicyLabel.append(sourcePolicy)
  accountForm.append(accountName.label, addAccount)
  poolForm.append(
    poolName.label,
    modelIds.label,
    concurrency.label,
    tokenLimit.label,
    sourcePolicyLabel,
    accountChoices,
    savePool,
  )
  section.append(
    heading,
    status,
    error,
    accountForm,
    accountList,
    poolHeading,
    poolForm,
    poolList,
  )
  options.root.replaceChildren(section)

  function render(state: ProviderAccountViewModelState): void {
    if (closed) return
    const organizationConnections = state.connections.filter(connection => (
      connection.owner.kind === 'organization'
      && connection.owner.organizationId === options.model.organizationId
    ))
    const activeConnections = organizationConnections.filter(connection => connection.state === 'active')
    status.textContent = state.status === 'loading'
      ? 'Loading organization accounts…'
      : state.submitting
        ? 'Saving organization account configuration…'
        : `${activeConnections.length} active account${activeConnections.length === 1 ? '' : 's'} · ${state.pools.length} pool${state.pools.length === 1 ? '' : 's'}`
    error.textContent = state.error === null
      ? ''
      : state.error.kind === 'authorization'
        ? 'Your enterprise role does not allow account-pool changes.'
        : 'The enterprise account-pool operation did not finish.'
    error.hidden = state.error === null
    addAccount.disabled = state.submitting
    savePool.disabled = state.submitting || activeConnections.length === 0
    savePool.textContent = editingPool === null ? 'Create pool' : 'Save pool'
    accountList.replaceChildren(...organizationConnections.map(connection => {
      const item = element(document, 'li', 'wwc-enterprise-provider-account-item')
      const text = element(document, 'span', 'wwc-enterprise-provider-account-text')
      text.textContent = `${connection.displayName} · ${connection.accountLabel ?? connection.state}`
      item.append(text)
      if (connection.loginPrompt !== null) {
        const link = element(document, 'a', 'wwc-enterprise-provider-login-link')
        const complete = element(document, 'button', 'wwc-enterprise-provider-complete')
        link.href = connection.loginPrompt.verificationUrl
        link.target = '_blank'
        link.rel = 'noopener noreferrer'
        link.textContent = `Open sign-in · code ${connection.loginPrompt.userCode}`
        complete.type = 'button'
        complete.textContent = 'Sign-in complete'
        complete.disabled = state.submitting
        complete.addEventListener('click', () => { void options.model.completeConnection(connection) })
        item.append(link, complete)
      }
      if (connection.state === 'active' || connection.state === 'refresh_required') {
        const refresh = element(document, 'button', 'wwc-enterprise-provider-refresh')
        refresh.type = 'button'
        refresh.textContent = 'Refresh sign-in'
        refresh.disabled = state.submitting
        refresh.addEventListener('click', () => { void options.model.refreshConnection(connection) })
        item.append(refresh)
      }
      if (connection.state !== 'revoked') {
        const disconnect = element(document, 'button', 'wwc-enterprise-provider-disconnect')
        disconnect.type = 'button'
        disconnect.textContent = 'Disconnect'
        disconnect.disabled = state.submitting
        disconnect.addEventListener('click', () => { void options.model.revokeConnection(connection) })
        item.append(disconnect)
      }
      return item
    }))
    const legend = element(document, 'legend', 'wwc-enterprise-provider-account-legend')
    legend.textContent = 'Accounts in this pool'
    accountChoices.replaceChildren(legend, ...activeConnections.map(connection => {
      const label = element(document, 'label', 'wwc-enterprise-provider-account-choice')
      const checkbox = element(document, 'input', 'wwc-enterprise-provider-account-checkbox')
      checkbox.type = 'checkbox'
      checkbox.value = connection.id
      checkbox.checked = editingPool === null
        || editingPool.accountConnectionIds.includes(connection.id)
      label.append(checkbox, document.createTextNode(connection.displayName))
      return label
    }))
    poolList.replaceChildren(...state.pools.map(pool => {
      const item = element(document, 'li', 'wwc-enterprise-provider-pool-item')
      const text = element(document, 'span', 'wwc-enterprise-provider-pool-text')
      text.textContent = `${pool.displayName} · ${pool.accountConnectionIds.length} accounts · ${pool.allowedModelIds.join(', ')} · ${pool.enabled ? 'enabled' : 'disabled'}`
      item.append(text)
      const edit = element(document, 'button', 'wwc-enterprise-provider-edit-pool')
      edit.type = 'button'
      edit.textContent = 'Edit pool'
      edit.disabled = state.submitting
      edit.addEventListener('click', () => {
        editingPool = pool
        poolName.value.value = pool.displayName
        modelIds.value.value = pool.allowedModelIds.join(',')
        concurrency.value.value = String(pool.maxConcurrentPerAccount)
        tokenLimit.value.value = String(pool.monthlyTokenLimitPerAccount)
        sourcePolicy.value = pool.sourcePolicy
        render(state)
      })
      item.append(edit)
      if (pool.enabled) {
        const disable = element(document, 'button', 'wwc-enterprise-provider-disable-pool')
        disable.type = 'button'
        disable.textContent = 'Disable pool'
        disable.disabled = state.submitting
        disable.addEventListener('click', () => { void options.model.disablePool(pool) })
        item.append(disable)
      }
      return item
    }))
  }

  accountForm.addEventListener('submit', event => {
    event.preventDefault()
    const displayName = accountName.value.value.trim()
    if (displayName.length === 0) return
    void options.model.startOrganizationConnection(options.nextConnectionId(), displayName).then(() => {
      if (options.model.state.error === null) accountName.value.value = ''
    })
  })
  poolForm.addEventListener('submit', event => {
    event.preventDefault()
    const selected = [...accountChoices.querySelectorAll<HTMLInputElement>('input:checked')]
      .map(choice => choice.value as ProviderAccountConnectionId)
    if (selected.length === 0) return
    const currentPool = editingPool
    void options.model.upsertPool({
      id: currentPool?.id ?? options.nextPoolId(),
      revision: currentPool?.revision ?? 0,
      displayName: poolName.value.value,
      accountConnectionIds: selected,
      allowedModelIds: modelIds.value.value.split(','),
      maxConcurrentPerAccount: Number(concurrency.value.value),
      monthlyTokenLimitPerAccount: Number(tokenLimit.value.value),
      sourcePolicy: sourcePolicy.value as (
        | 'enterprise_only'
        | 'allow_personal_default_personal'
        | 'allow_personal_default_pool'
      ),
    }).then(() => {
      if (options.model.state.error === null) {
        editingPool = null
        poolName.value.value = ''
        render(options.model.state)
      }
    })
  })
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
