import {
  AttentionItemId,
  DeliveryId,
  DeliverySpecId,
  SessionBindingId,
  StageRunId,
  type AttentionItemId as AttentionItemIdentifier,
  type DeliveryId as DeliveryIdentifier,
  type DeliverySpecId as DeliverySpecIdentifier,
  type SessionBindingId as SessionBindingIdentifier,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'

/**
 * Frozen plan-review data stored inside one AttentionItem context/resolution.
 * These are protocol fragments, not additional top-level Delivery objects.
 */
export const STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION = 2 as const

export const STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL =
  'winwincode.plan-review-context.v2' as const

export const STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL =
  'winwincode.plan-review-decision.v2' as const

export const STRONGFLOW_PLAN_REVIEW_ACTIONS = Object.freeze([
  'approve',
  'request_changes',
  'reject',
] as const)

export type StrongFlowPlanReviewAction = typeof STRONGFLOW_PLAN_REVIEW_ACTIONS[number]

export const STRONGFLOW_PLAN_REVIEW_DIAGRAM_KINDS = Object.freeze([
  'system-architecture',
  'process-flow',
] as const)

export type StrongFlowPlanReviewDiagramKind =
  typeof STRONGFLOW_PLAN_REVIEW_DIAGRAM_KINDS[number]

export const STRONGFLOW_PLAN_REVIEW_NODE_KINDS = Object.freeze([
  'interaction',
  'delivery-control',
  'execution',
  'repository',
  'component',
  'external',
  'data-store',
  'stage',
  'decision',
] as const)

export type StrongFlowPlanReviewNodeKind =
  typeof STRONGFLOW_PLAN_REVIEW_NODE_KINDS[number]

export const STRONGFLOW_SOLUTION_COMPONENT_KINDS = Object.freeze([
  'component',
  'external',
  'data-store',
] as const)

export type StrongFlowSolutionComponentKind =
  typeof STRONGFLOW_SOLUTION_COMPONENT_KINDS[number]

export const STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS = Object.freeze([
  'platform:dsh',
  'platform:strongflow',
  'platform:codex-core',
  'platform:repository',
] as const)

export interface StrongFlowPlanReviewSolutionComponent {
  readonly id: string
  readonly label: string
  readonly responsibility: string
  readonly kind: StrongFlowSolutionComponentKind
  readonly trustBoundary: string | null
  readonly unresolved: boolean
  /** Approved repository-relative prefixes used to project changed files onto this node. */
  readonly repositoryPathPrefixes: readonly string[]
}

export interface StrongFlowPlanReviewSolutionConnection {
  readonly id: string
  readonly from: string
  readonly to: string
  readonly label: string
}

/** Structured planner output frozen into the review Attention context. */
export interface StrongFlowPlanReviewSolution {
  readonly id: string
  readonly summary: string
  readonly approach: readonly string[]
  readonly components: readonly StrongFlowPlanReviewSolutionComponent[]
  readonly connections: readonly StrongFlowPlanReviewSolutionConnection[]
}

export interface StrongFlowPlanReviewDiagramNode {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly kind: StrongFlowPlanReviewNodeKind
  readonly trustBoundary: string | null
  readonly unresolved: boolean
}

export interface StrongFlowPlanReviewDiagramEdge {
  readonly id: string
  readonly from: string
  readonly to: string
  readonly label: string
}

export interface StrongFlowPlanReviewDiagram {
  readonly id: string
  readonly kind: StrongFlowPlanReviewDiagramKind
  readonly title: string
  readonly nodes: readonly StrongFlowPlanReviewDiagramNode[]
  readonly edges: readonly StrongFlowPlanReviewDiagramEdge[]
}

/** Immutable review set referenced by one blocking plan-review AttentionItem. */
export interface StrongFlowPlanReviewContext {
  readonly schemaVersion: typeof STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL
  readonly deliveryId: DeliveryIdentifier
  readonly deliverySpecId: DeliverySpecIdentifier
  readonly deliverySpecRevision: number
  readonly planningStageRunId: StageRunIdentifier
  readonly planningSessionBindingId: SessionBindingIdentifier
  readonly reviewStageRunId: StageRunIdentifier
  readonly attentionItemId: AttentionItemIdentifier
  readonly solution: StrongFlowPlanReviewSolution
  readonly architectureDiagram: StrongFlowPlanReviewDiagram
  readonly processDiagram: StrongFlowPlanReviewDiagram
  readonly risks: readonly string[]
  readonly unresolvedItems: readonly string[]
  readonly reviewSetSha256: string
  readonly preparedAtMillis: number
}

/** Human decision serialized into the existing AttentionItem.resolution field. */
export interface StrongFlowPlanReviewDecision {
  readonly schemaVersion: typeof STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL
  readonly action: StrongFlowPlanReviewAction
  readonly deliveryId: DeliveryIdentifier
  readonly deliverySpecId: DeliverySpecIdentifier
  readonly deliverySpecRevision: number
  readonly reviewStageRunId: StageRunIdentifier
  readonly attentionItemId: AttentionItemIdentifier
  readonly reviewSetSha256: string
  readonly comments: string
  readonly requestedChanges: readonly string[]
}

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const MAX_TEXT_LENGTH = 65_536
const MAX_COLLECTION_LENGTH = 200

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
    || (!allowEmpty && value.trim().length === 0)) {
    return failure(path, `must be ${allowEmpty ? 'a' : 'a non-empty'} bounded string`)
  }
  return value
}

