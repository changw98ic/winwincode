// SPDX-License-Identifier: Apache-2.0

import type {
  ChatViewModel,
  ChatViewModelState,
} from './chat-view-model.js'
import type { ControlPlaneClientError } from './community-control-plane-client.js'
import { mountButton } from '@winwincode/browser-ui'
import { mountFormField } from './components/form-field.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import type {
  ModelRouteAvailabilityProjection,
  ProductSessionId,
  RepositoryScope,
  Scope,
} from './generated/contracts.js'
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

export interface ChatDeliveryCreateInput {
  readonly title: string
  readonly goal: string
  readonly baseRevision: string
  readonly scope: readonly string[]
  readonly outOfScope: readonly string[]
  readonly constraints: readonly string[]
  readonly sourceProductSessionId: ProductSessionId | null
  readonly acceptanceCriteria: readonly string[]
}

export interface ChatDeliveryCreatorState {
  readonly status: 'idle' | 'submitting' | 'waiting' | 'created' | 'error' | 'closed'
  readonly error: ControlPlaneClientError | null
}

/** Structural composition seam; Chat does not import the StrongFlow feature model. */
export interface ChatDeliveryCreator {
  readonly state: ChatDeliveryCreatorState
  subscribe(listener: (state: ChatDeliveryCreatorState) => void): () => void
  create(input: ChatDeliveryCreateInput): Promise<void>
  cancelPending(): void
  close(): void
}

export interface ChatPageOptions {
  readonly root: HTMLElement
  readonly model: ChatViewModel
  readonly nextProductSessionId?: () => ProductSessionId
  readonly deliveryCreator?: ChatDeliveryCreator
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

export function chatComposerKeyAction(key: ChatComposerKey): ChatComposerKeyAction {
  if (key.key !== 'Enter') return 'ignore'
  if (key.isComposing || key.shiftKey) return 'newline'
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
  return candidate.status === ModelRouteAvailabilityStatus.Enabled
    && candidate.reason === ModelRouteAvailabilityReason.Ready
}

function readyModelRoutes(
  state: ChatViewModelState,
): readonly ModelRouteAvailabilityProjection[] {
  return state.modelRouteAvailability?.items.filter(modelRouteReady) ?? []
}

function modelRouteReasonLabel(reason: ModelRouteAvailabilityReason): string {
  if (reason === ModelRouteAvailabilityReason.Ready) return '就绪'
  if (reason === ModelRouteAvailabilityReason.NoProvider) return '没有可用的 Provider'
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return '凭据缺失或已撤销'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) {
    return '默认模型路由无效'
  }
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return 'Provider 或模型已停用'
  }
  return '请求池不可用'
}

function modelRouteSourceLabel(scope: Scope): string {
  if (scope.kind === 'organization') return '组织范围'
  if (scope.kind === 'workspace') return '工作区范围'
  if (scope.kind === 'project') return '项目范围'
  return '仓库范围'
}

function modelRouteIdentity(route: ModelRouteAvailabilityProjection['route']): string {
  return `${route.providerId}\u0000${route.modelId}\u0000${route.credentialReferenceId}`
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
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
    return '配置的 Provider 或模型不可用，请先检查设置再重试。'
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
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return '配置的模型凭据缺失或已被撤销。请检查设置。'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) {
    return '默认模型路由无效。请检查设置。'
  }
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return '配置的 Provider 或模型已停用。请检查设置。'
  }
  if (reason === ModelRouteAvailabilityReason.RequestPoolUnavailable) {
    return '所选模型请求池不可用。请重试或检查设置。'
  }
  return '没有可用的 Provider，因此未配置模型路由。请先打开设置再创建对话。'
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
      ? '描述你的想法，或输入 / 查看技能…'
      : '继续当前对话…',
    sendLabel: running ? '引导' : continuing ? '继续' : '发送',
    messageListBusy: state.status === 'loading'
      || state.status === 'refreshing'
      || state.realtime === 'reloading',
    composerDisabled: state.session === null
      || mutationBusy
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
  if (error.kind === 'authentication') return 'Sign in again before creating this Delivery.'
  if (error.kind === 'authorization') {
    return 'You do not have permission to create a Delivery in this repository.'
  }
  if (error.kind === 'network') {
    return 'The StrongFlow server could not be reached. The confirmed Chat draft is still here.'
  }
  if (error.kind === 'cancelled') {
    return 'Delivery creation was cancelled. The confirmed Chat draft is still here.'
  }
  if (error.code === 'REVISION_CONFLICT') {
    return 'The Delivery changed before StrongFlow could start. Retry the same confirmed draft.'
  }
  return 'The Delivery could not be created. The confirmed Chat draft is still here; retry it.'
}

