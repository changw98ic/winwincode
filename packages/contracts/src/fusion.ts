// SPDX-License-Identifier: Apache-2.0

/**
 * Fusion blind-panel wire contract (community.5.1 / FUSION-01).
 *
 * Authoritative Rust types: `crates/winwincode-fusion`. This module is the
 * TypeScript projection for host surfaces. Field names are camelCase and must
 * stay aligned with the Rust serde contract.
 *
 * Track boundary: Fusion is Blind Independent Reasoning over the same
 * canonical question and context. It is not a multi-agent role split, does
 * not block Community P0 work, and is not part of the E13 execution epic.
 * This contract intentionally does not import Kernel, Jev, or StrongFlow role
 * types so the Fusion epic can version independently.
 */

export const MIN_FUSION_PROVIDER_COUNT = 3
export const MAX_FUSION_PROVIDER_COUNT = 16

export interface FusionProviderCandidate {
  readonly id: string
  readonly provider: string
  readonly model: string
  readonly reasoningEffort?: string | null
}

export interface FusionBudget {
  readonly candidateTimeoutMillis: number
  readonly maxTotalTokens: number
}

export interface FusionInput {
  readonly question: string
  readonly canonicalContext: unknown
  readonly constraints: readonly string[]
  readonly expectedOutputSchema: Record<string, unknown>
  readonly providerCandidates: readonly FusionProviderCandidate[]
  readonly budget: FusionBudget
}

export interface FusionBlindPrompt {
  readonly question: string
  readonly canonicalContext: unknown
  readonly constraints: readonly string[]
  readonly expectedOutputSchema: Record<string, unknown>
}

export interface FusionTokenUsage {
  readonly inputTokens: number
  readonly outputTokens: number
  readonly totalTokens: number
}

export interface FusionProviderRequest {
  readonly panelId: string
  readonly candidateId: string
  readonly requestId: string
  readonly provider: string
  readonly model: string
  readonly reasoningEffort?: string | null
  readonly prompt: FusionBlindPrompt
  readonly maxTotalTokens: number
}

export interface FusionProviderAnswer {
  readonly providerResponseId: string
  readonly answer: unknown
  readonly tokenUsage?: FusionTokenUsage | null
}

export interface FusionCandidateAudit {
  readonly panelId: string
  readonly candidateId: string
  readonly requestId: string
  readonly inputDigest: string
  readonly requestPayloadDigest: string
  readonly provider: string
  readonly model: string
  readonly providerResponseId: string
  readonly tokenUsage?: FusionTokenUsage | null
  readonly elapsedMillis: number
}

export interface FusionCandidate {
  readonly audit: FusionCandidateAudit
  readonly answer: unknown
}

export interface FusionCandidateFailure {
  readonly panelId: string
  readonly candidateId: string
  readonly requestId: string
  readonly inputDigest: string
  readonly requestPayloadDigest: string
  readonly provider: string
  readonly model: string
  readonly code: string
  readonly message: string
  readonly elapsedMillis: number
}

export interface FusionPanelResult {
  readonly panelId: string
  readonly inputDigest: string
  readonly candidates: readonly FusionCandidate[]
  readonly failures: readonly FusionCandidateFailure[]
}

export type FusionPanelErrorCode =
  | 'Fusion panels require three to sixteen Providers'
  | 'Fusion panels require at least three distinct Providers'
  | 'Fusion candidate ids must be unique'
  | 'Fusion expected output schema must be an object'
  | 'Fusion budget limits must be positive'
  | 'Fusion text is empty or exceeds the bound'
  | 'Fusion constraints exceed the bound'

/** Canonical validation mirroring the Rust panel gate. */
export function validateFusionInput(input: FusionInput): FusionPanelErrorCode | null {
  if (!input.question.trim()) {
    return 'Fusion text is empty or exceeds the bound'
  }
  if (
    typeof input.expectedOutputSchema !== 'object'
    || input.expectedOutputSchema === null
    || Array.isArray(input.expectedOutputSchema)
  ) {
    return 'Fusion expected output schema must be an object'
  }
  if (
    input.budget.candidateTimeoutMillis <= 0
    || input.budget.maxTotalTokens <= 0
  ) {
    return 'Fusion budget limits must be positive'
  }
  if (
    input.providerCandidates.length < MIN_FUSION_PROVIDER_COUNT
    || input.providerCandidates.length > MAX_FUSION_PROVIDER_COUNT
  ) {
    return 'Fusion panels require three to sixteen Providers'
  }
  const ids = new Set<string>()
  const providers = new Set<string>()
  for (const candidate of input.providerCandidates) {
    if (!candidate.id.trim() || !candidate.provider.trim() || !candidate.model.trim()) {
      return 'Fusion text is empty or exceeds the bound'
    }
    if (ids.has(candidate.id)) {
      return 'Fusion candidate ids must be unique'
    }
    ids.add(candidate.id)
    providers.add(candidate.provider)
  }
  if (providers.size < MIN_FUSION_PROVIDER_COUNT) {
    return 'Fusion panels require at least three distinct Providers'
  }
  return null
}

/**
 * Builds the isolated prompt each Provider receives. Sibling candidates are
 * never included.
 */
export function fusionBlindPrompt(input: FusionInput): FusionBlindPrompt {
  return {
    question: input.question,
    canonicalContext: input.canonicalContext,
    constraints: [...input.constraints],
    expectedOutputSchema: input.expectedOutputSchema,
  }
}
