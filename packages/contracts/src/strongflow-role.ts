/** Canonical role roster and validated runtime configuration for StrongFlow. */

export const STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_ROLE_IDS = Object.freeze([
  'requirements',
  'solution',
  'planner',
  'executor',
  'reviewer',
  'verifier',
  'adversarial-verifier',
  'remediator',
] as const)

export type StrongFlowRoleId = typeof STRONGFLOW_ROLE_IDS[number]

export const STRONGFLOW_ROLE_ARTIFACT_KINDS = Object.freeze([
  'USER_REQUEST',
  'REQUIREMENT_SPEC',
  'SOLUTION_DESIGN',
  'SYSTEM_ARCHITECTURE_DIAGRAM',
  'PROCESS_FLOW_DIAGRAM',
  'HUMAN_REVIEW_RECORD',
  'EXECUTION_PLAN',
  'PATCH_MANIFEST',
  'REVIEW_REPORT',
  'VERIFICATION_REPORT',
  'REMEDIATION_REQUEST',
  'REMEDIATION_REPORT',
  'DELIVERY_RECEIPT',
] as const)

export type StrongFlowRoleArtifactKind = typeof STRONGFLOW_ROLE_ARTIFACT_KINDS[number]

export const STRONGFLOW_ROLE_TOOLS = Object.freeze([
  'artifact.read',
  'artifact.write',
  'workspace.read',
  'code.search',
  'candidate.diff',
  'command.run',
  'test.run',
  'candidate.patch',
] as const)

export type StrongFlowRoleTool = typeof STRONGFLOW_ROLE_TOOLS[number]

export type StrongFlowRoleWorkspaceMode =
  | 'source-read-only'
  | 'candidate-read-only'
  | 'candidate-write'

export interface StrongFlowRoleModelRoute {
  readonly provider: string
  readonly model: string
}

export interface StrongFlowRoleBudget {
  readonly maxTurns: number
  readonly maxWallTimeMillis: number
  readonly maxTotalTokens: number
  readonly maxCostUsdMicros: number
}

export interface StrongFlowRoleSandboxPolicy {
  readonly filesystem: 'read-only' | 'candidate-write'
  readonly network: 'disabled'
  readonly approval: 'never'
}

export interface StrongFlowRoleSpec {
  readonly id: StrongFlowRoleId
  readonly displayName: string
  readonly modelRoute: StrongFlowRoleModelRoute
  readonly reasoningEffort: string | null
  readonly budget: StrongFlowRoleBudget
  readonly systemInstructions: string
  readonly allowedTools: readonly StrongFlowRoleTool[]
  readonly sandboxPolicy: StrongFlowRoleSandboxPolicy
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
  readonly acceptedInputArtifacts: readonly StrongFlowRoleArtifactKind[]
  readonly requiredOutputArtifacts: readonly StrongFlowRoleArtifactKind[]
}

export interface StrongFlowRoleConfiguration {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION
  readonly roles: readonly StrongFlowRoleSpec[]
}

export interface StrongFlowRoleModelCatalogEntry {
  readonly provider: string
  readonly model: string
  readonly reasoningEfforts: readonly (string | null)[]
}

export interface StrongFlowRoleRuntimeAssignment {
  readonly modelRoute: StrongFlowRoleModelRoute
  readonly reasoningEffort: string | null
  readonly budget: StrongFlowRoleBudget
}

export type StrongFlowRoleRuntimeAssignments = Readonly<
  Record<StrongFlowRoleId, StrongFlowRoleRuntimeAssignment>
>

export type StrongFlowRoleConfigurationErrorCode =
  | 'INVALID_CONFIGURATION'
  | 'INVALID_MODEL_CATALOG'
  | 'UNKNOWN_ROLE'
  | 'DUPLICATE_ROLE'
  | 'UNKNOWN_MODEL_ROUTE'
  | 'UNKNOWN_REASONING_EFFORT'
  | 'UNKNOWN_TOOL'
  | 'UNKNOWN_ARTIFACT'
  | 'POLICY_MISMATCH'

