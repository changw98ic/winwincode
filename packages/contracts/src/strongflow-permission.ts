/** Canonical least-authority presets for every StrongFlow actor. */

export const STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_PERMISSION_PRESET_IDS = Object.freeze([
  'definition-read',
  'solution-read',
  'source-read',
  'candidate-write',
  'snapshot-verify',
  'remediation-write',
  'human-definition-review',
  'deterministic-finalizer',
] as const)

export type StrongFlowPermissionPresetId = typeof STRONGFLOW_PERMISSION_PRESET_IDS[number]

export const STRONGFLOW_PERMISSION_SUBJECT_KINDS = Object.freeze([
  'model-role',
  'human-reviewer',
  'deterministic-finalizer',
] as const)

export type StrongFlowPermissionSubjectKind =
  typeof STRONGFLOW_PERMISSION_SUBJECT_KINDS[number]

export const STRONGFLOW_PRIVILEGED_OPERATION_KINDS = Object.freeze([
  'network-elevation',
  'credential-reference',
  'permission-expansion',
  'budget-increase',
  'remote-publication',
] as const)

export type StrongFlowPrivilegedOperationKind =
  typeof STRONGFLOW_PRIVILEGED_OPERATION_KINDS[number]

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

export type StrongFlowPermissionFilesystemMode = 'none' | 'read-only' | 'candidate-write'

export interface StrongFlowFilesystemPermission {
  readonly mode: StrongFlowPermissionFilesystemMode
  readonly rootScope: 'none' | 'assigned-workspace'
  readonly temporaryWrites: 'none' | 'isolated-only'
  readonly symlinkEscape: 'deny'
}

export interface StrongFlowToolPermission {
  readonly allowed: readonly StrongFlowRoleTool[]
  readonly approvalTool: 'absent'
}

export type StrongFlowProcessPermissionMode =
  | 'disabled'
  | 'approved-plan-commands'
  | 'approved-snapshot-probes'
  | 'approved-remediation-commands'

export interface StrongFlowProcessPermission {
  readonly mode: StrongFlowProcessPermissionMode
  readonly childProcesses: 'disabled' | 'sandboxed-only'
  readonly environment: 'empty' | 'explicit-allowlist'
  readonly shellStartup: 'disabled'
}

export interface StrongFlowNetworkPermission {
  readonly access: 'disabled'
  readonly requestElevation: 'forbidden' | 'human-operation-approval'
}

export interface StrongFlowApprovalPermission {
  readonly definitionDecision: 'forbidden' | 'human-reviewer-only'
  readonly operationRequests: readonly StrongFlowPrivilegedOperationKind[]
  readonly operationDecisions: readonly StrongFlowPrivilegedOperationKind[]
  readonly selfApproval: 'forbidden'
}

export interface StrongFlowBudgetPermission {
  readonly consume: 'none' | 'assigned-role-budget'
  readonly requestIncrease: 'forbidden' | 'human-operation-approval'
  readonly decideIncrease: 'forbidden' | 'human-reviewer-only'
}

export type StrongFlowLocalPublicationScope =
  | 'requirement-spec-only'
  | 'solution-definition-only'
  | 'execution-plan-only'
  | 'patch-manifest-only'
  | 'review-verification-only'
  | 'remediation-output-only'
  | 'human-review-record-only'
  | 'delivery-receipt-only'

export interface StrongFlowPublicationPermission {
  readonly local: StrongFlowLocalPublicationScope
  readonly remoteExecute: 'forbidden' | 'human-approved-finalizer-only'
  readonly requestRemote: 'forbidden' | 'human-operation-approval'
  readonly decideRemote: 'forbidden' | 'human-reviewer-only'
}

export interface StrongFlowCredentialPermission {
  readonly use: 'none' | 'dsh-selected-model-reference'
  readonly rawValues: 'forbidden'
  readonly environment: 'excluded'
  readonly requestAdditional: 'forbidden' | 'human-operation-approval'
  readonly decideAdditional: 'forbidden' | 'human-reviewer-only'
}

