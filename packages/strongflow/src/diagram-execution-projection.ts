import { createHash } from 'node:crypto'

import {
  STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL,
  STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION,
  parseFrozenDeliveryCandidate,
  parseStrongFlowDiagramExecutionProjection,
  parseStrongFlowPlanReviewContextText,
  type Delivery,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type StrongFlowDiagramDiffFile,
  type StrongFlowDiagramDiffHunk,
  type StrongFlowDiagramExecutionDiagram,
  type StrongFlowDiagramExecutionProjection,
  type StrongFlowDiagramExecutionState,
  type StrongFlowDiagramNodeState,
  type StrongFlowPlanReviewContext,
  type StrongFlowPlanReviewDiagram,
} from '@winwincode/contracts'

import { assertFrozenDeliveryCandidateCurrent } from './candidate-evidence.js'
import { DeliveryRuntimeProjection } from './delivery-runtime-projection.js'
import type { StrongFlowExecutionFacts } from './execution-source.js'

export type StrongFlowDiagramExecutionProjectionErrorCode =
  | 'INVALID_PROJECTION_FACTS'
  | 'RUNTIME_PROJECTION_FAILED'
  | 'CANDIDATE_STALE'
  | 'AUTHORITATIVE_DIFF_MISSING'
  | 'AUTHORITATIVE_DIFF_INVALID'

export class StrongFlowDiagramExecutionProjectionError extends Error {
  readonly code: StrongFlowDiagramExecutionProjectionErrorCode

  constructor(
    code: StrongFlowDiagramExecutionProjectionErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowDiagramExecutionProjectionError'
    this.code = code
  }
}

interface LineSlice {
  readonly text: string
  readonly content: string
  readonly start: number
  readonly end: number
}

interface DiffSection {
  readonly lines: readonly LineSlice[]
  readonly content: string
}

interface ParsedDiff {
  readonly files: readonly StrongFlowDiagramDiffFile[]
  readonly hunks: readonly StrongFlowDiagramDiffHunk[]
  readonly additions: number
  readonly deletions: number
}

const PROCESS_NODE_BY_STAGE = Object.freeze({
  executing: 'process:executing',
  reworking: 'process:reworking',
} as const)