export class StrongFlowRoleConfigurationError extends Error {
  readonly code: StrongFlowRoleConfigurationErrorCode

  constructor(code: StrongFlowRoleConfigurationErrorCode, message: string) {
    super(message)
    this.name = 'StrongFlowRoleConfigurationError'
    this.code = code
  }
}

interface CanonicalRolePolicy {
  readonly displayName: string
  readonly systemInstructions: string
  readonly allowedTools: readonly StrongFlowRoleTool[]
  readonly sandboxPolicy: StrongFlowRoleSandboxPolicy
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
  readonly acceptedInputArtifacts: readonly StrongFlowRoleArtifactKind[]
  readonly requiredOutputArtifacts: readonly StrongFlowRoleArtifactKind[]
}

function frozenList<Value extends string>(...values: readonly Value[]): readonly Value[] {
  return Object.freeze([...values])
}

const READ_ONLY_SANDBOX: StrongFlowRoleSandboxPolicy = Object.freeze({
  filesystem: 'read-only',
  network: 'disabled',
  approval: 'never',
})

const CANDIDATE_WRITE_SANDBOX: StrongFlowRoleSandboxPolicy = Object.freeze({
  filesystem: 'candidate-write',
  network: 'disabled',
  approval: 'never',
})

const DEFINITION_INPUTS = frozenList<StrongFlowRoleArtifactKind>(
  'REQUIREMENT_SPEC',
  'SOLUTION_DESIGN',
  'SYSTEM_ARCHITECTURE_DIAGRAM',
  'PROCESS_FLOW_DIAGRAM',
)

const APPROVED_DEFINITION_INPUTS = frozenList<StrongFlowRoleArtifactKind>(
  ...DEFINITION_INPUTS,
  'HUMAN_REVIEW_RECORD',
)

const CANDIDATE_INPUTS = frozenList<StrongFlowRoleArtifactKind>(
  ...APPROVED_DEFINITION_INPUTS,
  'EXECUTION_PLAN',
  'PATCH_MANIFEST',
)

