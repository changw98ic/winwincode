// SPDX-License-Identifier: Apache-2.0

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
  'repository-scope': 'Repository Scope',
  'model-route': 'Model route',
  'credential-reference': 'Credential reference',
  'server-worker-health': 'Server and Worker health',
  'helper-availability': 'Helper availability',
  'first-chat-delivery': 'First Chat and Delivery',
})

const REASON_LABELS: Readonly<Record<string, string>> = Object.freeze({
  'signed-out': 'Sign in to start first-run setup.',
  'scope-selection-required': 'Choose an authorized repository Scope with the Scope selector.',
  'scope-not-authorized': 'The Scope in this URL is not authorized. Choose another Scope.',
  'scope-empty': 'This identity has no authorized repository Scope.',
  'no-provider': 'No provider is configured for model routing yet.',
  'credential-missing-or-revoked': 'The model route has no available credential.',
  'default-route-invalid': 'The default model route is no longer valid.',
  'provider-or-model-disabled': 'The provider or model is disabled.',
  'request-pool-unavailable': 'The model request pool is unavailable.',
  'no-ready-route': 'No model route is ready to run.',
  'no-credential-reference': 'No credential reference exists yet.',
  'credential-reference-unavailable': 'Every credential reference is missing or revoked.',
  'server-unreachable': 'The Control Plane server is not reachable right now.',
  'no-worker-reported': 'No local Worker is registered yet.',
  'no-enabled-worker-capacity': 'No enabled Worker is offering execution capacity.',
  'no-chat-session': 'No Chat session exists yet.',
  'no-delivery': 'No Delivery exists yet.',
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
  if (state.status === 'checking') return 'Checking first-run setup…'
  if (state.status === 'ready') return `First-run setup complete · ${complete} of 6 complete`
  return `First-run setup · ${complete} of 6 complete`
}

function reasonText(item: ReadinessItemState): string {
  if (item.status === 'ready') return 'Complete'
  if (item.status === 'blocked') return 'Waiting for the repository Scope.'
  if (item.status === 'unavailable') {
    return 'The check could not run just now. Recheck after fixing the entry above.'
  }
  return REASON_LABELS[item.reason ?? ''] ?? 'Attention is required for this step.'
}

function checkedText(item: ReadinessItemState): string | null {
  if (item.checkedAt === null) return null
  return `Checked ${item.checkedAt}`
}

/** Mount the first-run checklist panel against its read-only view-model facts. */
export function mountReadinessPage(options: ReadinessPageOptions): ReadinessPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-readiness')
  section.setAttribute('aria-label', 'First-run readiness')
  const heading = element(document, 'h2', 'wwc-readiness-heading')
  heading.id = 'wwc-readiness-title'
  heading.textContent = 'First-run readiness'
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
  recheck.textContent = 'Recheck'
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
        hint.textContent = 'Use the Scope selector above this checklist.'
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
    summary.textContent = summaryText(state)
    toggle.setAttribute('aria-expanded', state.collapsed ? 'false' : 'true')
    toggle.textContent = state.collapsed ? 'Show checklist' : 'Hide checklist'
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
