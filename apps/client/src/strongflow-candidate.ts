// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { assertMounted, removeNode } from './components/mounted-view.js'
import type {
  DeliveryCriterionResultProjection,
  EvidenceId,
} from './generated/contracts.js'
import type { StrongFlowProjection } from './strongflow-view-model.js'
import { strongFlowElement } from './strongflow-rendering.js'

export interface StrongFlowCandidateOptions {
  readonly onOpenEvidence: (evidenceId: EvidenceId) => void
}

export interface StrongFlowCandidateView {
  readonly root: HTMLElement
  update(projection: StrongFlowProjection): void
  close(): void
}

function mountEvidenceButtons(
  document: Document,
  parent: HTMLElement,
  options: StrongFlowCandidateOptions,
  className: string,
): KeyedCollectionView<EvidenceId, string, HTMLButtonElement> {
  const listeners = new WeakMap<HTMLButtonElement, () => void>()
  return mountKeyedCollection({
    parent,
    key: evidenceId => evidenceId,
    create() {
      const button = strongFlowElement(document, 'button', className) as HTMLButtonElement
      const onClick = () => {
        const evidenceId = button.dataset.evidenceId
        if (evidenceId !== undefined) options.onOpenEvidence(evidenceId as EvidenceId)
      }
      button.type = 'button'
      button.addEventListener('click', onClick)
      listeners.set(button, onClick)
      return button
    },
    update(button, evidenceId) {
      button.dataset.evidenceId = evidenceId
      button.textContent = `Open Evidence ${evidenceId}`
    },
    remove(button) {
      const onClick = listeners.get(button)
      if (onClick !== undefined) button.removeEventListener?.('click', onClick)
      listeners.delete(button)
    },
  })
}

function definition(
  document: Document,
  termText: string,
): readonly [HTMLElement, HTMLElement] {
  const term = document.createElement('dt')
  const value = document.createElement('dd')
  term.textContent = termText
  return [term, value]
}

interface CriterionItem {
  readonly criterion: DeliveryCriterionResultProjection
  readonly evidenceIds: readonly EvidenceId[]
}

interface CriterionRow {
  readonly summary: HTMLElement
  readonly evidence: HTMLElement
  readonly evidenceButtons: KeyedCollectionView<EvidenceId, string, HTMLButtonElement>
}

