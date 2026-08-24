import {
  STRONGFLOW_RUNTIME_EXECUTION_LIMITS,
  STRONGFLOW_RUNTIME_EXECUTION_PROTOCOL,
  STRONGFLOW_RUNTIME_EXECUTION_SCHEMA_VERSION,
  parseStrongFlowRuntimeExecutionProjection,
  type Delivery,
  type RuntimeEvent,
  type StrongFlowRuntimeAgent,
  type StrongFlowRuntimeEventReference,
  type StrongFlowRuntimeExecutionProjection,
  type StrongFlowRuntimeSessionProjection,
} from '@winwincode/contracts'

import { containsRawCredentialMaterial } from './credential-boundary.js'
import {
  projectDeliveryRuntime,
  type DeliveryRuntimeAgentNode,
  type DeliveryRuntimeEventLink,
  type DeliverySessionRuntimeView,
  type DeliveryStageRuntimeView,
} from './delivery-runtime-projection.js'

const MAX_PUBLIC_TEXT_LENGTH = 8_192
const MAX_PUBLIC_COMMAND_LENGTH = 4_096
const TRUNCATION_MARKER = '… [truncated]'

function safeText(value: string | null, limit = MAX_PUBLIC_TEXT_LENGTH): string | null {
  if (value === null) return null
  if (value.trim().length === 0) return null
  if (containsRawCredentialMaterial(value)) return '[REDACTED]'
  if (value.length <= limit) return value
  return `${value.slice(0, Math.max(0, limit - TRUNCATION_MARKER.length))}${TRUNCATION_MARKER}`
}

function sequenceValue(value: string): bigint {
  return BigInt(value)
}

function eventReference(link: DeliveryRuntimeEventLink): StrongFlowRuntimeEventReference {
  return Object.freeze({
    eventId: link.eventId,
    sourceRef: link.sourceRef,
    sequence: link.sequence,
    kind: link.kind,
  })
}

function recentBySequence<Value>(
  values: readonly Value[],
  limit: number,
  reference: (value: Value) => DeliveryRuntimeEventLink,
): readonly Value[] {
  return values.toSorted((left, right) => {
    const leftSequence = sequenceValue(reference(left).sequence)
    const rightSequence = sequenceValue(reference(right).sequence)
    if (leftSequence === rightSequence) return 0
    return leftSequence < rightSequence ? -1 : 1
  }).slice(-limit)
}

function boundedAgents(session: DeliverySessionRuntimeView): readonly StrongFlowRuntimeAgent[] {
  const byId = new Map(session.agents.map(agent => [agent.threadId, agent]))
  const recent = session.agents.toSorted((left, right) => {
    const sequenceOrder = sequenceValue(right.latestEvent.sequence)
      - sequenceValue(left.latestEvent.sequence)
    if (sequenceOrder !== 0n) return sequenceOrder < 0n ? -1 : 1
    return left.threadId.localeCompare(right.threadId)
  })
  const selected = new Map<string, DeliveryRuntimeAgentNode>()
  for (const candidate of recent) {
    const chain: DeliveryRuntimeAgentNode[] = []
    const visited = new Set<string>()
    let current: DeliveryRuntimeAgentNode | undefined = candidate
    while (current !== undefined && !selected.has(current.threadId)) {
      if (visited.has(current.threadId)) break
      visited.add(current.threadId)
      chain.unshift(current)
      current = current.parentThreadId === null ? undefined : byId.get(current.parentThreadId)
    }
    const missing = chain.filter(agent => !selected.has(agent.threadId))
    if (selected.size + missing.length > STRONGFLOW_RUNTIME_EXECUTION_LIMITS.agents) continue
    for (const agent of missing) selected.set(agent.threadId, agent)
    if (selected.size === STRONGFLOW_RUNTIME_EXECUTION_LIMITS.agents) break
  }
  const selectedIds = new Set(selected.keys())
  return Object.freeze([...selected.values()]
    .toSorted((left, right) => (
      (left.path ?? left.threadId).localeCompare(right.path ?? right.threadId)
    ))
    .map(agent => Object.freeze({
      threadId: agent.threadId,
      path: safeText(agent.path),
      parentThreadId: agent.parentThreadId !== null && selectedIds.has(agent.parentThreadId)
        ? agent.parentThreadId
        : null,
      nickname: safeText(agent.nickname),
      role: safeText(agent.role),
      status: agent.status,
      latestEvent: eventReference(agent.latestEvent),
    })))
}

