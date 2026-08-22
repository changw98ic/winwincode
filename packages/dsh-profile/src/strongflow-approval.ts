import type { Context } from '@deepseek-ai/cordis'
import type {
  StrongFlowRoleApprovalInteraction,
  StrongFlowRoleApprovalOutcome,
  StrongFlowRoleApprovalRequest,
} from '@winwincode/strongflow'

const OUTCOMES: readonly StrongFlowRoleApprovalOutcome[] = Object.freeze([
  'approved',
  'rejected',
  'cancelled',
  'unavailable',
])

declare module '@deepseek-ai/cordis' {
  interface Events {
    /**
     * Non-model StrongFlow operation decision. A DSH host/UI answerer owns the final outcome;
     * the fallback is always unavailable.
     * @mode waterfall
     */
    'winwincode/strongflow/approval/request'(
      request: StrongFlowRoleApprovalRequest,
      next: () => Promise<StrongFlowRoleApprovalOutcome>,
    ): Promise<StrongFlowRoleApprovalOutcome>
  }
}

function normalizedOutcome(value: unknown): StrongFlowRoleApprovalOutcome {
  return typeof value === 'string' && OUTCOMES.includes(value as StrongFlowRoleApprovalOutcome)
    ? value as StrongFlowRoleApprovalOutcome
    : 'unavailable'
}

/** Routes a kernel-owned role approval into the composed DSH interaction bus. */
export class DshStrongFlowApprovalInteraction implements StrongFlowRoleApprovalInteraction {
  readonly #ctx: Context

  constructor(ctx: Context) {
    this.#ctx = ctx
  }

  async request(request: StrongFlowRoleApprovalRequest): Promise<StrongFlowRoleApprovalOutcome> {
    if (request.signal.aborted) return 'cancelled'
    const answer = Promise.resolve().then(() => this.#ctx.waterfall(
      'winwincode/strongflow/approval/request',
      request,
      () => Promise.resolve('unavailable'),
    )).then(normalizedOutcome, () => 'unavailable' as const)
    return new Promise(resolve => {
      const onAbort = () => {
        request.signal.removeEventListener('abort', onAbort)
        resolve('cancelled')
      }
      request.signal.addEventListener('abort', onAbort, { once: true })
      void answer.then(outcome => {
        request.signal.removeEventListener('abort', onAbort)
        resolve(outcome)
      })
    })
  }
}
