// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import {
  candidateDiffMatchKeys,
  candidateDiffRows,
  effectiveCandidateDiffView,
  parseCandidateDiff,
  type CandidateDiffRow,
  type CandidateDiffViewMode,
  type ParsedCandidateDiff,
} from './strongflow-diff-model.js'
import { strongFlowElement } from './strongflow-rendering.js'
import type { StrongFlowCandidateDiffState } from './strongflow-view-model.js'

const NARROW_DIFF_VIEW_QUERY = '(max-width: 48rem)'
const MAX_DIFF_ROW_CHARACTERS = 2_000
const MAX_RENDERED_DIFF_ROWS = 500
const DEFAULT_DIFF_ROW_LIMIT = 300
const DEFAULT_DIFF_MATCH_LIMIT = 100

export interface CandidateDiffViewerOptions {
  readonly document: Document
  readonly rowLimit?: number
  readonly matchLimit?: number
  /** Deterministic narrow-viewport probe; browser hosts default to the media query. */
  readonly narrowViewport?: () => boolean
  readonly onLoadMoreDiff: () => void
  readonly onViewModeChange: (mode: CandidateDiffViewMode) => void
  readonly onLineChange?: (line: number | null) => void
}

export interface CandidateDiffViewerProps {
  readonly diff: StrongFlowCandidateDiffState
  readonly selectedPath: string | null
  readonly viewMode: CandidateDiffViewMode
  /** Digest of the Candidate the rendered file Diff must belong to. */
  readonly candidateDigest: string | null
  /** One-based changed-file line selected by the canonical route. */
  readonly selectedLine: number | null
}

export interface CandidateDiffViewer {
  readonly root: HTMLElement
  update(props: CandidateDiffViewerProps): void
  close(): void
}

interface DiffIdentity {
  readonly candidateDigest: string | null
  readonly path: string
  readonly fileDiffSha256: string | null
}

type CandidateDiffFocusAnchor =
  | {
      readonly kind: 'hunk-toggle'
      readonly hunkKey: string
    }
  | {
      readonly kind: 'row'
      readonly row: CandidateDiffRow
    }

function sameIdentity(left: DiffIdentity | null, right: DiffIdentity | null): boolean {
  if (left === null || right === null) return false
  return left.candidateDigest === right.candidateDigest
    && left.path === right.path
    && left.fileDiffSha256 === right.fileDiffSha256
}

function markerFor(type: string, side: 'old' | 'new' | 'unified'): string {
  if (type === 'context') return ' '
  if (type === 'modified') return side === 'new' ? '+' : '-'
  if (side === 'unified') return type === 'addition' ? '+' : '-'
  if (side === 'old') return type === 'deletion' ? '-' : ''
  return type === 'addition' ? '+' : ''
}

function elementChildren(node: HTMLElement): readonly HTMLElement[] {
  return [...node.children] as unknown as readonly HTMLElement[]
}

function hasClass(node: HTMLElement, className: string): boolean {
  return node.className.split(' ').includes(className)
}

function containsNode(parent: HTMLElement, node: HTMLElement | null): boolean {
  let current = node?.parentNode ?? null
  while (current !== null) {
    if (current === parent) return true
    current = current.parentNode
  }
  return false
}

function findDescendant(node: HTMLElement, className: string): HTMLElement | null {
  for (const child of elementChildren(node)) {
    if (hasClass(child, className)) return child
    const nested = findDescendant(child, className)
    if (nested !== null) return nested
  }
  return null
}

function formatCount(value: number): string {
  return value.toLocaleString('en-US')
}

function boundedLineText(text: string): {
  readonly visible: string
  readonly renderTruncated: boolean
} {
  if (text.length <= MAX_DIFF_ROW_CHARACTERS) {
    return { visible: text, renderTruncated: false }
  }
  return { visible: text.slice(0, MAX_DIFF_ROW_CHARACTERS), renderTruncated: true }
}

function routeLine(row: CandidateDiffRow): number | null {
  return row.kind === 'line' ? row.newLine : null
}

