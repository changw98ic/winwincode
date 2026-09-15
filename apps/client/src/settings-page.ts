// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError, ControlPlaneClientTransport } from './community-control-plane-client.js'
import {
  mountButton,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  type StatusTone,
} from '@winwincode/browser-ui'
import { mountEmptyState, mountTabs } from './components/index.js'
import { mountDeviceProviderPanel } from './device-provider-panel.js'
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
  readonly serverUrl: string
  readonly fetch?: ControlPlaneClientTransport['fetch']
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
    SETTINGS_CONCURRENCY_INVALID: '执行并发数必须在 1 到 10000 之间。',
    SETTINGS_PROVIDER_REQUIRED: '请输入服务商。',
    SETTINGS_MODEL_REQUIRED: '请输入模型。',
    SETTINGS_CREDENTIAL_ROUTE_INVALID: '请选择该服务商的 API Key。',
    SETTINGS_SNAPSHOT_REQUIRED: '请刷新设置后再保存。',
    SETTINGS_DECISION_IN_FLIGHT: '请等待当前设置更改完成。',
    SETTINGS_REVISION_REQUIRED: '请刷新设置后再提交此更改。',
    CREDENTIAL_DISPLAY_NAME_REQUIRED: '请输入凭据显示名称。',
    CREDENTIAL_PROVIDER_REQUIRED: '请输入服务商。',
    CREDENTIAL_SECRET_REQUIRED: '请填写 API Key。',
    CREDENTIAL_REFERENCE_STALE: '请刷新设置并重新选择 API Key。',
    INVALID_CLIENT_REQUEST: '请检查本地用户身份和工作区范围配置后重试。',
  })
  return labels[error.code] ?? null
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  const known = knownSettingsError(error)
  if (known !== null) return known
  if (error.code === 'REVISION_CONFLICT') {
    return '保存前设置已发生变化，请检查当前快照后重试。'
  }
  if (error.kind === 'authentication') return '请重新登录以管理本地模型服务商设置。'
  if (error.kind === 'authorization') return '你没有访问这些模型服务商设置的权限。'
  if (error.kind === 'network') return '无法连接设置服务器，请检查连接后重试。'
  if (error.kind === 'version') return '客户端与服务器版本不一致，请更新客户端后重试。'
  if (error.kind === 'cancelled') return '设置更新已取消。'
  if (error.kind === 'configuration') {
    return '请检查本地服务器地址和工作区范围配置后重试。'
  }
  return '模型服务商设置更新失败，请重试或检查服务器状态。'
}

