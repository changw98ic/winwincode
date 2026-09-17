// SPDX-License-Identifier: Apache-2.0

import { mountChatAttachments, showChatAttachment } from './chat-attachments.js'
import { readPreferences } from './preferences.js'

import { repositoryDisplayName } from './display-labels.js'
import { mountChatMarkdown } from './dsh-ui.js'

import type {
  ChatViewModel,
  ChatViewModelState,
} from './chat-view-model.js'
import type { ControlPlaneRepositorySummary, ControlPlaneClientError } from './community-control-plane-client.js'
import { mountButton } from '@winwincode/browser-ui'
import { mountFormField } from './components/form-field.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import type { HomeDeliveryListViewModel } from './home-dashboard-view-model.js'
import type {
  ModelRouteAvailabilityProjection,
  ProductSessionId,
  RepositoryScope,
} from './generated/contracts.js'
import { scopeHash } from '@winwincode/browser-core/scope-context'
import {
  ModelRouteAvailabilityReason,
  ModelRouteAvailabilityStatus,
} from './generated/contracts.js'
import {
  contextualDecisionPresentation,
  contextualDecisions,
} from './contextual-decision-view-model.js'
import {
  mountContextualDecisionCard,
  type ContextualDecisionCard,
} from './contextual-decision.js'
import type {
  ChatDeliveryCreateInput,
  ChatDeliveryCreator,
  ChatDeliveryCreatorState,
} from './chat-delivery-creator.js'

export type {
  ChatDeliveryCreateInput,
  ChatDeliveryCreator,
  ChatDeliveryCreatorState,
} from './chat-delivery-creator.js'

export interface ChatPageOptions {
  readonly root: HTMLElement
  readonly model: ChatViewModel
  readonly listDeviceRepositories?: (clientId: string) => Promise<readonly ControlPlaneRepositorySummary[]>
  readonly project?: { readonly clientId: string; readonly deviceName: string; readonly repository: ControlPlaneRepositorySummary }
  readonly onProjectChange?: (clientId: string, repositoryBindingId: string) => void
  readonly nextProductSessionId?: () => ProductSessionId
  readonly deliveryCreator?: ChatDeliveryCreator
  readonly deliveries?: HomeDeliveryListViewModel
  readonly scope?: RepositoryScope
  readonly settingsHref?: string
  /** Deterministic clock for the contextual decision card; defaults to Date.now. */
  readonly nowMillis?: () => number
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface ChatPage {
  close(): void
}

export interface ChatComposerKey {
  readonly key: string
  readonly ctrlKey?: boolean
  readonly metaKey?: boolean
  readonly shiftKey: boolean
  readonly isComposing: boolean
}

export type ChatComposerKeyAction = 'submit' | 'newline' | 'ignore'

export interface ChatPagePresentation {
  readonly statusText: string
  readonly emptyText: string
  readonly errorText: string | null
  readonly composerLabel: string
  readonly composerPlaceholder: string
  readonly sendLabel: string
  readonly messageListBusy: boolean
  readonly composerDisabled: boolean
  readonly cancelVisible: boolean
}

export function chatComposerKeyAction(key: ChatComposerKey, sendKey: 'enter' | 'mod-enter' = 'enter'): ChatComposerKeyAction {
  if (key.key !== 'Enter') return 'ignore'
  if (key.isComposing || key.shiftKey) return 'newline'
  if (sendKey === 'mod-enter' && !key.ctrlKey && !key.metaKey) return 'newline'
  return 'submit'
}

function stateLabel(state: ChatViewModelState): string {
  if (state.status === 'loading') return '正在加载对话…'
  if (state.status === 'refreshing') return '正在更新对话…'
  if (state.status === 'authentication-required') return '需要登录'
  if (state.status === 'authorization-denied') return '没有访问权限'
  if (state.status === 'cancelled') return '更新已取消'
  if (state.status === 'error') return '对话不可用'
  if (state.status === 'closed') return '对话已关闭'
  if (state.realtime === 'reconnecting') return '正在重新连接…'
  if (state.interaction.status === 'submitting') return '正在发送消息…'
  if (state.interaction.status === 'cancelling') return '正在停止运行…'
  if (state.interaction.status === 'waiting') return '等待服务器…'
  if (
    state.session === null
    && !readyModelRoutes(state).length
  ) return state.modelRouteAvailability?.reason === ModelRouteAvailabilityReason.NoProvider
    ? '需要先配置模型'
    : '模型路由不可用'
  if (state.session === null) return state.selectedModelRoute === null
    ? '选择一个模型路由'
    : '可以开始新对话'
  const sessionState = state.session?.state
  if (sessionState === 'running') return '运行中'
  if (sessionState === 'waiting_for_input') return '等待输入'
  if (sessionState === 'waiting_for_approval') return '等待批准'
  if (sessionState === 'cancelled') return '已取消'
  if (sessionState === 'closed') return '已完成'
  if (sessionState === 'failed') return '失败'
  if (sessionState === 'idle') return '就绪'
  return '选择一个会话'
}

function modelRouteReady(candidate: ModelRouteAvailabilityProjection): boolean {
  return candidate.status === ModelRouteAvailabilityStatus.Available
    && candidate.reason === ModelRouteAvailabilityReason.Ready
}

function readyModelRoutes(
  state: ChatViewModelState,
): readonly ModelRouteAvailabilityProjection[] {
  return state.modelRouteAvailability?.items.filter(modelRouteReady) ?? []
}

function modelRouteReasonLabel(reason: ModelRouteAvailabilityReason): string {
  if (reason === ModelRouteAvailabilityReason.Ready) return '就绪'
  if (reason === ModelRouteAvailabilityReason.RateLimited) return '速率受限'
  if (reason === ModelRouteAvailabilityReason.WindowExhausted) return '用量窗口已用尽'
  if (reason === ModelRouteAvailabilityReason.WeeklyExhausted) return '周用量已用尽'
  if (reason === ModelRouteAvailabilityReason.AuthenticationError) return '模型服务商认证失败'
  if (reason === ModelRouteAvailabilityReason.RuntimeStatusUnknown) return '模型服务商状态未知'
  if (reason === ModelRouteAvailabilityReason.NoProvider) return '没有可用的模型服务商'
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return '凭据缺失或已撤销'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) {
    return '默认模型路由无效'
  }
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return '模型服务商或模型已停用'
  }
  return '请求池不可用'
}

