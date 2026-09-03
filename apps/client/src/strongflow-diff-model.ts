// SPDX-License-Identifier: Apache-2.0

/** The two layouts the Candidate Diff viewer renders for one exact file Diff. */
export type CandidateDiffViewMode = 'unified' | 'side-by-side'

export type CandidateDiffLineKind = 'context' | 'addition' | 'deletion'

export interface CandidateDiffLine {
  readonly kind: CandidateDiffLineKind
  readonly text: string
  readonly oldLine: number | null
  readonly newLine: number | null
}

export interface CandidateDiffHunk {
  readonly key: string
  readonly header: string
  readonly oldStart: number
  readonly newStart: number
  readonly lines: readonly CandidateDiffLine[]
}

export interface ParsedCandidateDiff {
  readonly fileHeaders: readonly string[]
  readonly hunks: readonly CandidateDiffHunk[]
  readonly lineCount: number
  /** True when the bounded Diff bytes end inside a line instead of at a line break. */
  readonly truncatedTail: boolean
}

interface HunkHeaderParts {
  readonly header: string
  readonly oldStart: number
  readonly newStart: number
}

function hunkHeader(line: string, index: number): HunkHeaderParts | null {
  const match = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/u.exec(line)
  if (match === null) return null
  return {
    header: line,
    oldStart: Number(match[1]),
    newStart: Number(match[2]),
  }
}

/** Parse one bounded per-file unified Git Diff into exact hunks, line numbers, and line kinds. */
export function parseCandidateDiff(content: string): ParsedCandidateDiff {
  const body = content.replace(/\r\n?/gu, '\n')
  const lines = body.split('\n')
  if (lines.at(-1) === '') lines.pop()
  const truncatedTail = !body.endsWith('\n')
  const fileHeaders: string[] = []
  const hunks: CandidateDiffHunk[] = []
  let current: { parts: HunkHeaderParts, lines: CandidateDiffLine[] } | null = null
  let oldLine = 0
  let newLine = 0
  let lineCount = 0
  for (const line of lines) {
    if (line.startsWith('\\ ')) continue
    const parts = hunkHeader(line, hunks.length + 1)
    if (parts !== null) {
      current = { parts, lines: [] }
      hunks.push(Object.freeze({
        key: `hunk:${String(hunks.length + 1)}`,
        header: parts.header,
        oldStart: parts.oldStart,
        newStart: parts.newStart,
        lines: current.lines,
      }))
      oldLine = parts.oldStart
      newLine = parts.newStart
      continue
    }
    if (current === null) {
      if (line.length > 0) fileHeaders.push(line)
      continue
    }
    const marker = line.charAt(0)
    const text = line.slice(1)
    if (marker === '+') {
      current.lines.push(Object.freeze({
        kind: 'addition', text, oldLine: null, newLine,
      }))
      newLine += 1
      lineCount += 1
      continue
    }
    if (marker === '-') {
      current.lines.push(Object.freeze({
        kind: 'deletion', text, oldLine, newLine: null,
      }))
      oldLine += 1
      lineCount += 1
      continue
    }
    current.lines.push(Object.freeze({
      kind: 'context', text, oldLine, newLine,
    }))
    oldLine += 1
    newLine += 1
    lineCount += 1
  }
  return Object.freeze({
    fileHeaders: Object.freeze(fileHeaders),
    hunks: Object.freeze(hunks.map(hunk => Object.freeze({
      ...hunk,
      lines: Object.freeze([...hunk.lines]),
    }))),
    lineCount,
    truncatedTail,
  })
}

export interface CandidateDiffFileHeaderRow {
  readonly kind: 'file-header'
  readonly key: string
  readonly text: string
}

export interface CandidateDiffHunkHeaderRow {
  readonly kind: 'hunk-header'
  readonly key: string
  readonly hunkKey: string
  readonly header: string
  readonly expanded: boolean
  readonly hiddenContext: number
}

export interface CandidateDiffLineRow {
  readonly kind: 'line'
  readonly key: string
  readonly hunkKey: string
  /** Side-by-side rows carry `modified` when one removed line is paired with one added line. */
  readonly type: CandidateDiffLineKind | 'modified'
  readonly oldLine: number | null
  readonly newLine: number | null
  readonly oldText: string
  readonly newText: string
}

export type CandidateDiffRow =
  | CandidateDiffFileHeaderRow
  | CandidateDiffHunkHeaderRow
  | CandidateDiffLineRow

export interface CandidateDiffRowsOptions {
  readonly mode: CandidateDiffViewMode
  /** Hunks whose unchanged context lines are collapsed away. */
  readonly collapsedContextHunks: ReadonlySet<string>
  readonly limit: number
}

export interface CandidateDiffRowsResult {
  readonly rows: readonly CandidateDiffRow[]
  readonly totalRows: number
  readonly omittedRows: number
}

function lineRow(hunkKey: string, key: string, line: CandidateDiffLine): CandidateDiffLineRow {
  return Object.freeze({
    kind: 'line',
    key,
    hunkKey,
    type: line.kind,
    oldLine: line.oldLine,
    newLine: line.newLine,
    oldText: line.text,
    newText: line.text,
  })
}

