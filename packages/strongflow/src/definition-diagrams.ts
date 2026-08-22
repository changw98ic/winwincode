import { createHash } from 'node:crypto'
import { isDeepStrictEqual } from 'node:util'

import {
  DiagramId,
  materializeStrongFlowArtifact,
  parseStrongFlowArtifactAs,
  type DiagramId as DiagramIdentifier,
  type DiagramNodeId,
  type ProcessFlowDiagram,
  type ProcessFlowDiagramPayload,
  type ProcessFlowDiagramNode,
  type RequirementSpec,
  type SolutionDesign,
  type StrongFlowDiagramEdge,
  type SystemArchitectureDiagram,
  type SystemArchitectureDiagramNode,
  type SystemArchitectureDiagramPayload,
} from '@winwincode/contracts'

export const STRONGFLOW_DEFINITION_DIAGRAM_BUNDLE_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_DEFINITION_DIAGRAM_RENDER_SCHEMA_VERSION = 1 as const

const MAX_RENDER_NODES = 200
const MAX_RENDER_EDGES = 500
const MAX_RENDER_LABEL_LENGTH = 112
const MAX_RENDER_DESCRIPTION_LENGTH = 2_000
const NODE_WIDTH = 280
const NODE_HEIGHT = 148
const COLUMN_GAP = 48
const ROW_GAP = 52
const PADDING = 32

export type StrongFlowDefinitionDiagramErrorCode =
  | 'INVALID_DIAGRAM_INPUT'
  | 'DIAGRAM_PAIR_MISMATCH'
  | 'DIAGRAM_TEMPLATE_MISMATCH'
  | 'DIAGRAM_TOO_LARGE'
  | 'UNSAFE_RENDER_OUTPUT'

export class StrongFlowDefinitionDiagramError extends Error {
  readonly code: StrongFlowDefinitionDiagramErrorCode

  constructor(
    code: StrongFlowDefinitionDiagramErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowDefinitionDiagramError'
    this.code = code
  }
}

function diagramError(
  code: StrongFlowDefinitionDiagramErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowDefinitionDiagramError(code, message, options)
}

interface ProcessTemplateNode {
  readonly nodeId: DiagramNodeId
  readonly label: string
  readonly kind: ProcessFlowDiagramNode['kind']
  readonly description: string
  readonly roleId: ProcessFlowDiagramNode['roleId']
}

interface ProcessTemplateEdge {
  readonly edgeId: string
  readonly fromNodeId: DiagramNodeId
  readonly toNodeId: DiagramNodeId
  readonly label: string
}

/** Stable product flow; execution overlays must reuse these node identities. */
export const STRONGFLOW_DEFAULT_PROCESS_TEMPLATE_NODES: readonly ProcessTemplateNode[] =
  Object.freeze([
    Object.freeze({
      nodeId: 'process:01-requirements' as DiagramNodeId,
      label: '需求整理',
      kind: 'start',
      description: 'Requirements Analyst 从用户请求和已核实的仓库事实整理需求。',
      roleId: 'requirements',
    }),
    Object.freeze({
      nodeId: 'process:02-solution' as DiagramNodeId,
      label: '方案设计',
      kind: 'stage',
      description: 'Solution Architect 只针对当前需求设计方案。',
      roleId: 'solution',
    }),
    Object.freeze({
      nodeId: 'process:03-diagrams' as DiagramNodeId,
      label: '定义图生成',
      kind: 'stage',
      description: '生成系统架构图和流程图，并保留稳定节点身份。',
      roleId: 'solution',
    }),
    Object.freeze({
      nodeId: 'process:05-human-review' as DiagramNodeId,
      label: '人工审核',
      kind: 'human-review',
      description: '人工同时审核需求、方案和两张定义图。',
      roleId: null,
    }),
    Object.freeze({
      nodeId: 'process:06-revision' as DiagramNodeId,
      label: '退回修改',
      kind: 'decision',
      description: '按人工指定范围重新整理需求、方案或图。',
      roleId: null,
    }),
    Object.freeze({
      nodeId: 'process:07-approved' as DiagramNodeId,
      label: '定义已批准',
      kind: 'state',
      description: '批准只对当前四个定义制品有效。',
      roleId: null,
    }),
    Object.freeze({
      nodeId: 'process:08-rejected' as DiagramNodeId,
      label: '定义已拒绝',
      kind: 'end',
      description: '人工拒绝后作业结束，不进入执行。',
      roleId: null,
    }),
    Object.freeze({
      nodeId: 'process:09-planning' as DiagramNodeId,
      label: '执行计划',
      kind: 'stage',
      description: 'Planner 根据已批准定义生成有界执行计划。',
      roleId: 'planner',
    }),
    Object.freeze({
      nodeId: 'process:10-execution' as DiagramNodeId,
      label: '执行修改',
      kind: 'stage',
      description: 'Executor 只在候选工作区执行批准的计划。',
      roleId: 'executor',
    }),
    Object.freeze({
      nodeId: 'process:11-review' as DiagramNodeId,
      label: '代码审查',
      kind: 'stage',
      description: 'Reviewer 对冻结候选版本给出独立审查报告。',
      roleId: 'reviewer',
    }),
    Object.freeze({
      nodeId: 'process:12-verification' as DiagramNodeId,
      label: '独立验证',
      kind: 'stage',
      description: 'Verifier 和 Adversarial Verifier 执行冻结验收检查。',
      roleId: 'verifier',
    }),
    Object.freeze({
      nodeId: 'process:13-remediation' as DiagramNodeId,
      label: '有界修复',
      kind: 'stage',
      description: 'Remediator 只处理结构化修复请求，然后重新进入执行和验证。',
      roleId: 'remediator',
    }),
    Object.freeze({
      nodeId: 'process:14-completion-gate' as DiagramNodeId,
      label: '完成门禁',
      kind: 'decision',
      description: '程序核对当前候选版本的全部必需证据。',
      roleId: null,
    }),
    Object.freeze({
      nodeId: 'process:15-delivery' as DiagramNodeId,
      label: '交付',
      kind: 'end',
      description: '系统为通过门禁的准确候选版本生成交付回执。',
      roleId: null,
    }),
  ])

