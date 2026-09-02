// SPDX-License-Identifier: Apache-2.0

export type DraftValues = Readonly<Record<string, string>>

export interface DraftServerSnapshot<Values extends DraftValues> {
  readonly scope: string
  readonly revision: number
  readonly values: Values
}

export interface DraftSubmission<Values extends DraftValues> {
  readonly scope: string
  readonly revision: number
  readonly values: Values
}

export interface DraftFieldConflict {
  readonly field: string
  readonly baseValue: string
  readonly serverValue: string
  readonly draftValue: string
}

export interface EditableDraftState<Values extends DraftValues> {
  readonly scope: string | null
  readonly baseRevision: number | null
  readonly serverRevision: number | null
  readonly values: Values
  readonly dirtyFields: readonly (keyof Values & string)[]
  readonly conflicts: readonly DraftFieldConflict[]
  readonly revisionConflict: boolean
  readonly submission: DraftSubmission<Values> | null
}

export interface EditableDraftOptions<Values extends DraftValues> {
  /** Treat every revision change as relevant when at least one field is dirty. */
  readonly revisionSensitive?: boolean
  /** Values for these fields are replaced in conflict diagnostics. */
  readonly redactFields?: readonly (keyof Values & string)[]
}

/** Terminal outcome for one submitted draft, as decided by snapshot evidence. */
export type DraftSubmissionSettlement = 'in-flight' | 'success' | 'failure' | 'cancelled'

export interface DraftSubmissionEvidence {
  /** A snapshot reload or transport round trip is still in flight. */
  readonly busy: boolean
  /** The command transport reported a terminal failure for this submission. */
  readonly failed: boolean
  /** The reported failure was a user cancellation. */
  readonly cancelled: boolean
  /** The current snapshot proves the submitted change applied. */
  readonly confirmed: boolean
  /** The current snapshot proves the submitted change did not apply. */
  readonly refuted: boolean
}

/**
 * One decision seam for every mounted draft: a submission stays in flight
 * through unrelated reloads and only ends on transport failure or on snapshot
 * evidence, so pages never duplicate a second business state machine.
 */
export function settleDraftSubmission<Values extends DraftValues>(
  submission: DraftSubmission<Values> | null,
  evidence: DraftSubmissionEvidence,
): DraftSubmissionSettlement {
  if (submission === null) return 'in-flight'
  if (evidence.failed) return evidence.cancelled ? 'cancelled' : 'failure'
  if (evidence.busy) return 'in-flight'
  if (evidence.confirmed) return 'success'
  if (evidence.refuted) return 'failure'
  return 'in-flight'
}

export interface EditableDraft<Values extends DraftValues> {
  readonly state: EditableDraftState<Values>
  synchronize(snapshot: DraftServerSnapshot<Values> | null): void
  edit<Field extends keyof Values & string>(field: Field, value: Values[Field]): void
  beginSubmission(): DraftSubmission<Values> | null
  finishSubmission(outcome: 'success' | 'failure' | 'cancelled'): void
  resolveConflicts(resolution: 'keep-draft' | 'use-server'): void
  reset(): void
}

const emptyValues = <Values extends DraftValues>(): Values => Object.freeze({}) as Values

function copyValues<Values extends DraftValues>(values: Values): Values {
  return Object.freeze({ ...values }) as Values
}

function sameValue(left: string | undefined, right: string | undefined): boolean {
  return left === right
}

/**
 * Keep browser-owned form edits separate from the latest server snapshot.
 *
 * The controller deliberately has no storage adapter. Its lifetime is the
 * mounted page, so secrets and ordinary drafts never become persistent state.
 */