function projectionError(
  code: StrongFlowDiagramExecutionProjectionErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowDiagramExecutionProjectionError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function immutable<Value>(value: Value): Value {
  const clone = structuredClone(value)
  const pending: object[] = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()!
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function digest(value: string): string {
  return createHash('sha256').update(value).digest('hex')
}

function latestReviewContext(delivery: Delivery): StrongFlowPlanReviewContext | null {
  const matches = delivery.attentionItems.flatMap((item) => {
    try {
      const context = parseStrongFlowPlanReviewContextText(item.context)
      return context.deliveryId === delivery.id
        && context.deliverySpecId === delivery.spec.id
        && context.deliverySpecRevision === delivery.spec.revision
        ? [{ context, createdAtMillis: item.createdAtMillis }]
        : []
    } catch {
      return []
    }
  }).toSorted((left, right) => right.createdAtMillis - left.createdAtMillis)
  return matches[0]?.context ?? null
}

function writerRuns(delivery: Delivery) {
  return delivery.stageRuns.filter(run => (
    run.actorType === 'codex'
    && (run.stage === 'executing' || run.stage === 'reworking')
  ))
}

function latestWriter(delivery: Delivery) {
  return writerRuns(delivery).toSorted((left, right) => (
    right.startedAtMillis - left.startedAtMillis
    || right.attempt - left.attempt
    || right.id.localeCompare(left.id)
  ))[0] ?? null
}

function candidateFromEvents(events: readonly RuntimeEvent[]): FrozenDeliveryCandidate | null {
  for (const event of events.toReversed()) {
    const value = event.data.frozen_candidate ?? event.data.frozenCandidate
    if (value === undefined) continue
    try {
      return parseFrozenDeliveryCandidate(value, `runtime event ${event.id} frozen candidate`)
    } catch (error) {
      return projectionError(
        'INVALID_PROJECTION_FACTS',
        `runtime event ${event.id} contains a malformed frozen candidate`,
        error,
      )
    }
  }
  return null
}

function runtimeSnapshot(delivery: Delivery, events: readonly RuntimeEvent[]) {
  try {
    return new DeliveryRuntimeProjection({ delivery }).replay(events)
  } catch (error) {
    return projectionError(
      'RUNTIME_PROJECTION_FAILED',
      'diagram execution view could not rebuild the bound runtime events',
      error,
    )
  }
}

function scanLines(value: string): readonly LineSlice[] {
  const lines: LineSlice[] = []
  let start = 0
  while (start < value.length) {
    const newline = value.indexOf('\n', start)
    const end = newline < 0 ? value.length : newline + 1
    const content = value.slice(start, end)
    const withoutNewline = newline < 0 ? content : content.slice(0, -1)
    const text = withoutNewline.endsWith('\r')
      ? withoutNewline.slice(0, -1)
      : withoutNewline
    lines.push(Object.freeze({ text, content, start, end }))
    start = end
  }
  return Object.freeze(lines)
}

function splitDiffSections(value: string): readonly DiffSection[] {
  const lines = scanLines(value)
  const starts = lines.flatMap((line, index) => (
    line.text.startsWith('diff --git ') ? [index] : []
  ))
  if (starts.length === 0) {
    return projectionError(
      'AUTHORITATIVE_DIFF_INVALID',
      'authoritative diff does not contain Git file sections',
    )
  }
  return Object.freeze(starts.map((lineIndex, index) => {
    const endIndex = starts[index + 1] ?? lines.length
    const sectionLines = lines.slice(lineIndex, endIndex)
    const start = sectionLines[0]!.start
    const end = sectionLines.at(-1)!.end
    return Object.freeze({
      lines: Object.freeze(sectionLines),
      content: value.slice(start, end),
    })
  }))
}

function decodeGitQuotedPath(value: string): string {
  if (!value.startsWith('"') || !value.endsWith('"')) return value
  const body = value.slice(1, -1)
  const bytes: number[] = []
  const encoder = new TextEncoder()
  for (let index = 0; index < body.length; index += 1) {
    const character = String.fromCodePoint(body.codePointAt(index)!)
    if (character !== '\\') {
      bytes.push(...encoder.encode(character))
      index += character.length - 1
      continue
    }
    const next = body[index + 1]
    if (next === undefined) return projectionError('AUTHORITATIVE_DIFF_INVALID', 'Git path quote is incomplete')
    index += 1
    if (next === 'n') bytes.push(0x0a)
    else if (next === 'r') bytes.push(0x0d)
    else if (next === 't') bytes.push(0x09)
    else if (next === '"' || next === '\\') bytes.push(next.codePointAt(0)!)
    else if (/[0-7]/u.test(next)) {
      const octal = `${next}${body[index + 1] ?? ''}${body[index + 2] ?? ''}`.match(/^[0-7]{1,3}/u)?.[0] ?? next
      const byte = Number.parseInt(octal, 8)
      if (byte > 0xff) {
        return projectionError('AUTHORITATIVE_DIFF_INVALID', 'Git path quote contains an invalid byte')
      }
      bytes.push(byte)
      index += octal.length - 1
    } else bytes.push(...encoder.encode(next))
  }
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(Uint8Array.from(bytes))
  } catch (error) {
    return projectionError(
      'AUTHORITATIVE_DIFF_INVALID',
      'Git path quote is not valid UTF-8',
      error,
    )
  }
}

function headerPath(line: string, prefix: '--- ' | '+++ '): string | null {
  if (!line.startsWith(prefix)) return null
  const encoded = line.slice(prefix.length)
  if (encoded === '/dev/null') return null
  const decoded = decodeGitQuotedPath(encoded)
  if (!decoded.startsWith('a/') && !decoded.startsWith('b/')) {
    return projectionError('AUTHORITATIVE_DIFF_INVALID', 'Git diff path lacks an a/ or b/ prefix')
  }
  return decoded.slice(2)
}

function sectionPaths(section: DiffSection): {
  readonly path: string | null
  readonly previousPath: string | null
} {
  const previous = section.lines.flatMap(line => {
    const path = headerPath(line.text, '--- ')
    return line.text.startsWith('--- ') ? [path] : []
  })[0]
  const current = section.lines.flatMap(line => {
    const path = headerPath(line.text, '+++ ')
    return line.text.startsWith('+++ ') ? [path] : []
  })[0]
  return Object.freeze({
    path: current ?? previous ?? null,
    previousPath: previous !== undefined && previous !== null && previous !== current
      ? previous
      : null,
  })
}

function fallbackSectionPath(
  section: DiffSection,
  remainingPaths: readonly string[],
): string | null {
  const exact = remainingPaths.filter(path => (
    section.lines[0]?.text.includes(` a/${path} b/${path}`) === true
    || section.content.includes(`+++ b/${path}\n`)
    || section.content.includes(`--- a/${path}\n`)
  ))
  return exact.length === 1 ? exact[0]! : remainingPaths.length === 1 ? remainingPaths[0]! : null
}

function countChangedLines(lines: readonly LineSlice[]): {
  readonly additions: number
  readonly deletions: number
} {
  let additions = 0
  let deletions = 0
  for (const line of lines) {
    if (line.text.startsWith('+') && !line.text.startsWith('+++')) additions += 1
    if (line.text.startsWith('-') && !line.text.startsWith('---')) deletions += 1
  }
  return Object.freeze({ additions, deletions })
}

function sectionHunks(
  section: DiffSection,
  fileId: string,
): readonly StrongFlowDiagramDiffHunk[] {
  const starts = section.lines.flatMap((line, index) => (
    line.text.startsWith('@@ ') ? [index] : []
  ))
  const ranges = starts.length === 0
    ? [[0, section.lines.length] as const]
    : starts.map((start, index) => [start, starts[index + 1] ?? section.lines.length] as const)
  return Object.freeze(ranges.map(([start, end], index) => {
    const lines = section.lines.slice(start, end)
    const content = lines.map(line => line.content).join('')
    const sha256 = digest(content)
    const counts = countChangedLines(lines)
    return Object.freeze({
      id: `diagram-hunk:sha256:${digest(`${fileId}\u0000${content}`)}`,
      fileId,
      sha256,
      header: starts.length === 0
        ? `Diff metadata ${String(index + 1)}`
        : lines[0]!.text,
      content,
      additions: counts.additions,
      deletions: counts.deletions,
    })
  }))
}

function pathMatchesPrefix(path: string, prefix: string): boolean {
  return path === prefix || path.startsWith(`${prefix}/`)
}

function architectureNodeIds(
  context: StrongFlowPlanReviewContext,
  path: string,
): readonly string[] {
  const matches = context.solution.components.filter(component => (
    component.repositoryPathPrefixes.some(prefix => pathMatchesPrefix(path, prefix))
  ))
  return Object.freeze([
    'platform:repository',
    ...matches.map(component => component.id),
  ])
}

function parseAuthoritativeDiff(
  context: StrongFlowPlanReviewContext,
  candidate: FrozenDeliveryCandidate,
  unifiedDiff: string,
  processNodeId: string,
): ParsedDiff {
  if (digest(unifiedDiff) !== candidate.diffSha256) {
    return projectionError(
      'AUTHORITATIVE_DIFF_INVALID',
      'authoritative diff bytes do not match the frozen candidate identity',
    )
  }
  const sections = splitDiffSections(unifiedDiff)
  if (sections.length !== candidate.changedPaths.length) {
    return projectionError(
      'AUTHORITATIVE_DIFF_INVALID',
      'authoritative diff file total does not match the frozen candidate',
    )
  }
  const remaining = new Map(candidate.changedPaths.map(fact => [fact.path, fact]))
  const files: StrongFlowDiagramDiffFile[] = []
  const hunks: StrongFlowDiagramDiffHunk[] = []
  for (const section of sections) {
    const paths = sectionPaths(section)
    const path = paths.path ?? fallbackSectionPath(section, [...remaining.keys()])
    const fact = path === null ? undefined : remaining.get(path)
    if (path === null || fact === undefined) {
      return projectionError(
        'AUTHORITATIVE_DIFF_INVALID',
        'authoritative diff contains a path outside the frozen candidate',
      )
    }
    remaining.delete(path)
    const fileId = `diagram-file:sha256:${digest(`${candidate.candidateRef}\u0000${path}`)}`
    const fileHunks = sectionHunks(section, fileId)
    const counts = countChangedLines(section.lines)
    const nodeIds = [...architectureNodeIds(context, path), processNodeId]
    files.push(Object.freeze({
      id: fileId,
      path,
      previousPath: paths.previousPath,
      state: fact.state,
      additions: counts.additions,
      deletions: counts.deletions,
      hunkIds: Object.freeze(fileHunks.map(hunk => hunk.id)),
      nodeIds: Object.freeze(nodeIds),
    }))
    hunks.push(...fileHunks)
  }
  if (remaining.size > 0) {
    return projectionError(
      'AUTHORITATIVE_DIFF_INVALID',
      'authoritative diff omits a frozen candidate path',
    )
  }
  return Object.freeze({
    files: Object.freeze(files.toSorted((left, right) => left.path.localeCompare(right.path))),
    hunks: Object.freeze(hunks),
    additions: files.reduce((sum, file) => sum + file.additions, 0),
    deletions: files.reduce((sum, file) => sum + file.deletions, 0),
  })
}

function authoritativeDiff(
  candidate: FrozenDeliveryCandidate,
  events: readonly RuntimeEvent[],
  candidateDiff: string | null | undefined,
): string {
  if (candidateDiff !== undefined && candidateDiff !== null) {
    if (digest(candidateDiff) !== candidate.diffSha256) {
      return projectionError(
        'AUTHORITATIVE_DIFF_INVALID',
        'Git candidate diff bytes do not match the frozen candidate identity',
      )
    }
    return candidateDiff
  }
  const matches = events.filter(event => (
    event.kind === 'diff.updated'
    && typeof event.data.unified_diff === 'string'
    && digest(event.data.unified_diff) === candidate.diffSha256
  ))
  const diff = matches.at(-1)?.data.unified_diff
  if (typeof diff !== 'string') {
    return projectionError(
      'AUTHORITATIVE_DIFF_MISSING',
      'the frozen candidate has no matching authoritative runtime diff',
    )
  }
  return diff
}

function affectedPathsForWriter(
  delivery: Delivery,
  events: readonly RuntimeEvent[],
  writerId: string,
): readonly string[] {
  const snapshot = runtimeSnapshot(delivery, events)
  return snapshot.stages.find(stage => stage.stageRun.id === writerId)?.changedFiles ?? []
}

function annotatedNodeIds(delivery: Delivery): readonly string[] {
  const matches: { readonly resolvedAtMillis: number; readonly ids: readonly string[] }[] = []
  for (const item of delivery.attentionItems) {
    if (item.resolution === null) continue
    try {
      const value: unknown = JSON.parse(item.resolution)
      if (!isRecord(value)
        || value.protocol !== 'winwincode.delivery-remediation.v1'
        || !Array.isArray(value.annotations)) continue
      const ids = new Set<string>()
      for (const annotation of value.annotations) {
        if (isRecord(annotation) && typeof annotation.nodeId === 'string') ids.add(annotation.nodeId)
      }
      matches.push(Object.freeze({
        resolvedAtMillis: item.resolvedAtMillis ?? item.createdAtMillis,
        ids: Object.freeze([...ids]),
      }))
    } catch {
      // Other Attention protocols are intentionally ignored.
    }
  }
  return matches.toSorted((left, right) => right.resolvedAtMillis - left.resolvedAtMillis)[0]?.ids
    ?? Object.freeze([])
}

function fileCountByNode(
  context: StrongFlowPlanReviewContext,
  paths: readonly string[],
  processNodeId: string,
): Map<string, number> {
  const counts = new Map<string, number>()
  for (const path of paths) {
    for (const nodeId of [...architectureNodeIds(context, path), processNodeId]) {
      counts.set(nodeId, (counts.get(nodeId) ?? 0) + 1)
    }
  }
  return counts
}

function diagramProjection(
  diagram: StrongFlowPlanReviewDiagram,
  state: StrongFlowDiagramExecutionState,
  counts: ReadonlyMap<string, number>,
  filesByNode: ReadonlyMap<string, readonly string[]>,
): StrongFlowDiagramExecutionDiagram {
  const affectedState: StrongFlowDiagramNodeState = state === 'executing'
    ? 'affected-live'
    : 'affected-finished'
  return Object.freeze({
    diagramId: diagram.id,
    kind: diagram.kind,
    nodes: Object.freeze(diagram.nodes.map((node) => {
      const affectedFileCount = counts.get(node.id) ?? 0
      return Object.freeze({
        nodeId: node.id,
        state: affectedFileCount === 0 ? 'normal' as const : affectedState,
        affectedFileCount,
        fileIds: state === 'execution-finished'
          ? Object.freeze([...(filesByNode.get(node.id) ?? [])])
          : Object.freeze([]),
      })
    })),
  })
}

function finishedProjection(
  delivery: Delivery,
  context: StrongFlowPlanReviewContext,
  events: readonly RuntimeEvent[],
  candidate: FrozenDeliveryCandidate,
  candidateDiff: string | null | undefined,
): StrongFlowDiagramExecutionProjection {
  let current: FrozenDeliveryCandidate
  try {
    current = assertFrozenDeliveryCandidateCurrent(delivery, candidate)
  } catch (error) {
    return projectionError('CANDIDATE_STALE', 'diagram candidate is no longer current', error)
  }
  const run = delivery.stageRuns.find(entry => entry.id === current.producerStageRunId)
  const binding = delivery.sessionBindings.find(entry => entry.id === current.producerSessionBindingId)
  if (run === undefined
    || binding === undefined
    || (run.stage !== 'executing' && run.stage !== 'reworking')
    || run.finishedAtMillis === null
    || binding.dshSessionId === null
    || binding.codexSessionId === null) {
    return projectionError(
      'CANDIDATE_STALE',
      'diagram candidate lacks its exact completed producer provenance',
    )
  }
  const parsedDiff = parseAuthoritativeDiff(
    context,
    current,
    authoritativeDiff(current, events, candidateDiff),
    PROCESS_NODE_BY_STAGE[run.stage],
  )
  const filesByNode = new Map<string, string[]>()
  for (const file of parsedDiff.files) {
    for (const nodeId of file.nodeIds) {
      const ids = filesByNode.get(nodeId) ?? []
      ids.push(file.id)
      filesByNode.set(nodeId, ids)
    }
  }
  const counts = new Map([...filesByNode].map(([nodeId, ids]) => [nodeId, ids.length]))
  const snapshot = runtimeSnapshot(delivery, events)
  const stage = snapshot.stages.find(entry => entry.stageRun.id === run.id)
  const eventById = new Map(events.map(event => [event.id, event]))
  const sessions = stage?.sessions.filter(session => session.binding.id === binding.id) ?? []
  const agents = sessions.flatMap(session => session.agents).map(agent => Object.freeze({
    threadId: agent.threadId,
    path: agent.path,
    role: agent.role,
    status: agent.status,
  }))
  const activities = sessions.flatMap(session => session.activities).map(activity => Object.freeze({
    callId: activity.callId,
    type: activity.activityType,
    command: activity.command,
    status: activity.status,
    outcome: activity.outcome,
    exitCode: activity.exitCode,
    occurredAtMillis: eventById.get(activity.latestEvent.eventId)?.occurredAtMillis ?? null,
  }))
  const evidenceRefIds = delivery.evidence.filter(evidence => (
    evidence.candidateRef === current.candidateRef
  )).map(evidence => evidence.id)
  return parseStrongFlowDiagramExecutionProjection({
    schemaVersion: STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION,
    protocol: STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL,
    deliveryId: delivery.id,
    deliveryRevision: delivery.revision,
    reviewSetSha256: context.reviewSetSha256,
    state: 'execution-finished',
    architecture: diagramProjection(
      context.architectureDiagram,
      'execution-finished',
      counts,
      filesByNode,
    ),
    process: diagramProjection(
      context.processDiagram,
      'execution-finished',
      counts,
      filesByNode,
    ),
    affectedFileCount: parsedDiff.files.length,
    details: {
      candidate: current,
      diffSha256: current.diffSha256,
      files: parsedDiff.files,
      hunks: parsedDiff.hunks,
      additions: parsedDiff.additions,
      deletions: parsedDiff.deletions,
      provenance: {
        stageRunId: run.id,
        sessionBindingId: binding.id,
        deliveryTaskId: run.deliveryTaskId,
        stage: run.stage,
        role: run.role,
        attempt: run.attempt,
        dshSessionId: binding.dshSessionId,
        codexSessionId: binding.codexSessionId,
        startedAtMillis: run.startedAtMillis,
        finishedAtMillis: run.finishedAtMillis,
        agents,
        activities,
        evidenceRefIds,
      },
    },
    updatedAtMillis: Math.max(delivery.updatedAtMillis, run.finishedAtMillis),
  })
}

function nonFinishedProjection(
  delivery: Delivery,
  context: StrongFlowPlanReviewContext,
  events: readonly RuntimeEvent[],
  state: 'before-execution' | 'executing',
): StrongFlowDiagramExecutionProjection {
  const writer = latestWriter(delivery)
  const processNodeId = writer?.stage === 'reworking'
    || delivery.status === 'reworking'
    ? PROCESS_NODE_BY_STAGE.reworking
    : PROCESS_NODE_BY_STAGE.executing
  const paths = state === 'executing' && writer !== null
    ? affectedPathsForWriter(delivery, events, writer.id)
    : []
  const counts = fileCountByNode(context, paths, processNodeId)
  if (state === 'executing') {
    counts.set(processNodeId, Math.max(counts.get(processNodeId) ?? 0, 1))
  }
  if (state === 'executing' && paths.length === 0) {
    for (const nodeId of annotatedNodeIds(delivery)) counts.set(nodeId, 1)
  }
  return parseStrongFlowDiagramExecutionProjection({
    schemaVersion: STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION,
    protocol: STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL,
    deliveryId: delivery.id,
    deliveryRevision: delivery.revision,
    reviewSetSha256: context.reviewSetSha256,
    state,
    architecture: diagramProjection(context.architectureDiagram, state, counts, new Map()),
    process: diagramProjection(context.processDiagram, state, counts, new Map()),
    affectedFileCount: paths.length,
    details: null,
    updatedAtMillis: Math.max(
      delivery.updatedAtMillis,
      ...events.map(event => event.occurredAtMillis ?? 0),
    ),
  })
}

/**
 * Build the same two approved diagrams in one of three states. Live projections
 * deliberately contain no paths, hunks, commands, or evidence detail.
 */
export function projectStrongFlowDiagramExecution(
  delivery: Delivery,
  facts: StrongFlowExecutionFacts,
): StrongFlowDiagramExecutionProjection | null {
  if (!isRecord(facts) || !Array.isArray(facts.runtimeEvents)) {
    return projectionError('INVALID_PROJECTION_FACTS', 'diagram execution facts are invalid')
  }
  const context = latestReviewContext(delivery)
  if (context === null) return null
  const writer = latestWriter(delivery)
  const extractedCandidate = facts.candidate ?? candidateFromEvents(facts.runtimeEvents)
  const reenteringExecution = delivery.status === 'executing' || delivery.status === 'reworking'
  if (!reenteringExecution && extractedCandidate !== null) {
    return finishedProjection(
      delivery,
      context,
      facts.runtimeEvents,
      extractedCandidate,
      facts.candidateDiff,
    )
  }
  if (reenteringExecution || writer !== null) {
    return nonFinishedProjection(delivery, context, facts.runtimeEvents, 'executing')
  }
  return nonFinishedProjection(delivery, context, facts.runtimeEvents, 'before-execution')
}

/** Exact annotation identity lookup used by the service before accepting rework. */
export function diagramExecutionAnnotationExists(
  projection: StrongFlowDiagramExecutionProjection,
  annotation: {
    readonly diagramKind: 'system-architecture' | 'process-flow'
    readonly diagramId: string
    readonly nodeId: string
    readonly filePath: string
    readonly hunkSha256: string
  },
): boolean {
  if (projection.state !== 'execution-finished' || projection.details === null) return false
  const diagram = annotation.diagramKind === 'system-architecture'
    ? projection.architecture
    : projection.process
  if (diagram.diagramId !== annotation.diagramId) return false
  const node = diagram.nodes.find(entry => entry.nodeId === annotation.nodeId)
  if (node?.state !== 'affected-finished') return false
  const file = projection.details.files.find(entry => (
    entry.path === annotation.filePath
    && entry.nodeIds.includes(annotation.nodeId)
    && node.fileIds.includes(entry.id)
  ))
  return file !== undefined && file.hunkIds.some(hunkId => (
    projection.details?.hunks.find(hunk => hunk.id === hunkId)?.sha256
      === annotation.hunkSha256
  ))
}

/** Return a frozen candidate carried by the normalized runtime ledger, if present. */
export function frozenCandidateFromRuntimeEvents(
  events: readonly RuntimeEvent[],
): FrozenDeliveryCandidate | null {
  return candidateFromEvents(events)
}
