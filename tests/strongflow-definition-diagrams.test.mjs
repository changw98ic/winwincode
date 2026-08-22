import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DiagramId,
  JobId,
  RequirementId,
  SolutionId,
  UserRequestId,
  materializeStrongFlowArtifact,
} from '../packages/contracts/dist/index.js'
import {
  STRONGFLOW_REQUIRED_PROCESS_NODE_IDS,
  StrongFlowDefinitionDiagramError,
  generateStrongFlowDefinitionDiagrams,
  renderStrongFlowDefinitionDiagram,
  validateStrongFlowDefinitionDiagramPair,
} from '../packages/strongflow/dist/index.js'

const JOB_ID = JobId('diagram-job-1')
const USER_REQUEST_ID = UserRequestId('diagram-user-request-1')
const REQUIREMENT_ID = RequirementId('diagram-requirement-1')
const SOLUTION_ID = SolutionId('diagram-solution-1')

function eventInterval(suffix) {
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: `diagram-lineage-${suffix}`,
    contextId: `diagram-context-${suffix}`,
    generation: 1,
    kernelSessionId: `diagram-kernel-${suffix}`,
    kernelStreamId: `diagram-stream-${suffix}`,
    turnId: `diagram-turn-${suffix}`,
    firstSequence: '20',
    lastSequence: '22',
    eventCount: 3,
  })
}

function roleMetadata(artifactId, roleId, sourceArtifacts, suffix) {
  return Object.freeze({
    artifactId,
    jobId: JOB_ID,
    sourceArtifacts,
    producer: Object.freeze({
      kind: 'role',
      roleId,
      stageRunId: `diagram-stage-${suffix}`,
      attemptId: `diagram-attempt-${suffix}`,
    }),
    kernelEventInterval: eventInterval(suffix),
    createdAtMillis: 1_920_000_000_000,
  })
}

function definitionFixture(overrides = {}) {
  const requirement = materializeStrongFlowArtifact(
    'REQUIREMENT_SPEC',
    roleMetadata(
      REQUIREMENT_ID,
      'requirements',
      [{ artifactKind: 'USER_REQUEST', artifactId: USER_REQUEST_ID }],
      'requirements',
    ),
    {
      title: overrides.title ?? '审核 Agent 执行变更',
      summary: '需求、方案和两张图必须一起接受人工审核。',
      goals: [{ id: 'goal-diagrams', text: '每份方案都有两张可读的定义图。' }],
      nonGoals: [],
      constraints: [{ id: 'constraint-safe', text: '不执行模型提供的图形标记。' }],
      acceptanceCriteria: [{
        criterionId: 'criterion-pair',
        statement: '两张图引用当前需求和方案。',
        verification: '核对制品来源和渲染结果中的身份。',
      }],
      repositoryFacts: [],
      risks: [],
      openQuestions: overrides.openQuestions ?? [{
        questionId: 'question-auth',
        question: 'DSH 界面最终使用哪一种本地认证实现？',
        blocking: false,
      }],
    },
  )
  const solution = materializeStrongFlowArtifact(
    'SOLUTION_DESIGN',
    roleMetadata(
      SOLUTION_ID,
      'solution',
      [{ artifactKind: 'REQUIREMENT_SPEC', artifactId: REQUIREMENT_ID }],
      'solution',
    ),
    {
      requirementId: REQUIREMENT_ID,
      summary: '从结构化组件和固定流程生成安全图形。',
      decisions: [{
        decisionId: 'decision-render',
        title: '固定渲染器',
        decision: '由程序生成 Mermaid 和 SVG。',
        rationale: '模型文本不会作为可执行图形标记进入界面。',
        requirementItemIds: ['goal-diagrams', 'criterion-pair'],
      }],
      components: overrides.components ?? [
        {
          componentId: 'component-ui',
          name: 'StrongFlow 工作台',
          kind: 'surface',
          responsibility: '显示定义、图和人工审核入口。',
          trustBoundary: '本地 DSH 会话',
          sourcePaths: ['packages/strongflow/src/client.ts'],
        },
        {
          componentId: 'component-contracts',
          name: '制品校验器',
          kind: 'module',
          responsibility: '拒绝格式错误或身份不一致的制品。',
          trustBoundary: '本地进程',
          sourcePaths: ['packages/contracts/src/strongflow-artifact.ts'],
        },
        {
          componentId: 'component-provider',
          name: 'DSH 模型服务',
          kind: 'external',
          responsibility: '通过 DSH 路线提供模型流。',
          trustBoundary: '外部模型边界',
          sourcePaths: [],
        },
      ],
      connections: overrides.connections ?? [
        {
          connectionId: 'connection-ui-contracts',
          fromComponentId: 'component-ui',
          toComponentId: 'component-contracts',
          label: '提交审核操作',
        },
        {
          connectionId: 'connection-contracts-provider',
          fromComponentId: 'component-contracts',
          toComponentId: 'component-provider',
          label: '受管模型请求',
        },
      ],
      unresolvedFacts: overrides.unresolvedFacts ?? [{
        factId: 'fact-ui-width',
        question: '窄屏默认使用几列布局？',
        impact: '只影响后续界面适配，不改变稳定节点身份。',
      }],
      risks: [],
    },
  )
  return Object.freeze({ requirement, solution })
}