export interface StrongFlowAuditPermission {
  readonly required: true
  readonly sourceIdentity: 'job-role-operation'
  readonly credentialRedaction: 'required'
}

/** Complete authority contract. Every field is required and missing fields never grant access. */
export interface StrongFlowPermissionPolicy {
  readonly schemaVersion: typeof STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION
  readonly presetId: StrongFlowPermissionPresetId
  readonly subject: StrongFlowPermissionSubjectKind
  readonly filesystem: StrongFlowFilesystemPermission
  readonly tools: StrongFlowToolPermission
  readonly process: StrongFlowProcessPermission
  readonly network: StrongFlowNetworkPermission
  readonly approval: StrongFlowApprovalPermission
  readonly budget: StrongFlowBudgetPermission
  readonly publication: StrongFlowPublicationPermission
  readonly credentials: StrongFlowCredentialPermission
  readonly audit: StrongFlowAuditPermission
}

export const STRONGFLOW_PERMISSION_SUPPORTED_HOSTS = Object.freeze([
  'darwin/arm64',
  'darwin/x64',
  'linux/arm64',
  'linux/x64',
] as const)

/** Workspace paths that model roles and their child processes must never read or write. */
export const STRONGFLOW_CREDENTIAL_SENSITIVE_WORKSPACE_PATTERNS = Object.freeze([
  '**/.env',
  '**/.env.*',
  '**/.credentials.yaml',
  '**/.netrc',
  '**/.npmrc',
  '**/.pypirc',
  '**/*.pem',
  '**/*.key',
  '**/*.p12',
  '**/*.pfx',
  '**/id_rsa',
  '**/id_ed25519',
  '**/.docker/config.json',
] as const)

export type StrongFlowPermissionSupportedHost =
  typeof STRONGFLOW_PERMISSION_SUPPORTED_HOSTS[number]
export type StrongFlowPermissionHostPlatform = 'darwin' | 'linux'
export type StrongFlowPermissionHostArchitecture = 'arm64' | 'x64'

export interface StrongFlowPermissionEnforcementProfile {
  readonly schemaVersion: typeof STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION
  readonly platform: StrongFlowPermissionHostPlatform
  readonly architecture: StrongFlowPermissionHostArchitecture
  readonly filesystem: 'codex-restricted'
  readonly process: 'codex-sandboxed'
  readonly network: 'codex-restricted'
  readonly environment: 'explicit-allowlist'
  readonly approvals: 'source-identified-human'
  readonly credentials: 'dsh-reference-only'
  readonly publication: 'exact-identity-guard'
  readonly audit: 'durable-redacted'
}

export interface StrongFlowResolvedPermissionPolicy {
  readonly policy: StrongFlowPermissionPolicy
  readonly enforcement: StrongFlowPermissionEnforcementProfile
}

export type StrongFlowPermissionPolicyErrorCode =
  | 'INVALID_POLICY'
  | 'UNKNOWN_PRESET'
  | 'POLICY_MISMATCH'
  | 'UNSUPPORTED_PLATFORM'
  | 'ENFORCEMENT_UNAVAILABLE'

export class StrongFlowPermissionPolicyError extends Error {
  readonly code: StrongFlowPermissionPolicyErrorCode

  constructor(code: StrongFlowPermissionPolicyErrorCode, message: string) {
    super(message)
    this.name = 'StrongFlowPermissionPolicyError'
    this.code = code
  }
}

function frozenList<Value extends string>(...values: readonly Value[]): readonly Value[] {
  return Object.freeze([...values])
}

const MODEL_OPERATION_REQUESTS = frozenList<StrongFlowPrivilegedOperationKind>(
  ...STRONGFLOW_PRIVILEGED_OPERATION_KINDS,
)

const DEFINITION_TOOLS = frozenList<StrongFlowRoleTool>(
  'artifact.read',
  'artifact.write',
  'workspace.read',
  'code.search',
)