const BASE_PROCESS_TEMPLATE_EDGES: readonly ProcessTemplateEdge[] = Object.freeze([
  Object.freeze({
    edgeId: 'process-edge:requirements-solution',
    fromNodeId: 'process:01-requirements' as DiagramNodeId,
    toNodeId: 'process:02-solution' as DiagramNodeId,
    label: '',
  }),
  Object.freeze({
    edgeId: 'process-edge:solution-diagrams',
    fromNodeId: 'process:02-solution' as DiagramNodeId,
    toNodeId: 'process:03-diagrams' as DiagramNodeId,
    label: '',
  }),
  Object.freeze({
    edgeId: 'process-edge:review-revision',
    fromNodeId: 'process:05-human-review' as DiagramNodeId,
    toNodeId: 'process:06-revision' as DiagramNodeId,
    label: '要求修改',
  }),
  Object.freeze({
    edgeId: 'process-edge:revision-requirements',
    fromNodeId: 'process:06-revision' as DiagramNodeId,
    toNodeId: 'process:01-requirements' as DiagramNodeId,
    label: '需求',
  }),
  Object.freeze({
    edgeId: 'process-edge:revision-solution',
    fromNodeId: 'process:06-revision' as DiagramNodeId,
    toNodeId: 'process:02-solution' as DiagramNodeId,
    label: '方案',
  }),
  Object.freeze({
    edgeId: 'process-edge:revision-diagrams',
    fromNodeId: 'process:06-revision' as DiagramNodeId,
    toNodeId: 'process:03-diagrams' as DiagramNodeId,
    label: '图',
  }),
  Object.freeze({
    edgeId: 'process-edge:review-approved',
    fromNodeId: 'process:05-human-review' as DiagramNodeId,
    toNodeId: 'process:07-approved' as DiagramNodeId,
    label: '批准',
  }),
  Object.freeze({
    edgeId: 'process-edge:review-rejected',
    fromNodeId: 'process:05-human-review' as DiagramNodeId,
    toNodeId: 'process:08-rejected' as DiagramNodeId,
    label: '拒绝',
  }),
  Object.freeze({
    edgeId: 'process-edge:approved-planning',
    fromNodeId: 'process:07-approved' as DiagramNodeId,
    toNodeId: 'process:09-planning' as DiagramNodeId,
    label: '',
  }),
  Object.freeze({
    edgeId: 'process-edge:planning-execution',
    fromNodeId: 'process:09-planning' as DiagramNodeId,
    toNodeId: 'process:10-execution' as DiagramNodeId,
    label: '',
  }),
  Object.freeze({
    edgeId: 'process-edge:execution-review',
    fromNodeId: 'process:10-execution' as DiagramNodeId,
    toNodeId: 'process:11-review' as DiagramNodeId,
    label: '',
  }),
  Object.freeze({
    edgeId: 'process-edge:review-verification',
    fromNodeId: 'process:11-review' as DiagramNodeId,
    toNodeId: 'process:12-verification' as DiagramNodeId,
    label: '',
  }),
  Object.freeze({
    edgeId: 'process-edge:verification-remediation',
    fromNodeId: 'process:12-verification' as DiagramNodeId,
    toNodeId: 'process:13-remediation' as DiagramNodeId,
    label: '需要修复',
  }),
  Object.freeze({
    edgeId: 'process-edge:remediation-execution',
    fromNodeId: 'process:13-remediation' as DiagramNodeId,
    toNodeId: 'process:10-execution' as DiagramNodeId,
    label: '再次执行',
  }),
  Object.freeze({
    edgeId: 'process-edge:verification-gate',
    fromNodeId: 'process:12-verification' as DiagramNodeId,
    toNodeId: 'process:14-completion-gate' as DiagramNodeId,
    label: '通过',
  }),
  Object.freeze({
    edgeId: 'process-edge:gate-delivery',
    fromNodeId: 'process:14-completion-gate' as DiagramNodeId,
    toNodeId: 'process:15-delivery' as DiagramNodeId,
    label: '通过',
  }),
  Object.freeze({
    edgeId: 'process-edge:gate-remediation',
    fromNodeId: 'process:14-completion-gate' as DiagramNodeId,
    toNodeId: 'process:13-remediation' as DiagramNodeId,
    label: '未通过',
  }),
])