const ROLE_POLICIES: Readonly<Record<StrongFlowRoleId, CanonicalRolePolicy>> = Object.freeze({
  requirements: Object.freeze({
    displayName: 'Requirements Analyst',
    systemInstructions: 'Produce only a RequirementSpec from the user request and verified repository facts. Record goals, constraints, acceptance checks, risks, and unresolved questions. Do not choose architecture, implementation, files, commands, patches, model routes, or an approval outcome.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
    ),
    sandboxPolicy: READ_ONLY_SANDBOX,
    workspaceMode: 'source-read-only',
    acceptedInputArtifacts: frozenList('USER_REQUEST'),
    requiredOutputArtifacts: frozenList('REQUIREMENT_SPEC'),
  }),
  solution: Object.freeze({
    displayName: 'Solution Architect',
    systemInstructions: 'Produce a structured SolutionDesign and the two required diagram payloads for exactly one RequirementSpec. Record components, connections, trust boundaries, external systems, and unresolved facts; preserve the built-in process stages and stable node identities. Never emit Mermaid, SVG, HTML, scripts, external resources, or links. Do not approve the definition, plan execution, or modify the candidate workspace.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
    ),
    sandboxPolicy: READ_ONLY_SANDBOX,
    workspaceMode: 'source-read-only',
    acceptedInputArtifacts: frozenList('REQUIREMENT_SPEC'),
    requiredOutputArtifacts: frozenList(
      'SOLUTION_DESIGN',
      'SYSTEM_ARCHITECTURE_DIAGRAM',
      'PROCESS_FLOW_DIAGRAM',
    ),
  }),
  planner: Object.freeze({
    displayName: 'Planner',
    systemInstructions: 'Produce an ExecutionPlan only from the exact requirement, solution, diagrams, and authenticated human approval supplied to this run. Keep work bounded and verifiable. Do not change the approved definition, write candidate files, or declare completion.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
    ),
    sandboxPolicy: READ_ONLY_SANDBOX,
    workspaceMode: 'source-read-only',
    acceptedInputArtifacts: APPROVED_DEFINITION_INPUTS,
    requiredOutputArtifacts: frozenList('EXECUTION_PLAN'),
  }),
  executor: Object.freeze({
    displayName: 'Executor',
    systemInstructions: 'Implement only the approved ExecutionPlan in the assigned candidate workspace. Record the exact changed files, commands, tests, and evidence in a PatchManifest. Do not alter definition artifacts, approve work, or declare verification success.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
      'candidate.diff',
      'command.run',
      'test.run',
      'candidate.patch',
    ),
    sandboxPolicy: CANDIDATE_WRITE_SANDBOX,
    workspaceMode: 'candidate-write',
    acceptedInputArtifacts: frozenList('EXECUTION_PLAN'),
    requiredOutputArtifacts: frozenList('PATCH_MANIFEST'),
  }),
  reviewer: Object.freeze({
    displayName: 'Reviewer',
    systemInstructions: 'Inspect the frozen candidate against the approved definition and ExecutionPlan. Produce a ReviewReport with exact findings and evidence. Do not modify candidate files, approve the definition, or convert review findings into a completion decision.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
      'candidate.diff',
      'command.run',
      'test.run',
    ),
    sandboxPolicy: READ_ONLY_SANDBOX,
    workspaceMode: 'candidate-read-only',
    acceptedInputArtifacts: CANDIDATE_INPUTS,
    requiredOutputArtifacts: frozenList('REVIEW_REPORT'),
  }),
  verifier: Object.freeze({
    displayName: 'Verifier',
    systemInstructions: 'Run the frozen acceptance checks against the read-only candidate and produce a VerificationReport from observed evidence. Do not change candidate files, weaken checks, approve the definition, or declare final delivery.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
      'candidate.diff',
      'command.run',
      'test.run',
    ),
    sandboxPolicy: READ_ONLY_SANDBOX,
    workspaceMode: 'candidate-read-only',
    acceptedInputArtifacts: frozenList(...CANDIDATE_INPUTS, 'REVIEW_REPORT'),
    requiredOutputArtifacts: frozenList('VERIFICATION_REPORT'),
  }),
  'adversarial-verifier': Object.freeze({
    displayName: 'Adversarial Verifier',
    systemInstructions: 'Challenge the frozen candidate, approved assumptions, trust boundaries, failure handling, and negative cases from a read-only workspace. Produce an independent VerificationReport with reproducible evidence. Do not modify files or approve delivery.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
      'candidate.diff',
      'command.run',
      'test.run',
    ),
    sandboxPolicy: READ_ONLY_SANDBOX,
    workspaceMode: 'candidate-read-only',
    acceptedInputArtifacts: frozenList(
      ...CANDIDATE_INPUTS,
      'REVIEW_REPORT',
      'VERIFICATION_REPORT',
    ),
    requiredOutputArtifacts: frozenList('VERIFICATION_REPORT'),
  }),
  remediator: Object.freeze({
    displayName: 'Remediator',
    systemInstructions: 'Apply only the bounded RemediationRequest to the assigned candidate workspace. Preserve unrelated accepted work and record changes and evidence in a new PatchManifest and RemediationReport. Do not broaden scope, approve work, or declare verification success.',
    allowedTools: frozenList(
      'artifact.read',
      'artifact.write',
      'workspace.read',
      'code.search',
      'candidate.diff',
      'command.run',
      'test.run',
      'candidate.patch',
    ),
    sandboxPolicy: CANDIDATE_WRITE_SANDBOX,
    workspaceMode: 'candidate-write',
    acceptedInputArtifacts: frozenList(
      ...CANDIDATE_INPUTS,
      'REVIEW_REPORT',
      'VERIFICATION_REPORT',
      'REMEDIATION_REQUEST',
    ),
    requiredOutputArtifacts: frozenList('PATCH_MANIFEST', 'REMEDIATION_REPORT'),
  }),
})

