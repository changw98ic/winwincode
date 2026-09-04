// SPDX-License-Identifier: Apache-2.0

import type {
  ApprovalEffectiveDecisionScope,
  ApprovalProjection,
  ApprovalSanitizedDetailUnavailableReason,
  Instant,
} from './generated/contracts.js'
import { ApprovalProjectionCategory } from './generated/contracts.js'

/**
 * Every Approval text the page renders is bounded to this many characters.
 * The producer summary is free-form Worker text, so the Client never trusts
 * its length even though the Server already validated the projection.
 */
export const APPROVAL_TEXT_LIMIT = 200

const ELLIPSIS = '…'
const SEPARATOR = ' '
const WHITESPACE = /\s+/gu
// Bidi and zero-width controls change what an operator reads without changing
// the bytes a command runs, so they are removed before text is rendered.
const HIDDEN_CONTROLS = /[\u200B-\u200F\u202A-\u202E\u2066-\u2069\uFEFF]/gu

export type ApprovalImpactKind =
  | 'shell'
  | 'filesystem_write'
  | 'network'
  | 'mcp'
  | 'unknown'

export type ApprovalRiskLevel = 'high' | 'elevated' | 'moderate' | 'unknown'

export type ApprovalFieldKey =
  | 'command'
  | 'cwd'
  | 'fileImpact'
  | 'networkTargets'
  | 'mcpTarget'
  | 'requestedReason'

/** Why one requested risk field carries no text. */
export type ApprovalWithheldReason =
  | ApprovalSanitizedDetailUnavailableReason
  | 'not_in_secret_safe_projection'

export interface ApprovalBoundText {
  readonly text: string
  readonly truncated: boolean
}

export interface ApprovalRiskField {
  readonly key: ApprovalFieldKey
  readonly label: string
  readonly text: string | null
  readonly availability: 'available' | 'withheld'
  readonly note: string | null
  readonly withheldReason: ApprovalWithheldReason | null
  readonly withheldLabel: string | null
}

export interface ApprovalRiskLevelView {
  readonly level: ApprovalRiskLevel
  readonly label: string
  readonly rationale: string
}

export interface ApprovalDecisionScopeView {
  /** `unknown` is the fail-closed rendering of a projection without a scope. */
  readonly scope: 'once' | 'worker_session' | 'unknown'
  readonly label: string
  readonly detail: string
  /** approval.decide accepts no client-selected scope. */
  readonly selectable: false
}

export interface ApprovalExpiryView {
  readonly expiresAt: Instant
  readonly expired: boolean
  readonly millisRemaining: number | null
  readonly label: string
}

export interface ApprovalExecutionTargetView {
  readonly productSessionId: string
  readonly workerSessionId: string
  readonly executionJobId: string
  readonly stageRunId: string | null
  readonly label: string
}

export interface ApprovalRiskDetail {
  readonly approvalId: string
  readonly state: ApprovalProjection['state'] | 'unknown'
  readonly revision: number
  readonly subject: string
  readonly subjectTruncated: boolean
  readonly impact: ApprovalImpactKind
  readonly impactLabel: string
  readonly impactStatements: readonly string[]
  readonly risk: ApprovalRiskLevelView
  readonly fields: readonly ApprovalRiskField[]
  readonly fieldByKey: Readonly<Record<ApprovalFieldKey, ApprovalRiskField>>
  readonly decisionScope: ApprovalDecisionScopeView
  readonly expiry: ApprovalExpiryView
  readonly executionTarget: ApprovalExecutionTargetView
}

export interface ApprovalRiskDetailOptions {
  /** Card decision state already settled by the view model wins over the clock. */
  readonly expired?: boolean
  readonly nowMillis?: () => number
}

const IMPACT_LABELS: Readonly<Record<ApprovalImpactKind, string>> = Object.freeze({
  shell: 'Shell execution',
  filesystem_write: 'Filesystem write',
  network: 'Network access',
  mcp: 'MCP tool call',
  unknown: 'Unclassified action',
})

const IMPACT_STATEMENTS: Readonly<Record<ApprovalImpactKind, readonly string[]>> =
  Object.freeze({
    shell: Object.freeze(['Runs a shell command inside the delivery workspace.']),
    filesystem_write: Object.freeze(['Writes files inside the delivery workspace.']),
    network: Object.freeze(['Performs outbound network access.']),
    mcp: Object.freeze(['Calls an MCP tool through a connected server.']),
    unknown: Object.freeze([]),
  })

const RISK_LEVELS: Readonly<Record<ApprovalImpactKind, ApprovalRiskLevel>> = Object.freeze({
  shell: 'high',
  network: 'elevated',
  mcp: 'elevated',
  filesystem_write: 'moderate',
  unknown: 'unknown',
})