const CONVERSION_DIALOG_HEADING_ID = 'wwc-chat-convert-heading'
const DIAGRAM_ARCHITECTURE_VIEW_ID = 'wwc-chat-diagram-architecture'
const DIAGRAM_FLOW_VIEW_ID = 'wwc-chat-diagram-flow'

type ChatDiagramTab = 'architecture' | 'flow'

function diagramArchitectureNode(
  document: Document,
  glyphClass: string,
  label: string,
): HTMLElement {
  const node = element(document, 'div', 'wwc-chat-diagram-node')
  node.append(element(document, 'div', `wwc-chat-diagram-glyph ${glyphClass}`))
  const name = element(document, 'span', 'wwc-chat-diagram-label')
  name.textContent = label
  node.append(name)
  return node
}

function diagramFlowStep(document: Document, label: string): HTMLElement {
  const step = element(document, 'div', 'wwc-chat-diagram-step')
  step.textContent = label
  return step
}

function diagramLink(document: Document, arrow: boolean): HTMLElement {
  return element(document, 'div', arrow
    ? 'wwc-chat-diagram-link wwc-chat-diagram-link-arrow'
    : 'wwc-chat-diagram-link')
}

function diagramChain(
  document: Document,
  nodes: readonly HTMLElement[],
  arrow: boolean,
): HTMLElement {
  const panel = element(document, 'div', 'wwc-chat-diagram-panel')
  nodes.forEach((node, index) => {
    if (index > 0) panel.append(diagramLink(document, arrow))
    panel.append(node)
  })
  return panel
}

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
  const layout = element(document, 'div', 'wwc-chat')
  const conversation = element(document, 'section', 'wwc-chat-conversation')
  // Design pages 03a/03b: the page header carries only the project switcher
  // (owned by the shell) and the delegation entry chip.
  const header = element(document, 'header', 'wwc-chat-page-header')
  const projectSwitcher = element(document, 'button', 'wwc-chat-project')
  const heading = element(document, 'h2', 'wwc-chat-heading')
  const status = element(document, 'p', 'wwc-chat-status')
  // Design page 03b: the delegation entry is the accent chip on the top right.
  const delegationChip = mountButton({
    document,
    props: {
      className: 'wwc-chat-delegation-chip',
      label: '委托任务 0 · 待审核 ∨',
      type: 'button',
      variant: 'primary',
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
  const empty = element(document, 'p', 'wwc-chat-empty')
  const loadEarlier = element(document, 'button', 'wwc-chat-load-earlier')
  // Design page 03a: without a session the page centers the architecture and
  // flow diagrams behind the「架构图 | 流程图」tabs.
  const diagram = element(document, 'div', 'wwc-chat-diagram')
  const diagramTabs = element(document, 'div', 'wwc-chat-diagram-tabs')
  const architectureTab = element(document, 'button', 'wwc-chat-diagram-tab')
  const flowTab = element(document, 'button', 'wwc-chat-diagram-tab')
  const architectureView = element(document, 'div', 'wwc-chat-diagram-view')
  const flowView = element(document, 'div', 'wwc-chat-diagram-view')
  const architectureCaption = element(document, 'p', 'wwc-chat-diagram-caption')
  const flowCaption = element(document, 'p', 'wwc-chat-diagram-caption')
  const form = element(document, 'form', 'wwc-chat-composer')
  const composerLabel = element(document, 'label', 'wwc-chat-composer-label')
  const composer = element(document, 'textarea', 'wwc-chat-composer-input')
  const controls = element(document, 'div', 'wwc-chat-composer-controls')
  // Design page 03b: the composer bar carries an attach entry. No upload
  // contract exists yet, so the control renders disabled with its reason.
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
  const confirmationLabel = element(document, 'label', 'wwc-chat-convert-confirm-label')
  const confirmation = element(document, 'input', 'wwc-chat-convert-confirm')
  const confirmationText = element(document, 'span', 'wwc-chat-convert-confirm-text')
  const conversionError = element(document, 'p', 'wwc-chat-convert-error')
  const conversionSubmit = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-submit',
      label: 'Confirm and create Delivery',
      type: 'submit',
      variant: 'primary',
    },
  })
  const conversionCancel = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-cancel',
      label: 'Cancel conversion',
      type: 'button',
    },
  })
  let closed = false
  let conversionOpen = false
  let conversionSessionId: ProductSessionId | null = null
  let conversionFocusReturn: HTMLElement | null = null

  // UI-502: the Session's own pending inputs and approvals, decided in place
  // through this page's view-model commands instead of a detour to the global
  // Attention Center.  The card is a projection of this page's snapshot, so it
  // cannot drift from the state the rest of the page renders.
  // The card mounts into this detached root, so a hidden card adds no node to
  // the conversation and the page layout stays byte-identical when idle.
  const decisionCard: ContextualDecisionCard = mountContextualDecisionCard({
    root: document.createElement('div'),
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
  projectSwitcher.type = 'button'
  projectSwitcher.disabled = true
  projectSwitcher.textContent = 'winwincode ∨'
  projectSwitcher.title = '项目切换即将在侧栏提供'
  projectSwitcher.setAttribute('aria-label', '切换项目（暂未开放）')
  modelLabel.textContent = '默认模型'
  modelLabel.htmlFor = 'wwc-chat-model'
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
  architectureTab.type = 'button'
  architectureTab.textContent = '架构图'
  architectureTab.setAttribute('role', 'tab')
  architectureTab.setAttribute('aria-controls', DIAGRAM_ARCHITECTURE_VIEW_ID)
  flowTab.type = 'button'
  flowTab.textContent = '流程图'
  flowTab.setAttribute('role', 'tab')
  flowTab.setAttribute('aria-controls', DIAGRAM_FLOW_VIEW_ID)
  diagramTabs.setAttribute('role', 'tablist')
  architectureView.id = DIAGRAM_ARCHITECTURE_VIEW_ID
  architectureView.setAttribute('role', 'tabpanel')
  flowView.id = DIAGRAM_FLOW_VIEW_ID
  flowView.setAttribute('role', 'tabpanel')
  architectureCaption.textContent = '项目架构示意'
  flowCaption.textContent = '交付流程示意'
  architectureView.append(
    diagramChain(document, [
      diagramArchitectureNode(document, 'wwc-chat-diagram-glyph-web', 'Web UI'),
      diagramArchitectureNode(document, 'wwc-chat-diagram-glyph-backend', 'Backend'),
      diagramArchitectureNode(document, 'wwc-chat-diagram-glyph-client', 'Client'),
      diagramArchitectureNode(document, 'wwc-chat-diagram-glyph-worker', 'Worker'),
    ], false),
    architectureCaption,
  )
  flowView.append(
    diagramChain(document, [
      diagramFlowStep(document, '需求'),
      diagramFlowStep(document, '方案'),
      diagramFlowStep(document, '执行'),
      diagramFlowStep(document, '验收'),
    ], true),
    flowCaption,
  )
  diagramTabs.append(architectureTab, flowTab)
  diagram.append(diagramTabs, architectureView, flowView)
  diagram.hidden = true
  composerLabel.htmlFor = 'wwc-chat-composer'
  composer.id = 'wwc-chat-composer'
  composer.rows = 3
  composer.autocomplete = 'off'
  composer.placeholder = '描述你的想法，或输入 / 查看技能…'
  attach.type = 'button'
  attach.disabled = true
  attach.textContent = '+'
  attach.setAttribute('aria-label', '附加上下文（暂不可用）')
  attach.title = '附件暂不可用'
  cancel.type = 'button'
  cancel.textContent = '停止'
  send.type = 'submit'
  // Design page 03a: the composer sends through an accent square carrying the
  // paper-plane glyph; the spoken name stays the state-dependent action label.
  send.textContent = '➤'
  send.setAttribute('aria-label', '发送')
  receipt.hidden = true
  receiptLink.href = '#/strongflow'
  receiptLink.textContent = '查看'
  receipt.append(receiptText, receiptLink)

  error.append(errorText, retry)
  modelLabel.append(modelSelect)
  controls.append(attach, modelLabel, cancel, send)
  form.append(composerLabel, composer, controls, modelNotice, modelSettings)
  header.append(projectSwitcher, delegationChip.root)
  conversion.hidden = true
  // UI-604: the panel is a dialog in fact but was announced as plain page content,
  // opened without moving focus, and could only be dismissed with the pointer.
  conversion.setAttribute('role', 'dialog')
  conversion.setAttribute('aria-modal', 'false')
  conversionHeading.id = CONVERSION_DIALOG_HEADING_ID
  conversion.setAttribute('aria-labelledby', CONVERSION_DIALOG_HEADING_ID)
  delegationChip.root.setAttribute('aria-controls', CONVERSION_DIALOG_HEADING_ID)
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
      props: { id: 'chat-convert-title', label: 'Delivery title', control: conversionTitle, required: true },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-goal', label: 'Confirmed goal', control: conversionGoal, required: true },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-session', label: 'Source Chat', control: conversionSourceSession },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-scope', label: 'Repository Scope', control: conversionScope },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-model', label: 'Model context', control: conversionModel },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-baseline',
        label: 'Baseline revision',
        control: conversionBaseline,
        required: true,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-delivery-scope',
        label: 'In scope',
        help: 'Enter one confirmed result per line.',
        control: conversionDeliveryScope,
        required: true,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-out-of-scope',
        label: 'Out of scope',
        help: 'Enter one explicit exclusion per line.',
        control: conversionOutOfScope,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-constraints',
        label: 'Constraints',
        help: 'Enter one confirmed constraint per line.',
        control: conversionConstraints,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-criteria',
        label: 'Initial acceptance criteria',
        help: 'Enter one required result per line.',
        control: conversionCriteria,
        required: true,
      },
    }),
  ]
  conversionForm.append(
    ...conversionFields.map(field => field.root),
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
    decisionCard.root,
    conversion,
    error,
    loadEarlier,
    diagram,
    messages,
    empty,
    receipt,
    form,
  )
  layout.append(conversation)
  options.root.replaceChildren(layout)

  const setDiagramTab = (tab: ChatDiagramTab): void => {
    architectureTab.setAttribute('aria-selected', String(tab === 'architecture'))
    flowTab.setAttribute('aria-selected', String(tab === 'flow'))
    architectureView.hidden = tab !== 'architecture'
    flowView.hidden = tab !== 'flow'
  }
  setDiagramTab('architecture')

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
      const source = modelRouteSourceLabel(candidate.catalogSource)
      const defaultLabel = candidate.isDefault ? ' · 默认' : ''
      option.textContent = `${source} · ${candidate.providerDisplayName} / `
        + `${candidate.modelDisplayName}${defaultLabel} · `
        + modelRouteReasonLabel(candidate.reason)
      option.disabled = !modelRouteReady(candidate)
    },
  })
  const messageRows = new WeakMap<HTMLLIElement, {
    readonly article: HTMLElement
    readonly role: HTMLElement
    readonly content: HTMLElement
    readonly badge: HTMLElement
  }>()
  const messageCollection = mountKeyedCollection({
    parent: messages,
    key: (message: ChatViewModelState['messages'][number]) => message.id,
    create() {
      const item = document.createElement('li')
      const article = document.createElement('article')
      const role = document.createElement('h3')
      const content = document.createElement('p')
      const badge = document.createElement('span')
      badge.className = 'wwc-chat-message-state'
      article.append(role, content, badge)
      item.append(article)
      messageRows.set(item, { article, role, content, badge })
      return item
    },
    update(item, message: ChatViewModelState['messages'][number]) {
      const row = messageRows.get(item)
      if (row === undefined) return
      const stateText = messageStateText(message.state)
      row.article.dataset.role = message.role
      row.article.dataset.state = message.state
      row.article.setAttribute('aria-busy', String(message.state === 'streaming'))
      row.role.textContent = message.role === 'user' ? '你' : 'WinWinCode'
      row.content.textContent = message.content.length === 0 && message.state === 'streaming'
        ? '正在回复…'
        : message.content
      row.badge.hidden = stateText === null
      row.badge.textContent = stateText ?? ''
    },
    remove(item) { messageRows.delete(item) },
  })

  function pendingDelegationCount(): number {
    return options.deliveryCreator?.state.status === 'created' ? 1 : 0
  }

  function render(state: ChatViewModelState): void {
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
      confirmation.checked = false
    }
    status.textContent = presentation.statusText
    heading.hidden = state.session === null
    heading.textContent = state.session?.title ?? '新对话'
    diagram.hidden = state.session !== null
    messages.hidden = state.session === null
    messages.setAttribute('aria-busy', String(presentation.messageListBusy))
    composerLabel.textContent = presentation.composerLabel
    composer.placeholder = presentation.composerPlaceholder
    composer.disabled = readOnly || presentation.composerDisabled
    send.disabled = readOnly || presentation.composerDisabled || composer.value.trim().length === 0
    send.setAttribute('aria-label', presentation.sendLabel)
    cancel.hidden = !presentation.cancelVisible
    cancel.disabled = readOnly || state.interaction.status === 'cancelling'
    // Design page 03a keeps the empty canvas clean; setup guidance only
    // appears when no ready model route exists to start from.
    empty.hidden = state.messages.length > 0
      || (state.session === null && readyModelRoutes(state).length > 0)
    empty.textContent = presentation.emptyText
    loadEarlier.hidden = !state.messagePagination.hasMore
    loadEarlier.disabled = state.messagePagination.status === 'loading'
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = presentation.errorText === null

    const renderedModelRoutes = state.modelRouteAvailability?.items ?? Object.freeze([])
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
    modelSelect.disabled = availableRoutes.length === 0 || pageUnavailable
    modelSettings.hidden = availableRoutes.length > 0
    modelNotice.hidden = state.modelRouteSelectionIssue === null
    modelNotice.textContent = state.modelRouteSelectionIssue === null
      ? ''
      : '先前选择的模型路由已不可用：'
        + `${modelRouteReasonLabel(state.modelRouteSelectionIssue)}。`
        + '请选择一个已启用的路由。'
    delegationChip.update({
      className: 'wwc-chat-delegation-chip',
      label: `委托任务 ${pendingDelegationCount()} · 待审核 ∨`,
      type: 'button',
      variant: 'primary',
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
          conversionBaseline.value = ''
          conversionDeliveryScope.value = requirement
          conversionOutOfScope.value = ''
          conversionConstraints.value = ''
          conversionCriteria.value = ''
          confirmation.checked = false
        }
        conversionOpen = true
        renderConversion(options.deliveryCreator.state)
      },
    })
    delegationChip.root.hidden = state.session === null

    messageCollection.update(state.messages)

    // Design page 03b: a created Delivery surfaces as an in-flow receipt line
    // pointing at the StrongFlow surface for review.
    const deliveryCreated = options.deliveryCreator?.state.status === 'created'
    receipt.hidden = !deliveryCreated || state.session === null
    receiptText.textContent = deliveryCreated && state.session !== null
      ? `已委托 「${state.session.title}」`
      : ''

    const decisions = contextualDecisions({
      inputs: state.pendingInputs,
      approvals: state.pendingApprovals,
      attention: [],
      nowMillis: nowMillis(),
    })
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
    delegationChip.root.setAttribute('aria-expanded', String(conversionOpen))
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
      label: 'Confirm and create Delivery',
      busy,
      busyLabel: state.status === 'waiting' ? 'Waiting for Delivery…' : 'Creating Delivery…',
      disabled: readOnly || !conversionOpen || state.status === 'created' || state.status === 'closed',
      type: 'submit',
      variant: 'primary',
    })
    conversionCancel.update({
      className: 'wwc-chat-convert-cancel',
      label: busy ? 'Cancel pending creation' : 'Cancel conversion',
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
  const onComposerInput = () => {
    send.disabled = readOnly || chatPagePresentation(options.model.state).composerDisabled
      || composer.value.trim().length === 0
  }
  const onModelRouteChange = () => {
    const selectedOption = modelSelect.children[modelSelect.selectedIndex] as
      | HTMLOptionElement
      | undefined
    const selected = options.model.state.modelRouteAvailability?.items.find(candidate => (
      modelRouteIdentity(candidate.route) === selectedOption?.value
    ))
    if (selected === undefined || !modelRouteReady(selected)) return
    options.model.selectModelRoute(selected.route)
  }
  const onComposerKeydown = (event: KeyboardEvent) => {
    if (readOnly) return
    if (chatComposerKeyAction(event) !== 'submit') return
    event.preventDefault()
    form.requestSubmit()
  }
  const onComposerSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (readOnly) return
    const draft = composer.value.trim()
    if (draft.length === 0) return
    void options.model.submitMessage(draft).then(() => {
      if (options.model.state.interaction.status !== 'error') {
        composer.value = ''
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
    })
  }
  const onCancel = () => {
    if (readOnly) return
    void options.model.cancelSession('Stopped from the Chat page.')
  }
  const onArchitectureTab = () => { setDiagramTab('architecture') }
  const onFlowTab = () => { setDiagramTab('flow') }
  const onRetry = () => { void options.model.refresh() }
  const onLoadEarlier = () => { void options.model.loadMoreMessages() }

  composer.addEventListener('input', onComposerInput)
  modelSelect.addEventListener('change', onModelRouteChange)
  composer.addEventListener('keydown', onComposerKeydown)
  form.addEventListener('submit', onComposerSubmit)
  conversionForm.addEventListener('submit', onConversionSubmit)
  conversion.addEventListener('keydown', onConversionKeyDown)
  architectureTab.addEventListener('click', onArchitectureTab)
  flowTab.addEventListener('click', onFlowTab)
  cancel.addEventListener('click', onCancel)
  retry.addEventListener('click', onRetry)
  loadEarlier.addEventListener('click', onLoadEarlier)

  const unsubscribe = options.model.subscribe(render)
  const unsubscribeDeliveryCreator = options.deliveryCreator?.subscribe(next => {
    renderConversion(next)
    render(options.model.state)
  })
  void options.model.start()

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      unsubscribeDeliveryCreator?.()
      composer.removeEventListener('input', onComposerInput)
      modelSelect.removeEventListener('change', onModelRouteChange)
      composer.removeEventListener('keydown', onComposerKeydown)
      form.removeEventListener('submit', onComposerSubmit)
      conversionForm.removeEventListener('submit', onConversionSubmit)
      conversion.removeEventListener('keydown', onConversionKeyDown)
      architectureTab.removeEventListener('click', onArchitectureTab)
      flowTab.removeEventListener('click', onFlowTab)
      cancel.removeEventListener('click', onCancel)
      retry.removeEventListener('click', onRetry)
      loadEarlier.removeEventListener('click', onLoadEarlier)
      for (const field of conversionFields) field.close()
      decisionCard.close()
      delegationChip.close()
      conversionSubmit.close()
      conversionCancel.close()
      options.deliveryCreator?.close()
      messageCollection.close()
      modelOptions.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
