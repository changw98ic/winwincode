import {
  StrongFlowArtifactValidationError,
  materializeStrongFlowArtifact,
  type StrongFlowArtifactFor,
  type StrongFlowArtifactIdByKind,
  type StrongFlowArtifactReference,
  type StrongFlowRoleArtifactKind,
} from '@winwincode/contracts'

import type {
  StrongFlowRoleArtifactValidationContext,
  StrongFlowRoleArtifactValidator,
} from './role-runner.js'

type ValueOrFactory<Value> = Value | (
  (context: StrongFlowRoleArtifactValidationContext) => Value
)

export interface StrongFlowCanonicalRoleArtifactValidatorOptions<
  Kind extends StrongFlowRoleArtifactKind,
> {
  readonly kind: Kind
  readonly artifactId: ValueOrFactory<StrongFlowArtifactIdByKind[Kind]>
  readonly createdAtMillis: ValueOrFactory<number>
  /** Exact transitive sources when model-visible inputs intentionally stay narrow. */
  readonly sourceArtifacts?: ValueOrFactory<readonly StrongFlowArtifactReference[]>
  /** Same-turn artifacts that are legitimate sources but were not role inputs. */
  readonly companionSourceArtifacts?: ValueOrFactory<
    readonly StrongFlowArtifactReference[]
  >
}

function resolved<Value>(
  value: ValueOrFactory<Value>,
  context: StrongFlowRoleArtifactValidationContext,
): Value {
  return typeof value === 'function'
    ? (value as (context: StrongFlowRoleArtifactValidationContext) => Value)(context)
    : value
}

/**
 * Connects the role runner's strict model envelope to the one canonical artifact parser.
 * Identity, source inputs, producer, event interval, and timestamp remain program-owned.
 */
export function createStrongFlowCanonicalRoleArtifactValidator<
  Kind extends StrongFlowRoleArtifactKind,
>(
  options: StrongFlowCanonicalRoleArtifactValidatorOptions<Kind>,
): StrongFlowRoleArtifactValidator<Kind, StrongFlowArtifactFor<Kind>> {
  return Object.freeze({
    kind: options.kind,
    validate(
      value: unknown,
      context: StrongFlowRoleArtifactValidationContext,
    ): StrongFlowArtifactFor<Kind> {
      const acceptedKinds = context.roleSession.roleSpec.acceptedInputArtifacts
      if (
        context.artifactKind !== options.kind
        || acceptedKinds.length !== context.inputArtifactIds.length
      ) {
        throw new StrongFlowArtifactValidationError(
          'SOURCE_INPUT_MISMATCH',
          'roleValidationContext.inputArtifactIds',
          'role artifact inputs do not match the installed role contract',
        )
      }
      const inputSources: StrongFlowArtifactReference[] = acceptedKinds.map(
        (artifactKind, index) => {
          const artifactId = context.inputArtifactIds[index]
          if (artifactId === undefined) {
            throw new StrongFlowArtifactValidationError(
              'SOURCE_INPUT_MISMATCH',
              `roleValidationContext.inputArtifactIds[${index}]`,
              'role artifact input identity is missing',
            )
          }
          return Object.freeze({ artifactKind, artifactId })
        },
      )
      const sourceArtifacts: StrongFlowArtifactReference[] = options.sourceArtifacts === undefined
        ? [...inputSources]
        : [...resolved(options.sourceArtifacts, context)]
      if (inputSources.some(input => !sourceArtifacts.some(source => (
        source.artifactKind === input.artifactKind
        && source.artifactId === input.artifactId
      )))) {
        throw new StrongFlowArtifactValidationError(
          'SOURCE_INPUT_MISMATCH',
          'roleValidationContext.inputArtifactIds',
          'role artifact sources omit a model-visible input identity',
        )
      }
      if (options.companionSourceArtifacts !== undefined) {
        sourceArtifacts.push(...resolved(options.companionSourceArtifacts, context))
      }
      const interval = context.eventInterval
      if (
        interval.turnId === null
        || interval.firstSequence === null
        || interval.lastSequence === null
      ) {
        throw new StrongFlowArtifactValidationError(
          'INVALID_EVENT_INTERVAL',
          'roleValidationContext.eventInterval',
          'a successful role artifact requires a complete kernel event interval',
        )
      }
      return materializeStrongFlowArtifact(options.kind, {
        artifactId: resolved(options.artifactId, context),
        jobId: context.roleSession.jobId,
        sourceArtifacts: Object.freeze(sourceArtifacts),
        producer: Object.freeze({
          kind: 'role',
          roleId: context.roleSession.roleSpec.id,
          stageRunId: context.roleSession.stageRunId,
          attemptId: context.roleSession.attemptId,
        }),
        kernelEventInterval: Object.freeze({
          schemaVersion: 1,
          kernelSessionLineageId: context.roleSession.kernelSessionLineageId,
          contextId: interval.contextId,
          generation: interval.generation,
          kernelSessionId: interval.kernelSessionId,
          kernelStreamId: interval.kernelStreamId,
          turnId: interval.turnId,
          firstSequence: interval.firstSequence,
          lastSequence: interval.lastSequence,
          eventCount: interval.eventCount,
        }),
        createdAtMillis: resolved(options.createdAtMillis, context),
      }, value)
    },
  })
}