/** Returns the one ordered model-visible input contract for a canonical role. */
export function strongFlowRoleAcceptedInputArtifacts(
  roleId: StrongFlowRoleId,
): readonly StrongFlowRoleArtifactKind[] {
  return ROLE_POLICIES[roleId].acceptedInputArtifacts
}

function configurationError(
  code: StrongFlowRoleConfigurationErrorCode,
  message: string,
): never {
  throw new StrongFlowRoleConfigurationError(code, message)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactKeys(
  value: Record<string, unknown>,
  expected: readonly string[],
  label: string,
  code: StrongFlowRoleConfigurationErrorCode = 'INVALID_CONFIGURATION',
): void {
  const keys = Object.keys(value)
  if (
    keys.length !== expected.length
    || expected.some(key => !Object.hasOwn(value, key))
    || keys.some(key => !expected.includes(key))
  ) configurationError(code, `${label} has an unexpected shape`)
}

function portableText(value: unknown, label: string): string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > 200
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) configurationError('INVALID_CONFIGURATION', `${label} must be portable non-empty text`)
  return value
}

function positiveInteger(value: unknown, label: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1) {
    configurationError('INVALID_CONFIGURATION', `${label} must be a positive safe integer`)
  }
  return Number(value)
}

function stringArray<Value extends string>(
  value: unknown,
  allowed: readonly Value[],
  label: string,
  unknownCode: 'UNKNOWN_TOOL' | 'UNKNOWN_ARTIFACT',
): readonly Value[] {
  if (!Array.isArray(value) || value.length === 0) {
    configurationError('INVALID_CONFIGURATION', `${label} must be a non-empty array`)
  }
  const result: Value[] = []
  for (const entry of value) {
    if (typeof entry !== 'string' || !allowed.includes(entry as Value)) {
      configurationError(unknownCode, `${label} contains an unknown value`)
    }
    if (result.includes(entry as Value)) {
      configurationError('INVALID_CONFIGURATION', `${label} contains a duplicate value`)
    }
    result.push(entry as Value)
  }
  return Object.freeze(result)
}

function sameList<Value>(left: readonly Value[], right: readonly Value[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index])
}

function requirePolicy(condition: boolean, roleId: StrongFlowRoleId, field: string): void {
  if (!condition) {
    configurationError('POLICY_MISMATCH', `role ${roleId} changes canonical ${field}`)
  }
}

function catalogIndex(
  value: unknown,
): ReadonlyMap<string, ReadonlySet<string | null>> {
  if (!Array.isArray(value) || value.length === 0) {
    return configurationError(
      'INVALID_MODEL_CATALOG',
      'role model catalog must be a non-empty array',
    )
  }
  const result = new Map<string, ReadonlySet<string | null>>()
  for (const [index, entry] of value.entries()) {
    if (!isRecord(entry)) {
      return configurationError(
        'INVALID_MODEL_CATALOG',
        `role model catalog entry ${index} must be an object`,
      )
    }
    exactKeys(
      entry,
      ['provider', 'model', 'reasoningEfforts'],
      `role model catalog entry ${index}`,
      'INVALID_MODEL_CATALOG',
    )
    let provider: string
    let model: string
    try {
      provider = portableText(entry.provider, `role model catalog entry ${index}.provider`)
      model = portableText(entry.model, `role model catalog entry ${index}.model`)
    } catch (error) {
      if (error instanceof StrongFlowRoleConfigurationError) {
        return configurationError('INVALID_MODEL_CATALOG', error.message)
      }
      throw error
    }
    if (!Array.isArray(entry.reasoningEfforts) || entry.reasoningEfforts.length === 0) {
      return configurationError(
        'INVALID_MODEL_CATALOG',
        `role model catalog entry ${index}.reasoningEfforts must be non-empty`,
      )
    }
    const efforts = new Set<string | null>()
    for (const effort of entry.reasoningEfforts) {
      if (effort !== null) {
        try {
          portableText(effort, `role model catalog entry ${index}.reasoningEfforts`)
        } catch (error) {
          if (error instanceof StrongFlowRoleConfigurationError) {
            return configurationError('INVALID_MODEL_CATALOG', error.message)
          }
          throw error
        }
      }
      if (efforts.has(effort)) {
        return configurationError(
          'INVALID_MODEL_CATALOG',
          `role model catalog entry ${index} repeats a reasoning effort`,
        )
      }
      efforts.add(effort)
    }
    const key = `${provider}\u0000${model}`
    if (result.has(key)) {
      return configurationError(
        'INVALID_MODEL_CATALOG',
        `role model catalog repeats ${provider}/${model}`,
      )
    }
    result.set(key, Object.freeze(efforts))
  }
  return result
}