const CANDIDATE_WRITE_TOOLS = frozenList<StrongFlowRoleTool>(
  ...DEFINITION_TOOLS,
  'candidate.diff',
  'command.run',
  'test.run',
  'candidate.patch',
)

const SNAPSHOT_TOOLS = frozenList<StrongFlowRoleTool>(
  ...DEFINITION_TOOLS,
  'candidate.diff',
  'command.run',
  'test.run',
)

function frozenPolicy(policy: StrongFlowPermissionPolicy): StrongFlowPermissionPolicy {
  return Object.freeze({
    ...policy,
    filesystem: Object.freeze({ ...policy.filesystem }),
    tools: Object.freeze({
      ...policy.tools,
      allowed: Object.freeze([...policy.tools.allowed]),
    }),
    process: Object.freeze({ ...policy.process }),
    network: Object.freeze({ ...policy.network }),
    approval: Object.freeze({
      ...policy.approval,
      operationRequests: Object.freeze([...policy.approval.operationRequests]),
      operationDecisions: Object.freeze([...policy.approval.operationDecisions]),
    }),
    budget: Object.freeze({ ...policy.budget }),
    publication: Object.freeze({ ...policy.publication }),
    credentials: Object.freeze({ ...policy.credentials }),
    audit: Object.freeze({ ...policy.audit }),
  })
}

function modelPolicy(
  presetId: Extract<
    StrongFlowPermissionPresetId,
    | 'definition-read'
    | 'solution-read'
    | 'source-read'
    | 'candidate-write'
    | 'snapshot-verify'
    | 'remediation-write'
  >,
  input: {
    readonly filesystem: 'read-only' | 'candidate-write'
    readonly tools: readonly StrongFlowRoleTool[]
    readonly process: StrongFlowProcessPermissionMode
    readonly publication: StrongFlowLocalPublicationScope
  },
): StrongFlowPermissionPolicy {
  const processEnabled = input.process !== 'disabled'
  return frozenPolicy({
    schemaVersion: STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION,
    presetId,
    subject: 'model-role',
    filesystem: {
      mode: input.filesystem,
      rootScope: 'assigned-workspace',
      temporaryWrites: 'isolated-only',
      symlinkEscape: 'deny',
    },
    tools: {
      allowed: input.tools,
      approvalTool: 'absent',
    },
    process: {
      mode: input.process,
      childProcesses: processEnabled ? 'sandboxed-only' : 'disabled',
      environment: processEnabled ? 'explicit-allowlist' : 'empty',
      shellStartup: 'disabled',
    },
    network: {
      access: 'disabled',
      requestElevation: 'human-operation-approval',
    },
    approval: {
      definitionDecision: 'forbidden',
      operationRequests: MODEL_OPERATION_REQUESTS,
      operationDecisions: frozenList(),
      selfApproval: 'forbidden',
    },
    budget: {
      consume: 'assigned-role-budget',
      requestIncrease: 'human-operation-approval',
      decideIncrease: 'forbidden',
    },
    publication: {
      local: input.publication,
      remoteExecute: 'forbidden',
      requestRemote: 'human-operation-approval',
      decideRemote: 'forbidden',
    },
    credentials: {
      use: 'dsh-selected-model-reference',
      rawValues: 'forbidden',
      environment: 'excluded',
      requestAdditional: 'human-operation-approval',
      decideAdditional: 'forbidden',
    },
    audit: {
      required: true,
      sourceIdentity: 'job-role-operation',
      credentialRedaction: 'required',
    },
  })
}

const PERMISSION_POLICIES: Readonly<
  Record<StrongFlowPermissionPresetId, StrongFlowPermissionPolicy>
