// SPDX-License-Identifier: Apache-2.0

import type { ReviewSnippet } from './strongflow-review-view-model.js'

/**
 * VER-11 review-side retention state. `released` is the only observable
 * availability change: the Candidate retention authority released the pin, so
 * the evidence behind a historical conclusion is no longer retrievable.
 */
export type ReviewRetentionState = 'available' | 'released'

export interface ReviewPinReference {
  /** Stable view key: `candidate:<ref>` / `evidence:<id>` / `file:<path>`. */
  readonly key: string
  readonly kind: 'candidate' | 'evidence' | 'file' | 'artifact'
  readonly label: string
  readonly retention: ReviewRetentionState | null
}

export interface ReviewPin {
  readonly reference: ReviewPinReference
  readonly pinnedAt: string
  /** True once a pinned item's retention changed after pinning. */
  readonly availabilityChanged: boolean
}

export interface ReviewSnippetAnnotation {
  readonly attentionId: string
  readonly attentionTitle: string
  readonly snippet: ReviewSnippet
  readonly attachedAt: string
}

export interface StrongFlowReviewAnnotationsState {
  readonly pins: readonly ReviewPin[]
  readonly snippets: readonly ReviewSnippetAnnotation[]
  /** Set while an attach failed because no open Attention was selected. */
  readonly lastAttachError: string | null
}

export interface StrongFlowReviewAttentionAnchor {
  readonly id: string
  readonly title: string
}

/** Bounded view memory; pins and citations never grow without limit. */
export const REVIEW_PIN_LIMIT = 20
export const REVIEW_SNIPPET_LIMIT = 50

export function retentionLabel(retention: ReviewRetentionState | null): string {
  if (retention === 'released') return '证据现已不可取'
  if (retention === 'available') return '保留中'
  return '保留状态未知'
}

export interface StrongFlowReviewAnnotations {
  readonly state: StrongFlowReviewAnnotationsState
  subscribe(listener: (state: StrongFlowReviewAnnotationsState) => void): () => void
  pin(reference: ReviewPinReference): void
  unpin(key: string): void
  togglePin(reference: ReviewPinReference): void
  isPinned(key: string): boolean
  /** Refreshes one pinned item's retention; a change stays flagged. */
  setPinRetention(key: string, retention: ReviewRetentionState): void
  attachSnippet(
    attention: StrongFlowReviewAttentionAnchor | null,
    snippet: ReviewSnippet,
  ): { attached: boolean; reason: string | null }
  clearAttachError(): void
  close(): void
}

/**
 * VER-11 view-level retention memory for the review panel. This is presentation
 * state only: it pins which projections a reviewer marked important and keeps
 * their retention labels current. It never mutates Delivery, Candidate, or
 * Evidence facts and is not a second source of truth for them.
 */
export function createStrongFlowReviewAnnotations(): StrongFlowReviewAnnotations {
  const listeners = new Set<(state: StrongFlowReviewAnnotationsState) => void>()
  let pins: ReviewPin[] = []
  let snippets: ReviewSnippetAnnotation[] = []
  let lastAttachError: string | null = null
  let closed = false

  function publish(): void {
    const state = Object.freeze({
      pins: Object.freeze([...pins]),
      snippets: Object.freeze([...snippets]),
      lastAttachError,
    })
    for (const listener of listeners) listener(state)
  }

  return {
    get state(): StrongFlowReviewAnnotationsState {
      return Object.freeze({
        pins: Object.freeze([...pins]),
        snippets: Object.freeze([...snippets]),
        lastAttachError,
      })
    },

    subscribe(listener: (state: StrongFlowReviewAnnotationsState) => void): () => void {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },

    pin(reference: ReviewPinReference): void {
      if (closed || pins.some(pin => pin.reference.key === reference.key)) return
      pins = [
        { reference, pinnedAt: new Date().toISOString(), availabilityChanged: false },
        ...pins,
      ].slice(0, REVIEW_PIN_LIMIT)
      publish()
    },

    unpin(key: string): void {
      if (closed) return
      pins = pins.filter(pin => pin.reference.key !== key)
      publish()
    },

    togglePin(reference: ReviewPinReference): void {
      if (this.isPinned(reference.key)) this.unpin(reference.key)
      else this.pin(reference)
    },

    isPinned(key: string): boolean {
      return pins.some(pin => pin.reference.key === key)
    },

    setPinRetention(key: string, retention: ReviewRetentionState): void {
      if (closed) return
      let changed = false
      pins = pins.map(pin => {
        if (pin.reference.key !== key) return pin
        if (pin.reference.retention === retention) return pin
        changed = true
        return {
          ...pin,
          availabilityChanged: pin.availabilityChanged || retention === 'released',
          reference: { ...pin.reference, retention },
        }
      })
      if (changed) publish()
    },

    attachSnippet(
      attention: StrongFlowReviewAttentionAnchor | null,
      snippet: ReviewSnippet,
    ): { attached: boolean; reason: string | null } {
      if (closed) return { attached: false, reason: 'closed' }
      if (attention === null) {
        lastAttachError = '当前没有待处理的 Attention，错误引用未关联。'
        publish()
        return { attached: false, reason: 'no-open-attention' }
      }
      snippets = [
        {
          attentionId: attention.id,
          attentionTitle: attention.title,
          snippet,
          attachedAt: new Date().toISOString(),
        },
        ...snippets,
      ].slice(0, REVIEW_SNIPPET_LIMIT)
      lastAttachError = null
      publish()
      return { attached: true, reason: null }
    },

    clearAttachError(): void {
      if (closed || lastAttachError === null) return
      lastAttachError = null
      publish()
    },

    close(): void {
      if (closed) return
      closed = true
      listeners.clear()
    },
  }
}

