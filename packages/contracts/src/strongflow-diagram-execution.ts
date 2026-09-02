import {
  parseFrozenDeliveryCandidate,
  type FrozenDeliveryCandidate,
} from './delivery-candidate.js'
import {
  DeliveryId,
  DeliveryTaskId,
  EvidenceRefId,
  SessionBindingId,
  StageRunId,
  type DeliveryId as DeliveryIdentifier,
  type DeliveryTaskId as DeliveryTaskIdentifier,
  type EvidenceRefId as EvidenceRefIdentifier,
  type SessionBindingId as SessionBindingIdentifier,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'
import {
  STRONGFLOW_PLAN_REVIEW_DIAGRAM_KINDS,
  type StrongFlowPlanReviewDiagramKind,
} from './strongflow-plan-review.js'

/** Rebuildable workbench view; this is not an additional persisted Delivery object. */
export const STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL =
  'winwincode.diagram-execution-projection.v1' as const

export const STRONGFLOW_DIAGRAM_EXECUTION_STATES = Object.freeze([
  'before-execution',
  'executing',
  'execution-finished',
] as const)

export type StrongFlowDiagramExecutionState =
  typeof STRONGFLOW_DIAGRAM_EXECUTION_STATES[number]

export const STRONGFLOW_DIAGRAM_NODE_STATES = Object.freeze([
  'normal',
  'affected-live',
  'affected-finished',
] as const)

export type StrongFlowDiagramNodeState = typeof STRONGFLOW_DIAGRAM_NODE_STATES[number]

export interface StrongFlowDiagramExecutionNode {
  readonly nodeId: string
  readonly state: StrongFlowDiagramNodeState
  readonly affectedFileCount: number
  /** Exact file identities are available only after candidate freeze. */
  readonly fileIds: readonly string[]
}

export interface StrongFlowDiagramExecutionDiagram {
  readonly diagramId: string
  readonly kind: StrongFlowPlanReviewDiagramKind
  readonly nodes: readonly StrongFlowDiagramExecutionNode[]
}

export interface StrongFlowDiagramDiffFile {
  readonly id: string
  readonly path: string
  readonly previousPath: string | null
  readonly state: 'present' | 'deleted'
  readonly additions: number
  readonly deletions: number
  readonly hunkIds: readonly string[]
  readonly nodeIds: readonly string[]
}

export interface StrongFlowDiagramDiffHunk {
  readonly id: string
  readonly fileId: string
  readonly sha256: string
  readonly header: string
  /** Exact authoritative diff text, always rendered as text rather than markup. */
  readonly content: string
  readonly additions: number
  readonly deletions: number
}

export interface StrongFlowDiagramAgentProvenance {
  readonly threadId: string
  readonly path: string | null
  readonly role: string | null
  readonly status: string
}

export interface StrongFlowDiagramActivityProvenance {
  readonly callId: string
  readonly type: 'command' | 'test'
  readonly command: string | null
  readonly status: string
  readonly outcome: string
  readonly exitCode: number | null
  readonly occurredAtMillis: number | null
}

export interface StrongFlowDiagramExecutionProvenance {
  readonly stageRunId: StageRunIdentifier
  readonly sessionBindingId: SessionBindingIdentifier
  readonly deliveryTaskId: DeliveryTaskIdentifier | null
  readonly stage: 'executing' | 'reworking'
  readonly role: string
  readonly attempt: number
  readonly dshSessionId: string
  readonly codexSessionId: string
  readonly startedAtMillis: number
  readonly finishedAtMillis: number
  readonly agents: readonly StrongFlowDiagramAgentProvenance[]
  readonly activities: readonly StrongFlowDiagramActivityProvenance[]
  readonly evidenceRefIds: readonly EvidenceRefIdentifier[]
}

export interface StrongFlowDiagramExecutionDetails {
  readonly candidate: FrozenDeliveryCandidate
  readonly diffSha256: string
  readonly files: readonly StrongFlowDiagramDiffFile[]
  readonly hunks: readonly StrongFlowDiagramDiffHunk[]
  readonly additions: number
  readonly deletions: number
  readonly provenance: StrongFlowDiagramExecutionProvenance
}

export interface StrongFlowDiagramExecutionProjection {
  readonly schemaVersion: typeof STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL
  readonly deliveryId: DeliveryIdentifier
  readonly deliveryRevision: number
  readonly reviewSetSha256: string
  readonly state: StrongFlowDiagramExecutionState
  readonly architecture: StrongFlowDiagramExecutionDiagram
  readonly process: StrongFlowDiagramExecutionDiagram
  readonly affectedFileCount: number
  /** Concrete paths, hunks, commands, and evidence are absent until this is non-null. */
  readonly details: StrongFlowDiagramExecutionDetails | null
  readonly updatedAtMillis: number
}

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const MAX_COLLECTION_LENGTH = 100_000
const MAX_TEXT_LENGTH = 16 * 1_024 * 1_024

function failure(path: string, message: string): never {
  throw new TypeError(`${path} ${message}`)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) return failure(path, 'must be an object')
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  path: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    failure(path, 'has an unexpected shape')
  }
}