> = Object.freeze({
  'definition-read': modelPolicy('definition-read', {
    filesystem: 'read-only',
    tools: DEFINITION_TOOLS,
    process: 'disabled',
    publication: 'requirement-spec-only',
  }),
  'solution-read': modelPolicy('solution-read', {
    filesystem: 'read-only',
    tools: DEFINITION_TOOLS,
    process: 'disabled',
    publication: 'solution-definition-only',
  }),
  'source-read': modelPolicy('source-read', {
    filesystem: 'read-only',
    tools: DEFINITION_TOOLS,
    process: 'disabled',
    publication: 'execution-plan-only',
  }),
  'candidate-write': modelPolicy('candidate-write', {
    filesystem: 'candidate-write',
    tools: CANDIDATE_WRITE_TOOLS,
    process: 'approved-plan-commands',
    publication: 'patch-manifest-only',
  }),
  'snapshot-verify': modelPolicy('snapshot-verify', {
    filesystem: 'read-only',
    tools: SNAPSHOT_TOOLS,
    process: 'approved-snapshot-probes',
    publication: 'review-verification-only',
  }),
  'remediation-write': modelPolicy('remediation-write', {
    filesystem: 'candidate-write',
    tools: CANDIDATE_WRITE_TOOLS,
    process: 'approved-remediation-commands',
    publication: 'remediation-output-only',
  }),
  'human-definition-review': frozenPolicy({
    schemaVersion: STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION,
    presetId: 'human-definition-review',
    subject: 'human-reviewer',
    filesystem: {
      mode: 'none',
      rootScope: 'none',
      temporaryWrites: 'none',
      symlinkEscape: 'deny',
    },
    tools: {
      allowed: frozenList(),
      approvalTool: 'absent',
    },
    process: {
      mode: 'disabled',
      childProcesses: 'disabled',
      environment: 'empty',
      shellStartup: 'disabled',
    },
    network: {
      access: 'disabled',
      requestElevation: 'forbidden',
    },
    approval: {
      definitionDecision: 'human-reviewer-only',
      operationRequests: frozenList(),
      operationDecisions: MODEL_OPERATION_REQUESTS,
      selfApproval: 'forbidden',
    },
    budget: {
      consume: 'none',
      requestIncrease: 'forbidden',
      decideIncrease: 'human-reviewer-only',
    },
    publication: {
      local: 'human-review-record-only',
      remoteExecute: 'forbidden',
      requestRemote: 'forbidden',
      decideRemote: 'human-reviewer-only',
    },
    credentials: {
      use: 'none',
      rawValues: 'forbidden',
      environment: 'excluded',
      requestAdditional: 'forbidden',
      decideAdditional: 'human-reviewer-only',
    },
    audit: {
      required: true,
      sourceIdentity: 'job-role-operation',
      credentialRedaction: 'required',
    },
  }),
  'deterministic-finalizer': frozenPolicy({
    schemaVersion: STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION,
    presetId: 'deterministic-finalizer',
    subject: 'deterministic-finalizer',
    filesystem: {
      mode: 'none',
      rootScope: 'none',
      temporaryWrites: 'none',
      symlinkEscape: 'deny',
    },
    tools: {
      allowed: frozenList(),
      approvalTool: 'absent',
    },
    process: {
      mode: 'disabled',
      childProcesses: 'disabled',
      environment: 'empty',
      shellStartup: 'disabled',
    },
    network: {
      access: 'disabled',
      requestElevation: 'forbidden',
    },
    approval: {
      definitionDecision: 'forbidden',
      operationRequests: frozenList(),
      operationDecisions: frozenList(),
      selfApproval: 'forbidden',
    },
    budget: {
      consume: 'none',
      requestIncrease: 'forbidden',
      decideIncrease: 'forbidden',
    },
    publication: {
      local: 'delivery-receipt-only',
      remoteExecute: 'human-approved-finalizer-only',
      requestRemote: 'forbidden',
      decideRemote: 'forbidden',
    },
    credentials: {
      use: 'none',
      rawValues: 'forbidden',
      environment: 'excluded',
      requestAdditional: 'forbidden',
      decideAdditional: 'forbidden',
    },
    audit: {
      required: true,
      sourceIdentity: 'job-role-operation',
      credentialRedaction: 'required',
    },
  }),
})