const RISK_LABELS: Readonly<Record<ApprovalRiskLevel, string>> = Object.freeze({
  high: 'High risk',
  elevated: 'Elevated risk',
  moderate: 'Moderate risk',
  unknown: 'Risk unknown',
})

const RISK_RATIONALES: Readonly<Record<ApprovalRiskLevel, string>> = Object.freeze({
  high: 'A shell command can change the workspace and reach the network, so read the exact summary before approving.',
  elevated: 'The action reaches something outside this delivery workspace, so confirm the target before approving.',
  moderate: 'The action changes files inside this delivery workspace only.',
  unknown: 'The producer did not record a category, so the effect of this action cannot be classified.',
})

const FIELD_LABELS: Readonly<Record<ApprovalFieldKey, string>> = Object.freeze({
  command: 'Command summary',
  cwd: 'Working directory',
  fileImpact: 'File impact',
  networkTargets: 'Network targets',
  mcpTarget: 'MCP server and tool',
  requestedReason: 'Requested reason',
})

const WITHHELD_LABELS: Readonly<Record<ApprovalWithheldReason, string>> = Object.freeze({
  producer_unavailable: 'Not reported by the execution producer.',
  encoded_payload_redacted: 'Withheld · the tool payload was redacted before it was stored.',
  source_not_recorded: 'Not recorded for this action.',
  not_in_secret_safe_projection:
    'Withheld · the secret-safe Approval projection does not carry this field.',
})

const DECISION_SCOPES: Readonly<
  Partial<Record<ApprovalEffectiveDecisionScope, Omit<ApprovalDecisionScopeView, 'scope'>>>
> = Object.freeze({
  once: Object.freeze({
    label: 'Approve once',
    detail:
      'This decision covers this single request only and never extends to the Worker session.',
    selectable: false,
  }),
})

const UNKNOWN_DECISION_SCOPE = Object.freeze({
  label: 'Decision scope unavailable',
  detail:
    'The Control Plane did not report a decision scope, so treat this approval as one single request.',
  selectable: false,
})

/** Fold, strip, and bound one free-form producer string for rendering. */
export function boundApprovalText(value: string): ApprovalBoundText {
  const cleaned = (value ?? '').replace(HIDDEN_CONTROLS, '').replace(WHITESPACE, SEPARATOR).trim()
  if (cleaned.length <= APPROVAL_TEXT_LIMIT) {
    return Object.freeze({ text: cleaned, truncated: false })
  }
  return Object.freeze({
    text: `${cleaned.slice(0, APPROVAL_TEXT_LIMIT - 1)}${ELLIPSIS}`,
    truncated: true,
  })
}

export function approvalImpact(category: ApprovalProjectionCategory): ApprovalImpactKind {
  if (category === ApprovalProjectionCategory.Shell) return 'shell'
  if (category === ApprovalProjectionCategory.FilesystemWrite) return 'filesystem_write'
  if (category === ApprovalProjectionCategory.Network) return 'network'
  if (category === ApprovalProjectionCategory.Mcp) return 'mcp'
  return 'unknown'
}

export function approvalImpactStatements(
  category: ApprovalProjectionCategory,
): readonly string[] {
  return IMPACT_STATEMENTS[approvalImpact(category)]
}

export function approvalRiskLevel(category: ApprovalProjectionCategory): ApprovalRiskLevelView {
  const level = RISK_LEVELS[approvalImpact(category)]
  return Object.freeze({
    level,
    label: RISK_LABELS[level],
    rationale: RISK_RATIONALES[level],
  })
}

export function approvalDecisionScope(
  scope: ApprovalEffectiveDecisionScope,
): ApprovalDecisionScopeView {
  const resolved = scope === undefined ? undefined : DECISION_SCOPES[scope]
  if (resolved === undefined) return Object.freeze({ scope: 'unknown', ...UNKNOWN_DECISION_SCOPE })
  return Object.freeze({ scope, ...resolved })
}

export function approvalExpiry(expiresAt: Instant, nowMillis: number): ApprovalExpiryView {
  const instant = Date.parse(expiresAt)
  if (!Number.isFinite(instant)) {
    return Object.freeze({
      expiresAt,
      expired: true,
      millisRemaining: null,
      label: 'Expiry unknown · decision disabled',
    })
  }
  const millisRemaining = instant - nowMillis
  if (millisRemaining <= 0) {
    return Object.freeze({
      expiresAt,
      expired: true,
      millisRemaining: 0,
      label: `Expired ${expiresAt} · decision disabled`,
    })
  }
  return Object.freeze({
    expiresAt,
    expired: false,
    millisRemaining,
    label: `Expires ${expiresAt} · ${formatRemaining(millisRemaining)} left`,
  })
}