function text(value: unknown, path: string, allowEmpty = false): string {
  if (typeof value !== 'string'
    || value.length > MAX_TEXT_LENGTH
    || (!allowEmpty && value.trim().length === 0)
    || /[\u0000\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    return failure(path, 'must be bounded text')
  }
  return value
}

function portableId(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    return failure(path, 'must be a portable identifier')
  }
  return value
}

function sha256(value: unknown, path: string): string {
  if (typeof value !== 'string' || !SHA256_PATTERN.test(value)) {
    return failure(path, 'must be a lowercase SHA-256 digest')
  }
  return value
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    return failure(path, 'must be a non-negative safe integer')
  }
  return Number(value)
}

function positiveInteger(value: unknown, path: string): number {
  const parsed = nonNegativeInteger(value, path)
  if (parsed === 0) return failure(path, 'must be positive')
  return parsed
}

function timestamp(value: unknown, path: string, nullable = false): number | null {
  if (nullable && value === null) return null
  return nonNegativeInteger(value, path)
}

function array(value: unknown, path: string): readonly unknown[] {
  if (!Array.isArray(value) || value.length > MAX_COLLECTION_LENGTH) {
    return failure(path, 'must be a bounded array')
  }
  return value
}

function portablePath(value: unknown, path: string): string {
  const parsed = text(value, path)
  if (parsed.length > 4_096
    || parsed.startsWith('/')
    || parsed.includes('\\')
    || /^[A-Za-z]:/u.test(parsed)
    || parsed.split('/').some(segment => (
      segment.length === 0 || segment === '.' || segment === '..'
    ))) return failure(path, 'must be a repository-relative path')
  return parsed
}

function nullableText(value: unknown, path: string): string | null {
  return value === null ? null : text(value, path)
}

function identifier<Identifier>(
  value: unknown,
  path: string,
  factory: (input: string) => Identifier,
): Identifier {
  try {
    return factory(portableId(value, path))
  } catch {
    return failure(path, 'is invalid')
  }
}

function unique(values: readonly string[], path: string): void {
  if (new Set(values).size !== values.length) failure(path, 'must not contain duplicates')
}

function parseNode(value: unknown, path: string): StrongFlowDiagramExecutionNode {
  const input = record(value, path)
  exactKeys(input, ['nodeId', 'state', 'affectedFileCount', 'fileIds'], path)
  if (typeof input.state !== 'string'
    || !STRONGFLOW_DIAGRAM_NODE_STATES.includes(input.state as StrongFlowDiagramNodeState)) {
    failure(`${path}.state`, 'is unsupported')
  }
  const fileIds = array(input.fileIds, `${path}.fileIds`).map((entry, index) => (
    portableId(entry, `${path}.fileIds[${String(index)}]`)
  ))
  unique(fileIds, `${path}.fileIds`)
  const affectedFileCount = nonNegativeInteger(input.affectedFileCount, `${path}.affectedFileCount`)
  if ((input.state === 'normal') !== (affectedFileCount === 0)
    || (fileIds.length > 0 && fileIds.length !== affectedFileCount)) {
    failure(path, 'state and affected file identities do not agree')
  }
  return Object.freeze({
    nodeId: portableId(input.nodeId, `${path}.nodeId`),
    state: input.state as StrongFlowDiagramNodeState,
    affectedFileCount,
    fileIds: Object.freeze(fileIds),
  })
}