function policyError(
  code: StrongFlowPermissionPolicyErrorCode,
  message: string,
): never {
  throw new StrongFlowPermissionPolicyError(code, message)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[], label: string): void {
  const actual = Object.keys(value)
  if (
    actual.length !== keys.length
    || keys.some(key => !Object.hasOwn(value, key))
    || actual.some(key => !keys.includes(key))
  ) policyError('INVALID_POLICY', `${label} has an unexpected shape`)
}

function policyObject(value: unknown, keys: readonly string[], label: string): void {
  if (!isRecord(value)) policyError('INVALID_POLICY', `${label} must be an object`)
  exactKeys(value, keys, label)
}

function jsonEqual(left: unknown, right: unknown): boolean {
  if (left === right) return true
  if (Array.isArray(left) || Array.isArray(right)) {
    return Array.isArray(left)
      && Array.isArray(right)
      && left.length === right.length
      && left.every((entry, index) => jsonEqual(entry, right[index]))
  }
  if (!isRecord(left) || !isRecord(right)) return false
  const leftKeys = Object.keys(left).sort()
  const rightKeys = Object.keys(right).sort()
  return leftKeys.length === rightKeys.length
    && leftKeys.every((key, index) => (
      key === rightKeys[index]
      && jsonEqual(left[key], right[key])
    ))
}

function assertPolicyShape(value: Record<string, unknown>): void {
  exactKeys(value, [
    'schemaVersion',
    'presetId',
    'subject',
    'filesystem',
    'tools',
    'process',
    'network',
    'approval',
    'budget',
    'publication',
    'credentials',
    'audit',
  ], 'StrongFlow permission policy')
  policyObject(
    value.filesystem,
    ['mode', 'rootScope', 'temporaryWrites', 'symlinkEscape'],
    'StrongFlow filesystem permission',
  )
  policyObject(value.tools, ['allowed', 'approvalTool'], 'StrongFlow tool permission')
  if (!Array.isArray((value.tools as Record<string, unknown>).allowed)) {
    policyError('INVALID_POLICY', 'StrongFlow allowed tools must be an array')
  }
  policyObject(
    value.process,
    ['mode', 'childProcesses', 'environment', 'shellStartup'],
    'StrongFlow process permission',
  )
  policyObject(value.network, ['access', 'requestElevation'], 'StrongFlow network permission')
  policyObject(
    value.approval,
    ['definitionDecision', 'operationRequests', 'operationDecisions', 'selfApproval'],
    'StrongFlow approval permission',
  )
  const approval = value.approval as Record<string, unknown>
  if (!Array.isArray(approval.operationRequests) || !Array.isArray(approval.operationDecisions)) {
    policyError('INVALID_POLICY', 'StrongFlow approval operations must be arrays')
  }
  policyObject(
    value.budget,
    ['consume', 'requestIncrease', 'decideIncrease'],
    'StrongFlow budget permission',
  )
  policyObject(
    value.publication,
    ['local', 'remoteExecute', 'requestRemote', 'decideRemote'],
    'StrongFlow publication permission',
  )
  policyObject(
    value.credentials,
    ['use', 'rawValues', 'environment', 'requestAdditional', 'decideAdditional'],
    'StrongFlow credential permission',
  )
  policyObject(
    value.audit,
    ['required', 'sourceIdentity', 'credentialRedaction'],
    'StrongFlow audit permission',
  )
}

/** Return one immutable canonical preset by identity. */
export function strongFlowPermissionPolicyForPreset(
  presetId: StrongFlowPermissionPresetId,
): StrongFlowPermissionPolicy {
  if (!STRONGFLOW_PERMISSION_PRESET_IDS.includes(presetId)) {
    return policyError('UNKNOWN_PRESET', 'StrongFlow permission preset is unknown')
  }
  return PERMISSION_POLICIES[presetId]
}