function modelRouteIdentity(route: ModelRouteAvailabilityProjection['route']): string {
  return `${route.providerId}\u0000${route.modelId}\u0000${route.credentialReferenceId}`
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  if (error.code === 'CAPACITY_EXHAUSTED') return '设备正在处理其他对话，请等待它结束后重试。'
  if (error.code === 'CHAT_DEVICE_REQUIRED') return '请选择执行设备上的项目。'
  if (error.code === 'IDEMPOTENCY_CONFLICT') {
    return '本次新对话请求与之前的请求冲突，请重新开一个新对话。'
  }
  if (error.code === 'INVALID_REQUEST') {
    return '所选模型在该仓库不可用，请换一个模型后重试。'
  }
  if (error.code === 'WRONG_STATE') {
    return '该对话标识已被占用，请重新开一个新对话。'
  }
  if (error.code === 'SERVICE_UNAVAILABLE') {
    return '模型请求池或所选模型暂时不可用，请稍后重试。'
  }
  if (error.code === 'TRUSTED_FACTS_UNAVAILABLE') {
    return '配置的模型服务商或模型不可用，请先检查设置再重试。'
  }
  if (error.kind === 'authentication') return '请重新登录以继续该对话。'
  if (error.kind === 'authorization') return '你没有这个对话的访问权限。'
  if (error.kind === 'network') return '无法连接对话服务器，请检查网络后重试。'
  if (error.kind === 'version') return '客户端与服务器版本不一致，请更新客户端后重试。'
  if (error.kind === 'cancelled') return '对话更新已取消。'
  if (error.kind === 'configuration') return '对话需要有效的服务器配置。'
  return '对话更新失败，请重试或检查服务器状态。'
}

function modelRouteEmptyText(state: ChatViewModelState): string {
  const reason = state.modelRouteAvailability?.reason
  if (reason === ModelRouteAvailabilityReason.RateLimited) {
    return '所选模型服务商受到速率限制，请稍后重试或选择其他模型。'
  }
  if (reason === ModelRouteAvailabilityReason.WindowExhausted) {
    return '所选模型服务商的用量窗口已用尽，请在窗口重置后重试。'
  }
  if (reason === ModelRouteAvailabilityReason.WeeklyExhausted) {
    return '所选模型服务商的每周用量已用尽，请在下周重置后重试。'
  }
  if (reason === ModelRouteAvailabilityReason.AuthenticationError) {
    return '所选模型服务商拒绝了凭据，请在设置中轮换或替换凭据。'
  }
  if (reason === ModelRouteAvailabilityReason.RuntimeStatusUnknown) {
    return '所选模型服务商状态未知，请重试或选择其他路由。'
  }
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return '配置的模型凭据缺失或已被撤销。请检查设置。'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) {
    return '默认模型路由无效。请检查设置。'
  }
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return '配置的模型服务商或模型已停用，请检查设置。'
  }
  if (reason === ModelRouteAvailabilityReason.RequestPoolUnavailable) {
    return '所选模型请求池不可用。请重试或检查设置。'
  }
  return '没有可用的模型服务商，因此未配置模型路由。请先打开设置再创建对话。'
}

