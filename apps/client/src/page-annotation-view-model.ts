// SPDX-License-Identifier: Apache-2.0

/**
 * WWX-ANN-02 / RUN-09 page-pick and screenshot-degradation annotation model.
 *
 * Element pick is only available on same-origin injectable preview frames.
 * Cross-origin, revoked, or non-injectable pages always degrade to screenshot
 * coordinate annotation. The model never promises that every page is pickable.
 */

export type AnnotationSurfaceKind =
  | 'injectable-element'
  | 'screenshot-coordinate'

export type AnnotationInjectability =
  | 'same-origin'
  | 'cross-origin'
  | 'non-injectable'
  | 'revoked'

export interface AnnotationViewport {
  readonly width: number
  readonly height: number
  readonly devicePixelRatio: number
}

export interface AnnotationElementSummary {
  readonly tagName: string
  readonly role: string | null
  readonly accessibleName: string | null
  /** Stable, non-HTML locator; never raw innerHTML. */
  readonly locator: string
  readonly bounds: {
    readonly x: number
    readonly y: number
    readonly width: number
    readonly height: number
  }
}

export interface AnnotationScreenshotRegion {
  readonly x: number
  readonly y: number
  readonly width: number
  readonly height: number
}

export interface PageAnnotationDraft {
  readonly id: string
  readonly surfaceKind: AnnotationSurfaceKind
  readonly pagePath: string
  readonly viewport: AnnotationViewport
  readonly element: AnnotationElementSummary | null
  readonly region: AnnotationScreenshotRegion | null
  readonly comment: string
  readonly degradedReason: string | null
  readonly createdAt: string
}

export interface PageAnnotationPersisted {
  readonly id: string
  readonly body: string
  readonly candidateRef: string
  readonly workRunId: string
  readonly target: {
    readonly pagePath: string
    readonly viewport: AnnotationViewport
    readonly element: AnnotationElementSummary | null
    readonly region: AnnotationScreenshotRegion
  }
  readonly updatedAt: string
}

export interface PageAnnotationTransport {
  submit(input: {
    readonly draft: PageAnnotationDraft
    readonly expectedRevision: number
  }): Promise<{ readonly annotation: PageAnnotationPersisted; readonly catalogRevision: number }>
  list(input: { readonly deliveryId: string }): Promise<{
    readonly items: readonly PageAnnotationPersisted[]
    readonly catalogRevision: number
  }>
}

export type AnnotationState =
  | { readonly status: 'idle' }
  | {
    readonly status: 'ready'
    readonly injectability: AnnotationInjectability
    readonly surfaceKind: AnnotationSurfaceKind
    readonly degradationReason: string | null
    readonly drafts: readonly PageAnnotationDraft[]
    readonly notice: string | null
  }
  | {
    readonly status: 'submitted'
    readonly drafts: readonly PageAnnotationDraft[]
  }
  | {
    readonly status: 'submitting'
    readonly drafts: readonly PageAnnotationDraft[]
  }

export interface PageAnnotationViewModel {
  readonly state: AnnotationState
  subscribe(listener: (state: AnnotationState) => void): () => void
  prepare(input: {
    /** Stable candidate/source binding; drafts never cross this boundary. */
    readonly bindingKey?: string
    readonly pageUrl: string
    readonly pagePath: string
    readonly access: 'authorized' | 'revoked'
    readonly injectable: boolean
    readonly viewport: AnnotationViewport
    readonly deliveryId?: string
    readonly candidateRef?: string
    readonly workRunId?: string
  }): void
  pickElement(summary: AnnotationElementSummary): void
  markScreenshotRegion(region: AnnotationScreenshotRegion): void
  setComment(text: string): void
  addDraft(): void
  submit(): void
  close(): void
}

export interface PageAnnotationOptions {
  readonly now?: () => string
  readonly nextDraftId?: () => string
  readonly appOrigin: string
  readonly transport?: PageAnnotationTransport
}

function sameOrigin(appOrigin: string, pageUrl: string): boolean {
  try {
    return new URL(pageUrl).origin === new URL(appOrigin).origin
  } catch {
    return false
  }
}