/** Mount the bounded Candidate Diff viewer for the one trusted selected file. */
export function mountCandidateDiffViewer(
  options: CandidateDiffViewerOptions,
): CandidateDiffViewer {
  const { document } = options
  const rowLimit = options.rowLimit ?? DEFAULT_DIFF_ROW_LIMIT
  const matchLimit = options.matchLimit ?? DEFAULT_DIFF_MATCH_LIMIT
  const narrowQuery = document.defaultView?.matchMedia?.(NARROW_DIFF_VIEW_QUERY) ?? null

  const root = strongFlowElement(document, 'div', 'wwc-candidate-diff-viewer')
  const toolbar = strongFlowElement(document, 'div', 'wwc-candidate-diff-toolbar')
  const viewToggle = strongFlowElement(document, 'div', 'wwc-candidate-diff-view-toggle')
  const unifiedOption = strongFlowElement(
    document,
    'button',
    'wwc-candidate-diff-view-option',
  ) as HTMLButtonElement
  const sideBySideOption = strongFlowElement(
    document,
    'button',
    'wwc-candidate-diff-view-option',
  ) as HTMLButtonElement
  const searchLabel = document.createElement('label')
  const search = strongFlowElement(
    document,
    'input',
    'wwc-candidate-diff-search',
  ) as HTMLInputElement
  const contextToggle = strongFlowElement(
    document,
    'button',
    'wwc-candidate-diff-context-toggle',
  ) as HTMLButtonElement
  const status = strongFlowElement(document, 'p', 'wwc-candidate-diff-status')
  const matchStatus = strongFlowElement(document, 'p', 'wwc-candidate-diff-match-status')
  const scroll = strongFlowElement(document, 'div', 'wwc-candidate-diff-content')
  const table = strongFlowElement(document, 'table', 'wwc-candidate-diff-table')
  const caption = strongFlowElement(document, 'caption', 'wwc-candidate-diff-caption')
  const head = strongFlowElement(document, 'thead', 'wwc-candidate-diff-head')
  const headRow = strongFlowElement(document, 'tr', 'wwc-candidate-diff-head-row')
  const body = strongFlowElement(document, 'tbody', 'wwc-candidate-diff-body')
  const renderMore = strongFlowElement(
    document,
    'button',
    'wwc-candidate-diff-render-more',
  ) as HTMLButtonElement
  const loadMore = strongFlowElement(
    document,
    'button',
    'wwc-candidate-load-more-diff',
  ) as HTMLButtonElement

  let open = true
  let currentProps: CandidateDiffViewerProps | null = null
  let collapsedContextHunks = new Set<string>()
  let renderedRowCount = rowLimit
  let identity: DiffIdentity | null = null
  let effectiveMode: CandidateDiffViewMode = 'unified'
  let preferredMode: CandidateDiffViewMode = 'unified'
  let narrowViewport = false
  let matchKeys: readonly string[] = []
  let matchIndex = -1
  let visibleRows: readonly CandidateDiffRow[] = []
  let renderLimitReached = false

  viewToggle.setAttribute('role', 'group')
  viewToggle.setAttribute('aria-label', 'Diff layout')
  unifiedOption.type = 'button'
  unifiedOption.dataset.mode = 'unified'
  unifiedOption.textContent = 'Unified'
  sideBySideOption.type = 'button'
  sideBySideOption.dataset.mode = 'side-by-side'
  sideBySideOption.textContent = 'Side by side'
  viewToggle.append(unifiedOption, sideBySideOption)
  searchLabel.className = 'wwc-candidate-diff-search-label'
  searchLabel.textContent = 'Search in Diff'
  search.type = 'search'
  search.className = 'wwc-candidate-diff-search'
  search.placeholder = 'Text in the current Diff'
  searchLabel.append(search)
  contextToggle.type = 'button'
  contextToggle.textContent = 'Hide unchanged lines'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  matchStatus.setAttribute('role', 'status')
  matchStatus.setAttribute('aria-live', 'polite')
  scroll.tabIndex = 0
  scroll.setAttribute('role', 'group')
  scroll.setAttribute('aria-label', 'Selected file Diff')
  table.setAttribute('data-columns', '3')
  head.append(headRow)
  table.append(caption, head, body)
  renderMore.type = 'button'
  renderMore.textContent = 'Render more Diff rows'
  renderMore.hidden = true
  loadMore.type = 'button'
  loadMore.textContent = 'Load more Diff'
  loadMore.hidden = true
  toolbar.append(viewToggle, searchLabel, contextToggle)
  scroll.append(table)
  root.append(toolbar, status, matchStatus, scroll, renderMore, loadMore)

  const rows = mountKeyedCollection({
    parent: body,
    key: (row: CandidateDiffRow) => row.key,
    create() {
      const row = document.createElement('tr')
      row.className = 'wwc-candidate-diff-row'
      return row
    },
    update(row, current) {
      row.dataset.key = current.key
      row.dataset.kind = current.kind
      row.tabIndex = -1
      row.removeAttribute('aria-current')
      row.replaceChildren()
      if (current.kind === 'file-header') {
        row.dataset.pathText = current.text
        const cell = document.createElement('td')
        cell.className = 'wwc-candidate-diff-heading'
        cell.setAttribute('colspan', effectiveMode === 'side-by-side' ? '4' : '3')
        cell.textContent = current.text
        row.append(cell)
        return
      }
      if (current.kind === 'hunk-header') {
        const cell = document.createElement('td')
        cell.className = 'wwc-candidate-diff-hunk'
        cell.setAttribute('colspan', effectiveMode === 'side-by-side' ? '4' : '3')
        const toggle = document.createElement('button')
        toggle.type = 'button'
        toggle.className = 'wwc-candidate-diff-hunk-toggle'
        toggle.dataset.hunkKey = current.hunkKey
        toggle.textContent = current.header
        toggle.setAttribute('aria-expanded', String(current.expanded))
        cell.append(toggle)
        if (current.hiddenContext > 0) {
          const hidden = strongFlowElement(
            document,
            'span',
            'wwc-candidate-diff-hunk-hidden',
          )
          hidden.textContent = `${formatCount(current.hiddenContext)} unchanged lines hidden`
          cell.append(hidden)
        }
        row.append(cell)
        return
      }
      row.dataset.type = current.type
      row.dataset.hunkKey = current.hunkKey
      row.dataset.match = matchKeys.includes(current.key) ? 'true' : 'false'
      const line = routeLine(current)
      if (line !== null) row.dataset.line = String(line)
      else delete row.dataset.line
      if (line !== null && line === currentProps?.selectedLine) {
        row.setAttribute('aria-current', 'location')
        row.tabIndex = 0
      }
      const cells: HTMLTableCellElement[] = []
      if (effectiveMode === 'unified') {
        cells.push(numberCell('old', current.oldLine))
        cells.push(numberCell('new', current.newLine))
        cells.push(sourceCell('unified', current.type, current.oldText))
      } else {
        cells.push(numberCell('old', current.oldLine))
        cells.push(sourceCell('old', current.type, current.oldText))
        cells.push(numberCell('new', current.newLine))
        cells.push(sourceCell('new', current.type, current.newText))
      }
      row.replaceChildren(...cells)
    },
  })

  // UI-604: the Diff body is a real table, so its columns need names.  Without a
  // header row a screen reader reads "1 1 const one = 1" with no way to tell a
  // line number from the changed text.
  function renderColumnHeaders(): void {
    const labels = effectiveMode === 'side-by-side'
      ? ['Old line', 'Removed content', 'New line', 'Added content']
      : ['Old line', 'New line', 'Line content']
    headRow.replaceChildren()
    for (const label of labels) {
      const cell = document.createElement('th')
      cell.className = 'wwc-candidate-diff-column'
      cell.setAttribute('scope', 'col')
      cell.textContent = label
      headRow.append(cell)
    }
  }

  function numberCell(side: 'old' | 'new', value: number | null): HTMLTableCellElement {
    const cell = document.createElement('td')
    cell.className = 'wwc-candidate-diff-number'
    cell.dataset.side = side
    cell.textContent = value === null ? '' : String(value)
    return cell
  }

  function sourceCell(
    side: 'old' | 'new' | 'unified',
    type: string,
    text: string,
  ): HTMLTableCellElement {
    const cell = document.createElement('td')
    cell.className = 'wwc-candidate-diff-source'
    cell.dataset.side = side
    cell.dataset.type = type
    const marker = strongFlowElement(document, 'span', 'wwc-candidate-diff-marker')
    marker.textContent = markerFor(type, side)
    const content = strongFlowElement(document, 'span', 'wwc-candidate-diff-text')
    const boundedText = boundedLineText(text)
    content.textContent = boundedText.visible
    cell.append(marker, content)
    if (boundedText.renderTruncated) {
      const note = strongFlowElement(document, 'span', 'wwc-candidate-diff-line-truncated')
      note.textContent = '… line truncated for rendering'
      cell.append(note)
    }
    return cell
  }

  function hunkToggleNodes(): readonly HTMLButtonElement[] {
    return elementChildren(body)
      .filter(row => row.dataset.kind === 'hunk-header')
      .map(row => findDescendant(row, 'wwc-candidate-diff-hunk-toggle'))
      .filter((node): node is HTMLButtonElement => node !== null)
  }

  function rowNodes(): readonly HTMLElement[] {
    return elementChildren(body)
  }

  function rowIndexFor(node: HTMLElement | null): number {
    if (node === null) return -1
    return rowNodes().findIndex(row => row === node || containsNode(row, node))
  }

  function activeElement(): HTMLElement | null {
    return (document.activeElement as HTMLElement | null) ?? null
  }

  function currentFocusAnchor(): CandidateDiffFocusAnchor | null {
    const active = activeElement()
    const index = rowIndexFor(active)
    if (index < 0) return null
    const row = visibleRows[index]
    const node = rowNodes()[index]
    if (row === undefined || node === undefined) return null
    if (row.kind === 'hunk-header') {
      const toggle = findDescendant(node, 'wwc-candidate-diff-hunk-toggle')
      if (toggle !== null && (toggle === active || containsNode(toggle, active))) {
        return { kind: 'hunk-toggle', hunkKey: row.hunkKey }
      }
    }
    return { kind: 'row', row }
  }

  function semanticRowFor(
    anchor: CandidateDiffRow,
    candidates: readonly CandidateDiffRow[],
  ): CandidateDiffRow | null {
    const exact = candidates.find(candidate => candidate.key === anchor.key)
    if (exact !== undefined) return exact
    if (anchor.kind !== 'line') return null
    let best: CandidateDiffRow | null = null
    let bestScore = 0
    for (const candidate of candidates) {
      if (candidate.kind !== 'line' || candidate.hunkKey !== anchor.hunkKey) continue
      let score = 0
      if (anchor.oldLine !== null && candidate.oldLine === anchor.oldLine) score += 1
      if (anchor.newLine !== null && candidate.newLine === anchor.newLine) score += 1
      if (score > bestScore) {
        best = candidate
        bestScore = score
      }
    }
    return best
  }

  function restoreFocus(
    anchor: CandidateDiffFocusAnchor | null,
    candidates: readonly CandidateDiffRow[],
  ): void {
    if (anchor === null) return
    if (anchor.kind === 'hunk-toggle') {
      const row = candidates.find(candidate => (
        candidate.kind === 'hunk-header' && candidate.hunkKey === anchor.hunkKey
      ))
      if (row === undefined) return
      const node = rows.node(row.key)
      const toggle = node === null
        ? null
        : findDescendant(node, 'wwc-candidate-diff-hunk-toggle')
      toggle?.focus()
      return
    }
    const row = semanticRowFor(anchor.row, candidates)
    if (row === null) return
    const node = rows.node(row.key)
    if (node === null) return
    node.tabIndex = 0
    node.focus()
  }

  function focusHunk(delta: number): void {
    const toggles = hunkToggleNodes()
    if (toggles.length === 0) return
    const active = activeElement()
    const currentIndex = toggles.findIndex(toggle => active !== null
      && (toggle === active || containsNode(toggle, active)))
    const nextIndex = currentIndex < 0
      ? (delta > 0 ? 0 : toggles.length - 1)
      : (currentIndex + delta + toggles.length) % toggles.length
    const target = toggles[nextIndex]
    target?.focus()
  }

  function applyMatches(): void {
    matchKeys = candidateDiffMatchKeys(visibleRows, search.value, matchLimit)
    matchIndex = matchKeys.length > 0 ? 0 : -1
    for (const row of rowNodes()) {
      row.dataset.match = matchKeys.includes(row.dataset.key ?? '') ? 'true' : 'false'
    }
    matchStatus.hidden = search.value.trim().length === 0
    matchStatus.textContent = search.value.trim().length === 0
      ? ''
      : matchKeys.length === 0
        ? 'No matching lines in the rendered Diff.'
        : `Match 1 of ${formatCount(matchKeys.length)} in the rendered Diff.`
  }

  function focusMatch(delta: number): void {
    if (matchKeys.length === 0) return
    matchIndex = (matchIndex + delta + matchKeys.length) % matchKeys.length
    const key = matchKeys[matchIndex]
    const row = key === undefined ? null : rows.node(key)
    if (row !== null) {
      row.tabIndex = 0
      row.focus()
    }
    matchStatus.textContent = `Match ${formatCount(matchIndex + 1)} of ${formatCount(
      matchKeys.length,
    )} in the rendered Diff.`
  }

  function setCollapsedContext(collapsed: ReadonlySet<string>): void {
    collapsedContextHunks = new Set(collapsed)
    renderRows()
  }

  function renderRows(preserveFocus = true): void {
    contextToggle.textContent = collapsedContextHunks.size > 0
      ? 'Show unchanged lines'
      : 'Hide unchanged lines'
    const parsed = currentParsed
    if (parsed === null || identity === null) {
      visibleRows = []
      renderLimitReached = false
      rows.update([])
      renderMore.hidden = true
      renderMore.disabled = true
      return
    }
    const result = candidateDiffRows(parsed, {
      mode: effectiveMode,
      collapsedContextHunks,
      limit: renderedRowCount,
    })
    const focusAnchor = preserveFocus ? currentFocusAnchor() : null
    visibleRows = result.rows
    rows.update(result.rows)
    const remainingCapacity = Math.max(0, MAX_RENDERED_DIFF_ROWS - renderedRowCount)
    renderLimitReached = result.omittedRows > 0 && remainingCapacity === 0
    const appendableRows = Math.min(
      rowLimit,
      result.omittedRows,
      remainingCapacity,
    )
    renderMore.hidden = appendableRows === 0
    renderMore.disabled = appendableRows === 0
    renderMore.textContent = appendableRows === 0
      ? 'Diff row render limit reached'
      : `Render ${formatCount(appendableRows)} more Diff rows`
    restoreFocus(focusAnchor, result.rows)
    applyMatches()
  }

  function onBodyClick(event: Event): void {
    let current = event.target as HTMLElement | null
    while (current !== null && current !== body) {
      if (hasClass(current, 'wwc-candidate-diff-hunk-toggle')) {
        const hunkKey = current.dataset.hunkKey
        if (hunkKey !== undefined) {
          const next = new Set(collapsedContextHunks)
          if (next.has(hunkKey)) next.delete(hunkKey)
          else next.add(hunkKey)
          setCollapsedContext(next)
          const toggles = hunkToggleNodes()
          const target = toggles.find(toggle => toggle.dataset.hunkKey === hunkKey)
          target?.focus()
        }
        return
      }
      if (hasClass(current, 'wwc-candidate-diff-row')) {
        const line = current.dataset.line
        if (line !== undefined) {
          current.tabIndex = 0
          current.focus()
          options.onLineChange?.(Number(line))
        }
        return
      }
      current = current.parentNode as HTMLElement | null
    }
  }

  const onContextToggle = () => {
    const parsed = currentParsed
    if (parsed === null) return
    if (collapsedContextHunks.size > 0) {
      setCollapsedContext(new Set())
      contextToggle.focus()
      return
    }
    setCollapsedContext(new Set(parsed.hunks.map(hunk => hunk.key)))
    contextToggle.focus()
  }

  const onSearchInput = () => { applyMatches() }
  const onSearchKeyDown = (event: KeyboardEvent) => {
    if (event.key === 'Enter') {
      event.preventDefault()
      focusMatch(event.shiftKey ? -1 : 1)
    }
  }
  const onScrollKeyDown = (event: KeyboardEvent) => {
    if (event.key === 'Enter') {
      const active = activeElement()
      const line = active?.dataset.line
      if (line !== undefined) {
        event.preventDefault()
        options.onLineChange?.(Number(line))
      }
      return
    }
    if (event.key === 'j' || event.key === 'n') {
      event.preventDefault()
      focusHunk(1)
      return
    }
    if (event.key === 'k' || event.key === 'p') {
      event.preventDefault()
      focusHunk(-1)
      return
    }
    if (event.key === 'u') {
      event.preventDefault()
      if (unifiedOption.disabled !== true) options.onViewModeChange('unified')
      return
    }
    if (event.key === 's') {
      event.preventDefault()
      if (sideBySideOption.disabled !== true) options.onViewModeChange('side-by-side')
    }
  }
  const onUnified = () => {
    if (preferredMode !== 'unified') options.onViewModeChange('unified')
  }
  const onSideBySide = () => {
    if (preferredMode !== 'side-by-side') options.onViewModeChange('side-by-side')
  }
  const onRenderMore = () => {
    const nextRowCount = Math.min(
      MAX_RENDERED_DIFF_ROWS,
      renderedRowCount + rowLimit,
    )
    if (nextRowCount === renderedRowCount) {
      renderMore.hidden = true
      renderMore.disabled = true
      return
    }
    renderedRowCount = nextRowCount
    renderRows()
    if (currentProps !== null) status.textContent = statusTextFor(currentProps.diff)
  }
  const onLoadMore = () => { options.onLoadMoreDiff() }

  search.addEventListener('input', onSearchInput)
  search.addEventListener('keydown', onSearchKeyDown)
  scroll.addEventListener('keydown', onScrollKeyDown)
  body.addEventListener('click', onBodyClick)
  contextToggle.addEventListener('click', onContextToggle)
  unifiedOption.addEventListener('click', onUnified)
  sideBySideOption.addEventListener('click', onSideBySide)
  renderMore.addEventListener('click', onRenderMore)
  loadMore.addEventListener('click', onLoadMore)

  /** A viewport change can force the unified layout, so the rows must follow. */
  const onNarrowChange = () => {
    if (!open || currentProps === null) return
    update(currentProps)
  }
  narrowQuery?.addEventListener?.('change', onNarrowChange)

  let currentParsed: ParsedCandidateDiff | null = null

  function statusTextFor(diff: StrongFlowCandidateDiffState): string {
    if (diff.status === 'idle') return 'Select a changed file to load its Diff.'
    if (diff.status === 'loading') {
      return `Loading Diff for ${diff.path ?? 'the selected file'}…`
    }
    if (diff.status === 'unavailable') {
      return diff.unavailableReason === 'binary'
        ? 'Binary file preview is unavailable.'
        : 'This file encoding is not previewable.'
    }
    if (diff.status === 'error') return 'The selected Diff could not be loaded.'
    if (identity === null) return 'This Diff does not match the selected file yet.'
    const parsed = currentParsed
    if (parsed === null) return 'This Diff does not match the selected file yet.'
    if (parsed.hunks.length === 0) return 'This file has no line changes to display.'
    const visibleLineCount = visibleRows.filter(row => row.kind === 'line').length
    const parts: string[] = [
      `${formatCount(visibleLineCount)} of ${formatCount(
        parsed.lineCount,
      )} Diff lines shown.`,
    ]
    if (renderLimitReached) {
      parts.push(`The viewer renders at most ${formatCount(MAX_RENDERED_DIFF_ROWS)} rows.`)
    }
    if (diff.previewLimited) parts.push('Diff preview limit reached.')
    else if (diff.hasMore) {
      parts.push(`Showing the first ${formatCount(diff.loadedBytes)} of ${formatCount(
        diff.totalBytes ?? diff.loadedBytes,
      )} Diff bytes.`)
    }
    if (parsed.truncatedTail) parts.push('The Diff preview ends inside a line.')
    return parts.join(' ')
  }

  function update(props: CandidateDiffViewerProps): void {
    if (!open) throw new Error('Candidate Diff viewer is closed.')
    currentProps = props
    narrowViewport = options.narrowViewport?.() ?? narrowQuery?.matches ?? false
    const nextMode = effectiveCandidateDiffView(props.viewMode, narrowViewport)
    const modeChanged = nextMode !== effectiveMode
    effectiveMode = nextMode
    preferredMode = props.viewMode
    table.setAttribute('data-columns', effectiveMode === 'side-by-side' ? '4' : '3')
    renderColumnHeaders()
    unifiedOption.setAttribute('aria-pressed', String(effectiveMode === 'unified'))
    sideBySideOption.setAttribute('aria-pressed', String(effectiveMode === 'side-by-side'))
    sideBySideOption.disabled = narrowViewport
    viewToggle.dataset.narrow = String(narrowViewport)

    const nextIdentity: DiffIdentity | null = props.diff.status === 'idle'
      || props.diff.path === null
      || props.diff.path !== props.selectedPath
      ? null
      : {
          candidateDigest: props.candidateDigest,
          path: props.diff.path,
          fileDiffSha256: props.diff.fileDiffSha256,
        }
    const identityChanged = !sameIdentity(identity, nextIdentity)
    if (identityChanged) {
      identity = nextIdentity
      collapsedContextHunks = new Set()
      renderedRowCount = rowLimit
      search.value = ''
    }
    currentParsed = identity === null || props.diff.status !== 'ready'
      ? null
      : parseCandidateDiff(props.diff.content)
    const previousScroll = modeChanged ? scroll.scrollTop : null
    renderRows(!identityChanged)
    if (previousScroll !== null) scroll.scrollTop = previousScroll
    loadMore.hidden = props.diff.status !== 'error' && (
      props.diff.status !== 'ready' || !props.diff.hasMore
    )
    loadMore.textContent = props.diff.status === 'error' ? 'Retry Diff' : 'Load more Diff'
    loadMore.disabled = props.diff.status === 'loading'
    status.textContent = statusTextFor(props.diff)
    caption.textContent = props.selectedPath ?? 'No file selected'
  }

  update({
    diff: {
      status: 'idle',
      path: null,
      content: '',
      loadedBytes: 0,
      totalBytes: null,
      hasMore: false,
      previewLimited: false,
      fileDiffSha256: null,
      unavailableReason: null,
      error: null,
    },
    selectedPath: null,
    viewMode: 'unified',
    candidateDigest: null,
    selectedLine: null,
  })

  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      search.removeEventListener('input', onSearchInput)
      search.removeEventListener('keydown', onSearchKeyDown)
      scroll.removeEventListener('keydown', onScrollKeyDown)
      body.removeEventListener('click', onBodyClick)
      contextToggle.removeEventListener('click', onContextToggle)
      unifiedOption.removeEventListener('click', onUnified)
      sideBySideOption.removeEventListener('click', onSideBySide)
      renderMore.removeEventListener('click', onRenderMore)
      loadMore.removeEventListener('click', onLoadMore)
      narrowQuery?.removeEventListener?.('change', onNarrowChange)
      rows.close()
      root.remove()
    },
  }
}
