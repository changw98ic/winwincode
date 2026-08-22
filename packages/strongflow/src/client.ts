import type { Context } from '@deepseek-ai/cordis'

import { mountStrongFlowOperatorRemote } from './operator-remote-client.js'

interface ConversationViewRegistration {
  readonly name: 'conversation.view'
  readonly id: 'strongflow'
  readonly order: number
  readonly label: () => string
}

interface ConversationSlots {
  inject(name: 'conversation.view', register: () => unknown): void
  register(
    options: ConversationViewRegistration,
    component: (props: StrongFlowViewProps) => string,
  ): unknown
}

interface StrongFlowClientContext extends Context {
  readonly slots: ConversationSlots
}

export interface StrongFlowViewProps {
  readonly sessionId?: string
}

/** Client services required before the advanced conversation view is mounted. */
export const inject = ['slots', 'remote'] as const

/**
 * Stable first StrongFlow seat. Later workbench slices replace this body on
 * the same conversation-view id rather than creating a second advanced mode.
 */
export function StrongFlowView({ sessionId }: StrongFlowViewProps): string {
  const scope = sessionId === undefined ? '' : ` · ${sessionId}`
  return `StrongFlow · 需求 → 方案 → 人工审核 → 执行${scope}`
}

/** Adds StrongFlow as an opt-in tab while DSH's built-in Chat tab stays default. */
export async function apply(ctx: Context): Promise<() => Promise<void>> {
  const { slots } = ctx as StrongFlowClientContext
  const disposeRemote = await mountStrongFlowOperatorRemote(ctx)
  slots.inject('conversation.view', () => slots.register({
    name: 'conversation.view',
    id: 'strongflow',
    order: 100,
    label: () => 'StrongFlow',
  }, StrongFlowView))
  return disposeRemote
}