/** RUN-09: decide injectability; never assume every page can be picked. */
export function resolveInjectability(input: {
  readonly appOrigin: string
  readonly pageUrl: string
  readonly access: 'authorized' | 'revoked'
  readonly injectable: boolean
}): { readonly kind: AnnotationInjectability; readonly reason: string | null } {
  if (input.access === 'revoked') {
    return { kind: 'revoked', reason: '预览访问已撤销，仅可使用已保存截图坐标。' }
  }
  if (!sameOrigin(input.appOrigin, input.pageUrl)) {
    return {
      kind: 'cross-origin',
      reason: '跨源页面不可注入拾取桥，已降级为截图坐标批注。',
    }
  }
  if (!input.injectable) {
    return {
      kind: 'non-injectable',
      reason: '该页面不可注入，已降级为截图坐标批注。',
    }
  }
  return { kind: 'same-origin', reason: null }
}

export function surfaceKindFor(
  injectability: AnnotationInjectability,
): AnnotationSurfaceKind {
  return injectability === 'same-origin'
    ? 'injectable-element'
    : 'screenshot-coordinate'
}

function clampRegion(
  region: AnnotationScreenshotRegion,
  viewport: AnnotationViewport,
): AnnotationScreenshotRegion | null {
  if (![region.x, region.y, region.width, region.height].every(Number.isFinite)
    || region.width <= 0 || region.height <= 0) return null
  const x = Math.max(0, Math.min(viewport.width - 1, Math.round(region.x)))
  const y = Math.max(0, Math.min(viewport.height - 1, Math.round(region.y)))
  const width = Math.max(1, Math.min(viewport.width - x, Math.round(region.width)))
  const height = Math.max(1, Math.min(viewport.height - y, Math.round(region.height)))
  return Object.freeze({ x, y, width, height })
}

const DEFAULT_VIEWPORT: AnnotationViewport = Object.freeze({
  width: 1280,
  height: 800,
  devicePixelRatio: 1,
})