export function chatPagePresentation(state: ChatViewModelState): ChatPagePresentation {
  const error = state.interaction.error ?? state.messagePagination.error ?? state.error
  const running = state.session?.state === 'running'
  const continuing = state.session?.state === 'waiting_for_input'
  const mutationBusy = ['submitting', 'cancelling'].includes(state.interaction.status)
  return Object.freeze({
    statusText: stateLabel(state),
    emptyText: state.session === null
      ? readyModelRoutes(state).length > 0
        ? '先创建第一个对话，开始交流。'
        : modelRouteEmptyText(state)
      : '还没有消息。发送一条消息开始对话。',
    errorText: errorLabel(error),
    composerLabel: running
      ? '引导当前运行'
      : continuing
        ? '继续对话'
        : '给 WinWinCode 发消息',
    composerPlaceholder: state.session === null
      ? '描述你的想法…'
      : '继续当前对话…',
    sendLabel: running ? '引导' : continuing ? '继续' : '发送',
    messageListBusy: state.status === 'loading'
      || state.status === 'refreshing'
      || state.realtime === 'reloading',
    composerDisabled: mutationBusy
      || state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed',
    cancelVisible: running,
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

function messageStateText(state: string): string | null {
  if (state === 'streaming') return '正在输出'
  if (state === 'cancelled') return '已停止'
  if (state === 'failed') return '失败'
  return null
}

function confirmedRequirement(state: ChatViewModelState): string | null {
  return [...state.messages].reverse().find(message => (
    message.role === 'user'
    && message.state === 'completed'
    && message.content.trim().length > 0
  ))?.content.trim() ?? null
}

function deliveryConversionError(state: ChatDeliveryCreatorState): string | null {
  const error = state.error
  if (error === null) return null
  if (error.code.startsWith('STRONGFLOW_CREATE_')) return error.message
  if (error.kind === 'authentication') return '创建此交付前请重新登录。'
  if (error.kind === 'authorization') {
    return '你没有在此仓库中创建交付的权限。'
  }
  if (error.kind === 'network') {
    return '无法连接强流程服务器，已确认的对话草稿仍保留在此处。'
  }
  if (error.kind === 'cancelled') {
    return '交付创建已取消，已确认的对话草稿仍保留在此处。'
  }
  if (error.code === 'REVISION_CONFLICT') {
    return '强流程启动前交付已发生变化，请重试同一份已确认草稿。'
  }
  return '无法创建交付，已确认的对话草稿仍保留在此处，请重试。'
}

const CONVERSION_DIALOG_HEADING_ID = 'wwc-chat-convert-heading'

function focusableElement(value: Element | null | undefined): HTMLElement | null {
  if (value === null || typeof value !== 'object') return null
  return typeof Reflect.get(value, 'focus') === 'function'
    ? value as HTMLElement
    : null
}

/** Mount the default, keyboard-accessible Chat page against the read/write view-model only. */
export function mountChatPage(options: ChatPageOptions): ChatPage {
  const readOnly = options.readOnly === true
  const nowMillis = options.nowMillis ?? Date.now
  const document = options.root.ownerDocument
  const scopedHref = (href: string): string => options.scope === undefined ? href : scopeHash(href, options.scope)
  const layout = element(document, 'div', 'wwc-chat')
  const conversation = element(document, 'section', 'wwc-chat-conversation')
  // Design pages 03a/03b: the page header carries only the delegation entry
  // chip; the 「winwincode ∨」 project dropdown at the top left is the shell's
  // Scope selector, not a second Chat-owned control.
  const header = element(document, 'header', 'wwc-chat-page-header')
  const projectContext = element(document, 'a', 'wwc-chat-project-context')
  projectContext.href = scopedHref('#/projects')
  const heading = element(document, 'h2', 'wwc-chat-heading')
  const status = element(document, 'p', 'wwc-chat-status')
  // Design page 03b: the delegation entry is the accent chip on the top right.
  const delegationChip = mountButton({
    document,
    props: {
      className: 'wwc-chat-delegation-chip',
      label: '委托任务 … ∨',
      type: 'button',
      variant: 'primary',
      onActivate: () => {
        decisionRoot.hidden = !decisionRoot.hidden
        decisionRoot.setAttribute('aria-hidden', String(decisionRoot.hidden))
      },
    },
  })
  const modelLabel = element(document, 'label', 'wwc-chat-model-label')
  const modelSelect = element(document, 'select', 'wwc-chat-model')
  const modelSettings = element(document, 'a', 'wwc-chat-model-settings')
  const modelNotice = element(document, 'p', 'wwc-chat-model-notice')
  const error = element(document, 'div', 'wwc-chat-error')
  const errorText = element(document, 'span', 'wwc-chat-error-text')
  const retry = element(document, 'button', 'wwc-chat-retry')
  const messages = element(document, 'ol', 'wwc-chat-messages')
  const empty = element(document, 'div', 'wwc-chat-empty')
  const loadEarlier = element(document, 'button', 'wwc-chat-load-earlier')
  const decisionRoot = element(document, 'div', 'wwc-chat-decisions')
  const delegationPanel = element(document, 'section', 'wwc-chat-delegations')
  delegationPanel.hidden = true
  delegationPanel.setAttribute('aria-label', '当前对话的委托任务')
  const delegationStatus = element(document, 'p', 'wwc-chat-delegations-status')
  delegationStatus.setAttribute('role', 'status')
  const delegationList = element(document, 'ul', 'wwc-chat-delegations-list')
  const delegationRefresh = element(document, 'button', 'wwc-chat-delegations-refresh')
  delegationRefresh.type = 'button'
  delegationRefresh.textContent = '刷新任务'
  delegationRefresh.addEventListener('click', () => { void options.deliveries?.refresh() })
  const newDelegation = mountButton({ document, props: { className: 'wwc-chat-new-delegation', label: '新建委托', type: 'button' } })
  delegationPanel.id = 'wwc-chat-delegations'
  delegationPanel.append(delegationStatus, delegationList, delegationRefresh, newDelegation.root)
  decisionRoot.hidden = true
  decisionRoot.setAttribute('aria-hidden', 'true')
  const repositorySelect = document.createElement('select')
  repositorySelect.id = 'wwc-chat-device-repository'
  repositorySelect.setAttribute('aria-label', '设备上的项目')
  const repositoryStatus = document.createElement('span')
  repositoryStatus.setAttribute('role', 'status')
  let repositoryDevice: string | undefined
  let repositoryGeneration = 0
  let deviceRepositories: readonly ControlPlaneRepositorySummary[] = []
  async function loadDeviceRepositories(clientId: string | undefined): Promise<void> {
    if (clientId === repositoryDevice) return
    repositoryDevice = clientId
    const generation = ++repositoryGeneration
    repositorySelect.replaceChildren()
    repositorySelect.disabled = true
    if (clientId === undefined || options.listDeviceRepositories === undefined) return
    repositoryStatus.textContent = '正在读取设备项目…'
    try {
      const repositories = await options.listDeviceRepositories(clientId)
      if (closed || generation !== repositoryGeneration) return
      deviceRepositories = repositories
      for (const repository of repositories.filter(item => item.availability === 'available' || item.availability === 'dirty')) {
        const option = document.createElement('option')
        option.value = repository.repositoryBindingId
        option.textContent = repositoryDisplayName(repository.displayName, repository.repositoryBindingId, document.defaultView)
        repositorySelect.append(option)
      }
      if (options.project !== undefined) repositorySelect.value = options.project.repository.repositoryBindingId
      repositorySelect.disabled = readOnly || options.project !== undefined || repositorySelect.children.length === 0
      repositoryStatus.textContent = repositorySelect.children.length === 0 ? '请先在设备中添加并授权项目。' : ''
      render(options.model.state)
    } catch {
      if (!closed && generation === repositoryGeneration) repositoryStatus.textContent = '无法读取设备项目，请刷新。'
    }
  }
  const form = element(document, 'form', 'wwc-chat-composer')
  const composerLabel = element(document, 'label', 'wwc-chat-composer-label')
  const composer = element(document, 'textarea', 'wwc-chat-composer-input')
  const emptyMessage = element(document, 'p', 'wwc-chat-empty-message')
  const starterList = element(document, 'div', 'wwc-chat-starters')
  const starters = [
    ['修复一个问题', '帮我定位并修复一个问题'],
    ['实现一个功能', '帮我实现一个功能'],
    ['解释一段代码', '请解释这段代码的作用'],
  ] as const
  for (const [label, value] of starters) {
    const button = element(document, 'button', 'wwc-chat-starter')
    button.type = 'button'
    button.textContent = label
    button.addEventListener('click', () => {
      composer.value = value
      resizeComposer()
      composer.focus()
      onComposerInput()
    })
    starterList.append(button)
  }
  empty.append(emptyMessage, starterList)
  const controls = element(document, 'div', 'wwc-chat-composer-controls')
  const attach = element(document, 'button', 'wwc-chat-composer-attach')
  const cancel = element(document, 'button', 'wwc-chat-cancel')
  const send = element(document, 'button', 'wwc-chat-send')
  const receipt = element(document, 'p', 'wwc-chat-delegation-receipt')
  const receiptText = element(document, 'span', 'wwc-chat-delegation-receipt-text')
  const receiptLink = element(document, 'a', 'wwc-chat-delegation-receipt-link')
  const conversion = element(document, 'section', 'wwc-chat-convert')
  const conversionHeading = element(document, 'h3', 'wwc-chat-convert-heading')
  const conversionDetail = element(document, 'p', 'wwc-chat-convert-detail')
  const conversionForm = element(document, 'form', 'wwc-chat-convert-form')
  const conversionTitle = element(document, 'input', 'wwc-chat-convert-title')
  const conversionGoal = element(document, 'textarea', 'wwc-chat-convert-goal')
  const conversionSourceSession = element(
    document,
    'input',
    'wwc-chat-convert-source-session',
  )
  const conversionScope = element(document, 'input', 'wwc-chat-convert-scope')
  const conversionModel = element(document, 'input', 'wwc-chat-convert-model')
  const conversionBaseline = element(document, 'input', 'wwc-chat-convert-baseline')
  const conversionDeliveryScope = element(document, 'textarea', 'wwc-chat-convert-delivery-scope')
  const conversionOutOfScope = element(document, 'textarea', 'wwc-chat-convert-out-of-scope')
  const conversionConstraints = element(document, 'textarea', 'wwc-chat-convert-constraints')
  const conversionCriteria = element(document, 'textarea', 'wwc-chat-convert-criteria')
  const conversionVerification = element(document, 'input', 'wwc-chat-convert-verification')
  conversionVerification.required = true
  conversionVerification.placeholder = '例如：npm test'
  const confirmationLabel = element(document, 'label', 'wwc-chat-convert-confirm-label')
  const confirmation = element(document, 'input', 'wwc-chat-convert-confirm')
  const confirmationText = element(document, 'span', 'wwc-chat-convert-confirm-text')
  const conversionError = element(document, 'p', 'wwc-chat-convert-error')
  const conversionSubmit = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-submit',
      label: '确认并创建交付',
      type: 'submit',
      variant: 'primary',
    },
  })
  const conversionCancel = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-cancel',
      label: '取消转换',
      type: 'button',
    },
  })
  let closed = false
  let followMessages = true
  const onMessagesScroll = () => {
    followMessages = messages.scrollHeight - messages.clientHeight - messages.scrollTop < 48
  }
  messages.addEventListener('scroll', onMessagesScroll)
  const messageObserver = new MutationObserver(() => {
    if (followMessages && !closed) messages.scrollTop = messages.scrollHeight
  })
  messageObserver.observe(messages, { childList: true, characterData: true, subtree: true })
  let conversionOpen = false
  let conversionSessionId: ProductSessionId | null = null
  let conversionFocusReturn: HTMLElement | null = null

  // UI-502: the Session's own pending inputs and approvals, decided in place
  // through this page's view-model commands. The card is a projection of this
  // page's snapshot, so it
  // cannot drift from the state the rest of the page renders.
  // The card mounts into this detached root, so a hidden card adds no node to
  // the conversation and the page layout stays byte-identical when idle.
  const decisionCard: ContextualDecisionCard = mountContextualDecisionCard({
    root: decisionRoot,
    id: 'wwc-chat-decisions',
    title: '此对话中的决策',
    description: '不离开对话即可答复会话输入或批准工具调用。',
    readOnly,
    actions: {
      provideInput(item, value) {
        if (readOnly) return
        void options.model.respondToInput(item.id, 'provided', value)
      },
      cancelInput(item) {
        if (readOnly) return
        void options.model.respondToInput(item.id, 'cancelled', null)
      },
      decideApproval(item, decision, reason) {
        if (readOnly) return
        void options.model.decideApproval(item.id, decision, reason)
      },
    },
  })

  heading.textContent = '新对话'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  // 设计稿 03a:模型选择即下拉本身,不再渲染可见的静态标签。
  modelLabel.textContent = ''
  modelLabel.htmlFor = 'wwc-chat-model'
  modelSelect.setAttribute('aria-label', '默认模型')
  modelSelect.id = 'wwc-chat-model'
  modelSettings.href = options.settingsHref ?? '#/settings'
  modelSettings.textContent = '在设置中查看模型路由'
  modelNotice.setAttribute('role', 'status')
  modelNotice.setAttribute('aria-live', 'polite')
  modelNotice.hidden = true
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  error.hidden = true
  retry.type = 'button'
  retry.textContent = '重试'
  messages.setAttribute('aria-label', '对话消息')
  messages.setAttribute('aria-live', 'polite')
  messages.setAttribute('aria-relevant', 'additions text')
  loadEarlier.type = 'button'
  loadEarlier.textContent = '加载更早的消息'
  composerLabel.htmlFor = 'wwc-chat-composer'
  composer.id = 'wwc-chat-composer'
  composer.rows = 1
  composer.autocomplete = 'off'
  composer.placeholder = '描述你的想法…'
  attach.type = 'button'
  attach.disabled = readOnly
  attach.textContent = '+'
  attach.setAttribute('aria-label', '添加图片或文件')
  attach.title = '添加图片、文本或代码文件，也可粘贴图片或拖入文件'
  cancel.type = 'button'
  cancel.textContent = '停止'
  send.type = 'submit'
  // Design page 03a: the composer sends through an accent square carrying the
  // paper-plane glyph; the spoken name stays the state-dependent action label.
  // 源码安全扫描禁止本文件出现字面 URL;SVG 命名空间是 W3C 固定常量,
  // 在运行时拼装。
  const svgNs = ['http', '://', 'www.w3.org/2000/svg'].join('')
  const sendIcon = typeof document.createElementNS === 'function'
    ? (() => {
        const svg = document.createElementNS(svgNs, 'svg')
        svg.setAttribute('viewBox', '0 0 24 24')
        svg.setAttribute('fill', 'currentColor')
        svg.setAttribute('aria-hidden', 'true')
        svg.setAttribute('width', '18')
        svg.setAttribute('height', '18')
        const path = document.createElementNS(svgNs, 'path')
        path.setAttribute('d', 'M3 11.5 21 3l-8.5 18-2.3-7.2z')
        svg.append(path)
        return svg
      })()
    : null
  if (sendIcon !== null) send.append(sendIcon)
  else send.textContent = '➤'
  send.setAttribute('aria-label', '发送')
  receipt.hidden = true
  receiptLink.href = scopedHref('#/home')
  receiptLink.textContent = '查看'
  receipt.append(receiptText, receiptLink)

  error.append(errorText, retry)
  modelLabel.append(modelSelect)
  controls.append(attach, modelLabel, repositorySelect, repositoryStatus, cancel, send)
  form.append(composerLabel, composer, controls, modelNotice, modelSettings)
  header.append(projectContext, delegationChip.root, delegationPanel)
  conversion.hidden = true
  // UI-604: the panel is a dialog in fact but was announced as plain page content,
  // opened without moving focus, and could only be dismissed with the pointer.
  conversion.setAttribute('role', 'dialog')
  conversion.setAttribute('aria-modal', 'false')
  conversionHeading.id = CONVERSION_DIALOG_HEADING_ID
  conversion.setAttribute('aria-labelledby', CONVERSION_DIALOG_HEADING_ID)
  delegationChip.root.setAttribute('aria-controls', delegationPanel.id)
  newDelegation.root.setAttribute('aria-controls', CONVERSION_DIALOG_HEADING_ID)
  newDelegation.root.setAttribute('aria-expanded', 'false')
  delegationChip.root.setAttribute('aria-expanded', 'false')
  conversionHeading.textContent = '确认 StrongFlow 交付'
  conversionDetail.textContent = '创建交付前，请核对已确认的需求与确切的仓库上下文。'
  conversionTitle.type = 'text'
  conversionTitle.required = true
  conversionGoal.required = true
  conversionSourceSession.type = 'text'
  conversionSourceSession.readOnly = true
  conversionScope.type = 'text'
  conversionScope.readOnly = true
  conversionModel.type = 'text'
  conversionModel.readOnly = true
  conversionBaseline.type = 'text'
  conversionBaseline.required = true
  conversionDeliveryScope.required = true
  conversionCriteria.required = true
  confirmation.type = 'checkbox'
  confirmationText.textContent = '我已确认该目标与仓库范围。'
  confirmationLabel.append(confirmation, confirmationText)
  conversionError.setAttribute('role', 'alert')
  conversionError.setAttribute('aria-live', 'assertive')
  const conversionFields = [
    mountFormField({
      document,
      props: { id: 'chat-convert-title', label: '交付标题', control: conversionTitle, required: true },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-goal', label: '已确认目标', control: conversionGoal, required: true },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-session', label: '来源对话', control: conversionSourceSession },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-scope', label: '仓库范围', control: conversionScope },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-model', label: '模型上下文', control: conversionModel },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-baseline',
        label: '基准修订版',
        control: conversionBaseline,
        required: true,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-delivery-scope',
        label: '范围内',
        help: '每行输入一项已确认结果。',
        control: conversionDeliveryScope,
        required: true,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-out-of-scope',
        label: '范围外',
        help: '每行输入一项明确排除内容。',
        control: conversionOutOfScope,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-constraints',
        label: '约束',
        help: '每行输入一项已确认约束。',
        control: conversionConstraints,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-criteria',
        label: '初始验收标准',
        help: '每行输入一项必需结果。',
        control: conversionCriteria,
        required: true,
      },
    }),
  ]
  const contextDetails = element(document, 'details', 'wwc-chat-convert-context')
  const contextSummary = document.createElement('summary')
  contextSummary.textContent = '查看执行上下文与基准提交'
  contextDetails.append(contextSummary, ...conversionFields.slice(2, 6).map(field => field.root))
  conversionForm.append(
    ...conversionFields.slice(0, 2).map(field => field.root),
    ...conversionFields.slice(6).map(field => field.root),
    mountFormField({ document, props: {
      id: 'chat-convert-verification', label: '验收命令', required: true,
      help: '在所选项目内运行，审查与验证会保留执行结果作为证据。', control: conversionVerification,
    } }).root,
    contextDetails,
    confirmationLabel,
    conversionError,
    conversionSubmit.root,
    conversionCancel.root,
  )
  conversion.append(conversionHeading, conversionDetail, conversionForm)
  conversation.append(
    header,
    heading,
    status,
    decisionRoot,
    conversion,
    error,
    loadEarlier,
    messages,
    empty,
    receipt,
    form,
  )
  layout.append(conversation)
  options.root.replaceChildren(layout)
  const attachments = mountChatAttachments(form, attach, () => render(options.model.state))
  const attachmentHint = form.querySelector('small')
  const onAttachmentButtonClick = () => { if (attachmentHint !== null) attachmentHint.hidden = false }
  if (attachmentHint !== null) {
    attachmentHint.hidden = true
    attach.addEventListener('click', onAttachmentButtonClick)
  }

  type ModelOption =
    | { readonly key: 'empty' | 'placeholder'; readonly candidate: null }
    | { readonly key: string; readonly candidate: ModelRouteAvailabilityProjection }
  const modelOptions = mountKeyedCollection<ModelOption, string, HTMLOptionElement>({
    parent: modelSelect,
    key: item => item.key,
    create: () => document.createElement('option'),
    update(option, item) {
      option.value = item.key
      if (item.candidate === null) {
        option.textContent = item.key === 'empty'
          ? '未配置模型路由'
          : '选择一个可用的模型路由'
        option.disabled = false
        return
      }
      const candidate = item.candidate
      // Community: one model name is enough. Provider IDs, default flags, and
      // availability jargon belong in Settings, not the Chat composer.
      option.textContent = candidate.modelDisplayName
      option.disabled = !modelRouteReady(candidate)
    },
  })
  const messageRows = new WeakMap<HTMLLIElement, {
    readonly article: HTMLElement
    readonly role: HTMLElement
    readonly content: HTMLElement
    readonly badge: HTMLElement
    readonly artifacts: HTMLElement
    readonly markdown: ReturnType<typeof mountChatMarkdown> | null
  }>()
  const messageCollection = mountKeyedCollection({
    parent: messages,
    key: (message: ChatViewModelState['messages'][number]) => message.id,
    create(message) {
      const item = document.createElement('li')
      const article = document.createElement('article')
      const role = document.createElement('h3')
      const content = document.createElement(message.role === 'assistant' ? 'div' : 'p')
      const badge = document.createElement('span')
      const artifacts = document.createElement('div')
      badge.className = 'wwc-chat-message-state'
      article.append(role, content, badge, artifacts)
      item.append(article)
      const markdown = message.role === 'assistant' ? mountChatMarkdown(content) : null
      messageRows.set(item, { article, role, content, badge, artifacts, markdown })
      return item
    },
    update(item, message: ChatViewModelState['messages'][number]) {
      const row = messageRows.get(item)
      if (row === undefined) return
      const stateText = messageStateText(message.state)
      row.article.dataset.role = message.role
      row.article.dataset.state = message.state
      row.article.setAttribute('aria-busy', String(message.state === 'streaming'))
      row.role.textContent = message.role === 'user' ? readPreferences(document.defaultView).displayName || '你' : 'WinWinCode'
      const text = message.content.length === 0 && message.state === 'streaming'
        ? '正在回复…'
        : message.content
      if (row.markdown === null) row.content.textContent = text
      else row.markdown.update(text, message.state === 'streaming')
      row.badge.hidden = stateText === null
      row.badge.textContent = stateText ?? ''
      row.artifacts.replaceChildren(...(message.artifactRefs ?? []).map(artifact => {
        const download = document.createElement('button')
        download.type = 'button'
        download.textContent = '下载项目文件'
        download.addEventListener('click', async () => {
          download.disabled = true
          download.textContent = '正在下载…'
          try {
            const blob = await options.model.downloadArtifact(artifact.artifactId)
            const url = URL.createObjectURL(blob)
            const link = document.createElement('a')
            link.href = url
            link.download = 'winwincode-project.zip'
            link.click()
            setTimeout(() => URL.revokeObjectURL(url), 1000)
            download.textContent = '下载项目文件'
          } catch {
            download.textContent = '下载失败，点击重试'
          } finally { download.disabled = false }
        })
        return download
      }))
      for (const attachment of message.attachments ?? []) {
        const preview = document.createElement('button')
        preview.type = 'button'
        preview.className = 'wwc-chat-attached-file'
        preview.textContent = attachment.name
        preview.addEventListener('click', () => showChatAttachment(document, attachment))
        if (attachment.mediaType.startsWith('image/')) {
          const thumbnail = document.createElement('img')
          thumbnail.src = `data:${attachment.mediaType};base64,${attachment.content}`
          thumbnail.alt = ''
          preview.prepend(thumbnail)
        }
        row.artifacts.append(preview)
      }
    },
    remove(item) { messageRows.get(item)?.markdown?.close(); messageRows.delete(item) },
  })

  function render(state: ChatViewModelState): void {
    const selected = state.modelRouteAvailability?.items.find(item => state.selectedModelRoute !== null && modelRouteIdentity(item.route) === modelRouteIdentity(state.selectedModelRoute)
      && (options.project === undefined || item.clientId === options.project.clientId))
    repositorySelect.hidden = state.messages.length > 0 || options.listDeviceRepositories === undefined
    repositoryStatus.hidden = repositorySelect.hidden
    void loadDeviceRepositories(selected?.clientId)
    const repository = deviceRepositories.find(item => item.repositoryBindingId === repositorySelect.value)
      ?? options.project?.repository
    const projectName = repository === undefined ? undefined : repositoryDisplayName(repository.displayName, repository.repositoryBindingId, document.defaultView)
    const deviceName = options.project?.deviceName ?? state.session?.deviceContext?.deviceName ?? '待确认'
    projectContext.textContent = repository === undefined ? '选择项目'
      : `项目：${projectName} · 设备：${deviceName} · 分支：${repository.defaultBranch}`

    if (closed) return
    const presentation = chatPagePresentation(state)
    if (conversionSessionId !== null && conversionSessionId !== state.session?.id) {
      conversionOpen = false
      conversionSessionId = null
      conversionTitle.value = ''
      conversionGoal.value = ''
      conversionBaseline.value = ''
      conversionDeliveryScope.value = ''
      conversionOutOfScope.value = ''
      conversionConstraints.value = ''
      conversionCriteria.value = ''
      conversionVerification.value = ''
      confirmation.checked = false
    }
    status.textContent = presentation.statusText
    // 设计稿 03b:侧栏高亮当前会话行,并抑制「新对话」的重复高亮。
    // 列表由外壳渲染,这里只按当前会话标记激活行(FakeDocument 下无查询能力时跳过)。
    if (typeof document.querySelectorAll === 'function') {
      for (const row of Array.from(
        document.querySelectorAll<HTMLElement>('.wwc-sidebar-recent-item'),
      )) {
        const key = row.dataset?.sessionKey ?? null
        const active = state.session !== null && key !== null && key === state.session.id
        row.classList?.toggle?.('wwc-sidebar-recent-item-active', active)
      }
      const chatNav = document.querySelector('.wwc-navigation-link[data-surface="chat"]')
      chatNav?.classList?.toggle?.('wwc-navigation-link-in-session', state.session !== null)
    }
    heading.hidden = state.session === null
    heading.textContent = state.session?.title ?? '新对话'
    messages.hidden = state.session === null
    messages.setAttribute('aria-busy', String(presentation.messageListBusy))
    composerLabel.textContent = presentation.composerLabel
    composer.placeholder = presentation.composerPlaceholder
    composer.disabled = readOnly || presentation.composerDisabled
    attach.disabled = readOnly || presentation.composerDisabled
    send.disabled = (options.listDeviceRepositories !== undefined && !repositorySelect.value) || readOnly || presentation.composerDisabled || attachments.busy || (composer.value.trim().length === 0 && attachments.items.length === 0)
    send.setAttribute('aria-label', presentation.sendLabel)
    cancel.hidden = !presentation.cancelVisible
    cancel.disabled = readOnly || state.interaction.status === 'cancelling'
    // Design page 03a keeps the empty canvas clean; setup guidance only
    // appears when no ready model route exists to start from.
    empty.hidden = state.status === 'loading' || state.status === 'refreshing'
      || state.messages.length > 0
    emptyMessage.textContent = state.session === null && readyModelRoutes(state).length > 0
      ? '你想先完成什么？'
      : presentation.emptyText
    starterList.hidden = state.session !== null || readyModelRoutes(state).length === 0
    loadEarlier.hidden = !state.messagePagination.hasMore
    loadEarlier.disabled = state.messagePagination.status === 'loading'
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = presentation.errorText === null

    const renderedModelRoutes = (state.modelRouteAvailability?.items ?? []).filter(candidate => (
      options.project === undefined || candidate.clientId === options.project.clientId
    ))
    const availableRoutes = renderedModelRoutes.filter(modelRouteReady)
    const selectedIdentity = state.selectedModelRoute === null
      ? null
      : modelRouteIdentity(state.selectedModelRoute)
    const optionsToRender: readonly ModelOption[] = renderedModelRoutes.length === 0
      ? [{ key: 'empty', candidate: null }]
      : [
          ...(selectedIdentity === null && availableRoutes.length > 0
            ? [{ key: 'placeholder' as const, candidate: null }]
            : []),
          ...renderedModelRoutes.map(candidate => ({
            key: modelRouteIdentity(candidate.route),
            candidate,
          })),
        ]
    modelOptions.update(optionsToRender)
    const selectedOption = selectedIdentity === null
      ? optionsToRender.findIndex(item => item.candidate === null)
      : optionsToRender.findIndex(item => item.key === selectedIdentity)
    if (modelSelect.selectedIndex !== Math.max(0, selectedOption)) {
      modelSelect.selectedIndex = Math.max(0, selectedOption)
    }
    const pageUnavailable = state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed'
      || state.status === 'loading'
      || state.status === 'refreshing'
      || state.status === 'cancelled'
      || state.status === 'error'
      || ['submitting', 'cancelling'].includes(state.interaction.status)
    modelSelect.disabled = state.session !== null || availableRoutes.length === 0 || pageUnavailable
    modelSettings.hidden = state.modelRouteAvailability === null || availableRoutes.length > 0
    modelNotice.hidden = state.modelRouteSelectionIssue === null
    modelNotice.textContent = state.modelRouteSelectionIssue === null
      ? ''
      : '先前选择的模型路由已不可用：'
        + `${modelRouteReasonLabel(state.modelRouteSelectionIssue)}。`
        + '请选择一个已启用的路由。'
    const delegated = options.deliveries?.state.visible.filter(item => item.sourceProductSessionId === state.session?.id) ?? []
    const deliveryListReady = options.deliveries?.state.status === 'ready'
    const labels: Record<string, string> = { backlog: '准备方案', ready: '等待执行', in_progress: '执行中', waiting_dependency: '等待前置任务', waiting_human: '需要确认', candidate_ready: '等待验收', validating: '验证中', rework: '自动修复中', done: '已完成', failed: '失败', cancelled: '已取消' }
    delegationList.replaceChildren(...delegated.map(item => {
      const row = element(document, 'li', 'wwc-chat-delegation-row')
      const link = element(document, 'a', 'wwc-chat-delegation-link')
      link.href = scopedHref(`#/home/review?delivery=${encodeURIComponent(item.deliveryId)}`)
      const status = item.status === 'candidate_ready' && item.activeWorkRunId !== null
        ? '审查与验证' : labels[item.status] ?? item.status
      link.textContent = `${item.title} · ${status}`
      row.append(link)
      const facts = element(document, 'p', 'wwc-chat-delegation-facts')
      facts.textContent = [item.acceptance === undefined ? '' : `验收通过 ${item.acceptance.passed}/${item.acceptance.total}`,
        item.reworkAttemptsUsed ? `已返工 ${item.reworkAttemptsUsed} 次` : '',
        item.openAttentionCount ? `${item.openAttentionCount} 项需要处理` : '',
      ].filter(Boolean).join(' · ')
      row.append(facts)
      return row
    }))
    delegationStatus.textContent = options.deliveries?.state.status === 'error'
      ? '任务列表暂时无法更新，请重试。'
      : !deliveryListReady ? '正在读取委托任务…' : delegated.length === 0 ? '当前对话还没有委托任务。' : `当前对话共 ${delegated.length} 项委托`
    delegationChip.update({
      className: 'wwc-chat-delegation-chip',
      label: `委托任务 ${deliveryListReady ? delegated.length : '…'} ∨`,
      type: 'button',
      variant: delegated.length === 0 ? 'ghost' : 'primary',
      onActivate() {
        delegationPanel.hidden = !delegationPanel.hidden
        delegationChip.root.setAttribute('aria-expanded', String(!delegationPanel.hidden))
        if (!delegationPanel.hidden) void options.deliveries?.refresh()
      },
    })
    newDelegation.update({
      className: 'wwc-chat-new-delegation',
      label: '新建委托',
      type: 'button',
      disabled: readOnly || options.deliveryCreator === undefined
        || options.scope === undefined
        || state.session === null
        || confirmedRequirement(state) === null
        || pageUnavailable,
      onActivate() {
        const session = options.model.state.session
        const requirement = confirmedRequirement(options.model.state)
        if (
          options.deliveryCreator === undefined
          || options.scope === undefined
          || session === null
          || requirement === null
        ) return
        options.deliveryCreator.reset?.()
        if (conversionSessionId !== session.id) {
          conversionSessionId = session.id
          conversionTitle.value = session.title
          conversionGoal.value = requirement
          conversionSourceSession.value = session.id
          conversionScope.value = [
            options.scope.organizationId,
            options.scope.workspaceId,
            options.scope.projectId,
            options.scope.repositoryId,
          ].join(' / ')
          const route = options.model.state.selectedModelRoute
          conversionModel.value = route === null
            ? '模型上下文不可用'
            : `${route.providerId} / ${route.modelId}`
          conversionBaseline.value = repository?.headCommit ?? ''
          conversionDeliveryScope.value = requirement
          conversionOutOfScope.value = ''
          conversionConstraints.value = ''
          conversionCriteria.value = ''
          conversionVerification.value = ''
          confirmation.checked = false
        }
        conversionOpen = true
        renderConversion(options.deliveryCreator.state)
      },
    })
    delegationChip.root.hidden = state.session === null

    messageCollection.update(state.messages)

    // Design page 03b: a created Delivery surfaces as an in-flow receipt line
    // pointing at the task detail for review.
    const deliveryCreated = options.deliveryCreator?.state.status === 'created'
    receipt.hidden = !deliveryCreated || state.session === null
    receiptText.textContent = deliveryCreated && state.session !== null
      ? `已委托「${conversionTitle.value || state.session.title}」，已进入所选设备的执行队列。`
      : ''
    const createdId = options.deliveryCreator?.state.deliveryId
    receiptLink.href = scopedHref(createdId === undefined ? '#/home' : `#/home/review?delivery=${encodeURIComponent(createdId)}`)

    const decisions = contextualDecisions({
      inputs: state.pendingInputs,
      approvals: state.pendingApprovals,
      attention: [],
      nowMillis: nowMillis(),
    })
    decisionRoot.hidden = decisions.items.length === 0
    decisionRoot.setAttribute('aria-hidden', String(decisionRoot.hidden))
    decisionCard.update({
      view: decisions,
      presentation: contextualDecisionPresentation(decisions, {
        busy: ['submitting', 'cancelling', 'waiting'].includes(state.interaction.status),
        pageUnavailable: state.status === 'authentication-required'
          || state.status === 'authorization-denied'
          || state.status === 'closed',
        readOnly,
      }),
    })
  }

  function closeConversion(): void {
    if (!conversionOpen) return
    conversionOpen = false
    conversionSessionId = null
    renderConversion(options.deliveryCreator?.state ?? { status: 'idle', error: null })
  }

  function renderConversion(state: ChatDeliveryCreatorState): void {
    if (closed) return
    const busy = state.status === 'submitting' || state.status === 'waiting'
    const wasOpen = !conversion.hidden
    conversion.hidden = !conversionOpen
    newDelegation.root.setAttribute('aria-expanded', String(conversionOpen))
    if (conversionOpen && !wasOpen) {
      // UI-604: a keyboard user activating the trigger has to land inside the
      // dialog, and the trigger has to be remembered so closing restores them.
      conversionFocusReturn = focusableElement(document.activeElement)
      conversionTitle.focus()
    } else if (!conversionOpen && wasOpen) {
      conversionFocusReturn?.focus()
      conversionFocusReturn = null
    }
    conversionForm.setAttribute('aria-busy', String(busy))
    const visibleError = deliveryConversionError(state)
    conversionError.hidden = visibleError === null
    conversionError.textContent = visibleError ?? ''
    conversionSubmit.update({
      className: 'wwc-chat-convert-submit',
      label: '确认并创建交付',
      busy,
      busyLabel: state.status === 'waiting' ? '正在等待交付…' : '正在创建交付…',
      disabled: readOnly || !conversionOpen || state.status === 'created' || state.status === 'closed',
      type: 'submit',
      variant: 'primary',
    })
    conversionCancel.update({
      className: 'wwc-chat-convert-cancel',
      label: busy ? '取消待处理的创建请求' : '取消转换',
      type: 'button',
      onActivate() {
        if (options.deliveryCreator === undefined) return
        if (busy) {
          options.deliveryCreator.cancelPending()
          return
        }
        closeConversion()
      },
    })
  }

  const onConversionKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape' || !conversionOpen) return
    event.preventDefault()
    closeConversion()
  }
  function resizeComposer(): void {
    composer.style.height = 'auto'
    const maxHeight = 224
    composer.style.height = `${Math.min(Math.max(composer.scrollHeight, 56), maxHeight)}px`
  }

  const onComposerInput = () => {
    resizeComposer()
    send.disabled = (options.listDeviceRepositories !== undefined && !repositorySelect.value) || readOnly || chatPagePresentation(options.model.state).composerDisabled
      || attachments.busy || (composer.value.trim().length === 0 && attachments.items.length === 0)
  }
  const onModelRouteChange = () => {
    const selectedOption = modelSelect.children[modelSelect.selectedIndex] as
      | HTMLOptionElement
      | undefined
    const selected = options.model.state.modelRouteAvailability?.items.find(candidate => (
      modelRouteIdentity(candidate.route) === selectedOption?.value
      && (options.project === undefined || candidate.clientId === options.project.clientId)
    ))
    if (selected === undefined || !modelRouteReady(selected)) return
    options.model.selectModelRoute(selected.route)
  }
  const onComposerKeydown = (event: KeyboardEvent) => {
    if (readOnly) return
    if (chatComposerKeyAction(event, readPreferences(document.defaultView).sendKey) !== 'submit') return
    event.preventDefault()
    form.requestSubmit()
  }
  const onComposerSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (readOnly) return
    if (attachments.busy) return
    const attached = [...attachments.items]
    const draft = composer.value.trim() || (attached.length > 0 ? '请查看附件。' : '')
    if (draft.length === 0) return
    // 设计稿 03a:新对话空状态下,首条消息创建会话(标题取首行)后发送。
    if (options.model.state.messages.length === 0 && options.listDeviceRepositories !== undefined && !repositorySelect.value) {
      repositoryStatus.textContent = '请选择设备上的项目。'
      repositorySelect.focus()
      return
    }
    if (repositoryDevice !== undefined && repositorySelect.value !== '') options.onProjectChange?.(repositoryDevice, repositorySelect.value)
    const repositoryBindingId = repositorySelect.value
    const sessionId = options.nextProductSessionId?.() ?? null
    let submit: Promise<void>
    if (options.model.state.session === null && sessionId !== null) {
      submit = options.model
        .createSession({
          productSessionId: sessionId,
          repositoryBindingId,
          title: draft.split('\n')[0]?.trim().slice(0, 40) || '新对话',
        })
        .then(() => options.model.state.interaction.status === 'error' ? undefined : options.model.submitMessage(draft, repositoryBindingId, attached))
    } else {
      submit = options.model.submitMessage(draft, repositoryBindingId, attached)
    }
    void submit.then(() => {
      if (options.model.state.interaction.status !== 'error') {
        composer.value = ''
        attachments.clear()
        render(options.model.state)
      }
    })
  }
  const onConversionSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (readOnly) return
    if (
      options.deliveryCreator === undefined
      || !conversionOpen
      || conversionSessionId === null
      || !confirmation.checked
      || ['submitting', 'waiting', 'created', 'closed'].includes(
        options.deliveryCreator.state.status,
      )
    ) return
    void options.deliveryCreator.create({
      title: conversionTitle.value,
      goal: conversionGoal.value,
      baseRevision: conversionBaseline.value,
      scope: conversionDeliveryScope.value.split(/\r?\n/u),
      outOfScope: conversionOutOfScope.value.split(/\r?\n/u),
      constraints: conversionConstraints.value.split(/\r?\n/u),
      sourceProductSessionId: conversionSessionId,
      acceptanceCriteria: conversionCriteria.value.split(/\r?\n/u),
      verificationCommand: conversionVerification.value,
    })
  }
  const onCancel = () => {
    if (readOnly) return
    void options.model.cancelSession('从对话页面停止。')
  }
  const onRetry = () => { void options.model.refresh() }
  const onLoadEarlier = () => { void options.model.loadMoreMessages() }

  composer.addEventListener('input', onComposerInput)
  modelSelect.addEventListener('change', onModelRouteChange)
  composer.addEventListener('keydown', onComposerKeydown)
  form.addEventListener('submit', onComposerSubmit)
  conversionForm.addEventListener('submit', onConversionSubmit)
  delegationPanel.addEventListener('keydown', event => {
    if (event.key !== 'Escape') return
    event.preventDefault()
    delegationPanel.hidden = true
    delegationChip.root.setAttribute('aria-expanded', 'false')
    delegationChip.root.focus()
  })
  conversion.addEventListener('keydown', onConversionKeyDown)
  cancel.addEventListener('click', onCancel)
  retry.addEventListener('click', onRetry)
  loadEarlier.addEventListener('click', onLoadEarlier)

  const unsubscribe = options.model.subscribe(render)
  const unsubscribeDeliveryCreator = options.deliveryCreator?.subscribe(next => {
    if (next.status === 'created') {
      conversionOpen = false
      delegationPanel.hidden = false
      void options.deliveries?.refresh()
    }
    renderConversion(next)
    render(options.model.state)
  })
  const unsubscribeDeliveries = options.deliveries?.subscribe(() => render(options.model.state))
  void options.deliveries?.start()
  const deliveryTimer = options.deliveries === undefined ? null : setInterval(() => {
    if (document.visibilityState !== 'hidden') void options.deliveries?.refresh()
  }, 5000)
  void options.model.start()

  return {
    close() {
      if (closed) return
      closed = true
      messageObserver.disconnect()
      messages.removeEventListener('scroll', onMessagesScroll)
      unsubscribe()
      unsubscribeDeliveryCreator?.()
      unsubscribeDeliveries?.()
      if (deliveryTimer !== null) clearInterval(deliveryTimer)
      options.deliveries?.close()
      composer.removeEventListener('input', onComposerInput)
      attach.removeEventListener('click', onAttachmentButtonClick)
      modelSelect.removeEventListener('change', onModelRouteChange)
      composer.removeEventListener('keydown', onComposerKeydown)
      form.removeEventListener('submit', onComposerSubmit)
      conversionForm.removeEventListener('submit', onConversionSubmit)
      conversion.removeEventListener('keydown', onConversionKeyDown)
      cancel.removeEventListener('click', onCancel)
      retry.removeEventListener('click', onRetry)
      loadEarlier.removeEventListener('click', onLoadEarlier)
      for (const field of conversionFields) field.close()
      decisionCard.close()
      delegationChip.close()
      newDelegation.close()
      conversionSubmit.close()
      conversionCancel.close()
      options.deliveryCreator?.close()
      attachments.close()
      messageCollection.close()
      modelOptions.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