function formatRemaining(millis: number): string {
  const totalSeconds = Math.floor(millis / 1000)
  const hours = Math.floor(totalSeconds / 3600)
  const minutes = Math.floor((totalSeconds % 3600) / 60)
  const seconds = totalSeconds % 60
  if (hours > 0) return `${String(hours)}h ${String(minutes)}m`
  if (minutes > 0) return `${String(minutes)}m ${String(seconds)}s`
  return `${String(seconds)}s`
}

export function approvalWithheldLabel(reason: ApprovalWithheldReason): string {
  return WITHHELD_LABELS[reason]
}

function withheld(key: ApprovalFieldKey, reason: ApprovalWithheldReason): ApprovalRiskField {
  return Object.freeze({
    key,
    label: FIELD_LABELS[key],
    text: null,
    availability: 'withheld',
    note: null,
    withheldReason: reason,
    withheldLabel: WITHHELD_LABELS[reason],
  })
}

/**
 * Build the one secret-safe risk snapshot an Approval card renders.  Only the
 * sealed projection fields are read: category, subject, decision scope,
 * expiry, revision, state, and binding.  No structured tool detail exists on
 * the type, so every field the producer did not summarise is reported as
 * withheld instead of being guessed.  A projection that is missing one of the
 * fields added after it was persisted degrades to the fail-closed rendering
 * instead of throwing.
 */
export function approvalRiskDetail(
  projection: ApprovalProjection,
  options: ApprovalRiskDetailOptions = {},
): ApprovalRiskDetail {
  const nowMillis = (options.nowMillis ?? Date.now)()
  const summary = boundApprovalText(projection?.subject ?? '')
  const withheldReason: ApprovalWithheldReason =
    projection?.sanitizedDetail?.reason ?? 'not_in_secret_safe_projection'
  const impact = approvalImpact(projection?.category ?? ApprovalProjectionCategory.Unavailable)
  // The only command text the secret-safe projection carries is the producer
  // summary itself, so a shell action shows that summary and nothing more.
  const command: ApprovalRiskField = impact === 'shell' && summary.text !== ''
    ? Object.freeze({
      key: 'command',
      label: FIELD_LABELS.command,
      text: summary.text,
      availability: 'available',
      note: 'Producer summary. The secret-safe projection carries no parsed command.',
      withheldReason: null,
      withheldLabel: null,
    })
    : withheld('command', withheldReason)
  const fields: readonly ApprovalRiskField[] = Object.freeze([
    command,
    withheld('cwd', withheldReason),
    withheld('fileImpact', withheldReason),
    withheld('networkTargets', withheldReason),
    withheld('mcpTarget', withheldReason),
    withheld('requestedReason', withheldReason),
  ])
  const stageRunId = projection?.binding?.sessionIdentity?.stageRunId ?? null
  // An absent deadline fails closed inside approvalExpiry as "unknown".
  const expiresAt = (projection?.expiresAt ?? '') as Instant
  const expiry = options.expired === undefined
    ? approvalExpiry(expiresAt, nowMillis)
    : Object.freeze({
      ...approvalExpiry(expiresAt, nowMillis),
      expired: options.expired,
    })
  return Object.freeze({
    approvalId: projection?.id ?? '',
    state: projection?.state ?? 'unknown',
    revision: projection?.revision ?? 0,
    subject: summary.text,
    subjectTruncated: summary.truncated,
    impact,
    impactLabel: IMPACT_LABELS[impact],
    impactStatements: approvalImpactStatements(
      projection?.category ?? ApprovalProjectionCategory.Unavailable,
    ),
    risk: approvalRiskLevel(
      projection?.category ?? ApprovalProjectionCategory.Unavailable,
    ),
    fields,
    fieldByKey: Object.freeze(Object.fromEntries(
      fields.map(field => [field.key, field]),
    )) as Readonly<Record<ApprovalFieldKey, ApprovalRiskField>>,
    decisionScope: approvalDecisionScope(projection?.effectiveDecisionScope),
    expiry,
    executionTarget: Object.freeze({
      productSessionId: projection?.binding?.productSessionId ?? '',
      workerSessionId: projection?.binding?.workerSessionId ?? '',
      executionJobId: projection?.binding?.executionJobId ?? '',
      stageRunId,
      label: stageRunId === null
        ? 'ProductSession, ExecutionJob, and WorkerSession-bound'
        : 'ProductSession, StageRun, ExecutionJob, and WorkerSession-bound',
    }),
  })
}