function portableId(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    return failure(path, 'must be a portable identifier')
  }
  return value
}

function positiveInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1) {
    return failure(path, 'must be a positive safe integer')
  }
  return Number(value)
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    return failure(path, 'must be a non-negative safe integer')
  }
  return Number(value)
}

function stringList(
  value: unknown,
  path: string,
  options: { readonly minimum?: number } = {},
): readonly string[] {
  if (!Array.isArray(value)
    || value.length < (options.minimum ?? 0)
    || value.length > MAX_COLLECTION_LENGTH) {
    return failure(path, 'must be a bounded array')
  }
  const result = value.map((entry, index) => text(entry, `${path}[${String(index)}]`))
  if (new Set(result).size !== result.length) failure(path, 'must not contain duplicates')
  return Object.freeze(result)
}

function nullableText(value: unknown, path: string): string | null {
  return value === null ? null : text(value, path)
}

function repositoryPathPrefix(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > 4_096
    || value.startsWith('/')
    || value.endsWith('/')
    || value.includes('\\')
    || /^[A-Za-z]:/u.test(value)
    || /[\u0000-\u001f\u007f]/u.test(value)
    || ['*', '?', '[', ']', '{', '}', '!'].some(marker => value.includes(marker))
    || value.split('/').some(segment => (
      segment.length === 0 || segment === '.' || segment === '..'
    ))) {
    return failure(path, 'must be a literal repository-relative path prefix')
  }
  return value
}