export function createEditableDraft<Values extends DraftValues>(
  options: EditableDraftOptions<Values> = {},
): EditableDraft<Values> {
  const redacted = new Set<string>(options.redactFields ?? [])
  let scope: string | null = null
  let baseRevision: number | null = null
  let serverRevision: number | null = null
  let baseValues = emptyValues<Values>()
  let serverValues = emptyValues<Values>()
  let values = emptyValues<Values>()
  let dirtyFields = new Set<keyof Values & string>()
  let conflicts: readonly DraftFieldConflict[] = Object.freeze([])
  let revisionConflict = false
  let submission: DraftSubmission<Values> | null = null

  const exposed = (field: string, value: string | undefined): string => (
    redacted.has(field) ? '[redacted]' : value ?? ''
  )

  function state(): EditableDraftState<Values> {
    return Object.freeze({
      scope,
      baseRevision,
      serverRevision,
      values,
      dirtyFields: Object.freeze([...dirtyFields]),
      conflicts,
      revisionConflict,
      submission,
    })
  }

  function clear(): void {
    scope = null
    baseRevision = null
    serverRevision = null
    baseValues = emptyValues<Values>()
    serverValues = emptyValues<Values>()
    values = emptyValues<Values>()
    dirtyFields = new Set()
    conflicts = Object.freeze([])
    revisionConflict = false
    submission = null
  }

  function adopt(snapshot: DraftServerSnapshot<Values>): void {
    scope = snapshot.scope
    baseRevision = snapshot.revision
    serverRevision = snapshot.revision
    baseValues = copyValues(snapshot.values)
    serverValues = copyValues(snapshot.values)
    values = copyValues(snapshot.values)
    dirtyFields = new Set()
    conflicts = Object.freeze([])
    revisionConflict = false
    submission = null
  }

  const controller: EditableDraft<Values> = {
    get state() { return state() },
    synchronize(snapshot) {
      if (snapshot === null) {
        clear()
        return
      }
      if (scope === null || scope !== snapshot.scope) {
        adopt(snapshot)
        return
      }
      if (snapshot.revision < (serverRevision ?? 0)) return

      const nextServer = copyValues(snapshot.values)
      const nextValues = { ...values } as Record<string, string>
      const nextDirty = new Set(dirtyFields)
      const nextConflicts: DraftFieldConflict[] = []
      const nextBase = { ...baseValues } as Record<string, string>
      for (const field of Object.keys(snapshot.values) as (keyof Values & string)[]) {
        const draftValue = values[field]
        const currentServerValue = snapshot.values[field] ?? ''
        if (!nextDirty.has(field)) {
          nextValues[field] = currentServerValue
          nextBase[field] = currentServerValue
          continue
        }
        if (sameValue(draftValue, currentServerValue)) {
          nextDirty.delete(field)
          nextValues[field] = currentServerValue
          nextBase[field] = currentServerValue
          continue
        }
        const baseValue = baseValues[field]
        if (
          !sameValue(baseValue, currentServerValue)
          || (options.revisionSensitive === true && snapshot.revision !== baseRevision)
        ) {
          nextConflicts.push(Object.freeze({
            field,
            baseValue: exposed(field, baseValue),
            serverValue: exposed(field, currentServerValue),
            draftValue: exposed(field, draftValue),
          }))
        }
      }
      serverRevision = snapshot.revision
      serverValues = nextServer
      values = copyValues(nextValues as Values)
      dirtyFields = nextDirty
      conflicts = Object.freeze(nextConflicts)
      revisionConflict = nextConflicts.length > 0
      if (nextDirty.size === 0) {
        // Nothing is being edited against the old revision, so the whole
        // baseline advances and later edits compare against this snapshot.
        baseRevision = snapshot.revision
        baseValues = nextServer
      } else {
        // Mixed update: clean fields rebase per field, while dirty fields keep
        // the revision and values their conflict diagnostics came from.
        baseValues = copyValues(nextBase as Values)
      }
    },
    edit(field, value) {
      if (scope === null || submission !== null) return
      values = copyValues({ ...values, [field]: value } as Values)
      if (sameValue(value, serverValues[field])) dirtyFields.delete(field)
      else dirtyFields.add(field)
      conflicts = Object.freeze(conflicts.filter(conflict => conflict.field !== field))
      revisionConflict = conflicts.length > 0
    },
    beginSubmission() {
      if (
        scope === null
        || serverRevision === null
        || submission !== null
        || revisionConflict
      ) return null
      submission = Object.freeze({
        scope,
        revision: serverRevision,
        values: copyValues(values),
      })
      return submission
    },
    finishSubmission(outcome) {
      if (submission === null) return
      submission = null
      if (outcome === 'success') {
        baseRevision = serverRevision
        baseValues = serverValues
        values = serverValues
        dirtyFields = new Set()
        conflicts = Object.freeze([])
        revisionConflict = false
      }
    },
    resolveConflicts(resolution) {
      if (scope === null || serverRevision === null) return
      submission = null
      if (resolution === 'use-server') {
        baseValues = serverValues
        values = serverValues
        dirtyFields = new Set()
      } else {
        baseValues = serverValues
        const nextDirty = new Set<keyof Values & string>()
        for (const field of Object.keys(values) as (keyof Values & string)[]) {
          if (!sameValue(values[field], serverValues[field])) nextDirty.add(field)
        }
        dirtyFields = nextDirty
      }
      baseRevision = serverRevision
      conflicts = Object.freeze([])
      revisionConflict = false
    },
    reset() { clear() },
  }
  return controller
}