function sessionProjection(
  session: DeliverySessionRuntimeView,
): StrongFlowRuntimeSessionProjection {
  const agents = boundedAgents(session)
  const selectedAgentIds = new Set(agents.map(agent => agent.threadId))
  const agentEdges = session.agentEdges
    .filter(edge => (
      selectedAgentIds.has(edge.parentThreadId) && selectedAgentIds.has(edge.childThreadId)
    ))
    .slice(0, STRONGFLOW_RUNTIME_EXECUTION_LIMITS.agentEdges)
    .map(edge => Object.freeze({ ...edge }))
  const activities = recentBySequence(
    session.activities,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.activities,
    activity => activity.latestEvent,
  ).map(activity => Object.freeze({
    callId: activity.callId,
    activityType: activity.activityType,
    command: safeText(activity.command, MAX_PUBLIC_COMMAND_LENGTH),
    status: activity.status,
    outcome: activity.outcome,
    exitCode: activity.exitCode,
    latestEvent: eventReference(activity.latestEvent),
  }))
  const interactions = recentBySequence(
    session.interactions,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.interactions,
    interaction => interaction.requestedEvent,
  ).map(interaction => Object.freeze({
    id: interaction.id,
    interactionType: interaction.interactionType,
    blocking: interaction.blocking,
    status: interaction.status,
    questions: Object.freeze(interaction.questions
      .slice(0, STRONGFLOW_RUNTIME_EXECUTION_LIMITS.questions)
      .map(question => Object.freeze({
        id: question.id,
        header: safeText(question.header) ?? '[REDACTED]',
        question: safeText(question.question) ?? '[REDACTED]',
        isSecret: question.isSecret,
      }))),
    requestedEvent: eventReference(interaction.requestedEvent),
    resolvedEvent: interaction.resolvedEvent === null
      ? null
      : eventReference(interaction.resolvedEvent),
  }))
  const failures = recentBySequence(
    session.failures,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.failures,
    failure => failure.event,
  ).map(failure => Object.freeze({
    message: safeText(failure.message) ?? '[REDACTED]',
    code: safeText(failure.code),
    event: eventReference(failure.event),
  }))
  const evidence = session.evidenceLinks
    .toSorted((left, right) => {
      const leftSequence = sequenceValue(left.eventId.split('@').at(-1) ?? '0')
      const rightSequence = sequenceValue(right.eventId.split('@').at(-1) ?? '0')
      if (leftSequence === rightSequence) return left.type.localeCompare(right.type)
      return leftSequence < rightSequence ? -1 : 1
    })
    .slice(-STRONGFLOW_RUNTIME_EXECUTION_LIMITS.evidence)
    .map(link => Object.freeze({
      type: link.type,
      outcome: link.outcome,
      sourceRef: link.sourceRef,
      eventId: link.eventId,
    }))
  const usageEntries = Object.entries(session.usage?.totals ?? {})
    .filter((entry): entry is [string, number] => (
      Number.isSafeInteger(entry[1]) && entry[1] >= 0
    ))
    .toSorted(([left], [right]) => left.localeCompare(right))
    .slice(0, STRONGFLOW_RUNTIME_EXECUTION_LIMITS.usageMetrics)
  return Object.freeze({
    stageRunId: session.binding.stageRunId,
    sessionBindingId: session.binding.id,
    dshSessionId: session.binding.dshSessionId,
    codexSessionId: session.binding.codexSessionId,
    asOfSequence: session.asOfSequence,
    plan: session.plan === null
      ? null
      : Object.freeze({
        itemId: session.plan.itemId,
        explanation: safeText(session.plan.explanation),
        items: Object.freeze(session.plan.items
          .slice(0, STRONGFLOW_RUNTIME_EXECUTION_LIMITS.planItems)
          .map(item => Object.freeze({
            step: safeText(item.step) ?? '[REDACTED]',
            status: item.status,
          }))),
        text: safeText(session.plan.text),
        complete: session.plan.complete,
        latestEvent: eventReference(session.plan.latestEvent),
      }),
    agents,
    agentEdges: Object.freeze(agentEdges),
    activities: Object.freeze(activities),
    interactions: Object.freeze(interactions),
    failures: Object.freeze(failures),
    recovery: Object.freeze({
      state: session.recovery.state,
      failureCount: session.recovery.failureCount,
      recoveryCount: session.recovery.recoveryCount,
      lastFailureEvent: session.recovery.lastFailureEvent === null
        ? null
        : eventReference(session.recovery.lastFailureEvent),
      latestRecoveryEvent: session.recovery.latestRecoveryEvent === null
        ? null
        : eventReference(session.recovery.latestRecoveryEvent),
    }),
    diffSummary: session.diff === null
      ? null
      : Object.freeze({
        changedFileCount: session.diff.changedFiles.length,
        additions: session.diff.additions,
        deletions: session.diff.deletions,
        detailsVisible: false as const,
        event: eventReference(session.diff.event),
      }),
    usage: session.usage === null
      ? null
      : Object.freeze({
        totals: Object.freeze(Object.fromEntries(usageEntries)),
        event: eventReference(session.usage.event),
      }),
    evidence: Object.freeze(evidence),
  })
}

function latestSessions(
  stages: readonly DeliveryStageRuntimeView[],
): readonly DeliverySessionRuntimeView[] {
  return stages
    .flatMap(stage => stage.sessions.map(session => ({ stage, session })))
    .filter(({ session }) => session.binding.codexSessionId !== null)
    .toSorted((left, right) => (
      right.stage.stageRun.startedAtMillis - left.stage.stageRun.startedAtMillis
        || left.session.binding.id.localeCompare(right.session.binding.id)
    ))
    .slice(0, STRONGFLOW_RUNTIME_EXECUTION_LIMITS.sessions)
    .map(({ session }) => session)
}

/** Build a safe browser projection without exposing runtime logs or live diff details. */
export function projectStrongFlowRuntimeExecution(
  delivery: Delivery,
  events: Iterable<RuntimeEvent>,
): StrongFlowRuntimeExecutionProjection {
  const runtime = projectDeliveryRuntime(delivery, events)
  return parseStrongFlowRuntimeExecutionProjection({
    schemaVersion: STRONGFLOW_RUNTIME_EXECUTION_SCHEMA_VERSION,
    protocol: STRONGFLOW_RUNTIME_EXECUTION_PROTOCOL,
    deliveryId: runtime.deliveryId,
    deliveryRevision: runtime.deliveryRevision,
    sessions: latestSessions(runtime.stages).map(sessionProjection),
  })
}