export const STRONGFLOW_REQUIRED_PROCESS_NODE_IDS: readonly DiagramNodeId[] = Object.freeze(
  STRONGFLOW_DEFAULT_PROCESS_TEMPLATE_NODES.map(node => node.nodeId),
)

export interface GenerateStrongFlowDefinitionDiagramsOptions {
  readonly requirement: unknown
  readonly solution: unknown
  readonly systemArchitectureDiagramId: DiagramIdentifier
  readonly processFlowDiagramId: DiagramIdentifier
  readonly createdAtMillis: number
}

export interface ValidateStrongFlowDefinitionDiagramPairOptions {
  readonly requirement: unknown
  readonly solution: unknown
  readonly systemArchitectureDiagram: unknown
  readonly processFlowDiagram: unknown
}

export interface StrongFlowRenderedDefinitionDiagram {
  readonly schemaVersion: typeof STRONGFLOW_DEFINITION_DIAGRAM_RENDER_SCHEMA_VERSION
  readonly artifactKind: 'SYSTEM_ARCHITECTURE_DIAGRAM' | 'PROCESS_FLOW_DIAGRAM'
  readonly diagramId: DiagramIdentifier
  readonly requirementId: RequirementSpec['artifactId']
  readonly solutionId: SolutionDesign['artifactId']
  readonly visualState: 'before-execution'
  readonly layoutId: string
  readonly nodeIds: readonly DiagramNodeId[]
  readonly mermaid: string
  readonly mermaidSha256: string
  readonly svg: string
  readonly svgSha256: string
}

export interface StrongFlowDefinitionDiagramBundle {
  readonly schemaVersion: typeof STRONGFLOW_DEFINITION_DIAGRAM_BUNDLE_SCHEMA_VERSION
  readonly jobId: RequirementSpec['jobId']
  readonly requirementId: RequirementSpec['artifactId']
  readonly solutionId: SolutionDesign['artifactId']
  readonly systemArchitectureDiagram: SystemArchitectureDiagram
  readonly processFlowDiagram: ProcessFlowDiagram
  readonly rendered: Readonly<{
    systemArchitecture: StrongFlowRenderedDefinitionDiagram
    processFlow: StrongFlowRenderedDefinitionDiagram
  }>
}

function hash(value: string): string {
  return createHash('sha256').update(value).digest('hex')
}

function stableNodeId(prefix: string, sourceId: string): DiagramNodeId {
  return `${prefix}:${hash(sourceId).slice(0, 24)}` as DiagramNodeId
}

