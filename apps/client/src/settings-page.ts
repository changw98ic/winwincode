// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './community-control-plane-client.js'
import {
  mountButton,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  type StatusTone,
} from '@winwincode/browser-ui'
import { mountEmptyState, mountTabs } from './components/index.js'
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

/** One mounted Usage & health panel inside the 用量 tab. */
export interface SettingsUsagePanel {
  close(): void
}

export interface SettingsPageOptions {
  readonly root: HTMLElement
  readonly model: SettingsViewModel
  /**
   * Mounts the live Usage & health panel into the 用量 tab.  Wired by the
   * shell; invoked once when the tab is first opened, and the returned
   * binding is closed with the page.
   */
  readonly mountUsagePanel?: (
    root: HTMLElement,
  ) => Promise<SettingsUsagePanel | null> | SettingsUsagePanel | null
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
  if (error.kind === 'cancelled') return '设置更新已取消。'
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

// --- 本地草稿(无控制面契约的偏好) -------------------------------------------

type SettingsCategoryId = 'general' | 'providers' | 'execution' | 'storage' | 'diagnostics'

const SETTINGS_CATEGORIES: readonly {
  readonly id: SettingsCategoryId
  readonly label: string
  readonly title: string
  readonly description: string
}[] = Object.freeze([
  Object.freeze({
    id: 'general',
    label: '通用与个人',
    title: '通用与个人',
    description: '显示名称、界面语言、外观与发送偏好,保存后写入本地草稿。',
  }),
  Object.freeze({
    id: 'providers',
    label: '模型与 Provider',
    title: '模型与 Provider',
    description: '选择默认模型路由，管理只写一次的凭据引用。',
  }),
  Object.freeze({
    id: 'execution',
    label: '执行与强流程',
    title: '执行与强流程',
    description: '对新委托的任务生效。',
  }),
  Object.freeze({
    id: 'storage',
    label: '数据与存储',
    title: '数据与存储',
    description: '本地会话、任务记录与设置的备份、恢复与导出。',
  }),
  Object.freeze({
    id: 'diagnostics',
    label: '诊断与用量',
    title: '运行诊断与用量',
    description: '运行状态检查与用量概览。',
  }),
])

/** 设计稿 13:新会话默认模型下拉的“不指定”选项。 */
const FOLLOW_PROVIDER_DEFAULT = ''

function categoryOf(id: SettingsCategoryId): {
  readonly id: SettingsCategoryId
  readonly label: string
  readonly title: string
  readonly description: string
} {
  const found = SETTINGS_CATEGORIES.find(candidate => candidate.id === id)
  if (found === undefined) throw new Error(`unknown settings category: ${id}`)
  return found
}

/** 没有控制面契约的偏好只保留在页面内的本地草稿里,不进入网络状态。 */
const LOCAL_DRAFT_SAVED_TEXT = '已保存到本地草稿'
const NO_CONTRACT_TITLE = '暂不可用：等待控制面契约。'

function fillSelect(
  document: Document,
  select: HTMLSelectElement,
  options: readonly { readonly value: string; readonly label: string }[],
): void {
  for (const option of options) {
    const node = element(document, 'option', '')
    node.value = option.value
    node.textContent = option.label
    select.append(node)
  }
}

/**
 * Design pages 12-16 and 15: one settings page with a 设置分类 dropdown in the
 * top-right corner. 通用与个人 / 执行与强流程 / 数据与存储 / 诊断与用量 carry
 * presentation-only preferences stored as local drafts; 模型与 Provider keeps
 * the existing control-plane-backed route and Credential controls.
 */
export function mountSettingsPage(options: SettingsPageOptions): SettingsPage {
  const document = options.root.ownerDocument
  const pageDraftScope = options.model.draftScope
  const generalDraft = new Map<string, string>()
  const executionDraft = new Map<string, string>()
  const layout = element(document, 'section', 'wwc-settings')
  layout.dataset.wwcPage = 'management'
  let selectedCategory: SettingsCategoryId = 'general'
  const initialCategory = categoryOf(selectedCategory)

  // Design page 12: the display title stands alone — no status badge copy and
  // no description subtitle under it.
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: initialCategory.title,
      headingLevel: 2,
      className: 'wwc-settings-heading',
    },
  })
  const heading = pageHeader.root

  const categorySelect = element(document, 'select', 'wwc-settings-category-select')
  categorySelect.id = 'wwc-settings-category'
  categorySelect.setAttribute('aria-label', '设置分类')
  // 设计稿 12:收起态文案固定为「设置分类」(首选项),当前分类显示在页面标题里。
  const CATEGORY_SELECT_LABEL = '__label__'
  fillSelect(
    document,
    categorySelect,
    [
      { value: CATEGORY_SELECT_LABEL, label: '设置分类' },
      ...SETTINGS_CATEGORIES.map(category => ({
        value: category.id,
        label: category.label,
      })),
    ],
  )
  categorySelect.value = CATEGORY_SELECT_LABEL
  const headerActions = element(document, 'div', 'wwc-settings-header-actions')
  headerActions.append(categorySelect)
  const headerRow = element(document, 'div', 'wwc-settings-header')
  headerRow.append(heading, headerActions)

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
      label: '重试快照',
      className: 'wwc-settings-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: '重新连接事件流',
      className: 'wwc-settings-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: '模型设置不可用',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-settings-error',
    },
  })
  const error = errorState.root
  const errorText = errorState.message
  errorText.className = 'wwc-settings-error-text'

  /** 设计稿 12/14:左粗体标签 + 右控件的行式表单,行间发丝线。 */
  function settingsRow(
    className: string,
    labelText: string,
    control: HTMLElement,
    labelFor?: string,
  ): HTMLElement {
    const row = element(document, 'div', `wwc-settings-row ${className}`)
    const label = labelFor === undefined
      ? element(document, 'span', `${className}-label`)
      : element(document, 'label', `${className}-label`)
    label.textContent = labelText
    if (labelFor !== undefined) (label as HTMLLabelElement).htmlFor = labelFor
    row.append(label, control)
    return row
  }

  /** 折叠行:粗体标题 + ›,展开后显示说明性的空内容。 */
  function collapsedRow(
    id: string,
    labelText: string,
    content: HTMLElement,
    className: string,
  ): HTMLElement {
    const root = element(document, 'div', `wwc-settings-collapsed ${className}`)
    const button = element(document, 'button', 'wwc-settings-collapsed-button')
    button.type = 'button'
    button.id = `${id}-button`
    button.setAttribute('aria-expanded', 'false')
    button.setAttribute('aria-controls', `${id}-content`)
    // Design page 12: the disclosure chevron leads the row, before the label.
    const chevron = element(document, 'span', 'wwc-settings-collapsed-chevron')
    chevron.setAttribute('aria-hidden', 'true')
    chevron.textContent = '›'
    const label = element(document, 'span', 'wwc-settings-collapsed-label')
    label.textContent = labelText
    button.append(chevron, label)
    content.id = `${id}-content`
    content.hidden = true
    button.addEventListener('click', () => {
      const next = content.hidden
      content.hidden = !next
      button.setAttribute('aria-expanded', next ? 'true' : 'false')
    })
    root.append(button, content)
    return root
  }
  function localSaveRow(
    className: string,
    onSave: () => void,
  ): { readonly root: HTMLElement; readonly feedback: HTMLParagraphElement } {
    const root = element(document, 'div', `wwc-settings-local-save ${className}`)
    const save = element(document, 'button', `${className}-button`)
    save.type = 'button'
    save.dataset.wwcComponent = 'button'
    save.dataset.variant = 'primary'
    save.textContent = '保存设置'
    save.addEventListener('click', onSave)
    const feedback = element(document, 'p', `${className}-feedback`)
    feedback.setAttribute('role', 'status')
    root.append(save, feedback)
    return { root, feedback }
  }

  // --- 通用与个人(设计稿 12,默认分类) ---------------------------------------

  const generalSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-general',
  )
  generalSection.dataset.category = 'general'
  const generalName = labelledInput(
    document,
    'wwc-settings-general-name',
    '显示名称',
    'wwc-settings-general-name',
  )
  const generalLanguage = element(document, 'select', 'wwc-settings-general-language')
  generalLanguage.id = 'wwc-settings-general-language'
  fillSelect(document, generalLanguage, [
    { value: 'zh-Hans', label: '简体中文' },
    { value: 'en', label: 'English' },
  ])
  const generalAppearance = element(document, 'select', 'wwc-settings-general-appearance')
  generalAppearance.id = 'wwc-settings-general-appearance'
  fillSelect(document, generalAppearance, [
    { value: 'light', label: '浅色' },
    { value: 'dark', label: '深色' },
    { value: 'system', label: '跟随设备' },
  ])
  const generalSend = element(document, 'select', 'wwc-settings-general-send')
  generalSend.id = 'wwc-settings-general-send'
  fillSelect(document, generalSend, [
    { value: 'enter', label: 'Enter 发送' },
    { value: 'mod-enter', label: 'Cmd/Ctrl+Enter 发送' },
  ])
  const generalMoreContent = element(document, 'div', 'wwc-settings-general-more')
  const generalMoreEmpty = element(document, 'p', 'wwc-settings-general-more-empty')
  generalMoreEmpty.textContent = '其余偏好暂无可配置项。'
  generalMoreContent.append(generalMoreEmpty)
  generalName.input.value = generalDraft.get('displayName') ?? ''
  if (generalDraft.has('language')) generalLanguage.value = generalDraft.get('language') ?? ''
  if (generalDraft.has('appearance')) {
    generalAppearance.value = generalDraft.get('appearance') ?? ''
  }
  if (generalDraft.has('sendKey')) generalSend.value = generalDraft.get('sendKey') ?? ''
  const generalSave = localSaveRow('wwc-settings-general-save', () => {
    generalDraft.set('displayName', generalName.input.value)
    generalDraft.set('language', generalLanguage.value)
    generalDraft.set('appearance', generalAppearance.value)
    generalDraft.set('sendKey', generalSend.value)
    generalSave.feedback.textContent = LOCAL_DRAFT_SAVED_TEXT
  })
  const onGeneralNameInput = () => { generalSave.feedback.textContent = '' }
  generalName.input.addEventListener('input', onGeneralNameInput)
  generalSection.append(
    settingsRow(
      'wwc-settings-general-name-row',
      '显示名称',
      generalName.input,
      'wwc-settings-general-name',
    ),
    settingsRow(
      'wwc-settings-general-language-row',
      '界面语言',
      generalLanguage,
      'wwc-settings-general-language',
    ),
    settingsRow(
      'wwc-settings-general-appearance-row',
      '外观',
      generalAppearance,
      'wwc-settings-general-appearance',
    ),
    settingsRow(
      'wwc-settings-general-send-row',
      '发送方式',
      generalSend,
      'wwc-settings-general-send',
    ),
    collapsedRow(
      'wwc-settings-general-more',
      '其他偏好',
      generalMoreContent,
      'wwc-settings-general-more-row',
    ),
    generalSave.root,
  )

  // --- 模型与 Provider(设计稿 13;现有控制面契约) ----------------------------

  const providersSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-providers',
  )
  providersSection.dataset.category = 'providers'
  providersSection.hidden = true

  const routePanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-route',
      headingLevel: 3,
      title: '新会话默认模型',
      description: '更改后用于新创建的会话。',
      className: 'wwc-settings-route',
    },
  })
  const routeSection = routePanel.root
  const routeHeading = routePanel.title
  routeHeading.className = 'wwc-settings-section-heading'
  const defaultModel = element(document, 'select', 'wwc-settings-default-model')
  defaultModel.id = 'wwc-settings-default-model'
  const routeForm = element(document, 'form', 'wwc-settings-route-form')
  const provider = labelledInput(document, 'wwc-settings-provider', 'Provider ID', 'wwc-settings-provider')
  const model = labelledInput(document, 'wwc-settings-model', '模型 ID', 'wwc-settings-model')
  const credentialLabel = element(document, 'label', 'wwc-settings-credential-label')
  const credential = element(document, 'select', 'wwc-settings-credential')
  const concurrency = labelledInput(
    document,
    'wwc-settings-concurrency',
    'Worker 并发数',
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

  const providerListPanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-provider-list',
      headingLevel: 3,
      title: 'Provider 列表',
      description: '来自当前凭据引用;状态只反映本地密钥可用性。',
      className: 'wwc-settings-provider-list',
    },
  })
  const providerListSection = providerListPanel.root
  const providerListHeading = providerListPanel.title
  providerListHeading.className = 'wwc-settings-section-heading'
  const providerListRows = element(document, 'ul', 'wwc-settings-provider-rows')
  const providerListEmpty = element(document, 'p', 'wwc-settings-provider-list-empty')
  providerListEmpty.textContent = '尚未配置 Provider 凭据。添加凭据引用后显示在这里。'
  const addProvider = element(document, 'button', 'wwc-settings-add-provider')
  addProvider.type = 'button'
  addProvider.dataset.wwcComponent = 'button'
  addProvider.dataset.variant = 'primary'
  addProvider.textContent = '添加 Provider'

  const createPanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-create-credential',
      headingLevel: 3,
      title: '添加凭据引用',
      description: '本地密钥库定位符只提交一次，之后不再显示。',
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
      headingLevel: 3,
      title: '凭据引用',
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
      title: '暂无凭据引用',
      detail: 'Add a write-only Credential reference before choosing a default model route.',
      className: 'wwc-settings-credential-empty',
      headingLevel: 3,
    },
  })
  let closed = false


  credentialLabel.htmlFor = 'wwc-settings-credential'
  credentialLabel.textContent = '凭据引用'
  credential.id = 'wwc-settings-credential'
  credentialLabel.append(credential)
  concurrency.input.min = '1'
  concurrency.input.max = '10000'
  concurrency.input.step = '1'
  saveRoute.type = 'submit'
  saveRoute.textContent = '保存模型路由'
  saveRoute.dataset.wwcComponent = 'button'
  saveRoute.dataset.variant = 'primary'
  clearRoute.type = 'button'
  clearRoute.textContent = '清除默认路由'
  clearRoute.dataset.wwcComponent = 'button'
  clearRoute.dataset.variant = 'destructive'
  routeConflict.setAttribute('role', 'alert')
  routeConflict.hidden = true
  keepRouteDraft.type = 'button'
  keepRouteDraft.textContent = '保留我的草稿'
  useServerRoute.type = 'button'
  useServerRoute.textContent = '使用服务器值'
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
  routePanel.content.append(defaultModel, routeForm)

  // 设计稿 13:「添加 Provider」指向真实的添加凭据引用表单,不假造新增动作。
  addProvider.addEventListener('click', () => {
    createSection.scrollIntoView?.({ block: 'nearest' })
    if (createId.input.disabled !== true) createId.input.focus?.()
  })

  createHelp.textContent = '本地密钥库定位符只提交一次，之后不再显示。'
  createHelp.hidden = true
  createSecret.input.autocomplete = 'new-password'
  createSecret.input.spellcheck = false
  createButton.type = 'submit'
  createButton.textContent = '添加引用'
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

  referencesHelp.textContent = '仅显示不含敏感信息的生命周期元数据。'
  referencesHelp.hidden = true
  referencesPanel.content.append(referencesHelp, references, referencesEmpty.root)

  providersSection.append(routeSection, providerListSection, createSection, referencesSection)

  // --- 执行与强流程(设计稿 14;本地草稿) -------------------------------------

  const executionSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-execution',
  )
  executionSection.dataset.category = 'execution'
  executionSection.hidden = true
  const executionMode = element(document, 'select', 'wwc-settings-execution-mode')
  executionMode.id = 'wwc-settings-execution-mode'
  fillSelect(document, executionMode, [
    { value: 'strongflow', label: '强流程' },
    { value: 'chat', label: '对话内执行' },
  ])
  const executionReview = element(document, 'div', 'wwc-settings-execution-review')
  const executionReviewValue = element(document, 'span', 'wwc-settings-execution-review-value')
  executionReviewValue.textContent = '方案审核、交付验收'
  const executionReviewNote = element(document, 'span', 'wwc-settings-execution-review-note')
  executionReviewNote.textContent = '必经环节'
  executionReview.append(executionReviewValue, executionReviewNote)
  const executionScheduling = element(document, 'select', 'wwc-settings-execution-scheduling')
  executionScheduling.id = 'wwc-settings-execution-scheduling'
  fillSelect(document, executionScheduling, [
    { value: 'device', label: '跟随设备资源' },
    { value: 'fixed', label: '固定并发数' },
  ])
  const executionIsolation = element(document, 'div', 'wwc-settings-execution-isolation')
  const executionIsolationValue = element(document, 'span', 'wwc-settings-execution-isolation-value')
  executionIsolationValue.textContent = '独立工作树'
  const executionIsolationState = element(document, 'span', 'wwc-settings-execution-isolation-state')
  executionIsolationState.textContent = '已启用'
  executionIsolation.append(executionIsolationValue, executionIsolationState)
  const executionAdvancedContent = element(document, 'div', 'wwc-settings-execution-advanced')
  const executionAdvancedEmpty = element(document, 'p', 'wwc-settings-execution-advanced-empty')
  executionAdvancedEmpty.textContent = '执行权限由服务器策略控制,本地暂无可配置项。'
  executionAdvancedContent.append(executionAdvancedEmpty)
  if (executionDraft.has('taskMode')) executionMode.value = executionDraft.get('taskMode') ?? ''
  if (executionDraft.has('scheduling')) {
    executionScheduling.value = executionDraft.get('scheduling') ?? ''
  }
  const executionSave = localSaveRow('wwc-settings-execution-save', () => {
    executionDraft.set('taskMode', executionMode.value)
    executionDraft.set('scheduling', executionScheduling.value)
    executionSave.feedback.textContent = LOCAL_DRAFT_SAVED_TEXT
  })
  executionSection.append(
    settingsRow(
      'wwc-settings-execution-mode-row',
      '任务默认方式',
      executionMode,
      'wwc-settings-execution-mode',
    ),
    settingsRow('wwc-settings-execution-review-row', '审核节点', executionReview),
    settingsRow(
      'wwc-settings-execution-scheduling-row',
      '并发调度',
      executionScheduling,
      'wwc-settings-execution-scheduling',
    ),
    settingsRow('wwc-settings-execution-isolation-row', '任务隔离', executionIsolation),
    collapsedRow(
      'wwc-settings-execution-advanced',
      '权限与高级选项',
      executionAdvancedContent,
      'wwc-settings-execution-advanced-row',
    ),
    executionSave.root,
  )

  // --- 数据与存储(设计稿 16;控制面暂无备份/恢复/导出契约) --------------------

  const storagePanel = mountPanel({
    document,
    props: {
      id: 'wwc-settings-storage',
      headingLevel: 3,
      title: '备份与恢复',
      description: '备份内容:会话、任务记录与设置。',
      className: 'wwc-settings-storage',
    },
  })
  const storageSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-storage',
  )
  storageSection.dataset.category = 'storage'
  storageSection.hidden = true
  const storageLastBackup = element(document, 'p', 'wwc-settings-storage-last-backup')
  storageLastBackup.textContent = '上次备份:尚未创建'
  const storageActions = element(document, 'div', 'wwc-settings-storage-actions')
  const backupCreate = element(document, 'button', 'wwc-settings-backup-create')
  backupCreate.type = 'button'
  backupCreate.dataset.wwcComponent = 'button'
  backupCreate.dataset.variant = 'primary'
  backupCreate.textContent = '创建备份'
  backupCreate.disabled = true
  backupCreate.title = NO_CONTRACT_TITLE
  const backupRestore = element(document, 'button', 'wwc-settings-backup-restore')
  backupRestore.type = 'button'
  backupRestore.dataset.wwcComponent = 'button'
  backupRestore.dataset.variant = 'default'
  backupRestore.textContent = '从备份恢复'
  backupRestore.disabled = true
  backupRestore.title = NO_CONTRACT_TITLE
  storageActions.append(backupCreate, backupRestore)
  const exportRow = element(document, 'div', 'wwc-settings-row wwc-settings-export-row')
  const exportLabel = element(document, 'p', 'wwc-settings-export-label')
  exportLabel.textContent = '导出数据'
  const exportSelect = element(document, 'button', 'wwc-settings-export-select')
  exportSelect.type = 'button'
  exportSelect.dataset.wwcComponent = 'button'
  exportSelect.dataset.variant = 'default'
  exportSelect.textContent = '选择范围'
  exportSelect.disabled = true
  exportSelect.title = NO_CONTRACT_TITLE
  exportRow.append(exportLabel, exportSelect)
  const retentionContent = element(document, 'div', 'wwc-settings-retention')
  const retentionEmpty = element(document, 'p', 'wwc-settings-retention-empty')
  retentionEmpty.textContent = '暂无可配置的数据保留策略。'
  retentionContent.append(retentionEmpty)
  storagePanel.content.append(
    storageLastBackup,
    storageActions,
    exportRow,
    collapsedRow(
      'wwc-settings-retention',
      '数据保留与清理',
      retentionContent,
      'wwc-settings-retention-row',
    ),
  )
  storageSection.append(storagePanel.root)

  // --- 诊断与用量(设计稿 15) --------------------------------------------------

  const diagnosticsSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-diagnostics',
  )
  diagnosticsSection.dataset.category = 'diagnostics'
  diagnosticsSection.hidden = true

  const diagnosticsTabs = mountTabs({
    document,
    props: {
      id: 'wwc-settings-diagnostics-tabs',
      label: '诊断分类',
      tabs: [
        { id: 'run', label: '运行诊断', panelId: 'wwc-settings-diagnostics-run' },
        { id: 'usage', label: '用量', panelId: 'wwc-settings-diagnostics-usage' },
      ],
      selectedId: 'run',
      onSelect(id: string) {
        if (id === 'run' || id === 'usage') showDiagnosticsTab(id)
      },
    },
  })
  type DiagnosticsTabId = 'run' | 'usage'

  const runPanel = element(document, 'section', 'wwc-settings-diagnostics-run-panel')
  runPanel.id = 'wwc-settings-diagnostics-run'
  runPanel.setAttribute('role', 'tabpanel')
  runPanel.setAttribute('aria-label', '运行诊断')
  runPanel.tabIndex = -1
  const diagnosticsSummary = element(document, 'div', 'wwc-settings-diagnostics-summary')
  const diagnosticsSummaryIcon = element(
    document,
    'span',
    'wwc-settings-diagnostics-summary-icon',
  )
  diagnosticsSummaryIcon.setAttribute('aria-hidden', 'true')
  diagnosticsSummaryIcon.textContent = '✓'
  const diagnosticsSummaryText = element(
    document,
    'p',
    'wwc-settings-diagnostics-summary-text',
  )
  diagnosticsSummaryText.textContent = '当前运行正常'
  diagnosticsSummary.append(diagnosticsSummaryIcon, diagnosticsSummaryText)
  const diagnosticsRows = element(document, 'ul', 'wwc-settings-diagnostics-rows')
  const DIAGNOSTIC_ROW_LABELS = Object.freeze({
    device: '设备连接',
    model: '模型连接',
    tasks: '任务调度',
  } as const)
  type DiagnosticRowKey = keyof typeof DIAGNOSTIC_ROW_LABELS
  const diagnosticRowNodes = new Map<DiagnosticRowKey, {
    readonly item: HTMLLIElement
    readonly state: HTMLElement
  }>()
  for (const [key, label] of Object.entries(DIAGNOSTIC_ROW_LABELS) as [DiagnosticRowKey, string][]) {
    const item = element(document, 'li', 'wwc-settings-diagnostics-row')
    item.dataset.row = key
    const name = element(document, 'span', 'wwc-settings-diagnostics-row-name')
    name.textContent = label
    const state = element(document, 'span', 'wwc-settings-diagnostics-row-state')
    item.append(name, state)
    diagnosticsRows.append(item)
    diagnosticRowNodes.set(key, { item, state })
  }
  const runCheck = mountButton({
    document,
    props: {
      label: '运行检查',
      variant: 'primary',
      className: 'wwc-settings-diagnostics-run-check',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const recordsContent = element(document, 'div', 'wwc-settings-diagnostics-records')
  const recordsList = element(document, 'ul', 'wwc-settings-diagnostics-records-list')
  const recordsStatus = element(document, 'li', 'wwc-settings-diagnostics-record')
  recordsStatus.textContent = '状态:—'
  const recordsRevision = element(document, 'li', 'wwc-settings-diagnostics-record')
  recordsRevision.textContent = '快照:—'
  const recordsError = element(document, 'li', 'wwc-settings-diagnostics-record')
  recordsError.textContent = '异常:未记录到异常'
  recordsList.append(recordsStatus, recordsRevision, recordsError)
  recordsContent.append(recordsList)
  runPanel.append(
    diagnosticsSummary,
    diagnosticsRows,
    runCheck.root,
    collapsedRow(
      'wwc-settings-diagnostics-records',
      '诊断记录',
      recordsContent,
      'wwc-settings-diagnostics-records-row',
    ),
  )

  const usagePanel = element(document, 'section', 'wwc-settings-diagnostics-usage-panel')
  usagePanel.id = 'wwc-settings-diagnostics-usage'
  usagePanel.setAttribute('role', 'tabpanel')
  usagePanel.setAttribute('aria-label', '用量')
  usagePanel.tabIndex = -1
  usagePanel.hidden = true
  // Design page 15: the 用量 tab hosts the one live Usage/Provider/Worker
  // health summary panel.  The shell mounts it (lazily, on first open) so the
  // settings route only pays for the usage projection when it is viewed.
  const usageSlot = element(document, 'div', 'wwc-settings-usage-slot')
  usagePanel.append(usageSlot)
  let usageBinding: SettingsUsagePanel | null = null
  let usageMountStarted = false
  const onUsageFirstOpen = (): void => {
    if (usageMountStarted || options.mountUsagePanel === undefined) return
    usageMountStarted = true
    void Promise.resolve(options.mountUsagePanel(usageSlot)).then(binding => {
      if (binding === null) return
      if (closed) {
        binding.close()
        return
      }
      usageBinding = binding
    }).catch(() => {
      // The tab keeps its empty slot when only this panel fails to mount.
    })
  }

  diagnosticsSection.append(diagnosticsTabs.root, runPanel, usagePanel)

  function showDiagnosticsTab(next: DiagnosticsTabId): void {
    diagnosticsTabs.update({
      id: 'wwc-settings-diagnostics-tabs',
      label: '诊断分类',
      tabs: [
        { id: 'run', label: '运行诊断', panelId: 'wwc-settings-diagnostics-run' },
        { id: 'usage', label: '用量', panelId: 'wwc-settings-diagnostics-usage' },
      ],
      selectedId: next,
      onSelect(id: string) {
        if (id === 'run' || id === 'usage') showDiagnosticsTab(id)
      },
    })
    runPanel.hidden = next !== 'run'
    usagePanel.hidden = next !== 'usage'
    if (next === 'usage') onUsageFirstOpen()
  }

  function showCategory(next: SettingsCategoryId): void {
    selectedCategory = next
    const category = categoryOf(next)
    pageHeader.update({
      title: category.title,
      headingLevel: 2,
      className: 'wwc-settings-heading',
    })
    // 收起态文案固定为「设置分类」,不回写分类名。
    categorySelect.value = CATEGORY_SELECT_LABEL
    for (const candidate of SETTINGS_CATEGORIES) {
      const section = candidate.id === 'general'
        ? generalSection
        : candidate.id === 'providers'
          ? providersSection
          : candidate.id === 'execution'
            ? executionSection
            : candidate.id === 'storage' ? storageSection : diagnosticsSection
      section.hidden = candidate.id !== next
    }
  }
  const onCategoryChange = () => {
    const next = categorySelect.value as SettingsCategoryId
    if (SETTINGS_CATEGORIES.some(candidate => candidate.id === next)) showCategory(next)
    // 切换完成后收起态回到「设置分类」占位文案。
    categorySelect.value = CATEGORY_SELECT_LABEL
  }
  categorySelect.addEventListener('change', onCategoryChange)

  layout.append(headerRow, status, error, generalSection, providersSection, executionSection, storageSection, diagnosticsSection)
  options.root.replaceChildren(layout)

  interface CredentialChoice {
    readonly key: string
    readonly reference: CredentialReferenceProjection | null
  }
  interface CredentialRow {
    current: CredentialReferenceProjection
    readonly item: HTMLLIElement
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
    modelId: '模型 ID',
    credentialReferenceId: 'Credential reference',
    workerConcurrencyLimit: 'Worker 并发数',
  })
  const editProvider = () => { routeDraft.edit('providerId', provider.input.value) }
  const editModel = () => { routeDraft.edit('modelId', model.input.value) }
  const editCredential = () => {
    routeDraft.edit('credentialReferenceId', credential.value)
  }
  const editConcurrency = () => {
    routeDraft.edit('workerConcurrencyLimit', concurrency.input.value)
  }
  /** 设计稿 13:下拉与表单绑定同一个路由草稿;选择凭据时带出其 Provider。 */
  const editDefaultModel = () => {
    routeDraft.edit('credentialReferenceId', defaultModel.value)
    const match = options.model.state.credentials.find(
      reference => reference.id === defaultModel.value,
    )
    if (match !== undefined) routeDraft.edit('providerId', match.providerId)
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
  const defaultModelOptions = mountKeyedCollection<CredentialChoice, string, HTMLOptionElement>({
    parent: defaultModel,
    key: choice => choice.key,
    create: () => document.createElement('option'),
    update(choice, item) {
      choice.value = item.key
      choice.textContent = item.reference === null
        ? '跟随 Provider 默认'
        : `${item.reference.displayName} · ${item.reference.providerId}`
    },
  })
  const providerManageRows = new WeakMap<HTMLLIElement, {
    readonly name: HTMLElement
    readonly provider: HTMLElement
    readonly state: HTMLElement
    readonly stateText: HTMLElement
    readonly manage: HTMLButtonElement
    readonly onManage: () => void
  }>()
  const providerRowsCollection = mountKeyedCollection<
    CredentialReferenceProjection,
    string,
    HTMLLIElement
  >({
    parent: providerListRows,
    key: reference => reference.id,
    create(reference: CredentialReferenceProjection) {
      const item = element(document, 'li', 'wwc-settings-provider-row')
      const info = element(document, 'div', 'wwc-settings-provider-info')
      const name = element(document, 'p', 'wwc-settings-provider-name')
      const providerId = element(document, 'p', 'wwc-settings-provider-id')
      info.append(name, providerId)
      const state = element(document, 'p', 'wwc-settings-provider-state')
      const dot = element(document, 'span', 'wwc-settings-provider-dot')
      dot.setAttribute('aria-hidden', 'true')
      const stateText = element(document, 'span', 'wwc-settings-provider-state-text')
      state.append(dot, stateText)
      const manage = element(document, 'button', 'wwc-settings-provider-manage')
      manage.type = 'button'
      manage.dataset.wwcComponent = 'button'
      manage.dataset.variant = 'default'
      manage.textContent = '管理'
      const onManage = () => {
        const items = references.children ?? []
        for (const candidate of items) {
          if (candidate.getAttribute?.('data-reference-id') === reference.id) {
            candidate.scrollIntoView?.({ block: 'nearest' })
            return
          }
        }
      }
      manage.addEventListener('click', onManage)
      item.append(info, state, manage)
      providerManageRows.set(item, {
        name,
        provider: providerId,
        state,
        stateText,
        manage,
        onManage,
      })
      return item
    },
    update(item, reference: CredentialReferenceProjection) {
      const row = providerManageRows.get(item)
      if (row === undefined) return
      row.name.textContent = reference.displayName
      row.provider.textContent = reference.providerId
      const connected = reference.secretState === 'available'
      const revoked = reference.secretState === 'revoked'
      item.dataset.tone = connected ? 'success' : revoked ? 'danger' : 'neutral'
      row.state.dataset.tone = connected ? 'success' : revoked ? 'danger' : 'neutral'
      row.stateText.textContent = connected ? '已连接' : revoked ? '已吊销' : '未配置'
      row.manage.textContent = connected ? '管理' : '配置'
    },
    remove(item) {
      const row = providerManageRows.get(item)
      if (row === undefined) return
      row.manage.removeEventListener('click', row.onManage)
      providerManageRows.delete(item)
    },
  })
  const credentialRows = new WeakMap<HTMLLIElement, CredentialRow>()
  const credentialReferences = mountKeyedCollection({
    parent: references,
    key: (reference: CredentialReferenceProjection) => reference.id,
    create(reference: CredentialReferenceProjection) {
      const item = element(document, 'li', 'wwc-settings-credential-item')
      item.dataset.referenceId = reference.id
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
      rotate.textContent = '轮换密钥'
      rotate.dataset.wwcComponent = 'button'
      rotate.dataset.variant = 'default'
      revoke.type = 'button'
      revoke.textContent = '吊销引用'
      revoke.dataset.wwcComponent = 'button'
      revoke.dataset.variant = 'destructive'
      conflict.setAttribute('role', 'alert')
      conflict.hidden = true
      keepDraft.type = 'button'
      keepDraft.textContent = '保留本地密钥'
      useServer.type = 'button'
      useServer.textContent = '丢弃本地密钥'
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
        item,
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

  function renderDiagnostics(state: SettingsViewModelState, presentation: SettingsPagePresentation): void {
    const connectedRealtime = state.realtime === 'subscribed'
      || state.realtime === 'reloading'
    const deviceOk = connectedRealtime
    const deviceState = connectedRealtime
      ? '已连接'
      : state.realtime === 'reconnecting'
        ? '重连中'
        : state.realtime === 'access-revoked'
          ? '访问已撤销'
          : '未连接'
    const modelOk = state.settings !== null
      && (state.status === 'ready' || state.status === 'refreshing')
    const modelState = modelOk
      ? '可用'
      : state.status === 'authentication-required'
        ? '需登录'
        : state.status === 'authorization-denied'
          ? '无权限'
          : state.status === 'error'
            ? '不可用'
            : '读取中'
    const concurrency = state.settings?.workerConcurrencyLimit ?? null
    const tasksOk = concurrency !== null
    const tasksState = tasksOk ? `正常 · 并发上限 ${String(concurrency)}` : '未知'
    const allOk = deviceOk && modelOk && tasksOk
    diagnosticsSummaryIcon.textContent = allOk ? '✓' : '!'
    diagnosticsSummary.dataset.tone = allOk ? 'success' : 'warning'
    diagnosticsSummaryText.textContent = allOk ? '当前运行正常' : '需要关注'
    const states: Readonly<Record<DiagnosticRowKey, readonly [boolean, string]>> = Object.freeze({
      device: [deviceOk, deviceState],
      model: [modelOk, modelState],
      tasks: [tasksOk, tasksState],
    })
    for (const [key, [ok, text]] of Object.entries(states) as [DiagnosticRowKey, readonly [boolean, string]][]) {
      const row = diagnosticRowNodes.get(key)
      if (row === undefined) continue
      row.item.dataset.tone = ok ? 'success' : 'warning'
      row.state.textContent = `${ok ? '🟢' : '⚠'} ${text}`
    }
    runCheck.update({
      label: '运行检查',
      variant: 'primary',
      className: 'wwc-settings-diagnostics-run-check',
      busy: presentation.busy,
    })
    recordsStatus.textContent = `状态:${presentation.statusText}`
    recordsRevision.textContent = `快照:${
      state.settings === null ? '尚未取得' : `revision ${String(state.settings.revision)}`
    }`
    recordsError.textContent = `异常:${presentation.errorText ?? '未记录到异常'}`
  }

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
      title: '模型设置不可用',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-settings-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    const credentialChoices: readonly CredentialChoice[] = [
      { key: '', reference: null },
      ...state.credentials
        .filter(reference => reference.secretState === 'available')
        .map(reference => ({ key: reference.id, reference })),
    ]
    credentialOptions.update(credentialChoices)
    defaultModelOptions.update(credentialChoices)
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
    if (defaultModel.value !== routeCredentialId) defaultModel.value = routeCredentialId
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
    defaultModel.disabled = mutationsDisabled || routeSubmissionPending
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
    providerRowsCollection.update(state.credentials)
    providerListRows.hidden = state.credentials.length === 0
    providerListEmpty.hidden = state.credentials.length !== 0
    renderDiagnostics(state, presentation)
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
  defaultModel.addEventListener('change', editDefaultModel)
  concurrency.input.addEventListener('input', editConcurrency)
  routeForm.addEventListener('submit', onRouteSubmit)
  clearRoute.addEventListener('click', onClearRoute)
  keepRouteDraft.addEventListener('click', onKeepRouteDraft)
  useServerRoute.addEventListener('click', onUseServerRoute)
  createForm.addEventListener('submit', onCreateCredential)
  createId.input.addEventListener('input', onCreateIdInput)
  createName.input.addEventListener('input', onCreateNameInput)
  createProvider.input.addEventListener('input', onCreateProviderInput)
  showCategory(selectedCategory)
  showDiagnosticsTab('run')
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
      defaultModel.removeEventListener('change', editDefaultModel)
      concurrency.input.removeEventListener('input', editConcurrency)
      categorySelect.removeEventListener('change', onCategoryChange)
      routeForm.removeEventListener('submit', onRouteSubmit)
      clearRoute.removeEventListener('click', onClearRoute)
      keepRouteDraft.removeEventListener('click', onKeepRouteDraft)
      useServerRoute.removeEventListener('click', onUseServerRoute)
      createForm.removeEventListener('submit', onCreateCredential)
      createId.input.removeEventListener('input', onCreateIdInput)
      createName.input.removeEventListener('input', onCreateNameInput)
      createProvider.input.removeEventListener('input', onCreateProviderInput)
      createSecret.input.value = ''
      generalName.input.removeEventListener('input', onGeneralNameInput)
      providerRowsCollection.close()
      credentialReferences.close()
      credentialOptions.close()
      defaultModelOptions.close()
      diagnosticsTabs.close()
      runCheck.close()
      referencesEmpty.close()
      referencesPanel.close()
      providerListPanel.close()
      createPanel.close()
      routePanel.close()
      storagePanel.close()
      usageBinding?.close()
      statusBadge.close()
      retryButton.close()
      reconnectButton.close()
      errorState.close()
      pageHeader.close()
      options.root.replaceChildren()
    },
  }
}
