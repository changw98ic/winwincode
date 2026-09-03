// SPDX-License-Identifier: Apache-2.0

import {
  CandidateFileStatus,
  type CandidateFileProjection,
} from './generated/contracts.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import { mountCandidateDiffViewer, type CandidateDiffViewer } from './strongflow-diff-viewer.js'
import type { CandidateDiffViewMode } from './strongflow-diff-model.js'
import type {
  StrongFlowCandidateFilesState,
  StrongFlowProjection,
} from './strongflow-view-model.js'
import {
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

export type CandidateFileStatusFilter = 'all' | CandidateFileProjection['status']

export interface CandidateFileSummary {
  readonly total: number
  readonly additions: number
  readonly deletions: number
  readonly binary: number
  readonly unavailable: number
  readonly statuses: Readonly<Record<CandidateFileProjection['status'], number>>
}

export interface CandidateFileTreeOptions {
  readonly search: string
  readonly status: CandidateFileStatusFilter
  readonly collapsedDirectories: ReadonlySet<string>
  readonly selectedPath: string | null
  readonly limit: number
}

export type CandidateFileTreeRow = CandidateDirectoryTreeRow | CandidateChangedFileTreeRow

export interface CandidateDirectoryTreeRow {
  readonly key: string
  readonly kind: 'directory'
  readonly path: string
  readonly depth: number
  readonly expanded: boolean
  readonly descendantFiles: number
  readonly selected: false
}

export interface CandidateChangedFileTreeRow {
  readonly key: string
  readonly kind: 'file'
  readonly path: string
  readonly depth: number
  readonly expanded: false
  readonly descendantFiles: 0
  readonly selected: boolean
  readonly file: CandidateFileProjection
}

export interface CandidateFileTreeResult {
  readonly rows: readonly CandidateFileTreeRow[]
  readonly totalMatches: number
  readonly hiddenRows: number
}

interface CandidateDirectoryNode {
  readonly path: string
  readonly directories: Map<string, CandidateDirectoryNode>
  readonly files: CandidateFileProjection[]
  descendantFiles: number
}

function comparePath(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
}

export function candidateFileSummary(
  files: readonly CandidateFileProjection[],
): CandidateFileSummary {
  const statuses: Record<CandidateFileProjection['status'], number> = {
    added: 0,
    modified: 0,
    deleted: 0,
    renamed: 0,
    copied: 0,
    type_changed: 0,
  }
  let additions = 0
  let deletions = 0
  let binary = 0
  let unavailable = 0
  for (const file of files) {
    statuses[file.status] += 1
    additions += file.additions ?? 0
    deletions += file.deletions ?? 0
    if (file.binary) binary += 1
    if (file.binary || file.encoding !== 'utf-8') unavailable += 1
  }
  return Object.freeze({
    total: files.length,
    additions,
    deletions,
    binary,
    unavailable,
    statuses: Object.freeze(statuses),
  })
}

function candidateDirectoryNode(path: string): CandidateDirectoryNode {
  return { path, directories: new Map(), files: [], descendantFiles: 0 }
}

function filteredCandidateFiles(
  files: readonly CandidateFileProjection[],
  options: CandidateFileTreeOptions,
): readonly CandidateFileProjection[] {
  const search = options.search.trim().toLocaleLowerCase()
  return files.filter(file => (
    (options.status === 'all' || file.status === options.status)
    && (
      search.length === 0
      || file.path.toLocaleLowerCase().includes(search)
      || (file.oldPath?.toLocaleLowerCase().includes(search) ?? false)
    )
  )).sort((left, right) => comparePath(left.path, right.path))
}

/** Build one bounded, presentation-only directory tree from exact Candidate file metadata. */
export function candidateFileTreeRows(
  files: readonly CandidateFileProjection[],
  options: CandidateFileTreeOptions,
): CandidateFileTreeResult {
  if (!Number.isInteger(options.limit) || options.limit < 1 || options.limit > 500) {
    throw new RangeError('Candidate file tree limits must be integers between 1 and 500.')
  }
  const filtered = filteredCandidateFiles(files, options)
  const root = candidateDirectoryNode('')
  for (const file of filtered) {
    const parts = file.path.split('/')
    let directory = root
    directory.descendantFiles += 1
    for (const segment of parts.slice(0, -1)) {
      const path = directory.path.length === 0 ? segment : `${directory.path}/${segment}`
      let child = directory.directories.get(segment)
      if (child === undefined) {
        child = candidateDirectoryNode(path)
        directory.directories.set(segment, child)
      }
      directory = child
      directory.descendantFiles += 1
    }
    directory.files.push(file)
  }

  const rows: CandidateFileTreeRow[] = []
  const searchActive = options.search.trim().length > 0
  function appendDirectory(directory: CandidateDirectoryNode, depth: number): void {
    const directories = [...directory.directories.values()]
      .sort((left, right) => comparePath(left.path, right.path))
    for (const child of directories) {
      const expanded = searchActive || !options.collapsedDirectories.has(child.path)
      rows.push(Object.freeze({
        key: `directory:${child.path}`,
        kind: 'directory',
        path: child.path,
        depth,
        expanded,
        descendantFiles: child.descendantFiles,
        selected: false,
      }))
      if (expanded) appendDirectory(child, depth + 1)
    }
    for (const file of [...directory.files].sort((left, right) => comparePath(left.path, right.path))) {
      rows.push(Object.freeze({
        key: `file:${file.path}`,
        kind: 'file',
        path: file.path,
        depth,
        expanded: false,
        descendantFiles: 0,
        selected: file.path === options.selectedPath,
        file,
      }))
    }
  }
  appendDirectory(root, 1)
  const bounded = rows.slice(0, options.limit)
  return Object.freeze({
    rows: Object.freeze(bounded),
    totalMatches: filtered.length,
    hiddenRows: rows.length - bounded.length,
  })
}

export interface StrongFlowCandidateViewProps {
  readonly projection: StrongFlowProjection | null
  readonly candidateFiles: StrongFlowCandidateFilesState
  readonly viewMode: CandidateDiffViewMode
  readonly selectedLine: number | null
}

export interface StrongFlowCandidateViewOptions {
  readonly document: Document
  readonly limits: StrongFlowRenderLimits
  readonly viewMode?: CandidateDiffViewMode
  readonly selectedLine?: number | null
  readonly onViewModeChange?: (mode: CandidateDiffViewMode) => void
  readonly onLineChange?: (line: number | null) => void
  readonly onLoadFiles: () => void
  readonly onLoadMoreFiles: () => void
  readonly onSelectFile: (path: string) => void
  readonly onLoadMoreDiff: () => void
}

export interface StrongFlowCandidateView {
  readonly root: HTMLElement
  update(props: StrongFlowCandidateViewProps): void
  close(): void
}

const CANDIDATE_TREE_ROW_LIMIT = 200
const STATUS_LABELS: Readonly<Record<CandidateFileProjection['status'], string>> = {
  added: 'Added',
  modified: 'Modified',
  deleted: 'Deleted',
  renamed: 'Renamed',
  copied: 'Copied',
  type_changed: 'Type changed',
}

function fileName(path: string): string {
  return path.split('/').at(-1) ?? path
}

function formatCount(value: number): string {
  return value.toLocaleString('en-US')
}

function candidateViewIdentity(candidate: StrongFlowProjection['currentCandidate']): string | null {
  return candidate === null
    ? null
    : [
        candidate.candidateRef,
        candidate.candidateCommitId,
        candidate.candidateTreeId,
        candidate.deliverySpecId,
        String(candidate.deliverySpecRevision),
        candidate.diffSha256,
        candidate.frozenAt,
        candidate.producerSessionBindingId,
        candidate.producerStageRunId,
      ].join('\n')
}

/** Stable Candidate region that UI-301 can place in either desktop or tabbed layouts. */
export function mountStrongFlowCandidate(
  options: StrongFlowCandidateViewOptions,
): StrongFlowCandidateView {
  const { document } = options
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-view-candidate')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const workspace = strongFlowElement(document, 'section', 'wwc-candidate-workspace')
  const fileBrowser = strongFlowElement(document, 'section', 'wwc-candidate-file-browser')
  const summary = strongFlowElement(document, 'p', 'wwc-candidate-file-summary')
  const controls = strongFlowElement(document, 'div', 'wwc-candidate-file-controls')
  const searchLabel = document.createElement('label')
  const search = document.createElement('input')
  const statusLabel = document.createElement('label')
  const status = document.createElement('select')
  const tree = strongFlowElement(document, 'div', 'wwc-candidate-file-tree')
  const treeStatus = strongFlowElement(document, 'p', 'wwc-candidate-file-tree-status')
  const hiddenRows = strongFlowElement(document, 'p', 'wwc-candidate-file-hidden')
  const loadMoreFiles = strongFlowElement(
    document,
    'button',
    'wwc-candidate-load-more-files',
  ) as HTMLButtonElement
  const diff = strongFlowElement(document, 'section', 'wwc-candidate-diff')
  const diffHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const technical = strongFlowElement(
    document,
    'details',
    'wwc-candidate-technical-details',
  )
  const technicalSummary = document.createElement('summary')
  const technicalList = document.createElement('dl')
  const evidenceHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const evidence = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence')
  const evidenceOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const verdictHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const verdict = strongFlowElement(document, 'section', 'wwc-strongflow-verdict')
  const verdictBody = strongFlowElement(document, 'div', 'wwc-strongflow-verdict-body')
  const publicationHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const publication = strongFlowElement(document, 'section', 'wwc-strongflow-publication')
  const publicationBody = strongFlowElement(document, 'div', 'wwc-strongflow-publication-body')
  const collapsedDirectories = new Set<string>()
  let current: StrongFlowCandidateViewProps = {
    projection: null,
    viewMode: options.viewMode ?? 'unified',
    selectedLine: options.selectedLine ?? null,
    candidateFiles: {
      status: 'idle',
      items: [],
      hasMore: false,
      previewLimited: false,
      selectedPath: null,
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
      error: null,
    },
  }
  let open = true
  let requestedCandidateIdentity: string | null = null
  let visibleRows: readonly CandidateFileTreeRow[] = []
  let rovingRowKey: string | null = null

  const diffViewer: CandidateDiffViewer = mountCandidateDiffViewer({
    document,
    onLoadMoreDiff() {
      const selectedDiff = current.candidateFiles.diff
      if (selectedDiff.status === 'error' && selectedDiff.path !== null) {
        options.onSelectFile(selectedDiff.path)
      } else {
        options.onLoadMoreDiff()
      }
    },
    onViewModeChange(mode) {
      current = { ...current, viewMode: mode }
      diffViewer.update({
        diff: current.candidateFiles.diff,
        selectedPath: current.candidateFiles.selectedPath,
        viewMode: mode,
        candidateDigest: current.projection?.currentCandidate?.diffSha256 ?? null,
        selectedLine: current.selectedLine,
      })
      options.onViewModeChange?.(mode)
    },
    onLineChange(line) {
      current = { ...current, selectedLine: line }
      options.onLineChange?.(line)
      diffViewer.update({
        diff: current.candidateFiles.diff,
        selectedPath: current.candidateFiles.selectedPath,
        viewMode: current.viewMode,
        candidateDigest: current.projection?.currentCandidate?.diffSha256 ?? null,
        selectedLine: line,
      })
    },
  })

  root.dataset.view = 'frozen-candidate'
  heading.textContent = 'Candidate changes'
  empty.textContent = 'No candidate has been frozen.'
  searchLabel.textContent = 'Search changed files'
  search.type = 'search'
  search.className = 'wwc-candidate-file-search'
  search.placeholder = 'Path or previous path'
  searchLabel.append(search)
  statusLabel.textContent = 'Filter by status'
  status.className = 'wwc-candidate-file-status-filter'
  const statusOptions: readonly (readonly [CandidateFileStatusFilter, string])[] = [
    ['all', 'All statuses'],
    [CandidateFileStatus.Added, STATUS_LABELS.added],
    [CandidateFileStatus.Modified, STATUS_LABELS.modified],
    [CandidateFileStatus.Deleted, STATUS_LABELS.deleted],
    [CandidateFileStatus.Renamed, STATUS_LABELS.renamed],
    [CandidateFileStatus.Copied, STATUS_LABELS.copied],
    [CandidateFileStatus.TypeChanged, STATUS_LABELS.type_changed],
  ]
  for (const [value, label] of statusOptions) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = label
    status.append(option)
  }
  status.value = 'all'
  statusLabel.append(status)
  controls.append(searchLabel, statusLabel)
  tree.setAttribute('role', 'tree')
  tree.setAttribute('aria-label', 'Candidate changed files')
  treeStatus.setAttribute('role', 'status')
  treeStatus.setAttribute('aria-live', 'polite')
  loadMoreFiles.type = 'button'
  loadMoreFiles.textContent = 'Load more changed files'
  diffHeading.textContent = 'Selected file Diff'
  technicalSummary.textContent = 'Candidate technical details'
  technical.append(technicalSummary, technicalList)
  diff.append(diffHeading, diffViewer.root)
  fileBrowser.append(
    summary,
    controls,
    treeStatus,
    tree,
    hiddenRows,
    loadMoreFiles,
  )
  workspace.append(fileBrowser, diff, technical)
  evidenceHeading.textContent = 'Evidence'
  evidence.setAttribute('aria-label', 'Delivery evidence')
  verdictHeading.textContent = 'Verdict'
  verdict.append(verdictHeading, verdictBody)
  publicationHeading.textContent = 'Publication'
  publication.append(publicationHeading, publicationBody)
  root.append(heading, empty, workspace, evidenceHeading, evidence, evidenceOmitted, verdict, publication)

  const rowCollection = mountKeyedCollection({
    parent: tree,
    key: (row: CandidateFileTreeRow) => row.key,
    create(row) {
      const button = document.createElement('button')
      button.type = 'button'
      button.className = 'wwc-candidate-file-row'
      button.dataset.key = row.key
      return button
    },
    update(button, row) {
      button.dataset.key = row.key
      button.dataset.path = row.path
      button.dataset.kind = row.kind
      button.dataset.depth = String(Math.min(row.depth, 8))
      button.setAttribute('role', 'treeitem')
      button.setAttribute('aria-level', String(row.depth))
      button.tabIndex = rovingRowKey === row.key ? 0 : -1
      if (row.kind === 'directory') {
        delete button.dataset.status
        button.setAttribute('aria-expanded', String(row.expanded))
        button.removeAttribute('aria-selected')
        const name = strongFlowElement(document, 'span', 'wwc-candidate-directory-name')
        const count = strongFlowElement(document, 'span', 'wwc-candidate-directory-count')
        name.textContent = fileName(row.path)
        count.textContent = `${String(row.descendantFiles)} files`
        button.replaceChildren(name, count)
        return
      }
      button.removeAttribute('aria-expanded')
      button.dataset.status = row.file.status
      button.setAttribute('aria-selected', String(row.selected))
      const name = strongFlowElement(document, 'span', 'wwc-candidate-file-name')
      const state = strongFlowElement(document, 'span', 'wwc-candidate-file-state')
      const statistics = strongFlowElement(document, 'span', 'wwc-candidate-file-statistics')
      name.textContent = row.path
      state.textContent = STATUS_LABELS[row.file.status]
      statistics.textContent = row.file.additions === null || row.file.deletions === null
        ? 'Line statistics unavailable'
        : `+${formatCount(row.file.additions)} -${formatCount(row.file.deletions)}`
      const children: HTMLElement[] = [name, state, statistics]
      if (row.file.oldPath !== null) {
        const origin = strongFlowElement(
          document,
          'span',
          row.file.status === CandidateFileStatus.Renamed
            ? 'wwc-candidate-file-renamed'
            : 'wwc-candidate-file-origin',
        )
        origin.textContent = row.file.status === CandidateFileStatus.Renamed
          ? `Renamed from ${row.file.oldPath}`
          : `Copied from ${row.file.oldPath}`
        children.push(origin)
      }
      if (row.file.binary || row.file.encoding !== 'utf-8') {
        const unavailable = strongFlowElement(
          document,
          'span',
          'wwc-candidate-file-preview-unavailable',
        )
        unavailable.textContent = row.file.binary
          ? 'Binary · preview unavailable'
          : 'Encoding not previewable'
        children.push(unavailable)
      }
      button.replaceChildren(...children)
    },
  })

  function renderTree(): void {
    const result = candidateFileTreeRows(current.candidateFiles.items, {
      search: search.value,
      status: status.value as CandidateFileStatusFilter,
      collapsedDirectories,
      selectedPath: current.candidateFiles.selectedPath,
      limit: CANDIDATE_TREE_ROW_LIMIT,
    })
    visibleRows = result.rows
    rovingRowKey = visibleRows.find(row => row.selected)?.key ?? visibleRows[0]?.key ?? null
    rowCollection.update(result.rows)
    hiddenRows.hidden = result.hiddenRows === 0
    hiddenRows.textContent = result.hiddenRows === 0
      ? ''
      : `${formatCount(result.hiddenRows)} more tree rows are not rendered. Refine the search.`
    treeStatus.textContent = current.candidateFiles.status === 'loading'
      ? 'Loading changed files…'
      : current.candidateFiles.status === 'loading-more'
        ? `Loading more changed files after ${formatCount(current.candidateFiles.items.length)}…`
        : current.candidateFiles.status === 'error'
          ? 'Changed files could not be loaded. Retry from the Candidate panel.'
          : `${formatCount(result.totalMatches)} matching changed files.`
  }

  function focusRow(index: number): void {
    const row = visibleRows[index]
    if (row === undefined) return
    for (const visible of visibleRows) rowCollection.node(visible.key)?.setAttribute('tabindex', '-1')
    const node = rowCollection.node(row.key)
    node?.setAttribute('tabindex', '0')
    node?.focus()
  }

  const onTreeClick = (event: Event) => {
    const target = event.target as HTMLElement | null
    const button = target?.closest<HTMLElement>('.wwc-candidate-file-row') ?? null
    if (button === null || button.parentNode !== tree) return
    const row = visibleRows.find(item => item.key === button.dataset.key)
    if (row === undefined) return
    if (row.kind === 'directory') {
      if (row.expanded) collapsedDirectories.add(row.path)
      else collapsedDirectories.delete(row.path)
      renderTree()
      const nextIndex = visibleRows.findIndex(candidate => candidate.key === row.key)
      if (nextIndex >= 0) focusRow(nextIndex)
      return
    }
    options.onSelectFile(row.path)
  }
  const onTreeKeyDown = (event: KeyboardEvent) => {
    const target = event.target as HTMLElement | null
    if (target === null) return
    const index = visibleRows.findIndex(row => row.key === target.dataset.key)
    if (index < 0) return
    const row = visibleRows[index]
    if (row === undefined) return
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp' || event.key === 'Home' || event.key === 'End') {
      event.preventDefault()
      if (event.key === 'Home') focusRow(0)
      else if (event.key === 'End') focusRow(visibleRows.length - 1)
      else focusRow(Math.max(0, Math.min(visibleRows.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1))))
      return
    }
    if (event.key === 'ArrowRight' && row.kind === 'directory') {
      event.preventDefault()
      if (!row.expanded) {
        collapsedDirectories.delete(row.path)
        renderTree()
      } else {
        focusRow(index + 1)
      }
      return
    }
    if (event.key === 'ArrowLeft') {
      event.preventDefault()
      if (row.kind === 'directory' && row.expanded) {
        collapsedDirectories.add(row.path)
        renderTree()
        const nextIndex = visibleRows.findIndex(candidate => candidate.key === row.key)
        if (nextIndex >= 0) focusRow(nextIndex)
        return
      }
      const parent = row.path.includes('/') ? row.path.slice(0, row.path.lastIndexOf('/')) : null
      const parentIndex = visibleRows.findIndex(candidate => (
        candidate.kind === 'directory' && candidate.path === parent
      ))
      if (parentIndex >= 0) focusRow(parentIndex)
      return
    }
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault()
      target.click()
    }
  }
  const onSearch = () => { renderTree() }
  const onStatus = () => { renderTree() }
  const onLoadFiles = () => {
    if (current.candidateFiles.status === 'error') options.onLoadFiles()
    else options.onLoadMoreFiles()
  }
  tree.addEventListener('click', onTreeClick)
  tree.addEventListener('keydown', onTreeKeyDown)
  search.addEventListener('input', onSearch)
  status.addEventListener('change', onStatus)
  loadMoreFiles.addEventListener('click', onLoadFiles)

  function update(props: StrongFlowCandidateViewProps): void {
    if (!open) throw new Error('StrongFlow Candidate view is closed.')
    current = props
    const candidate = props.projection?.currentCandidate ?? null
    empty.hidden = candidate !== null
    workspace.hidden = candidate === null
    evidenceHeading.hidden = props.projection === null
    evidence.hidden = props.projection === null
    verdict.hidden = props.projection === null
    publication.hidden = props.projection === null
    if (candidate === null || props.projection === null) {
      requestedCandidateIdentity = null
      rowCollection.update([])
      return
    }
    const identity = candidateViewIdentity(candidate)
    if (props.candidateFiles.status !== 'idle') requestedCandidateIdentity = null
    if (props.candidateFiles.status === 'idle' && requestedCandidateIdentity !== identity) {
      requestedCandidateIdentity = identity
      options.onLoadFiles()
    }
    const fileSummary = candidateFileSummary(props.candidateFiles.items)
    summary.textContent = `${formatCount(fileSummary.total)} files loaded · +${formatCount(
      fileSummary.additions,
    )} -${formatCount(fileSummary.deletions)} · ${formatCount(
      fileSummary.unavailable,
    )} unavailable previews${props.candidateFiles.previewLimited ? ' · preview limit reached' : ''}`
    renderTree()
    loadMoreFiles.hidden = !props.candidateFiles.hasMore
      && props.candidateFiles.status !== 'error'
    loadMoreFiles.textContent = props.candidateFiles.status === 'error'
      ? 'Retry changed files'
      : 'Load more changed files'
    loadMoreFiles.disabled = props.candidateFiles.status === 'loading-more'

    const selectedDiff = props.candidateFiles.diff
    diffViewer.update({
      diff: selectedDiff,
      selectedPath: props.candidateFiles.selectedPath,
      viewMode: props.viewMode,
      candidateDigest: candidate.diffSha256,
      selectedLine: props.selectedLine,
    })

    technicalList.replaceChildren(
      ...definition(document, 'Candidate reference', candidate.candidateRef),
      ...definition(document, 'Commit', candidate.candidateCommitId),
      ...definition(document, 'Tree', candidate.candidateTreeId),
      ...definition(document, 'Candidate Diff digest', candidate.diffSha256),
      ...definition(document, 'File Diff digest', selectedDiff.fileDiffSha256 ?? 'Not loaded'),
      ...definition(document, 'Frozen', candidate.frozenAt),
    )

    const boundedEvidence = boundedItems(props.projection.evidence, options.limits.evidence)
    evidence.replaceChildren(...boundedEvidence.items.map(item => {
      const row = document.createElement('li')
      const title = document.createElement('strong')
      const source = document.createElement('p')
      title.textContent = `${item.type} · ${item.id}`
      source.textContent = item.sourceRef
      row.dataset.candidateRef = item.candidateRef
      row.append(title, source)
      return row
    }))
    evidenceOmitted.hidden = boundedEvidence.omitted === 0
    evidenceOmitted.textContent = boundedEvidence.omitted === 0
      ? ''
      : `${String(boundedEvidence.omitted)} more evidence records not shown.`

    verdict.dataset.status = props.projection.verdict?.status ?? ''
    if (props.projection.verdict === null) {
      verdictBody.textContent = 'No current Verdict is available.'
    } else {
      verdictBody.textContent = `${props.projection.verdict.status} · produced ${
        props.projection.verdict.producedAt
      }`
    }
    publication.dataset.status = props.projection.publication?.state ?? ''
    if (props.projection.publication === null) {
      publicationBody.textContent = 'No Publication has been created.'
    } else {
      publicationBody.textContent = `${props.projection.publication.state} · revision ${String(
        props.projection.publication.revision,
      )} · updated ${props.projection.publication.updatedAt}`
    }
  }

  update(current)
  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      tree.removeEventListener('click', onTreeClick)
      tree.removeEventListener('keydown', onTreeKeyDown)
      search.removeEventListener('input', onSearch)
      status.removeEventListener('change', onStatus)
      loadMoreFiles.removeEventListener('click', onLoadFiles)
      diffViewer.close()
      rowCollection.close()
      root.remove()
    },
  }
}

function definition(
  document: Document,
  termText: string,
  valueText: string,
): readonly [HTMLElement, HTMLElement] {
  const term = document.createElement('dt')
  const value = document.createElement('dd')
  term.textContent = termText
  value.textContent = valueText
  return [term, value]
}