function parseDiagram(
  value: unknown,
  path: string,
): StrongFlowDiagramExecutionDiagram {
  const input = record(value, path)
  exactKeys(input, ['diagramId', 'kind', 'nodes'], path)
  if (typeof input.kind !== 'string'
    || !STRONGFLOW_PLAN_REVIEW_DIAGRAM_KINDS.includes(
      input.kind as StrongFlowPlanReviewDiagramKind,
    )) failure(`${path}.kind`, 'is unsupported')
  const nodes = array(input.nodes, `${path}.nodes`).map((entry, index) => (
    parseNode(entry, `${path}.nodes[${String(index)}]`)
  ))
  unique(nodes.map(node => node.nodeId), `${path}.nodes`)
  return Object.freeze({
    diagramId: portableId(input.diagramId, `${path}.diagramId`),
    kind: input.kind as StrongFlowPlanReviewDiagramKind,
    nodes: Object.freeze(nodes),
  })
}

function parseFile(value: unknown, path: string): StrongFlowDiagramDiffFile {
  const input = record(value, path)
  exactKeys(input, [
    'id',
    'path',
    'previousPath',
    'state',
    'additions',
    'deletions',
    'hunkIds',
    'nodeIds',
  ], path)
  if (input.state !== 'present' && input.state !== 'deleted') {
    failure(`${path}.state`, 'is unsupported')
  }
  const hunkIds = array(input.hunkIds, `${path}.hunkIds`).map((entry, index) => (
    portableId(entry, `${path}.hunkIds[${String(index)}]`)
  ))
  const nodeIds = array(input.nodeIds, `${path}.nodeIds`).map((entry, index) => (
    portableId(entry, `${path}.nodeIds[${String(index)}]`)
  ))
  unique(hunkIds, `${path}.hunkIds`)
  unique(nodeIds, `${path}.nodeIds`)
  return Object.freeze({
    id: portableId(input.id, `${path}.id`),
    path: portablePath(input.path, `${path}.path`),
    previousPath: input.previousPath === null
      ? null
      : portablePath(input.previousPath, `${path}.previousPath`),
    state: input.state,
    additions: nonNegativeInteger(input.additions, `${path}.additions`),
    deletions: nonNegativeInteger(input.deletions, `${path}.deletions`),
    hunkIds: Object.freeze(hunkIds),
    nodeIds: Object.freeze(nodeIds),
  })
}

function parseHunk(value: unknown, path: string): StrongFlowDiagramDiffHunk {
  const input = record(value, path)
  exactKeys(input, [
    'id',
    'fileId',
    'sha256',
    'header',
    'content',
    'additions',
    'deletions',
  ], path)
  return Object.freeze({
    id: portableId(input.id, `${path}.id`),
    fileId: portableId(input.fileId, `${path}.fileId`),
    sha256: sha256(input.sha256, `${path}.sha256`),
    header: text(input.header, `${path}.header`),
    content: text(input.content, `${path}.content`, true),
    additions: nonNegativeInteger(input.additions, `${path}.additions`),
    deletions: nonNegativeInteger(input.deletions, `${path}.deletions`),
  })
}