function generate(definition = definitionFixture()) {
  return generateStrongFlowDefinitionDiagrams({
    ...definition,
    systemArchitectureDiagramId: DiagramId('diagram-system-1'),
    processFlowDiagramId: DiagramId('diagram-process-1'),
    createdAtMillis: 1_920_000_000_100,
  })
}

function diagramError(code) {
  return error => error instanceof StrongFlowDefinitionDiagramError && error.code === code
}

test('one current solution deterministically produces both required definition diagrams', () => {
  const definition = definitionFixture()
  const first = generate(definition)
  const second = generate(definition)

  assert.deepEqual(second, first)
  assert.equal(first.requirementId, REQUIREMENT_ID)
  assert.equal(first.solutionId, SOLUTION_ID)
  assert.equal(first.systemArchitectureDiagram.payload.requirementId, REQUIREMENT_ID)
  assert.equal(first.systemArchitectureDiagram.payload.solutionId, SOLUTION_ID)
  assert.equal(first.processFlowDiagram.payload.requirementId, REQUIREMENT_ID)
  assert.equal(first.processFlowDiagram.payload.solutionId, SOLUTION_ID)
  assert.deepEqual(
    first.systemArchitectureDiagram.sourceArtifacts,
    [
      { artifactKind: 'REQUIREMENT_SPEC', artifactId: REQUIREMENT_ID },
      { artifactKind: 'SOLUTION_DESIGN', artifactId: SOLUTION_ID },
    ],
  )
  assert.deepEqual(
    first.systemArchitectureDiagram.producer,
    definition.solution.producer,
  )
  assert.deepEqual(
    first.systemArchitectureDiagram.kernelEventInterval,
    definition.solution.kernelEventInterval,
  )

  const componentNodes = first.systemArchitectureDiagram.payload.nodes.filter(
    node => node.componentIds.length === 1,
  )
  assert.deepEqual(
    componentNodes.flatMap(node => node.componentIds).sort(),
    ['component-contracts', 'component-provider', 'component-ui'],
  )
  assert.ok(componentNodes.some(node => (
    node.kind === 'external' && node.trustBoundary === '外部模型边界'
  )))
  assert.ok(first.systemArchitectureDiagram.payload.nodes.some(node => node.unresolved))
  assert.ok(first.processFlowDiagram.payload.nodes.some(node => node.unresolved))
  for (const nodeId of STRONGFLOW_REQUIRED_PROCESS_NODE_IDS) {
    assert.ok(first.processFlowDiagram.payload.nodes.some(node => node.nodeId === nodeId))
  }

  assert.equal(first.rendered.systemArchitecture.visualState, 'before-execution')
  assert.equal(first.rendered.processFlow.visualState, 'before-execution')
  assert.match(first.rendered.systemArchitecture.svg, /role="img"/u)
  assert.match(first.rendered.systemArchitecture.svg, /data-state="normal"/u)
  assert.match(first.rendered.systemArchitecture.svg, /✓ 正常流转/u)
  assert.match(first.rendered.systemArchitecture.svg, /？ 未确认/u)
  assert.match(first.rendered.processFlow.mermaid, /fill:#dcfce7/u)
  assert.match(first.rendered.processFlow.svg, new RegExp(`data-requirement-id="${REQUIREMENT_ID}"`, 'u'))
  assert.equal(first.rendered.systemArchitecture.layoutId, second.rendered.systemArchitecture.layoutId)
  assert.equal(first.rendered.systemArchitecture.svgSha256, second.rendered.systemArchitecture.svgSha256)
})

test('the exact pair survives transport and regenerates the same evidence render', () => {
  const definition = definitionFixture()
  const generated = generate(definition)
  const transported = JSON.parse(JSON.stringify(generated))
  const validated = validateStrongFlowDefinitionDiagramPair({
    requirement: JSON.parse(JSON.stringify(definition.requirement)),
    solution: JSON.parse(JSON.stringify(definition.solution)),
    systemArchitectureDiagram: transported.systemArchitectureDiagram,
    processFlowDiagram: transported.processFlowDiagram,
  })
  assert.deepEqual(validated, generated)
  assert.ok(Object.isFrozen(validated))
  assert.ok(Object.isFrozen(validated.rendered))

  const tamperedSystem = structuredClone(transported.systemArchitectureDiagram)
  tamperedSystem.payload.nodes[0].label = '模型改写后的另一个标签'
  assert.throws(
    () => validateStrongFlowDefinitionDiagramPair({
      ...definition,
      systemArchitectureDiagram: tamperedSystem,
      processFlowDiagram: transported.processFlowDiagram,
    }),
    diagramError('DIAGRAM_TEMPLATE_MISMATCH'),
  )

  const staleSolution = structuredClone(definition.solution)
  staleSolution.artifactId = 'diagram-solution-stale'
  assert.throws(
    () => validateStrongFlowDefinitionDiagramPair({
      requirement: definition.requirement,
      solution: staleSolution,
      systemArchitectureDiagram: generated.systemArchitectureDiagram,
      processFlowDiagram: generated.processFlowDiagram,
    }),
    error => (
      error instanceof StrongFlowDefinitionDiagramError
      && ['INVALID_DIAGRAM_INPUT', 'DIAGRAM_PAIR_MISMATCH'].includes(error.code)
    ),
  )
})

test('safe punctuation is escaped and active markup fails before UI rendering', () => {
  const safeDefinition = definitionFixture({
    components: [{
      componentId: 'component-safe-markup',
      name: 'Core <UI> & "API"',
      kind: 'module',
      responsibility: '显示文本，不执行标记。',
      trustBoundary: 'local < boundary',
      sourcePaths: [],
    }],
    connections: [],
    openQuestions: [],
    unresolvedFacts: [],
  })
  const safe = generate(safeDefinition)
  assert.doesNotMatch(safe.rendered.systemArchitecture.svg, /<UI>/u)
  assert.match(safe.rendered.systemArchitecture.svg, /&lt;UI&gt;/u)
  assert.doesNotMatch(safe.rendered.systemArchitecture.mermaid, /\[".*"API"/u)
  assert.doesNotMatch(safe.rendered.systemArchitecture.svg, /<(?:script|iframe|image)\b/iu)
  assert.doesNotMatch(safe.rendered.systemArchitecture.svg, /\b(?:href|src)\s*=/iu)

  const unsafeDefinition = definitionFixture({
    components: [{
      componentId: 'component-unsafe',
      name: '</text><script>alert(1)</script>',
      kind: 'external',
      responsibility: 'unsafe fixture',
      trustBoundary: 'external',
      sourcePaths: [],
    }],
    connections: [],
    openQuestions: [],
    unresolvedFacts: [],
  })
  assert.throws(() => generate(unsafeDefinition), diagramError('UNSAFE_RENDER_OUTPUT'))

  const generated = generate(safeDefinition)
  const tooLarge = structuredClone(generated.systemArchitectureDiagram)
  tooLarge.payload.nodes[0].label = 'x'.repeat(113)
  assert.throws(
    () => renderStrongFlowDefinitionDiagram(tooLarge),
    diagramError('DIAGRAM_TOO_LARGE'),
  )
})

test('missing information remains a visible unresolved node instead of invented detail', () => {
  const withUnknowns = generate(definitionFixture())
  const withoutUnknowns = generate(definitionFixture({
    openQuestions: [],
    unresolvedFacts: [],
  }))
  assert.ok(withUnknowns.systemArchitectureDiagram.payload.nodes.some(node => (
    node.unresolved && node.label.startsWith('未确认：')
  )))
  assert.ok(withUnknowns.processFlowDiagram.payload.nodes.some(node => (
    node.nodeId === 'process:04-unresolved' && node.unresolved
  )))
  assert.ok(!withoutUnknowns.systemArchitectureDiagram.payload.nodes.some(node => node.unresolved))
  assert.ok(!withoutUnknowns.processFlowDiagram.payload.nodes.some(node => node.unresolved))
  assert.ok(withoutUnknowns.processFlowDiagram.payload.edges.some(edge => (
    edge.edgeId === 'process-edge:diagrams-review'
  )))
})
