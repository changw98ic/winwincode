// SPDX-License-Identifier: Apache-2.0

export interface DiffLine {
  readonly kind: 'added' | 'removed' | 'context' | 'hunk' | 'meta'
  readonly text: string
  readonly before: number | null
  readonly after: number | null
}

function changedRange(left: string, right: string): readonly [number, number] {
  let start = 0
  while (start < left.length && start < right.length && left[start] === right[start]) start += 1
  let end = 0
  while (end < left.length - start && end < right.length - start
    && left[left.length - 1 - end] === right[right.length - 1 - end]) end += 1
  return [start, left.length - end]
}

/** Read unified Git hunks; headers and no-newline markers never consume lines. */
export function diffLines(source: string): readonly DiffLine[] {
  let before: number | null = null
  let after: number | null = null
  const lines = source.split('\n')
  if (lines.at(-1) === '') lines.pop()
  return lines.map(text => {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/u.exec(text)
    if (hunk !== null) {
      before = Number(hunk[1]); after = Number(hunk[2])
      return { kind: 'hunk', text, before: null, after: null }
    }
    if (text.startsWith('diff ')) { before = null; after = null }
    if (before !== null && after !== null) {
      if (text.startsWith('+')) return { kind: 'added', text, before: null, after: after++ }
      if (text.startsWith('-')) return { kind: 'removed', text, before: before++, after: null }
      if (text.startsWith(' ')) return { kind: 'context', text, before: before++, after: after++ }
    }
    return { kind: 'meta', text, before: null, after: null }
  })
}

/** Untrusted diff text stays text, including source code that resembles HTML. */
export function renderDiff(document: Document, source: string): HTMLElement {
  const code = document.createElement('code')
  code.className = 'wwc-review-preview-code'
  const lines = diffLines(source)
  for (const [index, line] of lines.entries()) {
    if (line.kind === 'meta' && /^(?:diff --git |index |--- |\+\+\+ )/u.test(line.text)) continue
    const row = document.createElement('span')
    row.className = 'wwc-diff-line'
    row.dataset.diffKind = line.kind
    for (const [side, number] of [['before', line.before], ['after', line.after]] as const) {
      const gutter = document.createElement('span')
      gutter.className = 'wwc-diff-number'
      gutter.dataset.diffSide = side
      gutter.setAttribute('aria-hidden', 'true')
      gutter.textContent = number === null ? '' : String(number)
      row.append(gutter)
    }
    const content = document.createElement('span')
    content.className = 'wwc-diff-content'
    const marker = document.createElement('span')
    marker.className = 'wwc-diff-marker'
    marker.setAttribute('aria-hidden', 'true')
    marker.textContent = line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ' '
    content.append(marker)
    const raw = line.text.slice(1)
    const peer = lines[index + 1]?.kind === (line.kind === 'added' ? 'removed' : 'added')
      ? lines[index + 1]?.text.slice(1) : lines[index - 1]?.kind === (line.kind === 'added' ? 'removed' : 'added')
        ? lines[index - 1]?.text.slice(1) : null
    if (peer !== null && peer !== undefined && (line.kind === 'added' || line.kind === 'removed')) {
      const [start, end] = changedRange(raw, peer)
      content.append(document.createTextNode(raw.slice(0, start)))
      const changed = document.createElement('mark')
      changed.className = 'wwc-diff-inline-change'
      changed.textContent = raw.slice(start, end)
      content.append(changed, document.createTextNode(raw.slice(end)))
    } else content.append(document.createTextNode(
      line.kind === 'added' || line.kind === 'removed' || line.kind === 'context' ? raw : line.text,
    ))
    content.append(document.createTextNode('\n'))
    row.append(content)
    code.append(row)
  }
  return code
}
