// SPDX-License-Identifier: Apache-2.0

import type { ChatAttachment } from './generated/contracts.js'
import { mountAttachmentRail } from './dsh-ui.js'

const imageTypes = new Set(['image/png', 'image/jpeg', 'image/webp'])
const maxBytes = 128 * 1024
const byteSize = (item: ChatAttachment) => item.mediaType === 'text/plain'
  ? new TextEncoder().encode(item.content).length : Math.floor(item.content.length * 3 / 4)

export async function readChatFile(file: File): Promise<ChatAttachment> {
  if (!file.name.trim() || file.name.length > 255 || /[\u0000-\u001f\u007f]/u.test(file.name)) throw new Error('文件名无效。')
  if (file.size === 0) throw new Error('不能添加空文件。')
  if (imageTypes.has(file.type)) {
    if (file.size > 10 * 1024 * 1024) throw new Error('图片不能超过 10 MB。')
    let blob: Blob = file
    if (file.size > maxBytes) {
      const bitmap = await createImageBitmap(file)
      try {
        const canvas = document.createElement('canvas')
        for (const edge of [1440, 1024, 768]) {
          const scale = Math.min(1, edge / Math.max(bitmap.width, bitmap.height))
          canvas.width = Math.max(1, Math.round(bitmap.width * scale)); canvas.height = Math.max(1, Math.round(bitmap.height * scale))
          const context = canvas.getContext('2d')
          if (context === null) throw new Error('图片处理失败。')
          context.fillStyle = '#fff'; context.fillRect(0, 0, canvas.width, canvas.height)
          context.drawImage(bitmap, 0, 0, canvas.width, canvas.height)
          blob = await new Promise<Blob>((resolve, reject) => canvas.toBlob(value => value === null ? reject(new Error('图片处理失败。')) : resolve(value), 'image/jpeg', 0.8))
          if (blob.size <= maxBytes) break
        }
      } finally { bitmap.close() }
    }
    if (blob.size > maxBytes) throw new Error('图片压缩后仍过大，请裁剪后重试。')
    const bytes = new Uint8Array(await blob.arrayBuffer())
    let binary = ''
    for (const value of bytes) binary += String.fromCharCode(value)
    return { name: file.name, mediaType: blob.type, content: btoa(binary) }
  }
  if (file.size > 32 * 1024) throw new Error('文本或代码文件不能超过 32 KB。')
  let content: string
  try { content = new TextDecoder('utf-8', { fatal: true }).decode(await file.arrayBuffer()) }
  catch { throw new Error('请选择 UTF-8 文本、代码文件，或 PNG / JPEG / WebP 图片。') }
  if (/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(content)) throw new Error('该文件不是可读取的文本文件。')
  return { name: file.name, mediaType: 'text/plain', content }
}

export function showChatAttachment(document: Document, item: ChatAttachment): void {
  const dialog = document.createElement('dialog')
  dialog.className = 'wwc-attachment-preview'
  dialog.setAttribute('aria-label', item.name)
  const heading = document.createElement('h2'); heading.textContent = item.name
  const close = document.createElement('button'); close.type = 'button'; close.textContent = '关闭'
  close.onclick = () => dialog.close()
  dialog.append(heading, close)
  if (imageTypes.has(item.mediaType)) {
    const image = document.createElement('img'); image.alt = item.name
    image.src = `data:${item.mediaType};base64,${item.content}`; dialog.append(image)
  } else {
    const pre = document.createElement('pre'); pre.textContent = item.content; dialog.append(pre)
  }
  dialog.addEventListener('close', () => dialog.remove(), { once: true })
  document.body.append(dialog); dialog.showModal()
}