export function parseStrongFlowPlanReviewSolution(
  value: unknown,
  path = 'planReview.solution',
): StrongFlowPlanReviewSolution {
  const input = record(value, path)
  exactKeys(input, ['id', 'summary', 'approach', 'components', 'connections'], path)
  if (!Array.isArray(input.components)
    || input.components.length < 1
    || input.components.length > MAX_COLLECTION_LENGTH) {
    return failure(`${path}.components`, 'must contain at least one bounded component')
  }
  const components = input.components.map((entry, index) => {
    const componentPath = `${path}.components[${String(index)}]`
    const component = record(entry, componentPath)
    exactKeys(component, [
      'id',
      'label',
      'responsibility',
      'kind',
      'trustBoundary',
      'unresolved',
      'repositoryPathPrefixes',
    ], componentPath)
    const id = portableId(component.id, `${componentPath}.id`)
    if (STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS.includes(
      id as typeof STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS[number],
    )) failure(`${componentPath}.id`, 'collides with a platform node')
    if (typeof component.kind !== 'string'
      || !STRONGFLOW_SOLUTION_COMPONENT_KINDS.includes(
        component.kind as StrongFlowSolutionComponentKind,
      )) failure(`${componentPath}.kind`, 'is unsupported')
    if (typeof component.unresolved !== 'boolean') {
      failure(`${componentPath}.unresolved`, 'must be a boolean')
    }
    if (!Array.isArray(component.repositoryPathPrefixes)
      || component.repositoryPathPrefixes.length > MAX_COLLECTION_LENGTH) {
      failure(`${componentPath}.repositoryPathPrefixes`, 'must be a bounded array')
    }
    const repositoryPathPrefixes = component.repositoryPathPrefixes.map((entry, prefixIndex) => (
      repositoryPathPrefix(
        entry,
        `${componentPath}.repositoryPathPrefixes[${String(prefixIndex)}]`,
      )
    ))
    if (new Set(repositoryPathPrefixes).size !== repositoryPathPrefixes.length) {
      failure(`${componentPath}.repositoryPathPrefixes`, 'must not contain duplicates')
    }
    return Object.freeze({
      id,
      label: text(component.label, `${componentPath}.label`),
      responsibility: text(component.responsibility, `${componentPath}.responsibility`),
      kind: component.kind as StrongFlowSolutionComponentKind,
      trustBoundary: nullableText(component.trustBoundary, `${componentPath}.trustBoundary`),
      unresolved: component.unresolved,
      repositoryPathPrefixes: Object.freeze(repositoryPathPrefixes),
    })
  })
  if (new Set(components.map(component => component.id)).size !== components.length) {
    failure(`${path}.components`, 'contains duplicate component identities')
  }
  if (!Array.isArray(input.connections)
    || input.connections.length > MAX_COLLECTION_LENGTH) {
    return failure(`${path}.connections`, 'must be a bounded array')
  }
  const allowedEndpoints = new Set([
    ...STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS,
    ...components.map(component => component.id),
  ])
  const connections = input.connections.map((entry, index) => {
    const connectionPath = `${path}.connections[${String(index)}]`
    const connection = record(entry, connectionPath)
    exactKeys(connection, ['id', 'from', 'to', 'label'], connectionPath)
    const from = portableId(connection.from, `${connectionPath}.from`)
    const to = portableId(connection.to, `${connectionPath}.to`)
    if (!allowedEndpoints.has(from) || !allowedEndpoints.has(to) || from === to) {
      failure(connectionPath, 'must connect two distinct known nodes')
    }
    return Object.freeze({
      id: portableId(connection.id, `${connectionPath}.id`),
      from,
      to,
      label: text(connection.label, `${connectionPath}.label`),
    })
  })
  if (new Set(connections.map(connection => connection.id)).size !== connections.length) {
    failure(`${path}.connections`, 'contains duplicate connection identities')
  }
  return Object.freeze({
    id: portableId(input.id, `${path}.id`),
    summary: text(input.summary, `${path}.summary`),
    approach: stringList(input.approach, `${path}.approach`, { minimum: 1 }),
    components: Object.freeze(components),
    connections: Object.freeze(connections),
  })
}

