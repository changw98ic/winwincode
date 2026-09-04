// SPDX-License-Identifier: Apache-2.0

import type {
  AttentionItemId,
  DeliveryTaskDetailProjection,
} from './generated/contracts.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import type {
  StrongFlowAttentionDecisionInput,
  StrongFlowAttentionRemediationInput,
  StrongFlowProjection,
  StrongFlowSolutionReviewDecisionInput,
  StrongFlowViewModel,
  StrongFlowViewModelState,
} from './strongflow-view-model.js'
import {
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

/** Every reviewed surface a local note can be pinned to in this first version. */
export type StrongFlowReviewAnnotationKind =
  | 'file-line'
  | 'task'
  | 'solution-node'
  | 'criterion'

export type StrongFlowReviewAnnotationAnchor =
  | { readonly kind: 'file-line'; readonly path: string; readonly line: number }
  | { readonly kind: 'task'; readonly deliveryTaskId: string }
  | { readonly kind: 'solution-node'; readonly nodeId: string }
  | { readonly kind: 'criterion'; readonly criterionId: string }

/**
 * Exact frozen identity a note is pinned to. Notes live only in this browser,
 * so this identity is the only thing that can tell a note apart from the
 * snapshot it was written against.
 */
export interface StrongFlowReviewAnnotationIdentity {
  readonly deliveryId: string
  readonly deliveryRevision: number
  readonly candidateRef: string | null
  readonly candidateDigest: string | null
}

export interface StrongFlowReviewAnnotation {
  readonly id: string
  readonly kind: StrongFlowReviewAnnotationKind
  readonly anchor: StrongFlowReviewAnnotationAnchor
  readonly note: string
  readonly createdAtMillis: number
  readonly identity: StrongFlowReviewAnnotationIdentity
}

export type StrongFlowReviewStaleReason =
  | 'candidate-changed'
  | 'delivery-revision-changed'

export interface StrongFlowReviewAnnotationStaleness {
  readonly id: string
  readonly reason: StrongFlowReviewStaleReason
  readonly captured: StrongFlowReviewAnnotationIdentity
  readonly current: StrongFlowReviewAnnotationIdentity
}

/**
 * The one legal Control Plane command a submission composes into. Local notes
 * never introduce a second command, a ReviewRound, or a ReviewComment entity.
 */
export type StrongFlowReviewCommand = 'delivery.resolve_attention'

export type StrongFlowReviewTarget =
  | 'requested-changes'
  | 'attention-resolution'
  | 'bounded-rework'

export interface StrongFlowReviewComposeRequest {
  readonly target: StrongFlowReviewTarget
  /** Open Attention record that carries the composed resolution. */
  readonly attentionItemId?: string
  /** Solution node the bounded rework is scoped to. */
  readonly nodeId?: string
  /** Optional Task the bounded rework is scoped to. */
  readonly deliveryTaskId?: string | null
  /** Optional reviewer comment carried alongside requested changes. */
  readonly comments?: string
}

export interface StrongFlowReviewPlan {
  readonly target: StrongFlowReviewTarget
  readonly command: StrongFlowReviewCommand
  /** Final bounded scope the reviewer confirms before submitting. */
  readonly summary: readonly string[]
  readonly annotationIds: readonly string[]
  readonly solutionReview: StrongFlowSolutionReviewDecisionInput | null
  readonly attention: StrongFlowAttentionDecisionInput | null
}

export type StrongFlowReviewDraftErrorCode =
  | 'STRONGFLOW_REVIEW_DRAFT_INVALID'
  | 'STRONGFLOW_REVIEW_DRAFT_EMPTY'
  | 'STRONGFLOW_REVIEW_DRAFT_STALE'
  | 'STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE'
  | 'STRONGFLOW_REVIEW_DRAFT_ANCHOR_STALE'
  | 'STRONGFLOW_REVIEW_DRAFT_IN_FLIGHT'

export class StrongFlowReviewDraftError extends Error {
  readonly code: StrongFlowReviewDraftErrorCode

  constructor(code: StrongFlowReviewDraftErrorCode, message: string) {
    super(message)
    this.name = 'StrongFlowReviewDraftError'
    this.code = code
  }
}

export interface StrongFlowReviewAnnotationsState {
  readonly identity: StrongFlowReviewAnnotationIdentity | null
  readonly annotations: readonly StrongFlowReviewAnnotation[]
  readonly staleness: readonly StrongFlowReviewAnnotationStaleness[]
  /** Annotation ids carried by the submission that is still unresolved. */
  readonly submission: readonly string[] | null
}

export interface StrongFlowReviewDraftOptions {
  readonly nextId?: () => string
  readonly nowMillis?: () => number
  /** Highest accepted note length; keeps one browser draft bounded. */
  readonly maxNoteLength?: number
}

export interface StrongFlowReviewAnnotations {
  readonly state: StrongFlowReviewAnnotationsState
  /**
   * Re-pin the draft onto the latest snapshot. A different Delivery drops the
   * draft; the same Delivery keeps every note and only marks it stale.
   */
  synchronize(projection: StrongFlowProjection | null): void
  add(input: {
    readonly anchor: StrongFlowReviewAnnotationAnchor
    readonly note: string
  }): string
  update(id: string, note: string): void
  remove(id: string): void
  reanchor(id: string, anchor: StrongFlowReviewAnnotationAnchor): void
  /** Keep one stale note and pin it onto the current snapshot. */
  confirm(id: string): void
  /** Drop one stale note; the explicit alternative to confirming it. */
  discard(id: string): void
  /** Readable digest of every staged note, in staging order. */
  summarize(): readonly string[]
  compose(request: StrongFlowReviewComposeRequest): StrongFlowReviewPlan
  begin(plan: StrongFlowReviewPlan): void
  /**
   * Clear exactly the submitted notes on success; a failure or a cancellation
   * keeps every note so the reviewer can retry without retyping.
   */
  settle(outcome: 'success' | 'failure' | 'cancelled'): void
  reset(): void
}

const DEFAULT_MAX_NOTE_LENGTH = 2_000
const MAX_STAGED_NOTES = 500

const ANCHOR_KINDS: readonly StrongFlowReviewAnnotationKind[] = Object.freeze([
  'file-line',
  'task',
  'solution-node',
  'criterion',
])

/** Stable, human readable name of one reviewed target. */
export function strongFlowReviewAnchorLabel(
  anchor: StrongFlowReviewAnnotationAnchor,
): string {
  switch (anchor.kind) {
    case 'file-line': return `${anchor.path}:${String(anchor.line)}`
    case 'task': return anchor.deliveryTaskId
    case 'solution-node': return anchor.nodeId
    case 'criterion': return anchor.criterionId
  }
}

function identityOf(projection: StrongFlowProjection): StrongFlowReviewAnnotationIdentity {
  const candidate = projection.currentCandidate
  return Object.freeze({
    deliveryId: projection.delivery.deliveryId,
    deliveryRevision: projection.metadata.revisions.delivery,
    candidateRef: candidate?.candidateRef ?? null,
    candidateDigest: candidate?.diffSha256 ?? null,
  })
}

function sameIdentity(
  left: StrongFlowReviewAnnotationIdentity,
  right: StrongFlowReviewAnnotationIdentity,
): boolean {
  return left.deliveryRevision === right.deliveryRevision
    && left.candidateRef === right.candidateRef
    && left.candidateDigest === right.candidateDigest
}

function stalenessOf(
  annotation: StrongFlowReviewAnnotation,
  current: StrongFlowReviewAnnotationIdentity,
): StrongFlowReviewAnnotationStaleness | null {
  if (sameIdentity(annotation.identity, current)) return null
  const candidateChanged = annotation.identity.candidateDigest !== current.candidateDigest
    || annotation.identity.candidateRef !== current.candidateRef
  return Object.freeze({
    id: annotation.id,
    reason: candidateChanged ? 'candidate-changed' : 'delivery-revision-changed',
    captured: annotation.identity,
    current,
  })
}

/** Review-relevant targets the current snapshot still exposes. */
interface ReviewSnapshotContext {
  readonly taskIds: ReadonlySet<string>
  readonly nodeIds: ReadonlySet<string>
  readonly criterionIds: ReadonlySet<string>
  readonly openAttentionIds: readonly string[]
  readonly reviewPending: boolean
  readonly candidateDigest: string | null
  readonly deliveryRevision: number
}

function contextOf(projection: StrongFlowProjection): ReviewSnapshotContext {
  const review = projection.solutionReview
  return Object.freeze({
    taskIds: new Set(projection.delivery.tasks.map(task => task.id)),
    nodeIds: new Set([
      ...(review?.architectureDiagram.nodes ?? []),
      ...(review?.processDiagram.nodes ?? []),
    ].map(node => node.id)),
    criterionIds: new Set(
      (projection.verdict?.criteria ?? []).map(criterion => criterion.criterionId),
    ),
    openAttentionIds: Object.freeze(
      projection.attention
        .filter(item => item.status === 'open')
        .map(item => item.id),
    ),
    reviewPending: review?.reviewStatus === 'pending',
    candidateDigest: projection.currentCandidate?.diffSha256 ?? null,
    deliveryRevision: projection.metadata.revisions.delivery,
  })
}

function assertNote(note: string, maxNoteLength: number): string {
  if (typeof note !== 'string') {
    throw new StrongFlowReviewDraftError(
      'STRONGFLOW_REVIEW_DRAFT_INVALID',
      'A review note must be text.',
    )
  }
  const trimmed = note.trim()
  if (trimmed.length === 0) {
    throw new StrongFlowReviewDraftError(
      'STRONGFLOW_REVIEW_DRAFT_INVALID',
      'A review note must say something before it is staged.',
    )
  }
  if (trimmed.length > maxNoteLength) {
    throw new StrongFlowReviewDraftError(
      'STRONGFLOW_REVIEW_DRAFT_INVALID',
      `A review note must stay within ${String(maxNoteLength)} characters.`,
    )
  }
  return trimmed
}

function assertAnchor(
  anchor: StrongFlowReviewAnnotationAnchor,
): StrongFlowReviewAnnotationAnchor {
  if (anchor === null || typeof anchor !== 'object') {
    throw new StrongFlowReviewDraftError(
      'STRONGFLOW_REVIEW_DRAFT_INVALID',
      'A review note needs a reviewed target.',
    )
  }
  switch (anchor.kind) {
    case 'file-line': {
      const path = anchor.path.trim()
      if (path.length === 0 || !Number.isInteger(anchor.line) || anchor.line < 1) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_INVALID',
          'A file note needs a changed path and a one-based line.',
        )
      }
      return Object.freeze({ kind: 'file-line', path, line: anchor.line })
    }
    case 'task':
      if (anchor.deliveryTaskId.trim().length === 0) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_INVALID',
          'A Task note needs the reviewed Task.',
        )
      }
      return Object.freeze({ kind: 'task', deliveryTaskId: anchor.deliveryTaskId })
    case 'solution-node':
      if (anchor.nodeId.trim().length === 0) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_INVALID',
          'A solution note needs the reviewed node.',
        )
      }
      return Object.freeze({ kind: 'solution-node', nodeId: anchor.nodeId })
    case 'criterion':
      if (anchor.criterionId.trim().length === 0) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_INVALID',
          'An acceptance criterion note needs the reviewed criterion.',
        )
      }
      return Object.freeze({ kind: 'criterion', criterionId: anchor.criterionId })
    default:
      throw new StrongFlowReviewDraftError(
        'STRONGFLOW_REVIEW_DRAFT_INVALID',
        'A review note needs a reviewed target.',
      )
  }
}