export interface StrongFlowReviewAnnotationsPanelOptions {
  readonly root: HTMLElement
  readonly annotations: StrongFlowReviewAnnotations
}

export interface StrongFlowReviewAnnotationsPanel {
  close(): void
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

/** Renders pinned review artifacts and Attention-linked error citations. */
export function mountStrongFlowReviewAnnotations(
  options: StrongFlowReviewAnnotationsPanelOptions,
): StrongFlowReviewAnnotationsPanel {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-review-annotations')
  section.setAttribute('aria-labelledby', 'wwc-review-annotations-title')
  const heading = element(document, 'h3', 'wwc-review-annotations-heading')
  heading.id = 'wwc-review-annotations-title'
  heading.textContent = '已固定与已关联'
  const note = element(document, 'p', 'wwc-review-annotations-note')
  note.textContent = '固定列表仅保存在当前面板视图中，不改变服务端保留策略。'
  const pinList = element(document, 'ul', 'wwc-review-annotations-pins')
  const snippetList = element(document, 'ul', 'wwc-review-annotations-snippets')
  const attachError = element(document, 'p', 'wwc-review-annotations-error')
  section.append(heading, note, pinList, snippetList, attachError)
  options.root.append(section)

  function renderPins(pins: readonly ReviewPin[]): void {
    pinList.replaceChildren()
    if (pins.length === 0) {
      const empty = element(document, 'li', 'wwc-review-annotations-empty')
      empty.textContent = '尚未固定验收产物。'
      pinList.append(empty)
      return
    }
    for (const pin of pins) {
      const item = element(document, 'li', 'wwc-review-annotations-pin')
      item.dataset.reviewPinKey = pin.reference.key
      item.dataset.reviewPinKind = pin.reference.kind
      item.dataset.reviewRetention = pin.reference.retention ?? 'unknown'
      item.dataset.reviewAvailabilityChanged = pin.availabilityChanged ? 'true' : 'false'
      const label = element(document, 'span', 'wwc-review-annotations-pin-label')
      label.textContent = pin.reference.label
      const retention = element(document, 'span', 'wwc-review-annotations-pin-retention')
      retention.textContent = retentionLabel(pin.reference.retention)
      if (pin.availabilityChanged) {
        retention.dataset.changed = 'true'
        retention.textContent = `${retentionLabel(pin.reference.retention)}（固定后发生变化）`
      }
      const unpin = element(document, 'button', 'wwc-review-annotations-unpin')
      unpin.type = 'button'
      unpin.textContent = '取消固定'
      unpin.addEventListener('click', () => options.annotations.unpin(pin.reference.key))
      item.append(label, retention, unpin)
      pinList.append(item)
    }
  }

  function renderSnippets(snippets: readonly ReviewSnippetAnnotation[]): void {
    snippetList.replaceChildren()
    if (snippets.length === 0) {
      const empty = element(document, 'li', 'wwc-review-annotations-empty')
      empty.textContent = '尚未关联错误引用。'
      snippetList.append(empty)
      return
    }
    for (const entry of snippets) {
      const item = element(document, 'li', 'wwc-review-annotations-snippet')
      item.dataset.reviewAttentionId = entry.attentionId
      item.dataset.reviewSourceRef = entry.snippet.sourceRef
      const attention = element(document, 'span', 'wwc-review-annotations-snippet-attention')
      attention.textContent = `关联 Attention：${entry.attentionTitle}`
      const citation = element(document, 'span', 'wwc-review-annotations-snippet-citation')
      const command = entry.snippet.command ?? '(无命令)'
      const exit = entry.snippet.exitCode === null ? '' : ` · 退出码 ${entry.snippet.exitCode}`
      citation.textContent = `${command}${exit} · outcome ${entry.snippet.outcome}`
      const source = element(document, 'span', 'wwc-review-annotations-snippet-source')
      source.textContent = `来源 ${entry.snippet.sourceRef}（WorkRun ${entry.snippet.workRunId ?? '未知'}）`
      item.append(attention, citation, source)
      snippetList.append(item)
    }
  }

  function render(state: StrongFlowReviewAnnotationsState): void {
    renderPins(state.pins)
    renderSnippets(state.snippets)
    attachError.hidden = state.lastAttachError === null
    attachError.textContent = state.lastAttachError ?? ''
  }

  render(options.annotations.state)
  const unsubscribe = options.annotations.subscribe(render)

  return {
    close(): void {
      unsubscribe()
      section.remove()
    },
  }
}
