/** Canonical StrongFlow roles and the minimal policy applied by Codex Core. */

export const STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION = 3 as const
export const STRONGFLOW_ROLE_SESSION_POLICY_SCHEMA_VERSION = 1 as const

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

export const STRONGFLOW_VERIFICATION_ROLE_IDS = Object.freeze([
  'reviewer',
  'verifier',
  'adversarial-verifier',
] as const)

export type StrongFlowVerificationRoleId = typeof STRONGFLOW_VERIFICATION_ROLE_IDS[number]

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

export interface StrongFlowRoleSpec {
  readonly id: StrongFlowRoleId
  readonly displayName: string
  readonly modelRoute: StrongFlowRoleModelRoute
  readonly reasoningEffort: string | null
  readonly budget: StrongFlowRoleBudget
  readonly developerInstructions: string
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
}

export interface StrongFlowRoleConfiguration {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION
  readonly roles: readonly StrongFlowRoleSpec[]
}

/** Minimal host input that configures Codex Core without replacing its runtime or tools. */
export interface StrongFlowRoleSessionPolicy {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_SESSION_POLICY_SCHEMA_VERSION
  readonly roleId: StrongFlowRoleId
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
  readonly developerInstructions: string
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
  readonly developerInstructions: string
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
}

const ROLE_POLICIES: Readonly<Record<StrongFlowRoleId, CanonicalRolePolicy>> = Object.freeze({
  requirements: Object.freeze({
    displayName: 'Requirements Analyst',
    developerInstructions: 'Turn the user request and verified repository facts into a proposed DeliverySpec with explicit scope, constraints, acceptance criteria, risks, and unresolved questions. Keep requirements separate from solution choices. Do not approve the proposal or start implementation.',
    workspaceMode: 'source-read-only',
  }),
  solution: Object.freeze({
    displayName: 'Solution Architect',
    developerInstructions: 'Prepare a solution proposal for the exact approved DeliverySpec. Include structured system architecture and process-flow diagram data with stable node identities, components, connections, trust boundaries, external systems, and unresolved facts. Do not approve the proposal or modify the candidate.',
    workspaceMode: 'source-read-only',
  }),
  planner: Object.freeze({
    displayName: 'Planner',
    developerInstructions: 'Plan the approved delivery with Codex plan and multi-agent capabilities. Keep the work bounded by the approved DeliverySpec and solution, make verification explicit, and do not create a second task graph, modify candidate files, or declare delivery complete.',
    workspaceMode: 'source-read-only',
  }),
  executor: Object.freeze({
    displayName: 'Executor',
    developerInstructions: 'Implement only the approved delivery plan in the assigned candidate workspace. Use Codex tools, sandbox, approvals, plan, and subagents as needed. Preserve exact changed-file, command, test, diff, failure, recovery, and usage events. Do not approve or verify your own work.',
    workspaceMode: 'candidate-write',
  }),
  reviewer: Object.freeze({
    displayName: 'Reviewer',
    developerInstructions: 'Independently review the exact frozen candidate against the approved DeliverySpec and plan from a read-only workspace. Cite only observed Codex event evidence. The final response must follow the supplied winwincode.independent-verification-result.v1 JSON protocol. Do not modify the candidate or decide final delivery.',
    workspaceMode: 'candidate-read-only',
  }),
  verifier: Object.freeze({
    displayName: 'Verifier',
    developerInstructions: 'Independently verify every assigned acceptance criterion against the exact frozen candidate from a read-only workspace. Run checks through Codex Core, cite only observed Codex event evidence, and return the supplied winwincode.independent-verification-result.v1 JSON protocol. Do not modify the candidate or decide final delivery.',
    workspaceMode: 'candidate-read-only',
  }),
  'adversarial-verifier': Object.freeze({
    displayName: 'Adversarial Verifier',
    developerInstructions: 'Challenge the exact frozen candidate, approved assumptions, trust boundaries, failure handling, and negative cases from a read-only workspace. Cite reproducible Codex event evidence and return the supplied winwincode.independent-verification-result.v1 JSON protocol. Do not modify the candidate or decide final delivery.',
    workspaceMode: 'candidate-read-only',
  }),
  remediator: Object.freeze({
    displayName: 'Remediator',
    developerInstructions: 'Apply only the bounded rework requested from reviewed findings in the assigned candidate workspace. Use Codex tools, sandbox, approvals, plan, and subagents as needed, preserve unrelated accepted work, and produce fresh runtime evidence. Do not broaden scope, approve, or verify your own work.',
    workspaceMode: 'candidate-write',
  }),
})

/** Return the canonical workspace intent for one StrongFlow role. */
export function strongFlowRoleWorkspaceMode(
  roleId: StrongFlowRoleId,
): StrongFlowRoleWorkspaceMode {
  return ROLE_POLICIES[roleId].workspaceMode
}

/** Return the minimal immutable policy passed to Codex Core for one role-scoped session. */
export function strongFlowRoleSessionPolicy(
  roleId: StrongFlowRoleId,
): StrongFlowRoleSessionPolicy {
  const policy = ROLE_POLICIES[roleId]
  return Object.freeze({
    schemaVersion: STRONGFLOW_ROLE_SESSION_POLICY_SCHEMA_VERSION,
    roleId,
    workspaceMode: policy.workspaceMode,
    developerInstructions: policy.developerInstructions,
  })
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
    'developerInstructions',
    'workspaceMode',
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
  if (typeof value.developerInstructions !== 'string') {
    return configurationError(
      'INVALID_CONFIGURATION',
      `role ${id} developerInstructions must be a string`,
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
  requirePolicy(
    value.developerInstructions === policy.developerInstructions,
    id,
    'developerInstructions',
  )
  requirePolicy(value.workspaceMode === policy.workspaceMode, id, 'workspaceMode')

  return Object.freeze({
    id,
    displayName,
    modelRoute: Object.freeze({ provider, model }),
    reasoningEffort: reasoningEffort as string | null,
    budget,
    developerInstructions: value.developerInstructions,
    workspaceMode: value.workspaceMode as StrongFlowRoleWorkspaceMode,
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
      developerInstructions: policy.developerInstructions,
      workspaceMode: policy.workspaceMode,
    }
  })
  return parseStrongFlowRoleConfiguration({
    schemaVersion: STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION,
    roles,
  }, modelCatalog)
}