function parseRole(
  value: unknown,
  modelCatalog: ReadonlyMap<string, ReadonlySet<string | null>>,
): StrongFlowRoleSpec {
  if (!isRecord(value)) {
    return configurationError('INVALID_CONFIGURATION', 'role specification must be an object')
  }
  exactKeys(value, [
    'id',
    'displayName',
    'modelRoute',
    'reasoningEffort',
    'budget',
    'systemInstructions',
    'allowedTools',
    'sandboxPolicy',
    'workspaceMode',
    'acceptedInputArtifacts',
    'requiredOutputArtifacts',
  ], 'role specification')
  if (
    typeof value.id !== 'string'
    || !STRONGFLOW_ROLE_IDS.includes(value.id as StrongFlowRoleId)
  ) return configurationError('UNKNOWN_ROLE', 'StrongFlow role id is unknown')
  const id = value.id as StrongFlowRoleId
  const policy = ROLE_POLICIES[id]

  if (!isRecord(value.modelRoute)) {
    return configurationError('INVALID_CONFIGURATION', `role ${id} modelRoute must be an object`)
  }
  exactKeys(value.modelRoute, ['provider', 'model'], `role ${id} modelRoute`)
  const provider = portableText(value.modelRoute.provider, `role ${id} provider`)
  const model = portableText(value.modelRoute.model, `role ${id} model`)
  const efforts = modelCatalog.get(`${provider}\u0000${model}`)
  if (efforts === undefined) {
    return configurationError(
      'UNKNOWN_MODEL_ROUTE',
      `role ${id} selects unknown DSH model route ${provider}/${model}`,
    )
  }
  const reasoningEffort = value.reasoningEffort
  if (
    reasoningEffort !== null
    && (
      typeof reasoningEffort !== 'string'
      || reasoningEffort.length === 0
      || reasoningEffort.trim() !== reasoningEffort
    )
  ) {
    return configurationError(
      'INVALID_CONFIGURATION',
      `role ${id} reasoningEffort must be null or non-empty text`,
    )
  }
  if (!efforts.has(reasoningEffort as string | null)) {
    return configurationError(
      'UNKNOWN_REASONING_EFFORT',
      `role ${id} selects an unsupported reasoning effort`,
    )
  }

  if (!isRecord(value.budget)) {
    return configurationError('INVALID_CONFIGURATION', `role ${id} budget must be an object`)
  }
  exactKeys(
    value.budget,
    ['maxTurns', 'maxWallTimeMillis', 'maxTotalTokens', 'maxCostUsdMicros'],
    `role ${id} budget`,
  )
  const budget = Object.freeze({
    maxTurns: positiveInteger(value.budget.maxTurns, `role ${id} budget.maxTurns`),
    maxWallTimeMillis: positiveInteger(
      value.budget.maxWallTimeMillis,
      `role ${id} budget.maxWallTimeMillis`,
    ),
    maxTotalTokens: positiveInteger(
      value.budget.maxTotalTokens,
      `role ${id} budget.maxTotalTokens`,
    ),
    maxCostUsdMicros: positiveInteger(
      value.budget.maxCostUsdMicros,
      `role ${id} budget.maxCostUsdMicros`,
    ),
  })

  const displayName = portableText(value.displayName, `role ${id} displayName`)
  if (typeof value.systemInstructions !== 'string') {
    return configurationError(
      'INVALID_CONFIGURATION',
      `role ${id} systemInstructions must be a string`,
    )
  }
  const tools = stringArray(
    value.allowedTools,
    STRONGFLOW_ROLE_TOOLS,
    `role ${id} allowedTools`,
    'UNKNOWN_TOOL',
  )
  const acceptedInputs = stringArray(
    value.acceptedInputArtifacts,
    STRONGFLOW_ROLE_ARTIFACT_KINDS,
    `role ${id} acceptedInputArtifacts`,
    'UNKNOWN_ARTIFACT',
  )
  const requiredOutputs = stringArray(
    value.requiredOutputArtifacts,
    STRONGFLOW_ROLE_ARTIFACT_KINDS,
    `role ${id} requiredOutputArtifacts`,
    'UNKNOWN_ARTIFACT',
  )

  if (!isRecord(value.sandboxPolicy)) {
    return configurationError(
      'INVALID_CONFIGURATION',
      `role ${id} sandboxPolicy must be an object`,
    )
  }
  exactKeys(
    value.sandboxPolicy,
    ['filesystem', 'network', 'approval'],
    `role ${id} sandboxPolicy`,
  )
  if (
    value.sandboxPolicy.filesystem !== 'read-only'
    && value.sandboxPolicy.filesystem !== 'candidate-write'
  ) {
    return configurationError(
      'INVALID_CONFIGURATION',
      `role ${id} sandbox filesystem mode is unknown`,
    )
  }
  if (value.sandboxPolicy.network !== 'disabled') {
    return configurationError(
      'POLICY_MISMATCH',
      `role ${id} cannot enable sandbox network access`,
    )
  }
  if (value.sandboxPolicy.approval !== 'never') {
    return configurationError(
      'POLICY_MISMATCH',
      `role ${id} cannot receive approval authority`,
    )
  }
  if (
    value.workspaceMode !== 'source-read-only'
    && value.workspaceMode !== 'candidate-read-only'
    && value.workspaceMode !== 'candidate-write'
  ) {
    return configurationError(
      'INVALID_CONFIGURATION',
      `role ${id} workspaceMode is unknown`,
    )
  }

  requirePolicy(displayName === policy.displayName, id, 'displayName')
  requirePolicy(value.systemInstructions === policy.systemInstructions, id, 'systemInstructions')
  requirePolicy(sameList(tools, policy.allowedTools), id, 'allowedTools')
  requirePolicy(
    value.sandboxPolicy.filesystem === policy.sandboxPolicy.filesystem
    && value.sandboxPolicy.network === policy.sandboxPolicy.network
    && value.sandboxPolicy.approval === policy.sandboxPolicy.approval,
    id,
    'sandboxPolicy',
  )
  requirePolicy(value.workspaceMode === policy.workspaceMode, id, 'workspaceMode')
  requirePolicy(
    sameList(acceptedInputs, policy.acceptedInputArtifacts),
    id,
    'acceptedInputArtifacts',
  )
  requirePolicy(
    sameList(requiredOutputs, policy.requiredOutputArtifacts),
    id,
    'requiredOutputArtifacts',
  )

  return Object.freeze({
    id,
    displayName,
    modelRoute: Object.freeze({ provider, model }),
    reasoningEffort: reasoningEffort as string | null,
    budget,
    systemInstructions: value.systemInstructions,
    allowedTools: tools,
    sandboxPolicy: Object.freeze({
      filesystem: value.sandboxPolicy.filesystem as 'read-only' | 'candidate-write',
      network: 'disabled',
      approval: 'never',
    }),
    workspaceMode: value.workspaceMode as StrongFlowRoleWorkspaceMode,
    acceptedInputArtifacts: acceptedInputs,
    requiredOutputArtifacts: requiredOutputs,
  })
}

