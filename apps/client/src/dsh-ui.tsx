// SPDX-License-Identifier: Apache-2.0

import { createRoot } from 'react-dom/client'
import { MarkdownText } from './dsh/markdown/MarkdownText.js'

/** Mount the upstream renderer in the existing conversation message. */
export function mountChatMarkdown(root: HTMLElement): {
  update(text: string, streaming: boolean): void
  close(): void
} {
  const renderer = createRoot(root)
  let previous = '', wasStreaming = false
  return {
    update(text, streaming) {
      if (text === previous && streaming === wasStreaming) return
      previous = text; wasStreaming = streaming
      renderer.render(<MarkdownText text={text} streaming={streaming} />)
    },
    close() { renderer.unmount() },
  }
}

export { visibleSessions } from './dsh/SessionBrowser.js'
import { SessionBrowser, type SessionBrowserProps } from './dsh/SessionBrowser.js'
export function mountSessionBrowser(root: HTMLElement): { update(props: SessionBrowserProps): void; close(): void } {
  const renderer = createRoot(root)
  return { update(props) { renderer.render(<SessionBrowser {...props} />) }, close() { renderer.unmount() } }
}

import { AttachmentRail, type AttachmentRailItem } from './dsh/AttachmentRail.js'
export function mountAttachmentRail(root: HTMLElement, onOpen: (id: string) => void, onRemove: (id: string) => void): {
  update(items: readonly AttachmentRailItem[]): void
  close(): void
} {
  const renderer = createRoot(root)
  return {
    update(items) { renderer.render(items.length === 0 ? null : <AttachmentRail items={items}
      labels={{ group: '待发送附件', open: '查看附件', scrollLeft: '向前查看附件', scrollRight: '向后查看附件' }}
      onOpen={item => onOpen(item.id)} onRemove={item => onRemove(item.id)} />) },
    close() { renderer.unmount() },
  }
}