/** Validate a serialized policy and replace it with the canonical immutable preset. */
export function parseStrongFlowPermissionPolicy(value: unknown): StrongFlowPermissionPolicy {
  if (!isRecord(value)) {
    return policyError('INVALID_POLICY', 'StrongFlow permission policy must be an object')
  }
  assertPolicyShape(value)
  if (value.schemaVersion !== STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION) {
    return policyError('INVALID_POLICY', 'StrongFlow permission policy version is unsupported')
  }
  if (
    typeof value.presetId !== 'string'
    || !STRONGFLOW_PERMISSION_PRESET_IDS.includes(
      value.presetId as StrongFlowPermissionPresetId,
    )
  ) return policyError('UNKNOWN_PRESET', 'StrongFlow permission preset is unknown')
  const canonical = PERMISSION_POLICIES[value.presetId as StrongFlowPermissionPresetId]
  if (!jsonEqual(value, canonical)) {
    return policyError(
      'POLICY_MISMATCH',
      `StrongFlow permission preset ${value.presetId} was changed`,
    )
  }
  return canonical
}

/** Return the human reviewer's definition and operation-decision authority. */
export function strongFlowHumanReviewerPermissionPolicy(): StrongFlowPermissionPolicy {
  return PERMISSION_POLICIES['human-definition-review']
}

/** Return the non-model finalizer's exact local and remote publication authority. */
export function strongFlowDeterministicFinalizerPermissionPolicy(): StrongFlowPermissionPolicy {
  return PERMISSION_POLICIES['deterministic-finalizer']
}

function parseEnforcementProfile(value: unknown): StrongFlowPermissionEnforcementProfile {
  if (!isRecord(value)) {
    return policyError(
      'ENFORCEMENT_UNAVAILABLE',
      'StrongFlow permission enforcement profile must be an object',
    )
  }
  const keys = [
    'schemaVersion',
    'platform',
    'architecture',
    'filesystem',
    'process',
    'network',
    'environment',
    'approvals',
    'credentials',
    'publication',
    'audit',
  ] as const
  const actual = Object.keys(value)
  if (
    actual.length !== keys.length
    || keys.some(key => !Object.hasOwn(value, key))
    || actual.some(key => !keys.includes(key as typeof keys[number]))
  ) {
    return policyError(
      'ENFORCEMENT_UNAVAILABLE',
      'StrongFlow permission enforcement profile is incomplete',
    )
  }
  const host = `${String(value.platform)}/${String(value.architecture)}`
  if (!STRONGFLOW_PERMISSION_SUPPORTED_HOSTS.includes(host as StrongFlowPermissionSupportedHost)) {
    return policyError(
      'UNSUPPORTED_PLATFORM',
      `StrongFlow permission enforcement does not support ${host}`,
    )
  }
  if (
    value.schemaVersion !== STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION
    || value.filesystem !== 'codex-restricted'
    || value.process !== 'codex-sandboxed'
    || value.network !== 'codex-restricted'
    || value.environment !== 'explicit-allowlist'
    || value.approvals !== 'source-identified-human'
    || value.credentials !== 'dsh-reference-only'
    || value.publication !== 'exact-identity-guard'
    || value.audit !== 'durable-redacted'
  ) {
    return policyError(
      'ENFORCEMENT_UNAVAILABLE',
      'StrongFlow requires complete filesystem, process, network, credential, approval, publication, and audit enforcement',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION,
    platform: value.platform as StrongFlowPermissionHostPlatform,
    architecture: value.architecture as StrongFlowPermissionHostArchitecture,
    filesystem: 'codex-restricted',
    process: 'codex-sandboxed',
    network: 'codex-restricted',
    environment: 'explicit-allowlist',
    approvals: 'source-identified-human',
    credentials: 'dsh-reference-only',
    publication: 'exact-identity-guard',
    audit: 'durable-redacted',
  })
}

/**
 * Resolve a canonical policy only after the host proves every required enforcement family.
 * Callers must perform this check before publishing a role session or trusted actor.
 */
export function resolveStrongFlowPermissionPolicy(
  policyInput: unknown,
  enforcementInput: unknown,
): StrongFlowResolvedPermissionPolicy {
  const policy = parseStrongFlowPermissionPolicy(policyInput)
  const enforcement = parseEnforcementProfile(enforcementInput)
  return Object.freeze({ policy, enforcement })
}
