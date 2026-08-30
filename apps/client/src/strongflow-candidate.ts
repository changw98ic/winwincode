// SPDX-License-Identifier: Apache-2.0

import type { StrongFlowProjection } from './strongflow-view-model.js'
import {
  appendOmittedCount,
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

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
  limits: StrongFlowRenderLimits,
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
  }

  const evidenceHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const evidence = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence')
  const boundedEvidence = boundedItems(projection.evidence, limits.evidence)
  evidenceHeading.textContent = 'Evidence'
  evidence.setAttribute('aria-label', 'Delivery evidence')
  evidence.append(...boundedEvidence.items.map(item => {
    const row = document.createElement('li')
    const title = document.createElement('strong')
    const source = document.createElement('p')
    title.textContent = `${item.type} · ${item.id}`
    source.textContent = item.sourceRef
    row.dataset.candidateRef = item.candidateRef
    row.append(title, source)
    return row
  }))
  root.append(evidenceHeading, evidence)
  appendOmittedCount(document, root, boundedEvidence.omitted, 'evidence records')

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