/** Mount the Candidate, Verdict, and Publication projection with keyed Evidence entry controls. */
export function mountStrongFlowCandidate(
  document: Document,
  projection: StrongFlowProjection,
  options: StrongFlowCandidateOptions,
): StrongFlowCandidateView {
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-view-candidate')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const candidateEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const details = strongFlowElement(document, 'dl', 'wwc-strongflow-candidate-details')
  const candidateEvidence = strongFlowElement(document, 'div', 'wwc-strongflow-candidate-evidence')
  const [candidateTerm, candidateValue] = definition(document, 'Candidate')
  const [commitTerm, commitValue] = definition(document, 'Commit')
  const [treeTerm, treeValue] = definition(document, 'Tree')
  const [digestTerm, digestValue] = definition(document, 'Diff digest')
  const [frozenTerm, frozenValue] = definition(document, 'Frozen')

  const verdict = strongFlowElement(document, 'section', 'wwc-strongflow-verdict')
  const verdictHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const verdictEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const verdictStatus = document.createElement('strong')
  const verdictProduced = document.createElement('p')
  const criteria = strongFlowElement(document, 'ul', 'wwc-strongflow-criterion-results')

  const publication = strongFlowElement(document, 'section', 'wwc-strongflow-publication')
  const publicationHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const publicationEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const publicationStatus = document.createElement('strong')
  const publicationRevision = document.createElement('p')
  let open = true

  root.dataset.view = 'frozen-candidate'
  heading.textContent = 'Frozen candidate and Diff'
  candidateEmpty.textContent = 'No candidate has been frozen.'
  details.append(
    candidateTerm,
    candidateValue,
    commitTerm,
    commitValue,
    treeTerm,
    treeValue,
    digestTerm,
    digestValue,
    frozenTerm,
    frozenValue,
  )
  verdictHeading.textContent = 'Verdict'
  verdictEmpty.textContent = 'No current Verdict is available.'
  verdict.append(verdictHeading, verdictEmpty, verdictStatus, verdictProduced, criteria)
  publicationHeading.textContent = 'Publication'
  publicationEmpty.textContent = 'No Publication has been created.'
  publication.append(
    publicationHeading,
    publicationEmpty,
    publicationStatus,
    publicationRevision,
  )
  root.append(heading, candidateEmpty, details, candidateEvidence, verdict, publication)

  const candidateEvidenceButtons = mountEvidenceButtons(
    document,
    candidateEvidence,
    options,
    'wwc-strongflow-candidate-evidence-open',
  )
  const criterionRows = new WeakMap<HTMLLIElement, CriterionRow>()
  const criterionCollection = mountKeyedCollection<CriterionItem, string, HTMLLIElement>({
    parent: criteria,
    key: item => item.criterion.criterionId,
    create() {
      const item = document.createElement('li')
      const summary = document.createElement('span')
      const evidence = strongFlowElement(document, 'span', 'wwc-strongflow-criterion-evidence')
      const evidenceButtons = mountEvidenceButtons(
        document,
        evidence,
        options,
        'wwc-strongflow-criterion-evidence-open',
      )
      item.append(summary, evidence)
      criterionRows.set(item, { summary, evidence, evidenceButtons })
      return item
    },
    update(item, value) {
      const row = criterionRows.get(item)
      if (row === undefined) return
      item.dataset.criterionId = value.criterion.criterionId
      item.dataset.verdict = value.criterion.verdict
      row.summary.textContent = `${value.criterion.criterionId} · ${value.criterion.verdict}`
      row.evidenceButtons.update(value.evidenceIds)
      row.evidence.hidden = value.evidenceIds.length === 0
    },
    remove(item) {
      const row = criterionRows.get(item)
      row?.evidenceButtons.close()
      criterionRows.delete(item)
    },
  })

  function update(next: StrongFlowProjection): void {
    assertMounted(open, 'StrongFlowCandidate')
    const candidate = next.currentCandidate
    candidateEmpty.hidden = candidate !== null
    details.hidden = candidate === null
    candidateEvidence.hidden = candidate === null
    if (candidate === null) {
      candidateEvidenceButtons.update([])
      candidateValue.textContent = ''
      commitValue.textContent = ''
      treeValue.textContent = ''
      digestValue.textContent = ''
      frozenValue.textContent = ''
    } else {
      candidateValue.textContent = candidate.candidateRef
      commitValue.textContent = candidate.candidateCommitId
      treeValue.textContent = candidate.candidateTreeId
      digestValue.textContent = candidate.diffSha256
      frozenValue.textContent = candidate.frozenAt
      const evidenceIds = next.evidence
        .filter(row => row.candidateRef === candidate.candidateRef)
        .map(row => row.id)
      candidateEvidenceButtons.update(evidenceIds)
      candidateEvidence.hidden = evidenceIds.length === 0
    }

    const currentEvidenceIds = new Set(next.evidence.map(row => row.id))
    const currentVerdict = next.verdict
    verdictEmpty.hidden = currentVerdict !== null
    verdictStatus.hidden = currentVerdict === null
    verdictProduced.hidden = currentVerdict === null
    criteria.hidden = currentVerdict === null
    if (currentVerdict === null) {
      delete verdict.dataset.status
      verdictStatus.textContent = ''
      verdictProduced.textContent = ''
      criterionCollection.update([])
    } else {
      verdict.dataset.status = currentVerdict.status
      verdictStatus.textContent = currentVerdict.status
      verdictProduced.textContent = `Produced ${currentVerdict.producedAt}`
      criterionCollection.update(currentVerdict.criteria.map(criterion => ({
        criterion,
        evidenceIds: criterion.evidenceRefs.filter(evidenceId => currentEvidenceIds.has(evidenceId)),
      })))
    }

    const currentPublication = next.publication
    publicationEmpty.hidden = currentPublication !== null
    publicationStatus.hidden = currentPublication === null
    publicationRevision.hidden = currentPublication === null
    if (currentPublication === null) {
      delete publication.dataset.status
      publicationStatus.textContent = ''
      publicationRevision.textContent = ''
    } else {
      publication.dataset.status = currentPublication.state
      publicationStatus.textContent = currentPublication.state
      publicationRevision.textContent = `Revision ${String(currentPublication.revision)} · updated ${
        currentPublication.updatedAt
      }`
    }
  }

  update(projection)
  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      candidateEvidenceButtons.close()
      criterionCollection.close()
      removeNode(root)
    },
  }
}
