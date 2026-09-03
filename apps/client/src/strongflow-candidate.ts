// SPDX-License-Identifier: Apache-2.0

import type { EvidenceId } from './generated/contracts.js'
import type { StrongFlowProjection } from './strongflow-view-model.js'
import { strongFlowElement } from './strongflow-rendering.js'

export interface StrongFlowCandidateOptions {
  readonly onOpenEvidence: (evidenceId: EvidenceId) => void
}

function appendEvidenceButtons(
  document: Document,
  parent: HTMLElement,
  evidenceIds: readonly EvidenceId[],
  options: StrongFlowCandidateOptions,
  className: string,
): void {
  for (const evidenceId of evidenceIds) {
    const button = strongFlowElement(document, 'button', className) as HTMLButtonElement
    button.type = 'button'
    button.dataset.evidenceId = evidenceId
    button.textContent = `Open Evidence ${evidenceId}`
    button.addEventListener('click', () => { options.onOpenEvidence(evidenceId) })
    parent.append(button)
  }
}

function definition(
  document: Document,
  termText: string,
  valueText: string,
): readonly [HTMLElement, HTMLElement] {
  const term = document.createElement('dt')
  const value = document.createElement('dd')
  term.textContent = termText
  value.textContent = valueText
  return [term, value]
}

export function renderStrongFlowCandidate(
  document: Document,
  projection: StrongFlowProjection,
  options: StrongFlowCandidateOptions,
): HTMLElement {
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-view-candidate')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const candidate = projection.currentCandidate
  root.dataset.view = 'frozen-candidate'
  heading.textContent = 'Frozen candidate and Diff'
  root.append(heading)
  if (candidate === null) {
    const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
    empty.textContent = 'No candidate has been frozen.'
    root.append(empty)
  } else {
    const details = strongFlowElement(document, 'dl', 'wwc-strongflow-candidate-details')
    details.append(
      ...definition(document, 'Candidate', candidate.candidateRef),
      ...definition(document, 'Commit', candidate.candidateCommitId),
      ...definition(document, 'Tree', candidate.candidateTreeId),
      ...definition(document, 'Diff digest', candidate.diffSha256),
      ...definition(document, 'Frozen', candidate.frozenAt),
    )
    root.append(details)
    const candidateEvidence = projection.evidence
      .filter(row => row.candidateRef === candidate.candidateRef)
      .map(row => row.id)
    if (candidateEvidence.length > 0) {
      const evidence = strongFlowElement(document, 'div', 'wwc-strongflow-candidate-evidence')
      appendEvidenceButtons(
        document,
        evidence,
        candidateEvidence,
        options,
        'wwc-strongflow-candidate-evidence-open',
      )
      root.append(evidence)
    }
  }

  const verdictHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const verdict = strongFlowElement(document, 'section', 'wwc-strongflow-verdict')
  verdictHeading.textContent = 'Verdict'
  verdict.append(verdictHeading)
  if (projection.verdict === null) {
    const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
    empty.textContent = 'No current Verdict is available.'
    verdict.append(empty)
  } else {
    const status = document.createElement('strong')
    const produced = document.createElement('p')
    status.textContent = projection.verdict.status
    produced.textContent = `Produced ${projection.verdict.producedAt}`
    verdict.dataset.status = projection.verdict.status
    verdict.append(status, produced)
    const criteria = strongFlowElement(document, 'ul', 'wwc-strongflow-criterion-results')
    const currentEvidenceIds = new Set(projection.evidence.map(row => row.id))
    for (const criterion of projection.verdict.criteria) {
      const item = document.createElement('li')
      const summary = document.createElement('span')
      summary.textContent = `${criterion.criterionId} · ${criterion.verdict}`
      item.dataset.criterionId = criterion.criterionId
      item.dataset.verdict = criterion.verdict
      item.append(summary)
      appendEvidenceButtons(
        document,
        item,
        criterion.evidenceRefs.filter(evidenceId => currentEvidenceIds.has(evidenceId)),
        options,
        'wwc-strongflow-criterion-evidence-open',
      )
      criteria.append(item)
    }
    verdict.append(criteria)
  }
  root.append(verdict)

  const publicationHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const publication = strongFlowElement(document, 'section', 'wwc-strongflow-publication')
  publicationHeading.textContent = 'Publication'
  publication.append(publicationHeading)
  if (projection.publication === null) {
    const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
    empty.textContent = 'No Publication has been created.'
    publication.append(empty)
  } else {
    const status = document.createElement('strong')
    const revision = document.createElement('p')
    status.textContent = projection.publication.state
    revision.textContent = `Revision ${String(projection.publication.revision)} · updated ${
      projection.publication.updatedAt
    }`
    publication.dataset.status = projection.publication.state
    publication.append(status, revision)
  }
  root.append(publication)
  return root
}
