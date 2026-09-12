// SPDX-License-Identifier: Apache-2.0

import { formatInstant } from './format-instant.js'

import type {
  ReadinessItemId,
  ReadinessItemState,
  ReadinessViewModel,
  ReadinessViewModelState,
} from './readiness-view-model.js'

export interface ReadinessFixTarget {
  readonly href: string
  readonly label: string
}

export interface ReadinessPageOptions {
  readonly root: HTMLElement
  readonly model: ReadinessViewModel
  /**
   * Presentation-only fix entries; the application builds them from the current Scope
   * and the checked item facts.
   */
  readonly fixTarget: (item: ReadinessItemState) => ReadinessFixTarget | null
}

export interface ReadinessPage {
  close(): void
}

const ITEM_TITLES: Readonly<Record<ReadinessItemId, string>> = Object.freeze({
  'repository-scope': '仓库范围',
  'model-route': '模型路由',
  'credential-reference': '凭据引用',
  'server-worker-health': '服务器与执行进程健康',
  'helper-availability': '辅助程序可用性',
  'first-chat-delivery': '首次对话与交付',
})

const REASON_LABELS: Readonly<Record<string, string>> = Object.freeze({
  'signed-out': '请登录以开始首次运行设置。',
  'scope-selection-required': '请使用范围选择器选择已授权的仓库范围。',
  'scope-not-authorized': '此网址中的范围未获授权，请选择其他范围。',
  'scope-empty': '当前身份没有已授权的仓库范围。',
  'no-provider': '尚未配置模型服务商。',
  'credential-missing-or-revoked': '模型路由没有可用凭据。',
  'default-route-invalid': '默认模型路由已失效。',
  'provider-or-model-disabled': '模型服务商或模型已停用。',
  'request-pool-unavailable': '模型请求池不可用。',
  'no-ready-route': '没有可运行的模型路由。',
  'no-credential-reference': '尚未添加凭据引用。',
  'credential-reference-unavailable': '所有凭据引用均缺失或已撤销。',
  'server-unreachable': '目前无法连接控制平面服务器。',
  'no-worker-reported': '尚未注册本地执行进程。',
  'no-enabled-worker-capacity': '没有可用的执行容量。',
  'no-chat-session': '尚未创建对话。',
  'no-delivery': '尚未创建交付。',
})

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function completeCount(items: readonly ReadinessItemState[]): number {
  return items.filter(item => item.status === 'ready').length
}

function summaryText(state: ReadinessViewModelState): string {
  const complete = completeCount(state.items)
  if (state.status === 'checking') return '正在检查首次运行设置…'
  if (state.status === 'ready') return `首次运行设置完成 · 通过 ${String(complete)}/6`
  return `首次运行设置 · 通过 ${String(complete)}/6`
}

function reasonText(item: ReadinessItemState): string {
  if (item.status === 'ready') return '通过'
  if (item.status === 'blocked') return '等待仓库范围。'
  if (item.status === 'unavailable') {
    return '暂时无法执行该检查。先解决上方条目后重新检查。'
  }
  return REASON_LABELS[item.reason ?? ''] ?? '此步骤需要处理。'
}

function checkedText(item: ReadinessItemState): string | null {
  if (item.checkedAt === null) return null
  return `检查于 ${formatInstant(item.checkedAt)}`
}

/** Mount the first-run checklist panel against its read-only view-model facts. */
export function mountReadinessPage(options: ReadinessPageOptions): ReadinessPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-readiness')
  section.setAttribute('aria-label', '首次运行就绪检查')
  const heading = element(document, 'h2', 'wwc-readiness-heading')
  heading.id = 'wwc-readiness-title'
  heading.textContent = '首次运行就绪检查'
  const summary = element(document, 'p', 'wwc-readiness-summary')
  summary.setAttribute('role', 'status')
  summary.setAttribute('aria-live', 'polite')
  const toggle = element(document, 'button', 'wwc-readiness-toggle')
  toggle.type = 'button'
  toggle.setAttribute('aria-controls', 'wwc-readiness-items')
  const items = element(document, 'ul', 'wwc-readiness-items')
  items.id = 'wwc-readiness-items'
  const recheck = element(document, 'button', 'wwc-readiness-recheck')
  recheck.type = 'button'
  recheck.textContent = '重新检查'
  const header = element(document, 'div', 'wwc-readiness-header')
  header.append(heading, summary, toggle, recheck)
  section.append(header, items)
  options.root.replaceChildren(section)
  let closed = false

  const onToggle = () => {
    options.model.setCollapsed(!options.model.state.collapsed)
  }
  const onRecheck = () => { void options.model.refresh() }
  toggle.addEventListener('click', onToggle)
  recheck.addEventListener('click', onRecheck)

  function renderListItem(item: ReadinessItemState): HTMLLIElement {
    const row = element(document, 'li', 'wwc-readiness-item')
    row.dataset.itemId = item.id
    row.dataset.status = item.status
    const title = element(document, 'h3', 'wwc-readiness-item-title')
    title.textContent = ITEM_TITLES[item.id]
    const reason = element(document, 'p', 'wwc-readiness-item-reason')
    reason.textContent = reasonText(item)
    row.append(title, reason)
    const checked = checkedText(item)
    if (checked !== null) {
      const time = element(document, 'p', 'wwc-readiness-item-checked')
      time.textContent = checked
      row.append(time)
    }
    if (item.status === 'attention') {
      const target = options.fixTarget(item)
      if (target === null) {
        const hint = element(document, 'p', 'wwc-readiness-fix-hint')
        hint.textContent = '请使用上方清单中的范围选择器。'
        row.append(hint)
      } else {
        const link = element(document, 'a', 'wwc-readiness-fix')
        link.href = target.href
        link.textContent = target.label
        row.append(link)
      }
    }
    return row
  }

  function render(state: ReadinessViewModelState): void {
    if (closed) return
    // Design page 02: readiness is onboarding. Once every check passes the
    // section disappears instead of occupying a row on work surfaces.
    section.hidden = state.status === 'ready'
    summary.textContent = summaryText(state)
    toggle.setAttribute('aria-expanded', state.collapsed ? 'false' : 'true')
    toggle.textContent = state.collapsed ? '展开清单' : '收起清单'
    items.hidden = state.collapsed
    recheck.hidden = state.status === 'closed'
    items.replaceChildren(...state.items.map(renderListItem))
  }

  const unsubscribe = options.model.subscribe(render)

  return {
    close() {
      if (closed) return
      closed = true
      toggle.removeEventListener('click', onToggle)
      recheck.removeEventListener('click', onRecheck)
      unsubscribe()
      options.root.replaceChildren()
    },
  }
}