/**
 * Anchors are validated against the snapshot a note is written into, so a note
 * can never be staged against a Task, node or criterion the current Delivery
 * no longer exposes. File lines are free-form: the changed-path list belongs to
 * the Candidate file read, which is a separate bounded query.
 */
function assertAnchorInSnapshot(
  anchor: StrongFlowReviewAnnotationAnchor,
  context: ReviewSnapshotContext | null,
): void {
  if (context === null) return
  const stale = (() => {
    switch (anchor.kind) {
      case 'task': return !context.taskIds.has(anchor.deliveryTaskId)
      case 'solution-node': return !context.nodeIds.has(anchor.nodeId)
      case 'criterion': return !context.criterionIds.has(anchor.criterionId)
      case 'file-line': return false
    }
  })()
  if (stale) {
    throw new StrongFlowReviewDraftError(
      'STRONGFLOW_REVIEW_DRAFT_ANCHOR_STALE',
      'That reviewed target is not part of the current Delivery snapshot.',
    )
  }
}

let noteSequence = 0

/**
 * One browser-owned review draft. Notes never become ReviewRound or
 * ReviewComment entities: they exist only while the StrongFlow workbench is
 * mounted and they leave the browser as part of one existing legal command.
 */
export function createStrongFlowReviewAnnotations(
  options: StrongFlowReviewDraftOptions = {},
): StrongFlowReviewAnnotations {
  const nextId = options.nextId ?? (() => `note-${String(noteSequence += 1).padStart(4, '0')}`)
  const nowMillis = options.nowMillis ?? (() => Date.now())
  const maxNoteLength = options.maxNoteLength ?? DEFAULT_MAX_NOTE_LENGTH
  let identity: StrongFlowReviewAnnotationIdentity | null = null
  let context: ReviewSnapshotContext | null = null
  let annotations: readonly StrongFlowReviewAnnotation[] = Object.freeze([])
  let submission: readonly string[] | null = null

  function state(): StrongFlowReviewAnnotationsState {
    return Object.freeze({
      identity,
      annotations,
      staleness: Object.freeze(annotations.flatMap(annotation => {
        const stale = identity === null ? null : stalenessOf(annotation, identity)
        return stale === null ? [] : [stale]
      })),
      submission,
    })
  }

  function requireEditable(): void {
    if (submission !== null) {
      throw new StrongFlowReviewDraftError(
        'STRONGFLOW_REVIEW_DRAFT_IN_FLIGHT',
        'Wait for the current review submission to settle.',
      )
    }
    if (identity === null) {
      throw new StrongFlowReviewDraftError(
        'STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE',
        'Open a Delivery before staging review notes.',
      )
    }
  }

  function requireId(id: string): StrongFlowReviewAnnotation {
    const found = annotations.find(annotation => annotation.id === id)
    if (found === undefined) {
      throw new StrongFlowReviewDraftError(
        'STRONGFLOW_REVIEW_DRAFT_INVALID',
        'That staged note is no longer in this review draft.',
      )
    }
    return found
  }

  function replace(next: readonly StrongFlowReviewAnnotation[]): void {
    annotations = Object.freeze(next.map(annotation => Object.freeze({ ...annotation })))
  }

  /** Editing or confirming a note re-pins it onto the current snapshot. */
  function repin(annotation: StrongFlowReviewAnnotation): StrongFlowReviewAnnotation {
    return { ...annotation, identity: identity! }
  }

  const controller: StrongFlowReviewAnnotations = {
    get state() { return state() },
    synchronize(projection) {
      // A missing snapshot is transient; dropping the browser draft on it
      // would destroy notes the reviewer already wrote.
      if (projection === null) return
      const nextIdentity = identityOf(projection)
      if (identity !== null && nextIdentity.deliveryId !== identity.deliveryId) {
        // Notes are scoped to one Delivery, so a different Delivery starts a
        // new draft instead of silently carrying notes across business scope.
        identity = nextIdentity
        context = contextOf(projection)
        annotations = Object.freeze([])
        submission = null
        return
      }
      identity = nextIdentity
      context = contextOf(projection)
    },
    add(input) {
      requireEditable()
      const anchor = assertAnchor(input.anchor)
      const note = assertNote(input.note, maxNoteLength)
      assertAnchorInSnapshot(anchor, context)
      if (annotations.length >= MAX_STAGED_NOTES) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_INVALID',
          `A review draft stays within ${String(MAX_STAGED_NOTES)} notes.`,
        )
      }
      const annotation: StrongFlowReviewAnnotation = Object.freeze({
        id: nextId(),
        kind: anchor.kind,
        anchor,
        note,
        createdAtMillis: nowMillis(),
        identity: identity!,
      })
      replace([...annotations, annotation])
      return annotation.id
    },
    update(id, note) {
      requireEditable()
      const current = requireId(id)
      const trimmed = assertNote(note, maxNoteLength)
      replace(annotations.map(annotation => (
        annotation.id === id ? { ...repin(current), note: trimmed } : annotation
      )))
    },
    remove(id) {
      requireEditable()
      requireId(id)
      replace(annotations.filter(annotation => annotation.id !== id))
    },
    reanchor(id, anchor) {
      requireEditable()
      requireId(id)
      const next = assertAnchor(anchor)
      assertAnchorInSnapshot(next, context)
      replace(annotations.map(annotation => (
        annotation.id === id
          ? { ...repin(annotation), anchor: next, kind: next.kind }
          : annotation
      )))
    },
    confirm(id) {
      requireEditable()
      const current = requireId(id)
      if (identity === null) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE',
          'Open a Delivery before re-confirming review notes.',
        )
      }
      replace(annotations.map(annotation => (
        annotation.id === id ? repin(current) : annotation
      )))
    },
    discard(id) {
      requireEditable()
      requireId(id)
      replace(annotations.filter(annotation => annotation.id !== id))
    },
    summarize() {
      return Object.freeze(annotations.map(annotation => (
        `${strongFlowReviewAnchorLabel(annotation.anchor)} — ${annotation.note}`
      )))
    },
    compose(request) {
      if (submission !== null) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_IN_FLIGHT',
          'Wait for the current review submission to settle.',
        )
      }
      if (identity === null || context === null) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE',
          'Open a Delivery before composing review notes.',
        )
      }
      if (annotations.length === 0) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_EMPTY',
          'Stage at least one review note before composing a submission.',
        )
      }
      const stale = annotations.flatMap(annotation => {
        const entry = stalenessOf(annotation, identity!)
        return entry === null ? [] : [entry]
      })
      if (stale.length > 0) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_STALE',
          `${String(stale.length)} of ${String(annotations.length)} staged notes were written `
            + 'against an earlier Candidate or revision. Re-confirm or discard them first.',
        )
      }
      const lines = controller.summarize()
      const noteCount = `${String(annotations.length)} note${annotations.length === 1 ? '' : 's'}`
      const submittedIds = (): readonly string[] =>
        annotations.map(annotation => annotation.id)
      if (request.target === 'requested-changes') {
        if (!context.reviewPending) {
          throw new StrongFlowReviewDraftError(
            'STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE',
            'The current Delivery has no pending solution review to return changes on.',
          )
        }
        const comments = (request.comments ?? '').trim()
        return Object.freeze({
          target: request.target,
          command: 'delivery.resolve_attention' as const,
          summary: Object.freeze([
            `Requested changes · ${noteCount} staged`,
            ...(comments.length === 0 ? [] : [`Comment · ${comments}`]),
            ...lines,
          ]),
          annotationIds: Object.freeze(submittedIds()),
          solutionReview: Object.freeze({
            action: 'request_changes' as const,
            comments,
            requestedChanges: Object.freeze([...lines]),
          }),
          attention: null,
        })
      }
      const attentionItemId = request.attentionItemId ?? ''
      if (!context.openAttentionIds.includes(attentionItemId)) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE',
          'Choose an open Attention record to carry these notes.',
        )
      }
      const resolution = [`${noteCount} staged for this decision:`, ...lines].join('\n')
      if (request.target === 'attention-resolution') {
        return Object.freeze({
          target: request.target,
          command: 'delivery.resolve_attention' as const,
          summary: Object.freeze([
            `Attention resolution · ${attentionItemId}`,
            `${noteCount} staged`,
            ...lines,
          ]),
          annotationIds: Object.freeze(submittedIds()),
          solutionReview: null,
          attention: Object.freeze({
            attentionItemId: attentionItemId as AttentionItemId,
            decision: 'resolve' as const,
            resolution,
            remediation: null,
          }),
        })
      }
      const nodeId = request.nodeId ?? ''
      if (!context.nodeIds.has(nodeId)) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_ANCHOR_STALE',
          'Choose a solution node from the current solution review.',
        )
      }
      const requestedTaskId = request.deliveryTaskId ?? null
      if (requestedTaskId !== null && !context.taskIds.has(requestedTaskId)) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_ANCHOR_STALE',
          'Choose a Task from the current Delivery.',
        )
      }
      return Object.freeze({
        target: request.target,
        command: 'delivery.resolve_attention' as const,
        summary: Object.freeze([
          `Bounded rework scope · ${nodeId}`,
          `Delivery task · ${requestedTaskId ?? 'none'}`,
          `Candidate · ${context.candidateDigest ?? 'none'}`,
          `Delivery revision · ${String(context.deliveryRevision)}`,
          `${noteCount} staged`,
          ...lines,
        ]),
        annotationIds: Object.freeze(submittedIds()),
        solutionReview: null,
        attention: Object.freeze({
          attentionItemId: attentionItemId as AttentionItemId,
          decision: 'resolve' as const,
          resolution,
          remediation: Object.freeze({
            deliveryTaskId:
              requestedTaskId as DeliveryTaskDetailProjection['id'] | null,
            nodeId,
            instructions: lines.join('\n'),
          }),
        }),
      })
    },
    begin(plan) {
      if (submission !== null) {
        throw new StrongFlowReviewDraftError(
          'STRONGFLOW_REVIEW_DRAFT_IN_FLIGHT',
          'Wait for the current review submission to settle.',
        )
      }
      submission = Object.freeze([...plan.annotationIds])
    },
    settle(outcome) {
      if (submission === null) return
      const submitted = submission
      submission = null
      if (outcome !== 'success') return
      // Only the notes that travelled with this command leave the draft.
      const cleared = new Set(submitted)
      replace(annotations.filter(annotation => !cleared.has(annotation.id)))
    },
    reset() {
      identity = null
      context = null
      annotations = Object.freeze([])
      submission = null
    },
  }
  return controller
}