export function settingsPagePresentation(
  state: SettingsViewModelState,
): SettingsPagePresentation {
  const visibleError = state.interaction.error ?? state.error
  const statusText = state.interaction.status === 'submitting'
    ? '正在保存模型服务商设置…'
    : state.interaction.status === 'waiting'
      ? '已接受更改，正在等待当前快照…'
      : state.status === 'loading'
        ? '正在加载模型服务商设置…'
        : state.status === 'refreshing' || state.realtime === 'reloading'
          ? '正在更新模型服务商设置…'
          : state.realtime === 'reconnecting'
            ? '正在重新连接…'
            : state.status === 'authentication-required'
              ? '需要登录'
              : state.status === 'authorization-denied'
                ? '访问被拒绝'
                : state.status === 'cancelled'
                  ? '更新已取消'
                  : state.status === 'error'
                    ? '模型服务商设置不可用'
                    : state.status === 'closed'
                      ? '模型服务商设置已关闭'
                      : state.settings === null
                        ? '没有设置快照'
                        : `就绪 · 修订版 ${String(state.settings.revision)}`
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
    label: '模型',
    title: '模型',
    description: '选择默认模型，添加服务商 API Key。',
  }),
  Object.freeze({
    id: 'execution',
    label: '执行与强流程',
    title: '执行与强流程',
    description: '对新委托的任务生效',
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
 * Device-owned Provider configuration controls.
 */
export function mountSettingsPage(options: SettingsPageOptions): SettingsPage {
  const document = options.root.ownerDocument
  const generalDraft = new Map<string, string>()
  const executionDraft = new Map<string, string>()
  const layout = element(document, 'section', 'wwc-settings')
  layout.dataset.wwcPage = 'management'
  let selectedCategory: SettingsCategoryId = 'providers'
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
      label: '正在加载模型服务商设置…',
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
      onActivate: () => { void options.model.refresh(); void deviceProviders.refresh() },
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

  // --- 模型与服务商(设计稿 13;现有控制面契约) ------------------------------

  const providersSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-providers',
  )
  providersSection.dataset.category = 'providers'
  providersSection.hidden = true

  let closed = false
  const deviceProviders = mountDeviceProviderPanel({ root: providersSection, serverUrl: options.serverUrl,
    ...(options.fetch === undefined ? {} : { fetch: options.fetch }),
    ...(options.readOnly === undefined ? {} : { readOnly: options.readOnly }) })

  // --- 执行与强流程(设计稿 14;本地草稿) -------------------------------------

  const executionSection = element(
    document,
    'section',
    'wwc-settings-category wwc-settings-execution',
  )
  executionSection.dataset.category = 'execution'
  executionSection.hidden = true
  const concurrencyForm = document.createElement('form')
  const concurrencyInput = document.createElement('input')
  concurrencyInput.type = 'number'; concurrencyInput.min = '1'; concurrencyInput.max = '10000'; concurrencyInput.required = true
  concurrencyInput.id = 'wwc-settings-worker-concurrency'
  const concurrencyLabel = document.createElement('label')
  concurrencyLabel.htmlFor = concurrencyInput.id; concurrencyLabel.textContent = '执行并发上限'
  const concurrencySave = document.createElement('button')
  concurrencySave.type = 'submit'; concurrencySave.textContent = '保存并发上限'
  concurrencyForm.append(concurrencyLabel, concurrencyInput, concurrencySave)
  concurrencyForm.addEventListener('submit', event => {
    event.preventDefault()
    if (options.readOnly !== true && !settingsPagePresentation(options.model.state).mutationsDisabled && concurrencyForm.reportValidity()) {
      void options.model.updateSettings({ workerConcurrencyLimit: Number(concurrencyInput.value) })
    }
  })
  executionSection.append(concurrencyForm)
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
      description: '备份内容：会话、任务记录与设置。',
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
  storageLastBackup.textContent = '上次备份：尚未创建'
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
  backupRestore.dataset.variant = 'ghost'
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
      onActivate: () => { void options.model.refresh(); void deviceProviders.refresh() },
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

  // 设计稿差异:仅「执行与强流程」(14)在标题下带副标题;12/13/15/16 均无。
  const CATEGORIES_WITH_DESCRIPTION: ReadonlySet<SettingsCategoryId> = new Set(['execution'])

  function showCategory(next: SettingsCategoryId): void {
    selectedCategory = next
    const category = categoryOf(next)
    pageHeader.update({
      title: category.title,
      ...(CATEGORIES_WITH_DESCRIPTION.has(next) ? { description: category.description } : {}),
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

  function renderDiagnostics(state: SettingsViewModelState, presentation: SettingsPagePresentation): void {
    const deviceOk = deviceProviders.online
    const deviceState = deviceOk ? '已连接' : '未连接'
    const modelOk = deviceProviders.configured
    const modelState = modelOk ? '已配置（请运行连接测试）' : '未配置'
    const concurrency = state.settings?.workerConcurrencyLimit ?? null
    if (document.activeElement !== concurrencyInput) concurrencyInput.value = concurrency === null ? '' : String(concurrency)
    concurrencyInput.disabled = options.readOnly === true || presentation.mutationsDisabled
    concurrencySave.disabled = concurrencyInput.disabled
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
      state.settings === null ? '尚未取得' : `修订版 ${String(state.settings.revision)}`
    }`
    recordsError.textContent = `异常:${presentation.errorText ?? '未记录到异常'}`
  }

  function render(state: SettingsViewModelState): void {
    if (closed) return
    const presentation = settingsPagePresentation(state)
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
    renderDiagnostics(state, presentation)
  }

  showCategory(selectedCategory)
  showDiagnosticsTab('run')
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      deviceProviders.close()
      categorySelect.removeEventListener('change', onCategoryChange)
      generalName.input.removeEventListener('input', onGeneralNameInput)
      diagnosticsTabs.close()
      runCheck.close()
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