function sideBySideLineRow(
  hunkKey: string,
  key: string,
  old: CandidateDiffLine | null,
  new_: CandidateDiffLine | null,
): CandidateDiffLineRow {
  const type: CandidateDiffLineKind | 'modified' = old === null
    ? 'addition'
    : new_ === null
      ? 'deletion'
      : old.text === new_.text
        ? 'context'
        : 'modified'
  return Object.freeze({
    kind: 'line',
    key,
    hunkKey,
    type,
    oldLine: old?.oldLine ?? null,
    newLine: new_?.newLine ?? null,
    oldText: old?.text ?? '',
    newText: new_?.text ?? '',
  })
}

function unifiedHunkRows(hunk: CandidateDiffHunk): readonly CandidateDiffRow[] {
  return hunk.lines.map((line, index) => lineRow(hunk.key, `${hunk.key}:line:${String(index)}`, line))
}

function sideBySideHunkRows(hunk: CandidateDiffHunk): readonly CandidateDiffRow[] {
  const rows: CandidateDiffRow[] = []
  let index = 0
  let current = hunk.lines[index]
  while (current !== undefined) {
    if (current.kind === 'context') {
      rows.push(sideBySideLineRow(
        hunk.key,
        `${hunk.key}:pair:${String(rows.length)}`,
        current,
        current,
      ))
      index += 1
      current = hunk.lines[index]
      continue
    }
    const deletions: CandidateDiffLine[] = []
    const additions: CandidateDiffLine[] = []
    while (current !== undefined && current.kind === 'deletion') {
      deletions.push(current)
      index += 1
      current = hunk.lines[index]
    }
    while (current !== undefined && current.kind === 'addition') {
      additions.push(current)
      index += 1
      current = hunk.lines[index]
    }
    const pairCount = Math.max(deletions.length, additions.length)
    for (let pair = 0; pair < pairCount; pair += 1) {
      rows.push(sideBySideLineRow(
        hunk.key,
        `${hunk.key}:pair:${String(rows.length)}`,
        deletions[pair] ?? null,
        additions[pair] ?? null,
      ))
    }
  }
  return rows
}

/** Build the bounded, presentation-ready row window for one Candidate Diff layout. */
export function candidateDiffRows(
  parsed: ParsedCandidateDiff,
  options: CandidateDiffRowsOptions,
): CandidateDiffRowsResult {
  if (!Number.isInteger(options.limit) || options.limit < 1 || options.limit > 500) {
    throw new RangeError('Candidate Diff row limits must be integers between 1 and 500.')
  }
  const unbounded: CandidateDiffRow[] = parsed.fileHeaders.map((text, index) => Object.freeze({
    kind: 'file-header',
    key: `file-header:${String(index)}`,
    text,
  }))
  for (const hunk of parsed.hunks) {
    const collapsed = options.collapsedContextHunks.has(hunk.key)
    const hiddenContext = collapsed
      ? hunk.lines.filter(line => line.kind === 'context').length
      : 0
    unbounded.push(Object.freeze({
      kind: 'hunk-header',
      key: `hunk-header:${hunk.key}`,
      hunkKey: hunk.key,
      header: hunk.header,
      expanded: !collapsed,
      hiddenContext,
    }))
    const hunkRows = options.mode === 'side-by-side'
      ? sideBySideHunkRows(hunk)
      : unifiedHunkRows(hunk)
    for (const row of hunkRows) {
      if (collapsed && row.kind === 'line' && row.type === 'context') continue
      unbounded.push(row)
    }
  }
  const bounded = unbounded.slice(0, options.limit)
  return Object.freeze({
    rows: Object.freeze(bounded),
    totalRows: unbounded.length,
    omittedRows: Math.max(0, unbounded.length - bounded.length),
  })
}

/** Narrow viewports always fall back to the unified layout so lines stay readable. */
export function effectiveCandidateDiffView(
  preferred: CandidateDiffViewMode,
  narrowViewport: boolean,
): CandidateDiffViewMode {
  return narrowViewport ? 'unified' : preferred
}

/** Match the already rendered rows only, so searching never reads an unbounded Diff. */
export function candidateDiffMatchKeys(
  rows: readonly CandidateDiffRow[],
  query: string,
  limit: number,
): readonly string[] {
  if (!Number.isInteger(limit) || limit < 1 || limit > 500) {
    throw new RangeError('Candidate Diff match limits must be integers between 1 and 500.')
  }
  const needle = query.trim().toLocaleLowerCase()
  if (needle.length === 0) return Object.freeze([])
  const keys: string[] = []
  for (const row of rows) {
    if (row.kind !== 'line') continue
    const haystack = [row.oldText, row.newText]
    if (haystack.some(text => text.toLocaleLowerCase().includes(needle))) {
      keys.push(row.key)
      if (keys.length >= limit) break
    }
  }
  return Object.freeze(keys)
}