function systemPayload(
  requirement: RequirementSpec,
  solution: SolutionDesign,
): SystemArchitectureDiagramPayload {
  const components = [...solution.payload.components]
    .sort((left, right) => left.componentId.localeCompare(right.componentId))
  const nodeByComponent = new Map<string, DiagramNodeId>()
  const nodes: SystemArchitectureDiagramNode[] = components.map(component => {
    const nodeId = stableNodeId('system:component', component.componentId)
    nodeByComponent.set(component.componentId, nodeId)
    return Object.freeze({
      nodeId,
      label: component.name,
      kind: component.kind,
      description: component.responsibility,
      trustBoundary: component.trustBoundary,
      unresolved: false,
      componentIds: Object.freeze([component.componentId]),
      sourcePaths: component.sourcePaths,
    })
  })
  for (const question of [...requirement.payload.openQuestions]
    .sort((left, right) => left.questionId.localeCompare(right.questionId))) {
    nodes.push(Object.freeze({
      nodeId: stableNodeId('system:question', question.questionId),
      label: `未确认：${question.questionId}`,
      kind: 'unresolved',
      description: question.question,
      trustBoundary: null,
      unresolved: true,
      componentIds: Object.freeze([]),
      sourcePaths: Object.freeze([]),
    }))
  }
  for (const fact of [...solution.payload.unresolvedFacts]
    .sort((left, right) => left.factId.localeCompare(right.factId))) {
    nodes.push(Object.freeze({
      nodeId: stableNodeId('system:unresolved', fact.factId),
      label: `未确认：${fact.factId}`,
      kind: 'unresolved',
      description: `${fact.question}；影响：${fact.impact}`,
      trustBoundary: null,
      unresolved: true,
      componentIds: Object.freeze([]),
      sourcePaths: Object.freeze([]),
    }))
  }
  const edges: StrongFlowDiagramEdge[] = [...solution.payload.connections]
    .sort((left, right) => left.connectionId.localeCompare(right.connectionId))
    .map(connection => {
      const fromNodeId = nodeByComponent.get(connection.fromComponentId)
      const toNodeId = nodeByComponent.get(connection.toComponentId)
      if (fromNodeId === undefined || toNodeId === undefined) {
        diagramError(
          'INVALID_DIAGRAM_INPUT',
          'solution connection references a component that cannot be rendered',
        )
      }
      return Object.freeze({
        edgeId: `system:connection:${hash(connection.connectionId).slice(0, 24)}`,
        fromNodeId,
        toNodeId,
        label: connection.label,
      })
    })
  return Object.freeze({
    requirementId: requirement.artifactId,
    solutionId: solution.artifactId,
    title: `系统架构：${requirement.payload.title}`,
    nodes: Object.freeze(nodes),
    edges: Object.freeze(edges),
  })
}

function unresolvedProcessNode(
  requirement: RequirementSpec,
  solution: SolutionDesign,
): ProcessFlowDiagramNode | undefined {
  const questions = requirement.payload.openQuestions.map(entry => entry.question)
  const facts = solution.payload.unresolvedFacts.map(entry => entry.question)
  if (questions.length === 0 && facts.length === 0) return undefined
  return Object.freeze({
    nodeId: 'process:04-unresolved' as DiagramNodeId,
    label: `待确认信息（${questions.length + facts.length}）`,
    kind: 'state',
    description: [...questions, ...facts].join('；'),
    roleId: null,
    unresolved: true,
  })
}

function processPayload(
  requirement: RequirementSpec,
  solution: SolutionDesign,
): ProcessFlowDiagramPayload {
  const unresolved = unresolvedProcessNode(requirement, solution)
  const nodes: ProcessFlowDiagramNode[] = STRONGFLOW_DEFAULT_PROCESS_TEMPLATE_NODES.map(
    node => Object.freeze({ ...node, unresolved: false }),
  )
  if (unresolved !== undefined) nodes.splice(3, 0, unresolved)
  const edges: StrongFlowDiagramEdge[] = BASE_PROCESS_TEMPLATE_EDGES.map(
    edge => Object.freeze({ ...edge }),
  )
  if (unresolved === undefined) {
    edges.push(Object.freeze({
      edgeId: 'process-edge:diagrams-review',
      fromNodeId: 'process:03-diagrams' as DiagramNodeId,
      toNodeId: 'process:05-human-review' as DiagramNodeId,
      label: '',
    }))
  } else {
    edges.push(
      Object.freeze({
        edgeId: 'process-edge:diagrams-unresolved',
        fromNodeId: 'process:03-diagrams' as DiagramNodeId,
        toNodeId: unresolved.nodeId,
        label: '保留未知项',
      }),
      Object.freeze({
        edgeId: 'process-edge:unresolved-review',
        fromNodeId: unresolved.nodeId,
        toNodeId: 'process:05-human-review' as DiagramNodeId,
        label: '显式展示',
      }),
    )
  }
  return Object.freeze({
    requirementId: requirement.artifactId,
    solutionId: solution.artifactId,
    title: `流程流转：${requirement.payload.title}`,
    nodes: Object.freeze(nodes),
    edges: Object.freeze(edges),
  })
}

function definitionInputs(
  requirementValue: unknown,
  solutionValue: unknown,
): { readonly requirement: RequirementSpec; readonly solution: SolutionDesign } {
  let requirement: RequirementSpec
  let solution: SolutionDesign
  try {
    requirement = parseStrongFlowArtifactAs('REQUIREMENT_SPEC', requirementValue)
    solution = parseStrongFlowArtifactAs('SOLUTION_DESIGN', solutionValue)
  } catch (error) {
    diagramError('INVALID_DIAGRAM_INPUT', 'definition artifact validation failed', { cause: error })
  }
  if (
    requirement.jobId !== solution.jobId
    || solution.payload.requirementId !== requirement.artifactId
    || solution.producer.kind !== 'role'
    || solution.producer.roleId !== 'solution'
    || solution.kernelEventInterval === null
  ) {
    diagramError(
      'DIAGRAM_PAIR_MISMATCH',
      'solution and requirement do not form one current definition pair',
    )
  }
  return Object.freeze({ requirement, solution })
}

