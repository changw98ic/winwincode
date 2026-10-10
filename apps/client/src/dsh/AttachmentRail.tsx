import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import clsx from 'clsx'
const IconChevronLeftOutline14 = () => <span aria-hidden="true">‹</span>
const IconChevronRightOutline14 = () => <span aria-hidden="true">›</span>
const IconCloseFill14 = (_props: { size?: number }) => <span aria-hidden="true">×</span>

const css = {
  "arrow": "wwc-dsh-attachment-arrow",
  "arrowLeft": "wwc-dsh-attachment-arrowLeft",
  "arrowRight": "wwc-dsh-attachment-arrowRight",
  "item": "wwc-dsh-attachment-item",
  "rail": "wwc-dsh-attachment-rail",
  "remove": "wwc-dsh-attachment-remove",
  "root": "wwc-dsh-attachment-root",
  "thumbnail": "wwc-dsh-attachment-thumbnail"
} as const

export interface AttachmentRailItem {
  id: string
  previewUrl: string | null
  alt: string
  removeLabel: string
}

export interface AttachmentRailLabels {
  group: string
  open: string
  scrollLeft: string
  scrollRight: string
}

const WHEEL_LINE_PX = 16

function pageBehavior(): ScrollBehavior {
  return window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth'
}

export function AttachmentRail<T extends AttachmentRailItem>({ items, labels, onOpen, onRemove }: {
  items: readonly T[]
  labels: AttachmentRailLabels
  onOpen: (item: T) => void
  onRemove: (item: T) => void
}) {
  const railRef = useRef<HTMLDivElement | null>(null)
  const countRef = useRef<number | null>(null)
  const [edges, setEdges] = useState({ left: false, right: false })
  const updateEdges = useCallback(() => {
    const el = railRef.current
    if (el === null) return
    const left = el.scrollLeft > 1
    const right = el.scrollLeft < el.scrollWidth - el.clientWidth - 1
    setEdges(prev => prev.left === left && prev.right === right ? prev : { left, right })
  }, [])
  useLayoutEffect(() => {
    const grew = countRef.current !== null && items.length > countRef.current
    countRef.current = items.length
    const el = railRef.current
    if (el === null) return
    if (grew) el.scrollLeft = el.scrollWidth - el.clientWidth
    updateEdges()
  }, [items.length, updateEdges])
  useEffect(() => {
    const el = railRef.current
    if (el === null) return
    let disconnect = (): void => {}
    if (typeof ResizeObserver !== 'undefined') {
      const observer = new ResizeObserver(updateEdges)
      observer.observe(el)
      disconnect = () => { observer.disconnect() }
    }
    const onWheel = (event: globalThis.WheelEvent): void => {
      if (event.deltaY === 0) return
      const scale = event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? WHEEL_LINE_PX
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE ? el.clientWidth : 1
      event.preventDefault()
      el.scrollBy({
        left: event.deltaX !== 0
          ? event.deltaX * scale
          : Math.sign(event.deltaY) * Math.min(Math.abs(event.deltaY) * scale, 60),
        behavior: 'auto',
      })
    }
    el.addEventListener('wheel', onWheel, { passive: false })
    return () => {
      disconnect()
      el.removeEventListener('wheel', onWheel)
    }
  }, [updateEdges])
  const page = (direction: -1 | 1): void => {
    const el = railRef.current
    if (el === null) return
    el.scrollBy({ left: direction * Math.max(el.clientWidth - 64, 200), behavior: pageBehavior() })
  }
  return (
    <div className={css.root}>
      {edges.left && (
        <button
          type="button"
          className={clsx(css.arrow, css.arrowLeft)}
          aria-label={labels.scrollLeft}
          onClick={() => { page(-1) }}
        >
          <IconChevronLeftOutline14 />
        </button>
      )}
      <div
        ref={railRef}
        className={css.rail}
        role="group"
        aria-label={labels.group}
        onScroll={updateEdges}
      >
        {items.map(item => (
          <div key={item.id} className={css.item}>
            <button
              type="button"
              className={css.thumbnail}
              title={labels.open}
              onClick={() => { onOpen(item) }}
            >
              {item.previewUrl === null ? <span className="wwc-attachment-file">{item.alt}</span> : <img src={item.previewUrl} alt={item.alt} />}
            </button>
            <button
              type="button"
              className={css.remove}
              aria-label={item.removeLabel}
              onClick={() => { onRemove(item) }}
            >
              <IconCloseFill14 size={12} />
            </button>
          </div>
        ))}
      </div>
      {edges.right && (
        <button
          type="button"
          className={clsx(css.arrow, css.arrowRight)}
          aria-label={labels.scrollRight}
          onClick={() => { page(1) }}
        >
          <IconChevronRightOutline14 />
        </button>
      )}
    </div>
  )
}
