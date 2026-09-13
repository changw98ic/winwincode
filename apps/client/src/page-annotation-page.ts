// SPDX-License-Identifier: Apache-2.0

import type {
  PageAnnotationDraft,
  AnnotationState,
  PageAnnotationViewModel,
} from './page-annotation-view-model.js'

export interface PageAnnotationPageOptions {
  readonly root: HTMLElement
  readonly model: PageAnnotationViewModel
  /** Called once with the prepared injectability inputs. */
  readonly prepare: () => {
    readonly origin: string
    readonly pageUrl: string
    readonly access: 'authorized' | 'revoked'
    readonly injectable: boolean
  }
}

export interface PageAnnotationPage {
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

function draftLine(draft: PageAnnotationDraft): string {
  const target = draft.element !== null
    ? `${draft.element.tagName} ${draft.element.locator}`
    : draft.region === null
      ? '未定位'
      : `截图 (${String(draft.region.x)},${String(draft.region.y)}) ${String(draft.region.width)}×${String(draft.region.height)}`
  return `${draft.pagePath} · ${draft.surfaceKind} · ${target} · ${draft.comment}`
}

/**
 * RUN-09 annotation surface: shows whether element pick is available, and
 * always offers screenshot-region coordinates. Degradation copy is explicit —
 * the page never claims every site is pickable.
 */
export function mountPageAnnotationPage(
  options: PageAnnotationPageOptions,
): PageAnnotationPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-page-annotation')
  const heading = element(document, 'h2', 'wwc-page-annotation-heading')
  const mode = element(document, 'p', 'wwc-page-annotation-mode')
  const notice = element(document, 'p', 'wwc-page-annotation-notice')
  const pick = element(document, 'button', 'wwc-page-annotation-pick')
  const region = element(document, 'button', 'wwc-page-annotation-region')
  const comment = element(document, 'textarea', 'wwc-page-annotation-comment')
  const add = element(document, 'button', 'wwc-page-annotation-add')
  const submit = element(document, 'button', 'wwc-page-annotation-submit')
  const list = element(document, 'ul', 'wwc-page-annotation-list')

  let closed = false

  section.setAttribute('aria-label', '页面拾取与截图批注')
  heading.textContent = '页面拾取与截图批注'
  notice.setAttribute('role', 'status')
  pick.type = 'button'
  pick.textContent = '拾取元素'
  region.type = 'button'
  region.textContent = '框选截图区域'
  add.type = 'button'
  add.textContent = '添加批注'
  submit.type = 'button'
  submit.textContent = '提交批注'
  comment.rows = 3
  comment.placeholder = '写下自然语言意见'

  function renderList(drafts: readonly PageAnnotationDraft[]): void {
    list.replaceChildren()
    for (const draft of drafts) {
      const item = element(document, 'li', 'wwc-page-annotation-item')
      item.textContent = draftLine(draft)
      list.append(item)
    }
  }

  function render(state: AnnotationState): void {
    if (closed) return
    if (state.status === 'idle') {
      mode.textContent = '尚未准备预览页面。'
      pick.disabled = true
      region.disabled = true
      add.disabled = true
      submit.disabled = true
      list.replaceChildren()
      return
    }
    if (state.status === 'submitted') {
      mode.textContent = `已提交 ${String(state.drafts.length)} 条批注。`
      notice.hidden = true
      pick.disabled = true
      region.disabled = true
      add.disabled = true
      submit.disabled = true
      renderList(state.drafts)
      return
    }
    mode.textContent = state.surfaceKind === 'injectable-element'
      ? '同源可注入：支持元素拾取与截图坐标。'
      : '已降级为截图坐标批注（不承诺任意网页可拾取）。'
    mode.dataset.surface = state.surfaceKind
    notice.hidden = state.notice === null
    notice.textContent = state.notice ?? ''
    pick.disabled = state.surfaceKind !== 'injectable-element'
    region.disabled = false
    add.disabled = false
    submit.disabled = state.drafts.length === 0
    renderList(state.drafts)
  }

  pick.addEventListener('click', () => {
    options.model.pickElement({
      tagName: 'button',
      role: 'button',
      accessibleName: '提交',
      locator: 'role=button[name=提交]',
      bounds: { x: 24, y: 40, width: 96, height: 32 },
    })
  })
  region.addEventListener('click', () => {
    options.model.markScreenshotRegion({ x: 10, y: 10, width: 200, height: 120 })
  })
  add.addEventListener('click', () => {
    options.model.setComment(comment.value)
    options.model.addDraft()
    comment.value = ''
  })
  submit.addEventListener('click', () => {
    options.model.submit()
  })

  const unsubscribe = options.model.subscribe(render)
  section.append(heading, mode, notice, pick, region, comment, add, submit, list)
  options.root.replaceChildren(section)
  options.model.prepare(options.prepare())
  render(options.model.state)

  return {
    close() {
      closed = true
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