export function createPageAnnotationViewModel(
  options: PageAnnotationOptions,
): PageAnnotationViewModel {
  const now = options.now ?? (() => new Date().toISOString())
  const nextDraftId = options.nextDraftId ?? (() => `ann_${String(Date.now())}`)
  let state: AnnotationState = Object.freeze({ status: 'idle' })
  const listeners = new Set<(next: AnnotationState) => void>()
  let pendingElement: AnnotationElementSummary | null = null
  let pendingRegion: AnnotationScreenshotRegion | null = null
  let comment = ''
  let pagePath = '/'
  let viewport = DEFAULT_VIEWPORT
  let annotationBindingKey: string | null = null
  let deliveryId: string | null = null
  let candidateRef: string | null = null
  let workRunId: string | null = null
  let expectedRevision = 0
  let restoreGeneration = 0
  let closed = false

  function emit(next: AnnotationState): void {
    state = Object.freeze(next)
    for (const listener of listeners) listener(state)
  }

  function requireReady(): Extract<AnnotationState, { status: 'ready' }> | null {
    return state.status === 'ready' ? state : null
  }

  function persistedDraft(item: PageAnnotationPersisted): PageAnnotationDraft {
    return Object.freeze({
      id: item.id,
      surfaceKind: item.target.element === null ? 'screenshot-coordinate' : 'injectable-element',
      pagePath: item.target.pagePath,
      viewport: item.target.viewport,
      element: item.target.element,
      region: item.target.region,
      comment: item.body,
      degradedReason: null,
      createdAt: item.updatedAt,
    })
  }

  async function restorePersisted(binding: string, requestedDeliveryId: string): Promise<void> {
    const generation = ++restoreGeneration
    try {
      const result = await options.transport?.list({ deliveryId: requestedDeliveryId })
      if (result === undefined || closed || generation !== restoreGeneration
        || annotationBindingKey !== binding) return
      expectedRevision = result.catalogRevision
      const restored = result.items
        .filter(item => (candidateRef === null || item.candidateRef === candidateRef)
          && (workRunId === null || item.workRunId === workRunId))
        .map(persistedDraft)
      const current = requireReady()
      if (current === null) return
      emit({ ...current, drafts: Object.freeze(restored), notice: null })
    } catch {
      const current = requireReady()
      if (current !== null && generation === restoreGeneration && annotationBindingKey === binding) {
        emit({ ...current, notice: '已打开本地批注，历史批注暂时无法恢复。' })
      }
    }
  }

  return {
    get state() {
      return state
    },
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    prepare(input) {
      if (closed) return
      const resolved = resolveInjectability({
        appOrigin: options.appOrigin,
        pageUrl: input.pageUrl,
        access: input.access,
        injectable: input.injectable,
      })
      pagePath = input.pagePath
      viewport = input.viewport
      deliveryId = input.deliveryId ?? null
      candidateRef = input.candidateRef ?? null
      workRunId = input.workRunId ?? null
      expectedRevision = 0
      const previousDrafts = state.status === 'ready' || state.status === 'submitted'
        ? state.drafts
        : Object.freeze([])
      const preserveDrafts = input.bindingKey !== undefined
        && annotationBindingKey === input.bindingKey
        && state.status !== 'idle'
      annotationBindingKey = input.bindingKey ?? null
      const drafts = input.bindingKey === undefined
        ? previousDrafts
        : preserveDrafts
          ? previousDrafts
          : Object.freeze([])
      pendingElement = null
      pendingRegion = null
      comment = ''
      emit({
        status: 'ready',
        injectability: resolved.kind,
        surfaceKind: surfaceKindFor(resolved.kind),
        degradationReason: resolved.reason,
        drafts,
        notice: resolved.reason,
      })
      if (options.transport !== undefined && input.deliveryId !== undefined && input.bindingKey !== undefined) {
        void restorePersisted(input.bindingKey, input.deliveryId)
      }
    },
    pickElement(summary) {
      if (closed) return
      const ready = requireReady()
      if (ready === null) return
      if (ready.surfaceKind !== 'injectable-element') {
        emit({
          ...ready,
          notice: '当前页面不支持元素拾取，请使用截图坐标。',
        })
        return
      }
      pendingElement = Object.freeze(summary)
      pendingRegion = Object.freeze({
        x: summary.bounds.x,
        y: summary.bounds.y,
        width: summary.bounds.width,
        height: summary.bounds.height,
      })
      emit({ ...ready, notice: null })
    },
    markScreenshotRegion(region) {
      if (closed) return
      const ready = requireReady()
      if (ready === null) return
      const clamped = clampRegion(region, viewport)
      if (clamped === null) {
        emit({ ...ready, notice: '截图区域无效。' })
        return
      }
      pendingRegion = clamped
      if (ready.surfaceKind === 'screenshot-coordinate') pendingElement = null
      emit({ ...ready, notice: null })
    },
    setComment(text) {
      comment = text.slice(0, 4000)
    },
    addDraft() {
      if (closed) return
      const ready = requireReady()
      if (ready === null) return
      if (pendingRegion === null) {
        emit({ ...ready, notice: '请先拾取元素或框选截图区域。' })
        return
      }
      if (comment.trim().length === 0) {
        emit({ ...ready, notice: '请填写批注意见。' })
        return
      }
      if (ready.drafts.length >= 50) {
        emit({ ...ready, notice: '一次最多记录 50 条页面批注。' })
        return
      }
      const draft: PageAnnotationDraft = Object.freeze({
        id: nextDraftId(),
        surfaceKind: ready.surfaceKind,
        pagePath,
        viewport,
        element: ready.surfaceKind === 'injectable-element' ? pendingElement : null,
        region: pendingRegion,
        comment: comment.trim(),
        degradedReason: ready.degradationReason,
        createdAt: now(),
      })
      pendingElement = null
      pendingRegion = null
      comment = ''
      emit({
        ...ready,
        drafts: Object.freeze([...ready.drafts, draft]),
        notice: null,
      })
    },
    submit() {
      if (closed) return
      const ready = requireReady()
      if (ready === null) return
      if (ready.drafts.length === 0) {
        emit({ ...ready, notice: '没有可提交的批注。' })
        return
      }
      if (options.transport === undefined || deliveryId === null) {
        emit({ status: 'submitted', drafts: ready.drafts })
        return
      }
      const drafts = ready.drafts
      emit({ status: 'submitting', drafts })
      void (async () => {
        try {
          const submitted: PageAnnotationDraft[] = []
          for (const draft of drafts) {
            const result = await options.transport?.submit({ draft, expectedRevision })
            if (result === undefined) throw new Error('批注提交适配器不可用')
            expectedRevision = result.catalogRevision
            submitted.push(persistedDraft(result.annotation))
          }
          if (closed) return
          emit({ status: 'submitted', drafts: Object.freeze(submitted) })
        } catch {
          if (closed) return
          emit({
            status: 'ready',
            injectability: ready.injectability,
            surfaceKind: ready.surfaceKind,
            degradationReason: ready.degradationReason,
            drafts,
            notice: '批注提交失败，请刷新任务后重试。',
          })
        }
      })()
    },
    close() {
      closed = true
      listeners.clear()
      state = Object.freeze({ status: 'idle' })
    },
  }
}