function sameRoleProduction(
  solution: SolutionDesign,
  diagram: SystemArchitectureDiagram | ProcessFlowDiagram,
): boolean {
  return isDeepStrictEqual(solution.producer, diagram.producer)
    && isDeepStrictEqual(solution.kernelEventInterval, diagram.kernelEventInterval)
}

function bundle(
  requirement: RequirementSpec,
  solution: SolutionDesign,
  systemArchitectureDiagram: SystemArchitectureDiagram,
  processFlowDiagram: ProcessFlowDiagram,
): StrongFlowDefinitionDiagramBundle {
  const systemArchitecture = renderStrongFlowDefinitionDiagram(systemArchitectureDiagram)
  const processFlow = renderStrongFlowDefinitionDiagram(processFlowDiagram)
  return Object.freeze({
    schemaVersion: STRONGFLOW_DEFINITION_DIAGRAM_BUNDLE_SCHEMA_VERSION,
    jobId: requirement.jobId,
    requirementId: requirement.artifactId,
    solutionId: solution.artifactId,
    systemArchitectureDiagram,
    processFlowDiagram,
    rendered: Object.freeze({ systemArchitecture, processFlow }),
  })
}

/** Deterministically derives both required definition diagrams from one validated solution. */
export function generateStrongFlowDefinitionDiagrams(
  options: GenerateStrongFlowDefinitionDiagramsOptions,
): StrongFlowDefinitionDiagramBundle {
  const { requirement, solution } = definitionInputs(options.requirement, options.solution)
  if (options.systemArchitectureDiagramId === options.processFlowDiagramId) {
    diagramError('INVALID_DIAGRAM_INPUT', 'the two definition diagrams require distinct ids')
  }
  const sourceArtifacts = Object.freeze([
    Object.freeze({ artifactKind: 'REQUIREMENT_SPEC' as const, artifactId: requirement.artifactId }),
    Object.freeze({ artifactKind: 'SOLUTION_DESIGN' as const, artifactId: solution.artifactId }),
  ])
  const common = {
    jobId: requirement.jobId,
    sourceArtifacts,
    producer: solution.producer,
    kernelEventInterval: solution.kernelEventInterval,
    createdAtMillis: options.createdAtMillis,
  }
  let systemArchitectureDiagram: SystemArchitectureDiagram
  let processFlowDiagram: ProcessFlowDiagram
  try {
    systemArchitectureDiagram = materializeStrongFlowArtifact(
      'SYSTEM_ARCHITECTURE_DIAGRAM',
      {
        ...common,
        artifactId: DiagramId(options.systemArchitectureDiagramId),
      },
      systemPayload(requirement, solution),
    )
    processFlowDiagram = materializeStrongFlowArtifact(
      'PROCESS_FLOW_DIAGRAM',
      {
        ...common,
        artifactId: DiagramId(options.processFlowDiagramId),
      },
      processPayload(requirement, solution),
    )
  } catch (error) {
    if (error instanceof StrongFlowDefinitionDiagramError) throw error
    diagramError('INVALID_DIAGRAM_INPUT', 'generated diagram validation failed', { cause: error })
  }
  return bundle(requirement, solution, systemArchitectureDiagram, processFlowDiagram)
}

/** Accepts only the exact built-in diagrams for the supplied requirement and solution. */
export function validateStrongFlowDefinitionDiagramPair(
  options: ValidateStrongFlowDefinitionDiagramPairOptions,
): StrongFlowDefinitionDiagramBundle {
  const { requirement, solution } = definitionInputs(options.requirement, options.solution)
  let systemArchitectureDiagram: SystemArchitectureDiagram
  let processFlowDiagram: ProcessFlowDiagram
  try {
    systemArchitectureDiagram = parseStrongFlowArtifactAs(
      'SYSTEM_ARCHITECTURE_DIAGRAM',
      options.systemArchitectureDiagram,
    )
    processFlowDiagram = parseStrongFlowArtifactAs(
      'PROCESS_FLOW_DIAGRAM',
      options.processFlowDiagram,
    )
  } catch (error) {
    diagramError('INVALID_DIAGRAM_INPUT', 'definition diagram validation failed', { cause: error })
  }
  if (
    systemArchitectureDiagram.jobId !== requirement.jobId
    || processFlowDiagram.jobId !== requirement.jobId
    || systemArchitectureDiagram.artifactId === processFlowDiagram.artifactId
    || systemArchitectureDiagram.payload.requirementId !== requirement.artifactId
    || processFlowDiagram.payload.requirementId !== requirement.artifactId
    || systemArchitectureDiagram.payload.solutionId !== solution.artifactId
    || processFlowDiagram.payload.solutionId !== solution.artifactId
    || !sameRoleProduction(solution, systemArchitectureDiagram)
    || !sameRoleProduction(solution, processFlowDiagram)
  ) {
    diagramError(
      'DIAGRAM_PAIR_MISMATCH',
      'definition diagrams do not belong to the exact current solution run',
    )
  }
  if (
    !isDeepStrictEqual(
      systemArchitectureDiagram.payload,
      systemPayload(requirement, solution),
    )
    || !isDeepStrictEqual(
      processFlowDiagram.payload,
      processPayload(requirement, solution),
    )
  ) {
    diagramError(
      'DIAGRAM_TEMPLATE_MISMATCH',
      'definition diagrams do not match the deterministic built-in templates',
    )
  }
  return bundle(requirement, solution, systemArchitectureDiagram, processFlowDiagram)
}