export function parseStrongFlowPlanReviewDiagram(
  value: unknown,
  path = 'planReview.diagram',
): StrongFlowPlanReviewDiagram {
  const input = record(value, path)
  exactKeys(input, ['id', 'kind', 'title', 'nodes', 'edges'], path)
  if (typeof input.kind !== 'string'
    || !STRONGFLOW_PLAN_REVIEW_DIAGRAM_KINDS.includes(
      input.kind as StrongFlowPlanReviewDiagramKind,
    )) failure(`${path}.kind`, 'is unsupported')
  if (!Array.isArray(input.nodes)
    || input.nodes.length < 1
    || input.nodes.length > MAX_COLLECTION_LENGTH) {
    return failure(`${path}.nodes`, 'must contain a bounded node set')
  }
  const nodes = input.nodes.map((entry, index) => {
    const nodePath = `${path}.nodes[${String(index)}]`
    const node = record(entry, nodePath)
    exactKeys(node, [
      'id',
      'label',
      'description',
      'kind',
      'trustBoundary',
      'unresolved',
    ], nodePath)
    if (typeof node.kind !== 'string'
      || !STRONGFLOW_PLAN_REVIEW_NODE_KINDS.includes(
        node.kind as StrongFlowPlanReviewNodeKind,
      )) failure(`${nodePath}.kind`, 'is unsupported')
    if (typeof node.unresolved !== 'boolean') {
      failure(`${nodePath}.unresolved`, 'must be a boolean')
    }
    return Object.freeze({
      id: portableId(node.id, `${nodePath}.id`),
      label: text(node.label, `${nodePath}.label`),
      description: text(node.description, `${nodePath}.description`),
      kind: node.kind as StrongFlowPlanReviewNodeKind,
      trustBoundary: nullableText(node.trustBoundary, `${nodePath}.trustBoundary`),
      unresolved: node.unresolved,
    })
  })
  const nodeIds = new Set(nodes.map(node => node.id))
  if (nodeIds.size !== nodes.length) failure(`${path}.nodes`, 'contains duplicate node identities')
  if (!Array.isArray(input.edges)
    || input.edges.length > MAX_COLLECTION_LENGTH) {
    return failure(`${path}.edges`, 'must be a bounded array')
  }
  const edges = input.edges.map((entry, index) => {
    const edgePath = `${path}.edges[${String(index)}]`
    const edge = record(entry, edgePath)
    exactKeys(edge, ['id', 'from', 'to', 'label'], edgePath)
    const from = portableId(edge.from, `${edgePath}.from`)
    const to = portableId(edge.to, `${edgePath}.to`)
    if (!nodeIds.has(from) || !nodeIds.has(to) || from === to) {
      failure(edgePath, 'must connect two distinct nodes in this diagram')
    }
    return Object.freeze({
      id: portableId(edge.id, `${edgePath}.id`),
      from,
      to,
      label: text(edge.label, `${edgePath}.label`),
    })
  })
  if (new Set(edges.map(edge => edge.id)).size !== edges.length) {
    failure(`${path}.edges`, 'contains duplicate edge identities')
  }
  return Object.freeze({
    id: portableId(input.id, `${path}.id`),
    kind: input.kind as StrongFlowPlanReviewDiagramKind,
    title: text(input.title, `${path}.title`),
    nodes: Object.freeze(nodes),
    edges: Object.freeze(edges),
  })
}

export function parseStrongFlowPlanReviewContext(
  value: unknown,
  path = 'planReview.context',
): StrongFlowPlanReviewContext {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'planningStageRunId',
    'planningSessionBindingId',
    'reviewStageRunId',
    'attentionItemId',
    'solution',
    'architectureDiagram',
    'processDiagram',
    'risks',
    'unresolvedItems',
    'reviewSetSha256',
    'preparedAtMillis',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION) {
    failure(`${path}.schemaVersion`, 'is unsupported')
  }
  if (input.protocol !== STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL) {
    failure(`${path}.protocol`, 'is unsupported')
  }
  const architectureDiagram = parseStrongFlowPlanReviewDiagram(
    input.architectureDiagram,
    `${path}.architectureDiagram`,
  )
  const processDiagram = parseStrongFlowPlanReviewDiagram(
    input.processDiagram,
    `${path}.processDiagram`,
  )
  if (architectureDiagram.kind !== 'system-architecture'
    || processDiagram.kind !== 'process-flow') {
    failure(path, 'must contain one system architecture and one process-flow diagram')
  }
  if (typeof input.reviewSetSha256 !== 'string'
    || !SHA256_PATTERN.test(input.reviewSetSha256)) {
    failure(`${path}.reviewSetSha256`, 'must be a lowercase SHA-256 digest')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
    protocol: STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL,
    deliveryId: DeliveryId(portableId(input.deliveryId, `${path}.deliveryId`)),
    deliverySpecId: DeliverySpecId(portableId(input.deliverySpecId, `${path}.deliverySpecId`)),
    deliverySpecRevision: positiveInteger(
      input.deliverySpecRevision,
      `${path}.deliverySpecRevision`,
    ),
    planningStageRunId: StageRunId(
      portableId(input.planningStageRunId, `${path}.planningStageRunId`),
    ),
    planningSessionBindingId: SessionBindingId(
      portableId(input.planningSessionBindingId, `${path}.planningSessionBindingId`),
    ),
    reviewStageRunId: StageRunId(
      portableId(input.reviewStageRunId, `${path}.reviewStageRunId`),
    ),
    attentionItemId: AttentionItemId(
      portableId(input.attentionItemId, `${path}.attentionItemId`),
    ),
    solution: parseStrongFlowPlanReviewSolution(input.solution, `${path}.solution`),
    architectureDiagram,
    processDiagram,
    risks: stringList(input.risks, `${path}.risks`),
    unresolvedItems: stringList(input.unresolvedItems, `${path}.unresolvedItems`),
    reviewSetSha256: input.reviewSetSha256,
    preparedAtMillis: nonNegativeInteger(input.preparedAtMillis, `${path}.preparedAtMillis`),
  })
}