interface ReviewDraftRow {
  readonly annotation: StrongFlowReviewAnnotation
  readonly stale: StrongFlowReviewAnnotationStaleness | null
  readonly editing: boolean
  readonly locked: boolean
}

interface ReviewDraftPanelRow {
  current: ReviewDraftRow
  readonly item: HTMLElement
  readonly label: HTMLElement
  readonly note: HTMLElement
  readonly staleNote: HTMLElement
  readonly confirm: HTMLButtonElement
  readonly discard: HTMLButtonElement
  readonly edit: HTMLButtonElement
  readonly editor: HTMLTextAreaElement
  readonly save: HTMLButtonElement
  readonly remove: HTMLButtonElement
  readonly onEdit: () => void
  readonly onSave: () => void
  readonly onRemove: () => void
  readonly onConfirm: () => void
  readonly onDiscard: () => void
}

export interface StrongFlowReviewPanelOptions {
  readonly document: Document
  readonly root: HTMLElement
  readonly draft: StrongFlowReviewAnnotations
  readonly model: StrongFlowViewModel
  readonly limits?: Pick<StrongFlowRenderLimits, 'tasks' | 'attention'>
  /** Presentation-only capability; Server authorization stays authoritative. */
  readonly readOnly?: boolean
  /** Page-local fail-closed flag: a historical review blocks current mutations. */
  readonly isHistoricalReviewOpen?: () => boolean
}