type DefinitionDiagram = SystemArchitectureDiagram | ProcessFlowDiagram
type DefinitionDiagramNode =
  | SystemArchitectureDiagramNode
  | ProcessFlowDiagramNode

function canonicalDiagram(value: unknown): DefinitionDiagram {
  try {
    const system = parseStrongFlowArtifactAs('SYSTEM_ARCHITECTURE_DIAGRAM', value)
    return system
  } catch (systemError) {
    try {
      return parseStrongFlowArtifactAs('PROCESS_FLOW_DIAGRAM', value)
    } catch (processError) {
      diagramError('INVALID_DIAGRAM_INPUT', 'value is not a definition diagram', {
        cause: new AggregateError([systemError, processError]),
      })
    }
  }
}

function renderLimits(diagram: DefinitionDiagram): void {
  if (
    diagram.payload.nodes.length === 0
    || diagram.payload.nodes.length > MAX_RENDER_NODES
    || diagram.payload.edges.length > MAX_RENDER_EDGES
    || diagram.payload.title.length > 240
  ) diagramError('DIAGRAM_TOO_LARGE', 'diagram exceeds safe render limits')
  for (const node of diagram.payload.nodes) {
    if (
      node.label.length > MAX_RENDER_LABEL_LENGTH
      || node.description.length > MAX_RENDER_DESCRIPTION_LENGTH
      || ('trustBoundary' in node
        && node.trustBoundary !== null
        && node.trustBoundary.length > MAX_RENDER_LABEL_LENGTH)
    ) diagramError('DIAGRAM_TOO_LARGE', 'diagram text exceeds safe render limits')
  }
  if (diagram.payload.edges.some(edge => edge.label.length > 240)) {
    diagramError('DIAGRAM_TOO_LARGE', 'diagram edge text exceeds safe render limits')
  }
  const textValues = [
    diagram.payload.title,
    ...diagram.payload.nodes.flatMap(node => [
      node.label,
      node.description,
      ...('trustBoundary' in node && node.trustBoundary !== null
        ? [node.trustBoundary]
        : []),
    ]),
    ...diagram.payload.edges.map(edge => edge.label),
  ]
  if (textValues.some(value => (
    /<\s*\/?\s*(?:script|iframe|object|embed|foreignObject|img|image|a)\b/iu.test(value)
    || /\b(?:javascript|data):/iu.test(value)
    || /\b(?:href|src|on[a-z]+)\s*=/iu.test(value)
  ))) diagramError('UNSAFE_RENDER_OUTPUT', 'diagram text contains active markup')
}

function escapeXml(value: string): string {
  return value.replace(/[&<>"']/gu, character => {
    switch (character) {
      case '&': return '&amp;'
      case '<': return '&lt;'
      case '>': return '&gt;'
      case '"': return '&quot;'
      default: return '&apos;'
    }
  })
}

function escapeMermaid(value: string): string {
  return Array.from(value).map(character => {
    if (/^[\p{L}\p{N} .,:：，。/_-]$/u.test(character)) return character
    return `&#x${character.codePointAt(0)?.toString(16).toUpperCase()};`
  }).join('')
}

function nodeAlias(nodeId: DiagramNodeId): string {
  return `node_${hash(nodeId).slice(0, 16)}`
}

function nodeLabel(node: DefinitionDiagramNode): string {
  const unresolved = node.unresolved ? '？未确认 · ' : '✓ 正常流转 · '
  const boundary = 'trustBoundary' in node && node.trustBoundary !== null
    ? ` · 边界：${node.trustBoundary}`
    : ''
  return `${unresolved}${node.label}${boundary}`
}

