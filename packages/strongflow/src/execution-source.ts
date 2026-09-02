import type {
  Delivery,
  FrozenDeliveryCandidate,
  RuntimeEvent,
} from '@winwincode/contracts'

/** Read-only DSH/Codex facts shared by every StrongFlow execution projection. */
export interface StrongFlowExecutionFacts {
  readonly runtimeEvents: readonly RuntimeEvent[]
  readonly candidate: FrozenDeliveryCandidate | null
  /** Exact Git base-to-candidate diff regenerated from the frozen candidate. */
  readonly candidateDiff?: string | null
}

export interface StrongFlowExecutionSource {
  read(delivery: Delivery): Promise<StrongFlowExecutionFacts>
}