export function parseStrongFlowPlanReviewContextText(
  value: string,
  path = 'planReview.context',
): StrongFlowPlanReviewContext {
  try {
    return parseStrongFlowPlanReviewContext(JSON.parse(value) as unknown, path)
  } catch (error) {
    if (error instanceof TypeError) throw error
    return failure(path, 'must be valid JSON')
  }
}

export function parseStrongFlowPlanReviewDecision(
  value: unknown,
  path = 'planReview.decision',
): StrongFlowPlanReviewDecision {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'action',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'reviewStageRunId',
    'attentionItemId',
    'reviewSetSha256',
    'comments',
    'requestedChanges',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION) {
    failure(`${path}.schemaVersion`, 'is unsupported')
  }
  if (input.protocol !== STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL) {
    failure(`${path}.protocol`, 'is unsupported')
  }
  if (typeof input.action !== 'string'
    || !STRONGFLOW_PLAN_REVIEW_ACTIONS.includes(
      input.action as StrongFlowPlanReviewAction,
    )) failure(`${path}.action`, 'is unsupported')
  if (typeof input.reviewSetSha256 !== 'string'
    || !SHA256_PATTERN.test(input.reviewSetSha256)) {
    failure(`${path}.reviewSetSha256`, 'must be a lowercase SHA-256 digest')
  }
  const action = input.action as StrongFlowPlanReviewAction
  const comments = text(input.comments, `${path}.comments`, true)
  const requestedChanges = stringList(input.requestedChanges, `${path}.requestedChanges`)
  if (action === 'approve' && requestedChanges.length > 0) {
    failure(`${path}.requestedChanges`, 'must be empty when approving')
  }
  if (action === 'request_changes' && requestedChanges.length === 0) {
    failure(`${path}.requestedChanges`, 'must identify at least one requested change')
  }
  if (action === 'reject' && comments.trim().length === 0) {
    failure(`${path}.comments`, 'must explain a rejection')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
    protocol: STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL,
    action,
    deliveryId: DeliveryId(portableId(input.deliveryId, `${path}.deliveryId`)),
    deliverySpecId: DeliverySpecId(portableId(input.deliverySpecId, `${path}.deliverySpecId`)),
    deliverySpecRevision: positiveInteger(
      input.deliverySpecRevision,
      `${path}.deliverySpecRevision`,
    ),
    reviewStageRunId: StageRunId(
      portableId(input.reviewStageRunId, `${path}.reviewStageRunId`),
    ),
    attentionItemId: AttentionItemId(
      portableId(input.attentionItemId, `${path}.attentionItemId`),
    ),
    reviewSetSha256: input.reviewSetSha256,
    comments,
    requestedChanges,
  })
}

export function parseStrongFlowPlanReviewDecisionText(
  value: string,
  path = 'planReview.decision',
): StrongFlowPlanReviewDecision {
  try {
    return parseStrongFlowPlanReviewDecision(JSON.parse(value) as unknown, path)
  } catch (error) {
    if (error instanceof TypeError) throw error
    return failure(path, 'must be valid JSON')
  }
}

export function serializeStrongFlowPlanReviewDecision(
  value: StrongFlowPlanReviewDecision,
): string {
  return JSON.stringify(parseStrongFlowPlanReviewDecision(value))
}