function renderMermaid(diagram: DefinitionDiagram): string {
  const direction = diagram.artifactKind === 'SYSTEM_ARCHITECTURE_DIAGRAM' ? 'LR' : 'TD'
  const lines = [
    `%% schemaVersion=${STRONGFLOW_DEFINITION_DIAGRAM_RENDER_SCHEMA_VERSION}`,
    `%% diagramId=${diagram.artifactId}`,
    `%% requirementId=${diagram.payload.requirementId}`,
    `%% solutionId=${diagram.payload.solutionId}`,
    `flowchart ${direction}`,
  ]
  const aliases = new Set<string>()
  for (const node of diagram.payload.nodes) {
    const alias = nodeAlias(node.nodeId)
    if (aliases.has(alias)) diagramError('UNSAFE_RENDER_OUTPUT', 'diagram node alias collided')
    aliases.add(alias)
    lines.push(`  ${alias}["${escapeMermaid(nodeLabel(node))}"]`)
  }
  for (const edge of diagram.payload.edges) {
    const from = nodeAlias(edge.fromNodeId)
    const to = nodeAlias(edge.toNodeId)
    lines.push(edge.label.length === 0
      ? `  ${from} --> ${to}`
      : `  ${from} -->|"${escapeMermaid(edge.label)}"| ${to}`)
  }
  lines.push('  classDef normal fill:#dcfce7,stroke:#16a34a,color:#14532d,stroke-width:2px;')
  lines.push('  classDef unresolved fill:#dcfce7,stroke:#16a34a,color:#14532d,stroke-width:2px,stroke-dasharray:6 4;')
  for (const node of diagram.payload.nodes) {
    lines.push(`  class ${nodeAlias(node.nodeId)} ${node.unresolved ? 'unresolved' : 'normal'};`)
  }
  return `${lines.join('\n')}\n`
}

interface NodePosition {
  readonly x: number
  readonly y: number
}

function wrappedLabel(value: string): readonly string[] {
  const characters = Array.from(value)
  const lines: string[] = []
  for (let index = 0; index < characters.length; index += 28) {
    lines.push(characters.slice(index, index + 28).join(''))
  }
  return Object.freeze(lines)
}