export function parseStrongFlowRoleConfiguration(
  value: unknown,
  modelCatalogInput: readonly StrongFlowRoleModelCatalogEntry[],
): StrongFlowRoleConfiguration {
  const modelCatalog = catalogIndex(modelCatalogInput)
  if (!isRecord(value)) {
    return configurationError(
      'INVALID_CONFIGURATION',
      'StrongFlow role configuration must be an object',
    )
  }
  exactKeys(value, ['schemaVersion', 'roles'], 'StrongFlow role configuration')
  if (value.schemaVersion !== STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION) {
    return configurationError(
      'INVALID_CONFIGURATION',
      'StrongFlow role configuration schemaVersion is unsupported',
    )
  }
  if (!Array.isArray(value.roles)) {
    return configurationError(
      'INVALID_CONFIGURATION',
      'StrongFlow role configuration roles must be an array',
    )
  }
  const roles = new Map<StrongFlowRoleId, StrongFlowRoleSpec>()
  for (const roleInput of value.roles) {
    const role = parseRole(roleInput, modelCatalog)
    if (roles.has(role.id)) {
      return configurationError('DUPLICATE_ROLE', `role ${role.id} appears more than once`)
    }
    roles.set(role.id, role)
  }
  if (roles.size !== STRONGFLOW_ROLE_IDS.length) {
    return configurationError(
      'INVALID_CONFIGURATION',
      'StrongFlow role configuration must define all eight canonical roles',
    )
  }
  const ordered = STRONGFLOW_ROLE_IDS.map(roleId => {
    const role = roles.get(roleId)
    if (role === undefined) {
      return configurationError('INVALID_CONFIGURATION', `role ${roleId} is missing`)
    }
    return role
  })
  return Object.freeze({
    schemaVersion: STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION,
    roles: Object.freeze(ordered),
  })
}