function parseProvenance(
  value: unknown,
  path: string,
): StrongFlowDiagramExecutionProvenance {
  const input = record(value, path)
  exactKeys(input, [
    'stageRunId',
    'sessionBindingId',
    'deliveryTaskId',
    'stage',
    'role',
    'attempt',
    'dshSessionId',
    'codexSessionId',
    'startedAtMillis',
    'finishedAtMillis',
    'agents',
    'activities',
    'evidenceRefIds',
  ], path)
  if (input.stage !== 'executing' && input.stage !== 'reworking') {
    failure(`${path}.stage`, 'must be an execution stage')
  }
  const agents = array(input.agents, `${path}.agents`).map((entry, index) => {
    const agentPath = `${path}.agents[${String(index)}]`
    const agent = record(entry, agentPath)
    exactKeys(agent, ['threadId', 'path', 'role', 'status'], agentPath)
    return Object.freeze({
      threadId: portableId(agent.threadId, `${agentPath}.threadId`),
      path: agent.path === null ? null : text(agent.path, `${agentPath}.path`),
      role: nullableText(agent.role, `${agentPath}.role`),
      status: portableId(agent.status, `${agentPath}.status`),
    })
  })
  unique(agents.map(agent => agent.threadId), `${path}.agents`)
  const activities = array(input.activities, `${path}.activities`).map((entry, index) => {
    const activityPath = `${path}.activities[${String(index)}]`
    const activity = record(entry, activityPath)
    exactKeys(activity, [
      'callId',
      'type',
      'command',
      'status',
      'outcome',
      'exitCode',
      'occurredAtMillis',
    ], activityPath)
    if (activity.type !== 'command' && activity.type !== 'test') {
      failure(`${activityPath}.type`, 'is unsupported')
    }
    if (activity.exitCode !== null
      && (!Number.isSafeInteger(activity.exitCode) || Object.is(activity.exitCode, -0))) {
      failure(`${activityPath}.exitCode`, 'must be an integer or null')
    }
    return Object.freeze({
      callId: portableId(activity.callId, `${activityPath}.callId`),
      type: activity.type,
      command: nullableText(activity.command, `${activityPath}.command`),
      status: portableId(activity.status, `${activityPath}.status`),
      outcome: portableId(activity.outcome, `${activityPath}.outcome`),
      exitCode: activity.exitCode === null ? null : Number(activity.exitCode),
      occurredAtMillis: timestamp(
        activity.occurredAtMillis,
        `${activityPath}.occurredAtMillis`,
        true,
      ),
    })
  })
  unique(activities.map(activity => activity.callId), `${path}.activities`)
  const evidenceRefIds = array(input.evidenceRefIds, `${path}.evidenceRefIds`).map(
    (entry, index) => identifier(
      entry,
      `${path}.evidenceRefIds[${String(index)}]`,
      EvidenceRefId,
    ),
  )
  unique(evidenceRefIds, `${path}.evidenceRefIds`)
  const startedAtMillis = timestamp(input.startedAtMillis, `${path}.startedAtMillis`)
  const finishedAtMillis = timestamp(input.finishedAtMillis, `${path}.finishedAtMillis`)
  if (startedAtMillis === null || finishedAtMillis === null || finishedAtMillis < startedAtMillis) {
    failure(path, 'timestamps are invalid')
  }
  return Object.freeze({
    stageRunId: identifier(input.stageRunId, `${path}.stageRunId`, StageRunId),
    sessionBindingId: identifier(
      input.sessionBindingId,
      `${path}.sessionBindingId`,
      SessionBindingId,
    ),
    deliveryTaskId: input.deliveryTaskId === null
      ? null
      : identifier(input.deliveryTaskId, `${path}.deliveryTaskId`, DeliveryTaskId),
    stage: input.stage,
    role: text(input.role, `${path}.role`),
    attempt: positiveInteger(input.attempt, `${path}.attempt`),
    dshSessionId: portableId(input.dshSessionId, `${path}.dshSessionId`),
    codexSessionId: portableId(input.codexSessionId, `${path}.codexSessionId`),
    startedAtMillis,
    finishedAtMillis,
    agents: Object.freeze(agents),
    activities: Object.freeze(activities),
    evidenceRefIds: Object.freeze(evidenceRefIds),
  })
}