function renderSvg(diagram: DefinitionDiagram, layoutId: string): string {
  const columns = diagram.artifactKind === 'SYSTEM_ARCHITECTURE_DIAGRAM'
    ? Math.min(3, diagram.payload.nodes.length)
    : 3
  const rows = Math.ceil(diagram.payload.nodes.length / columns)
  const width = PADDING * 2 + columns * NODE_WIDTH + (columns - 1) * COLUMN_GAP
  const height = PADDING * 2 + rows * NODE_HEIGHT + (rows - 1) * ROW_GAP
  const positions = new Map<DiagramNodeId, NodePosition>()
  for (const [index, node] of diagram.payload.nodes.entries()) {
    positions.set(node.nodeId, Object.freeze({
      x: PADDING + (index % columns) * (NODE_WIDTH + COLUMN_GAP),
      y: PADDING + Math.floor(index / columns) * (NODE_HEIGHT + ROW_GAP),
    }))
  }
  const titleId = `diagram-title-${layoutId.slice(-16)}`
  const descriptionId = `diagram-description-${layoutId.slice(-16)}`
  const output = [
    `<svg xmlns="http://www.w3.org/2000/svg" role="img" aria-labelledby="${titleId} ${descriptionId}" viewBox="0 0 ${width} ${height}" data-schema-version="1" data-visual-state="before-execution" data-layout-id="${layoutId}" data-diagram-id="${escapeXml(diagram.artifactId)}" data-requirement-id="${escapeXml(diagram.payload.requirementId)}" data-solution-id="${escapeXml(diagram.payload.solutionId)}">`,
    `  <title id="${titleId}">${escapeXml(diagram.payload.title)}</title>`,
    `  <desc id="${descriptionId}">执行前状态；所有节点均为绿色正常流转，未确认节点另有问号和虚线标记。</desc>`,
    '  <defs>',
    '    <marker id="definition-arrow" markerWidth="10" markerHeight="10" refX="9" refY="3" orient="auto" markerUnits="strokeWidth">',
    '      <path d="M0,0 L0,6 L9,3 z" fill="#64748b"/>',
    '    </marker>',
    '  </defs>',
  ]
  for (const edge of diagram.payload.edges) {
    const from = positions.get(edge.fromNodeId)
    const to = positions.get(edge.toNodeId)
    if (from === undefined || to === undefined) {
      diagramError('INVALID_DIAGRAM_INPUT', 'diagram edge has no render position')
    }
    const x1 = from.x + NODE_WIDTH / 2
    const y1 = from.y + NODE_HEIGHT / 2
    const x2 = to.x + NODE_WIDTH / 2
    const y2 = to.y + NODE_HEIGHT / 2
    output.push(
      `  <g data-edge-id="${escapeXml(edge.edgeId)}">`,
      `    <line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" stroke="#64748b" stroke-width="2" marker-end="url(#definition-arrow)"/>`,
      ...(edge.label.length === 0
        ? []
        : [`    <text x="${(x1 + x2) / 2}" y="${(y1 + y2) / 2 - 6}" text-anchor="middle" font-family="system-ui,sans-serif" font-size="12" fill="#334155">${escapeXml(edge.label)}</text>`]),
      '  </g>',
    )
  }
  for (const node of diagram.payload.nodes) {
    const position = positions.get(node.nodeId)
    if (position === undefined) diagramError('INVALID_DIAGRAM_INPUT', 'node has no position')
    const lines = wrappedLabel(node.label)
    const boundary = 'trustBoundary' in node ? node.trustBoundary : null
    const accessible = [
      node.unresolved ? '未确认' : '正常流转',
      node.label,
      node.description,
      ...(boundary === null ? [] : [`信任边界 ${boundary}`]),
    ].join('；')
    output.push(
      `  <g id="diagram-${nodeAlias(node.nodeId)}" role="group" aria-label="${escapeXml(accessible)}" tabindex="0" data-node-id="${escapeXml(node.nodeId)}" data-state="normal" data-unresolved="${String(node.unresolved)}">`,
      `    <title>${escapeXml(accessible)}</title>`,
      `    <rect x="${position.x}" y="${position.y}" width="${NODE_WIDTH}" height="${NODE_HEIGHT}" rx="14" fill="#dcfce7" stroke="#16a34a" stroke-width="2"${node.unresolved ? ' stroke-dasharray="6 4"' : ''}/>` ,
      `    <text x="${position.x + 18}" y="${position.y + 28}" font-family="system-ui,sans-serif" font-size="13" font-weight="700" fill="#166534">${node.unresolved ? '？ 未确认' : '✓ 正常流转'}</text>`,
      `    <text x="${position.x + 18}" y="${position.y + 56}" font-family="system-ui,sans-serif" font-size="16" font-weight="600" fill="#14532d">`,
      ...lines.map((line, index) => `      <tspan x="${position.x + 18}" dy="${index === 0 ? 0 : 20}">${escapeXml(line)}</tspan>`),
      '    </text>',
      ...(boundary === null
        ? []
        : [`    <text x="${position.x + 18}" y="${position.y + NODE_HEIGHT - 14}" font-family="system-ui,sans-serif" font-size="12" fill="#166534">边界：${escapeXml(boundary)}</text>`]),
      '  </g>',
    )
  }
  output.push('</svg>', '')
  return output.join('\n')
}

function assertSafeOutput(mermaid: string, svg: string): void {
  if (
    /(?:^|\n)\s*(?:click\s|%%\{)/iu.test(mermaid)
    || /<(?:script|foreignObject|iframe|object|embed)\b/iu.test(svg)
    || /\b(?:href|xlink:href|src)\s*=/iu.test(svg)
    || /\bon[a-z]+\s*=/iu.test(svg)
  ) diagramError('UNSAFE_RENDER_OUTPUT', 'diagram renderer produced active content')
}

/** Produces deterministic, non-interactive Mermaid and SVG for the approved pre-run view. */
export function renderStrongFlowDefinitionDiagram(
  value: unknown,
): StrongFlowRenderedDefinitionDiagram {
  const diagram = canonicalDiagram(value)
  renderLimits(diagram)
  const layoutSource = JSON.stringify({
    artifactKind: diagram.artifactKind,
    nodeIds: diagram.payload.nodes.map(node => node.nodeId),
    edges: diagram.payload.edges.map(edge => ({
      edgeId: edge.edgeId,
      fromNodeId: edge.fromNodeId,
      toNodeId: edge.toNodeId,
    })),
  })
  const layoutId = `diagram-layout-sha256-${hash(layoutSource)}`
  const mermaid = renderMermaid(diagram)
  const svg = renderSvg(diagram, layoutId)
  assertSafeOutput(mermaid, svg)
  return Object.freeze({
    schemaVersion: STRONGFLOW_DEFINITION_DIAGRAM_RENDER_SCHEMA_VERSION,
    artifactKind: diagram.artifactKind,
    diagramId: diagram.artifactId,
    requirementId: diagram.payload.requirementId,
    solutionId: diagram.payload.solutionId,
    visualState: 'before-execution',
    layoutId,
    nodeIds: Object.freeze(diagram.payload.nodes.map(node => node.nodeId)),
    mermaid,
    mermaidSha256: hash(mermaid),
    svg,
    svgSha256: hash(svg),
  })
}