export function createStrongFlowRoleConfiguration(
  assignmentsInput: StrongFlowRoleRuntimeAssignments,
  modelCatalog: readonly StrongFlowRoleModelCatalogEntry[],
): StrongFlowRoleConfiguration {
  if (!isRecord(assignmentsInput)) {
    return configurationError(
      'INVALID_CONFIGURATION',
      'StrongFlow role runtime assignments must be an object',
    )
  }
  exactKeys(
    assignmentsInput,
    STRONGFLOW_ROLE_IDS,
    'StrongFlow role runtime assignments',
  )
  const roles = STRONGFLOW_ROLE_IDS.map(id => {
    const assignment = assignmentsInput[id]
    if (!isRecord(assignment)) {
      return configurationError(
        'INVALID_CONFIGURATION',
        `role ${id} runtime assignment must be an object`,
      )
    }
    exactKeys(
      assignment,
      ['modelRoute', 'reasoningEffort', 'budget'],
      `role ${id} runtime assignment`,
    )
    const policy = ROLE_POLICIES[id]
    return {
      id,
      displayName: policy.displayName,
      modelRoute: assignment.modelRoute,
      reasoningEffort: assignment.reasoningEffort,
      budget: assignment.budget,
      systemInstructions: policy.systemInstructions,
      allowedTools: policy.allowedTools,
      sandboxPolicy: policy.sandboxPolicy,
      workspaceMode: policy.workspaceMode,
      acceptedInputArtifacts: policy.acceptedInputArtifacts,
      requiredOutputArtifacts: policy.requiredOutputArtifacts,
    }
  })
  return parseStrongFlowRoleConfiguration({
    schemaVersion: STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION,
    roles,
  }, modelCatalog)
}