function parseDetails(value: unknown, path: string): StrongFlowDiagramExecutionDetails {
  const input = record(value, path)
  exactKeys(input, [
    'candidate',
    'diffSha256',
    'files',
    'hunks',
    'additions',
    'deletions',
    'provenance',
  ], path)
  const candidate = parseFrozenDeliveryCandidate(input.candidate, `${path}.candidate`)
  const files = array(input.files, `${path}.files`).map((entry, index) => (
    parseFile(entry, `${path}.files[${String(index)}]`)
  ))
  const hunks = array(input.hunks, `${path}.hunks`).map((entry, index) => (
    parseHunk(entry, `${path}.hunks[${String(index)}]`)
  ))
  unique(files.map(file => file.id), `${path}.files`)
  unique(files.map(file => file.path), `${path}.files.path`)
  unique(hunks.map(hunk => hunk.id), `${path}.hunks`)
  const fileById = new Map(files.map(file => [file.id, file]))
  const hunkById = new Map(hunks.map(hunk => [hunk.id, hunk]))
  if (hunks.some(hunk => !fileById.has(hunk.fileId))
    || files.some(file => file.hunkIds.some(hunkId => hunkById.get(hunkId)?.fileId !== file.id))) {
    failure(path, 'file and hunk relationships are inconsistent')
  }
  const diffSha256 = sha256(input.diffSha256, `${path}.diffSha256`)
  if (diffSha256 !== candidate.diffSha256
    || files.length !== candidate.changedPaths.length
    || files.some(file => !candidate.changedPaths.some(pathFact => (
      pathFact.path === file.path && pathFact.state === file.state
    )))
    || nonNegativeInteger(input.additions, `${path}.additions`)
      !== files.reduce((sum, file) => sum + file.additions, 0)
    || nonNegativeInteger(input.deletions, `${path}.deletions`)
      !== files.reduce((sum, file) => sum + file.deletions, 0)) {
    failure(path, 'candidate, diff, and file totals do not agree')
  }
  return Object.freeze({
    candidate,
    diffSha256,
    files: Object.freeze(files),
    hunks: Object.freeze(hunks),
    additions: Number(input.additions),
    deletions: Number(input.deletions),
    provenance: parseProvenance(input.provenance, `${path}.provenance`),
  })
}

/** Parse one safe diagram state returned by the StrongFlow host. */
export function parseStrongFlowDiagramExecutionProjection(
  value: unknown,
  path = 'diagramExecutionProjection',
): StrongFlowDiagramExecutionProjection {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'deliveryId',
    'deliveryRevision',
    'reviewSetSha256',
    'state',
    'architecture',
    'process',
    'affectedFileCount',
    'details',
    'updatedAtMillis',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL) {
    failure(path, 'uses an unsupported protocol')
  }
  if (typeof input.state !== 'string'
    || !STRONGFLOW_DIAGRAM_EXECUTION_STATES.includes(
      input.state as StrongFlowDiagramExecutionState,
    )) failure(`${path}.state`, 'is unsupported')
  const architecture = parseDiagram(input.architecture, `${path}.architecture`)
  const process = parseDiagram(input.process, `${path}.process`)
  if (architecture.kind !== 'system-architecture' || process.kind !== 'process-flow') {
    failure(path, 'must contain the architecture and process diagrams')
  }
  const details = input.details === null ? null : parseDetails(input.details, `${path}.details`)
  const affectedFileCount = nonNegativeInteger(
    input.affectedFileCount,
    `${path}.affectedFileCount`,
  )
  const nodes = [...architecture.nodes, ...process.nodes]
  if ((input.state === 'execution-finished') !== (details !== null)
    || (input.state === 'before-execution' && affectedFileCount !== 0)
    || (details !== null && affectedFileCount !== details.files.length)
    || (details === null && nodes.some(node => node.fileIds.length > 0))
    || (input.state === 'before-execution' && nodes.some(node => node.state !== 'normal'))
    || (input.state === 'executing' && nodes.some(node => (
      node.state !== 'normal' && node.state !== 'affected-live'
    )))
    || (input.state === 'execution-finished' && nodes.some(node => (
      node.state !== 'normal' && node.state !== 'affected-finished'
    )))) {
    failure(path, 'state does not match its detail boundary or node states')
  }
  if (details !== null) {
    const knownFileIds = new Set(details.files.map(file => file.id))
    if (nodes.some(node => node.fileIds.some(fileId => !knownFileIds.has(fileId)))) {
      failure(path, 'a diagram node references an unknown diff file')
    }
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION,
    protocol: STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL,
    deliveryId: identifier(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliveryRevision: positiveInteger(input.deliveryRevision, `${path}.deliveryRevision`),
    reviewSetSha256: sha256(input.reviewSetSha256, `${path}.reviewSetSha256`),
    state: input.state as StrongFlowDiagramExecutionState,
    architecture,
    process,
    affectedFileCount,
    details,
    updatedAtMillis: nonNegativeInteger(input.updatedAtMillis, `${path}.updatedAtMillis`),
  })
}