/** DSH thumbnail rail, with file/paste/drop inputs bound to the existing composer. */
export function mountChatAttachments(form: HTMLFormElement, button: HTMLButtonElement, changed: () => void) {
  const document = form.ownerDocument
  const input = document.createElement('input'); input.type = 'file'; input.multiple = true; input.hidden = true
  const root = document.createElement('div')
  const error = document.createElement('p'); error.setAttribute('role', 'alert'); error.hidden = true
  const hint = document.createElement('small'); hint.textContent = '图片、文本或代码 · 最多 4 个附件 · 文本 32 KB · 合计 128 KB，较大图片会压缩'
  root.className = 'wwc-chat-attachments'
  form.prepend(root, error, input)
  form.append(hint)
  let items: { id: string; attachment: ChatAttachment }[] = []
  let busy = false, closed = false
  const rail = mountAttachmentRail(root, id => {
    const item = items.find(item => item.id === id); if (item !== undefined) showChatAttachment(document, item.attachment)
  }, id => { if (!busy) { items = items.filter(item => item.id !== id); render() } })
  const render = () => {
    rail.update(items.map(({ id, attachment }) => ({ id, alt: attachment.name, removeLabel: `移除 ${attachment.name}`, previewUrl: imageTypes.has(attachment.mediaType) ? `data:${attachment.mediaType};base64,${attachment.content}` : null })))
    changed()
  }
  const add = async (files: File[]) => {
    if (busy || button.disabled || closed) return
    busy = true; error.hidden = true; changed()
    try {
      if (items.length + files.length > 4) throw new Error('最多添加 4 个附件。')
      const added = await Promise.all(files.map(readChatFile))
      if ([...items.map(item => item.attachment), ...added].reduce((total, item) => total + byteSize(item), 0) > maxBytes) throw new Error('附件合计超过 128 KB，请移除部分附件。')
      if (!closed) items.push(...added.map(attachment => ({ id: crypto.randomUUID(), attachment })))
    } catch (cause) { error.textContent = cause instanceof Error ? cause.message : '附件读取失败。'; error.hidden = false }
    finally { busy = false; if (!closed) render() }
  }
  const pick = () => input.click()
  const quote = document.createElement('button'); quote.type = 'button'; quote.textContent = '引用代码'
  const quoteCode = () => {
    if (button.disabled || busy) return
    const dialog = document.createElement('dialog'); dialog.className = 'wwc-attachment-preview'
    dialog.setAttribute('aria-label', '引用代码')
    const fields = document.createElement('form')
    const name = document.createElement('input'); name.value = '代码片段.txt'; name.required = true; name.maxLength = 255; name.setAttribute('aria-label', '文件名称')
    const code = document.createElement('textarea'); code.rows = 12; code.required = true; code.setAttribute('aria-label', '引用的代码')
    code.value = document.defaultView?.getSelection()?.toString() ?? ''
    const submit = document.createElement('button'); submit.textContent = '添加到消息'
    const cancel = document.createElement('button'); cancel.type = 'button'; cancel.textContent = '取消'; cancel.onclick = () => dialog.close()
    fields.append(name, code, submit, cancel)
    fields.onsubmit = event => { event.preventDefault(); void add([new File([code.value], name.value, { type: 'text/plain' })]); dialog.close() }
    dialog.append(fields); dialog.addEventListener('close', () => dialog.remove(), { once: true })
    document.body.append(dialog); dialog.showModal(); code.focus()
  }
  quote.addEventListener('click', quoteCode); button.after(quote)
  const selected = () => { void add([...input.files ?? []]); input.value = '' }
  const paste = (event: ClipboardEvent) => {
    const files = [...event.clipboardData?.files ?? []]
    if (files.length > 0) { event.preventDefault(); void add(files) }
  }
  const drag = (event: DragEvent) => { if (event.dataTransfer?.types.includes('Files')) { event.preventDefault(); event.dataTransfer.dropEffect = button.disabled ? 'none' : 'copy' } }
  const drop = (event: DragEvent) => { if (event.dataTransfer?.types.includes('Files')) { event.preventDefault(); void add([...event.dataTransfer.files]) } }
  button.addEventListener('click', pick); input.addEventListener('change', selected)
  form.addEventListener('paste', paste); form.addEventListener('dragover', drag); form.addEventListener('drop', drop)
  return {
    get items(): readonly ChatAttachment[] { return items.map(item => item.attachment) },
    get busy() { return busy },
    clear() { items = []; render() },
    close() { closed = true; rail.close(); quote.removeEventListener('click', quoteCode); button.removeEventListener('click', pick); input.removeEventListener('change', selected); form.removeEventListener('paste', paste); form.removeEventListener('dragover', drag); form.removeEventListener('drop', drop) },
  }
}