export interface StrongFlowReviewPanel {
  readonly root: HTMLElement
  /** Re-read the model and the page flags; cheap enough for every render. */
  update(): void
  close(): void
}

const TARGET_LABELS: readonly {
  readonly value: StrongFlowReviewTarget
  readonly label: string
}[] = Object.freeze([
  { value: 'requested-changes', label: 'Requested changes' },
  { value: 'attention-resolution', label: 'Attention resolution' },
  { value: 'bounded-rework', label: 'Bounded rework' },
])

function anchorKindLabel(value: StrongFlowReviewAnnotationKind): string {
  switch (value) {
    case 'file-line': return 'File line'
    case 'task': return 'Task'
    case 'solution-node': return 'Solution node'
    case 'criterion': return 'Acceptance criterion'
  }
}

function anchorKindNoun(value: StrongFlowReviewAnnotationKind): string {
  switch (value) {
    case 'file-line': return 'File'
    case 'task': return 'Task'
    case 'solution-node': return 'Solution'
    case 'criterion': return 'Criterion'
  }
}

/**
 * The staged-notes workbench panel. It owns no business state: the draft holds
 * the browser notes and the StrongFlow view model stays the only command
 * authority, so one submit is always exactly one existing legal command.
 */
export function mountStrongFlowReviewPanel(
  options: StrongFlowReviewPanelOptions,
): StrongFlowReviewPanel {
  const { document, draft, model } = options
  const limits = options.limits ?? { tasks: 100, attention: 50 }
  const readOnly = options.readOnly === true

  const root = options.root
  root.className = 'wwc-strongflow-review-draft'
  root.setAttribute('aria-label', 'Staged review notes')

  const heading = strongFlowElement(document, 'h4', 'wwc-strongflow-review-draft-heading')
  heading.textContent = 'Staged review notes'
  const hint = strongFlowElement(document, 'p', 'wwc-strongflow-review-draft-hint')
  hint.textContent = 'Notes stay in this browser until one review command carries them.'

  const list = strongFlowElement(document, 'ul', 'wwc-strongflow-review-draft-list')

  const kindLabel = document.createElement('label')
  const kind = document.createElement('select')
  for (const value of ANCHOR_KINDS) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = anchorKindLabel(value)
    kind.append(option)
  }
  kind.className = 'wwc-strongflow-review-draft-kind'
  kind.value = 'file-line'
  kindLabel.textContent = 'Reviewed target kind'
  kindLabel.append(kind)

  const anchorField = document.createElement('label')
  const anchor = document.createElement('input')
  anchor.className = 'wwc-strongflow-review-draft-anchor'
  anchor.type = 'text'
  anchorField.textContent = 'Changed path'
  anchorField.append(anchor)

  const lineLabel = document.createElement('label')
  const line = document.createElement('input')
  line.className = 'wwc-strongflow-review-draft-line'
  line.type = 'number'
  line.min = '1'
  line.value = '1'
  lineLabel.textContent = 'Line'
  lineLabel.append(line)

  const noteLabel = document.createElement('label')
  const note = document.createElement('textarea')
  note.className = 'wwc-strongflow-review-draft-note'
  noteLabel.textContent = 'Note'
  noteLabel.append(note)

  const add = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-review-draft-add',
  ) as HTMLButtonElement
  add.type = 'button'
  add.textContent = 'Stage note'

  const summaryButton = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-review-draft-summary-button',
  ) as HTMLButtonElement
  summaryButton.type = 'button'
  summaryButton.textContent = 'Summarize notes'
  const summary = strongFlowElement(document, 'ul', 'wwc-strongflow-review-draft-summary')

  const targetLabel = document.createElement('label')
  const target = document.createElement('select')
  for (const entry of TARGET_LABELS) {
    const option = document.createElement('option')
    option.value = entry.value
    option.textContent = entry.label
    target.append(option)
  }
  target.className = 'wwc-strongflow-review-draft-target'
  target.value = 'attention-resolution'
  targetLabel.textContent = 'Compose into'
  targetLabel.append(target)

  const attentionLabel = document.createElement('label')
  const attention = document.createElement('select')
  attention.className = 'wwc-strongflow-review-draft-attention'
  attentionLabel.textContent = 'Open Attention'
  attentionLabel.append(attention)

  const nodeLabel = document.createElement('label')
  const node = document.createElement('select')
  node.className = 'wwc-strongflow-review-draft-node'
  nodeLabel.textContent = 'Solution node'
  nodeLabel.append(node)

  const taskLabel = document.createElement('label')
  const task = document.createElement('select')
  task.className = 'wwc-strongflow-review-draft-task'
  taskLabel.textContent = 'Delivery task'
  taskLabel.append(task)

  const commentsLabel = document.createElement('label')
  const comments = document.createElement('textarea')
  commentsLabel.textContent = 'Comment for requested changes'
  commentsLabel.append(comments)

  const reworkFields = strongFlowElement(document, 'div', 'wwc-strongflow-review-draft-rework')
  reworkFields.append(nodeLabel, taskLabel)

  const submit = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-review-draft-submit',
  ) as HTMLButtonElement
  submit.type = 'button'
  submit.textContent = 'Submit staged notes'

  const staleBanner = strongFlowElement(document, 'div', 'wwc-strongflow-review-draft-stale')
  staleBanner.setAttribute('role', 'alert')
  const staleText = strongFlowElement(document, 'p', 'wwc-strongflow-review-draft-stale-text')
  const confirmAll = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-review-draft-confirm',
  ) as HTMLButtonElement
  confirmAll.type = 'button'
  confirmAll.textContent = 'Re-confirm on current candidate'
  staleBanner.append(staleText, confirmAll)

  const failure = strongFlowElement(document, 'p', 'wwc-strongflow-review-draft-failure')
  failure.setAttribute('role', 'alert')

  // The final bounded scope is rendered before submit, so the reviewer always
  // sees exactly what one command will carry and for which Candidate.
  const scopeHeading = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-draft-scope-heading',
  )
  scopeHeading.textContent = 'Final scope for this submission'
  const scope = strongFlowElement(document, 'ul', 'wwc-strongflow-review-draft-scope')
  const scopeBlocker = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-draft-scope-blocker',
  )

  const rows = new WeakMap<HTMLElement, ReviewDraftPanelRow>()
  const rowCollection = mountKeyedCollection({
    parent: list,
    key: (item: ReviewDraftRow) => item.annotation.id,
    create(item: ReviewDraftRow) {
      const item_ = strongFlowElement(document, 'li', 'wwc-strongflow-review-draft-row')
      const label = strongFlowElement(document, 'p', 'wwc-strongflow-review-draft-anchor-label')
      const noteText = strongFlowElement(document, 'p', 'wwc-strongflow-review-draft-note-text')
      const staleNote = strongFlowElement(document, 'p', 'wwc-strongflow-review-draft-stale-note')
      const confirm = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-review-draft-row-confirm',
      ) as HTMLButtonElement
      const discard = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-review-draft-row-discard',
      ) as HTMLButtonElement
      const edit = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-review-draft-edit',
      ) as HTMLButtonElement
      const editor = document.createElement('textarea')
      editor.className = 'wwc-strongflow-review-draft-note-input'
      const save = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-review-draft-save',
      ) as HTMLButtonElement
      const remove = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-review-draft-remove',
      ) as HTMLButtonElement
      const row: ReviewDraftPanelRow = {
        current: item,
        item: item_,
        label,
        note: noteText,
        staleNote,
        confirm,
        discard,
        edit,
        editor,
        save,
        remove,
        onEdit: () => {
          editing = row.current.annotation.id
          update()
        },
        onSave: () => {
          if (apply(() => draft.update(row.current.annotation.id, editor.value))) editing = null
          update()
        },
        onRemove: () => {
          apply(() => draft.remove(row.current.annotation.id))
          update()
        },
        onConfirm: () => {
          apply(() => draft.confirm(row.current.annotation.id))
          update()
        },
        onDiscard: () => {
          apply(() => draft.discard(row.current.annotation.id))
          update()
        },
      }
      confirm.type = 'button'
      confirm.textContent = 'Re-confirm'
      confirm.addEventListener('click', row.onConfirm)
      discard.type = 'button'
      discard.textContent = 'Discard'
      discard.addEventListener('click', row.onDiscard)
      edit.type = 'button'
      edit.textContent = 'Edit'
      edit.addEventListener('click', row.onEdit)
      save.type = 'button'
      save.textContent = 'Save'
      save.addEventListener('click', row.onSave)
      remove.type = 'button'
      remove.textContent = 'Remove'
      remove.addEventListener('click', row.onRemove)
      item_.append(label, noteText, staleNote, confirm, discard, edit, editor, save, remove)
      rows.set(item_, row)
      return item_
    },
    update(node, item) {
      const row = rows.get(node)
      if (row === undefined) return
      row.current = item
      const annotation = item.annotation
      row.label.textContent = `${anchorKindNoun(annotation.kind)} · `
        + strongFlowReviewAnchorLabel(annotation.anchor)
      row.note.textContent = annotation.note
      row.staleNote.textContent = item.stale === null
        ? ''
        : item.stale.reason === 'candidate-changed'
          ? 'The candidate changed after this note was staged.'
          : 'The Delivery revision changed after this note was staged.'
      row.staleNote.hidden = item.stale === null
      row.confirm.hidden = item.stale === null
      row.discard.hidden = item.stale === null
      row.edit.hidden = item.editing || item.locked
      row.remove.hidden = item.locked
      row.save.hidden = !item.editing
      row.editor.hidden = !item.editing
      if (item.editing && row.editor.value !== annotation.note) {
        row.editor.value = annotation.note
      }
      row.confirm.disabled = item.locked
      row.discard.disabled = item.locked
      row.edit.disabled = item.locked
      row.save.disabled = item.locked
      row.remove.disabled = item.locked
    },
    remove(node) {
      const row = rows.get(node)
      if (row === undefined) return
      node.removeEventListener('click', row.onConfirm)
      node.removeEventListener('click', row.onDiscard)
      node.removeEventListener('click', row.onEdit)
      node.removeEventListener('click', row.onSave)
      node.removeEventListener('click', row.onRemove)
    },
  })

  let editing: string | null = null
  let lastContextKey = ''
  const unsubscribe = model.subscribe(() => { update() })

  function apply(action: () => void): boolean {
    try {
      action()
      setFailure(null)
      return true
    } catch (error) {
      setFailure(error instanceof StrongFlowReviewDraftError
        ? error.message
        : 'That staged note could not be updated.')
      return false
    }
  }

  function setFailure(message: string | null): void {
    failure.textContent = message ?? ''
    failure.hidden = message === null
  }

  function fillSelect(
    select: HTMLSelectElement,
    values: readonly { readonly value: string; readonly label: string }[],
  ): string {
    const offered = values.length > 0 ? values : [{ value: '', label: 'Nothing available' }]
    const selected = offered.some(entry => entry.value === select.value)
      ? select.value
      : offered[0]!.value
    select.replaceChildren()
    for (const entry of offered) {
      const option = document.createElement('option')
      option.value = entry.value
      option.textContent = entry.label
      select.append(option)
    }
    select.value = selected
    return selected
  }

  function activeTarget(): StrongFlowReviewTarget {
    return TARGET_LABELS.find(entry => entry.value === target.value)?.value
      ?? 'attention-resolution'
  }

  function pageLocked(): boolean {
    return readOnly || (options.isHistoricalReviewOpen?.() ?? false)
  }

  function anchorFromForm(): StrongFlowReviewAnnotationAnchor {
    const raw = anchor.value.trim()
    switch (kind.value) {
      case 'task': return { kind: 'task', deliveryTaskId: raw }
      case 'solution-node': return { kind: 'solution-node', nodeId: raw }
      case 'criterion': return { kind: 'criterion', criterionId: raw }
      default: return { kind: 'file-line', path: raw, line: Number(line.value) }
    }
  }

  function onKindChange(): void {
    anchorField.textContent = kind.value === 'file-line'
      ? 'Changed path'
      : kind.value === 'task'
        ? 'Task id'
        : kind.value === 'solution-node'
          ? 'Solution node id'
          : 'Acceptance criterion id'
    anchorField.append(anchor)
    update()
  }

  function onAdd(): void {
    apply(() => {
      draft.add({ anchor: anchorFromForm(), note: note.value })
      note.value = ''
    })
    update()
  }

  function onSummarize(): void {
    summary.replaceChildren()
    for (const line_ of draft.summarize()) {
      const item_ = document.createElement('li')
      item_.textContent = line_
      summary.append(item_)
    }
    summary.hidden = draft.state.annotations.length === 0
    summaryButton.textContent = summary.hidden ? 'Summarize notes' : 'Refresh summary'
  }

  function onConfirmAll(): void {
    apply(() => {
      for (const entry of [...draft.state.staleness]) draft.confirm(entry.id)
    })
    update()
  }

  /** Settle one submission on view-model evidence only. */
  function settle(): void {
    const interaction = model.state.interaction ?? { status: 'idle', error: null }
    if (interaction.status === 'error') {
      draft.settle(interaction.error?.kind === 'cancelled' ? 'cancelled' : 'failure')
      return
    }
    // `runCommand` leaves the interaction idle again once the accepted command
    // has been applied and the following snapshot published, so an idle
    // interaction after our own round trip is the success evidence.
    if (interaction.status === 'idle') draft.settle('success')
  }

  function onSubmit(): void {
    const state = model.state
    if (state.projection === null || pageLocked()) return
    const plan = (() => {
      try {
        return draft.compose({
          target: activeTarget(),
          attentionItemId: attention.value,
          nodeId: node.value,
          deliveryTaskId: task.value.length === 0 ? null : task.value,
          comments: comments.value,
        })
      } catch (error) {
        setFailure(error instanceof StrongFlowReviewDraftError
          ? error.message
          : 'These staged notes cannot be composed into a review command.')
        return null
      }
    })()
    if (plan === null) {
      update()
      return
    }
    setFailure(null)
    draft.begin(plan)
    update()
    void (plan.solutionReview !== null
      ? model.decideSolutionReview(plan.solutionReview)
      : model.resolveAttention(plan.attention!)
    ).then(() => {
      settle()
      update()
    })
  }

  function refreshChoices(state: StrongFlowViewModelState): void {
    const projection = state.projection
    fillSelect(
      attention,
      boundedItems(
        (projection?.attention ?? []).filter(item => item.status === 'open'),
        limits.attention,
      ).items.map(item => ({ value: item.id, label: item.title })),
    )
    fillSelect(
      task,
      [
        { value: '', label: 'No specific Task' },
        ...boundedItems(projection?.delivery.tasks ?? [], limits.tasks)
          .items.map(item => ({ value: item.id, label: item.title })),
      ],
    )
    const review = projection?.solutionReview ?? null
    fillSelect(
      node,
      [
        ...(review?.architectureDiagram.nodes ?? []),
        ...(review?.processDiagram.nodes ?? []),
      ].map(item => ({ value: item.id, label: item.label })),
    )
  }

  function refreshScope(staleCount: number): void {
    const draftState = draft.state
    const annotationCount = draftState.annotations.length
    if (annotationCount === 0 || staleCount > 0 || pageLocked()) {
      scopeBlocker.textContent = annotationCount === 0
        ? 'Stage a note to see what one review command will carry.'
        : 'Resolve the stale notes to see the final scope.'
      scopeBlocker.hidden = false
      scope.hidden = true
      scope.replaceChildren()
      return
    }
    try {
      const plan = draft.compose({
        target: activeTarget(),
        attentionItemId: attention.value,
        nodeId: node.value,
        deliveryTaskId: task.value.length === 0 ? null : task.value,
        comments: comments.value,
      })
      scope.replaceChildren()
      for (const line_ of plan.summary) {
        const item_ = document.createElement('li')
        item_.textContent = line_
        scope.append(item_)
      }
      scope.hidden = false
      scopeBlocker.hidden = true
    } catch (error) {
      scope.replaceChildren()
      scope.hidden = true
      scopeBlocker.textContent = error instanceof StrongFlowReviewDraftError
        ? error.message
        : 'These notes cannot be composed into one review command yet.'
      scopeBlocker.hidden = false
    }
  }

  function update(): void {
    const state = model.state
    draft.synchronize(state.projection)
    const draftState = draft.state
    const locked = pageLocked() || draftState.submission !== null
    const staleIds = new Set(draftState.staleness.map(entry => entry.id))
    rowCollection.update(boundedItems([...draftState.annotations], MAX_STAGED_NOTES)
      .items.map(annotation => ({
        annotation,
        stale: staleIds.has(annotation.id)
          ? draftState.staleness.find(entry => entry.id === annotation.id) ?? null
          : null,
        editing: editing === annotation.id,
        locked,
      })))

    const target_ = activeTarget()
    const contextKey = [
      draftState.identity?.deliveryId ?? '',
      String(draftState.identity?.deliveryRevision ?? 0),
      draftState.identity?.candidateDigest ?? '',
      String(draftState.annotations.length),
      target_,
      String(pageLocked()),
    ].join(':')
    if (contextKey !== lastContextKey) {
      lastContextKey = contextKey
      refreshChoices(state)
    }

    const staleCount = draftState.staleness.length
    staleBanner.hidden = staleCount === 0
    if (staleCount > 0) {
      staleText.textContent = `${String(staleCount)} of ${String(draftState.annotations.length)}`
        + ' staged notes are stale: the candidate changed or the Delivery revision moved.'
        + ' Re-confirm them on the current candidate, or discard them.'
    }

    refreshScope(staleCount)
    const busy = state.status === 'loading'
      || state.status === 'refreshing'
      || state.realtime === 'reloading'
      || state.interaction?.status === 'submitting'
      || state.interaction?.status === 'waiting'
    const hasNotes = draftState.annotations.length > 0
    const targetReady = target_ === 'requested-changes'
      ? state.projection?.solutionReview?.reviewStatus === 'pending'
      : attention.value.length > 0 && (target_ !== 'bounded-rework' || node.value.length > 0)
    const disabled = locked || busy
    add.disabled = disabled
    submit.disabled = disabled || !hasNotes || staleCount > 0 || !targetReady
    kind.disabled = disabled
    anchor.disabled = disabled
    line.disabled = disabled
    note.disabled = disabled
    target.disabled = disabled
    attention.disabled = disabled
    node.disabled = disabled
    task.disabled = disabled
    comments.disabled = disabled
    summaryButton.disabled = disabled
    confirmAll.disabled = disabled
    reworkFields.hidden = target_ !== 'bounded-rework'
    commentsLabel.hidden = target_ !== 'requested-changes'
    lineLabel.hidden = kind.value !== 'file-line'
  }

  root.append(
    heading,
    hint,
    staleBanner,
    list,
    summaryButton,
    summary,
    scopeHeading,
    scope,
    scopeBlocker,
    kindLabel,
    anchorField,
    lineLabel,
    noteLabel,
    add,
    targetLabel,
    attentionLabel,
    reworkFields,
    commentsLabel,
    submit,
    failure,
  )
  staleBanner.hidden = true
  summary.hidden = true
  scope.hidden = true
  scopeBlocker.hidden = true
  failure.hidden = true
  reworkFields.hidden = true
  commentsLabel.hidden = true
  lineLabel.hidden = true
  kind.addEventListener('change', onKindChange)
  anchor.addEventListener('input', onKindChange)
  add.addEventListener('click', onAdd)
  summaryButton.addEventListener('click', onSummarize)
  target.addEventListener('change', update)
  attention.addEventListener('change', update)
  node.addEventListener('change', update)
  task.addEventListener('change', update)
  submit.addEventListener('click', onSubmit)
  confirmAll.addEventListener('click', onConfirmAll)
  update()

  return {
    root,
    update,
    close() {
      unsubscribe()
      kind.removeEventListener('change', onKindChange)
      anchor.removeEventListener('input', onKindChange)
      add.removeEventListener('click', onAdd)
      summaryButton.removeEventListener('click', onSummarize)
      target.removeEventListener('change', update)
      attention.removeEventListener('change', update)
      node.removeEventListener('change', update)
      task.removeEventListener('change', update)
      submit.removeEventListener('click', onSubmit)
      confirmAll.removeEventListener('click', onConfirmAll)
      rowCollection.close()
      root.replaceChildren()
    },
  }
}
