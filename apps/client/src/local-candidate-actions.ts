// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneCandidateSummary } from './control-plane-client.js'
import {
  candidateDisplayState,
  candidateDisplayStateText,
  candidateDisplayStateTone,
  candidateResultText,
  candidateResultTone,
  candidateSupportsApply,
  candidateSupportsBranch,
  candidateSupportsDiscard,
  shortCommitText,
  type LocalCandidateAction,
  type LocalCandidateInteraction,
  type LocalCandidateViewModel,
} from './local-candidate-view-model.js'

export interface LocalCandidateCardOptions {
  readonly document: Document
  /** The shared candidate interaction model behind every mounted card. */
  readonly model: LocalCandidateViewModel
  /** Deterministic injection seam; production defaults to the browser Blob URL. */
  readonly createObjectUrl?: (text: string) => string
  /** Deterministic injection seam; production defaults to the browser revoke. */
  readonly revokeObjectUrl?: (url: string) => void
}

export interface LocalCandidateCard {
  readonly root: HTMLElement
  /** Re-render the card for its candidate's current projection. */
  update(candidate: ControlPlaneCandidateSummary): void
  close(): void
}

/** Why discarding is dangerous, and what confirming means. */
const DISCARD_COPY = 'Discarding removes the retained candidate ref on the device. '
  + 'The candidate can no longer be applied or recovered.'

/**
 * The one copy per candidate action failure; every entry also reaches the
 * screen reader through the alert role of the failure line.
 */
function failureText(failure: LocalCandidateInteraction & { readonly kind: 'failed' }): string {
  switch (failure.failure) {
    case 'invalid-request': return 'The apply request was incomplete. Check the target branch and expected HEAD.'
    case 'candidate-not-found': return 'This candidate no longer exists on the device.'
    case 'client-not-found': return 'The Client device is no longer connected to this account.'
    case 'client-offline': return 'The device is offline right now.'
    case 'permission-denied': return 'You no longer have permission for this repository.'
    case 'wrong-state': return 'The candidate changed on the Server. Refresh and try again.'
    case 'rate-limited': return 'Too many attempts. Wait a moment, then try again.'
    case 'unavailable': return 'The request did not go through. Check the connection and try again.'
  }
}

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

/**
 * Mount one candidate status card: the ref and short commit identity, the
 * state badge, the result history, the branch, apply, and discard entries,
 * and the explicit confirmation plus failure copy for the dangerous paths.
 * The module owns DOM and ARIA only; every click translates into one
 * view-model intent, and every capability comes from the Server projection.
 */
export function mountLocalCandidateCard(options: LocalCandidateCardOptions): LocalCandidateCard {
  const document = options.document
  let currentRef = ''
  let currentCandidate: ControlPlaneCandidateSummary | null = null
  let lastInteractionKey = 'rest'
  let historySignature: string | null = null
  let objectUrl: string | null = null
  let closed = false

  const createObjectUrl = options.createObjectUrl
    ?? ((text: string) => URL.createObjectURL(new Blob([text], { type: 'text/plain' })))
  const revokeObjectUrl = options.revokeObjectUrl ?? ((url: string) => URL.revokeObjectURL(url))

  const card = element(document, 'article', 'wwc-candidate-card')
  const stateBadge = element(document, 'span', 'wwc-candidate-card-state')
  const refLine = element(document, 'p', 'wwc-candidate-card-ref')
  const commitLine = element(document, 'p', 'wwc-candidate-card-commit')
  const branchLine = element(document, 'p', 'wwc-candidate-card-branch')

  const actions = element(document, 'div', 'wwc-candidate-card-actions')
  const branchCreate = element(document, 'button', 'wwc-candidate-card-branch-create')
  const apply = element(document, 'button', 'wwc-candidate-card-apply')
  const discard = element(
    document,
    'button',
    'wwc-candidate-card-discard wwc-candidate-card-danger',
  )
  const download = element(document, 'button', 'wwc-candidate-card-conflict-download')

  const notice = element(document, 'div', 'wwc-candidate-card-notice')

  const applyConfirm = element(document, 'div', 'wwc-candidate-card-confirm-apply')
  const applyConfirmText = element(document, 'p', 'wwc-candidate-card-confirm-apply-text')
  const applyBranchLabel = element(document, 'label', 'wwc-candidate-card-apply-label')
  const applyBranchInput = element(document, 'input', 'wwc-candidate-card-apply-branch')
  const applyHeadLabel = element(document, 'label', 'wwc-candidate-card-apply-label')
  const applyHeadInput = element(document, 'input', 'wwc-candidate-card-apply-head')
  const applyAccept = element(
    document,
    'button',
    'wwc-candidate-card-apply-accept wwc-candidate-card-danger',
  )
  const applyKeep = element(document, 'button', 'wwc-candidate-card-apply-keep')

  const discardConfirm = element(document, 'div', 'wwc-candidate-card-confirm-discard')
  const discardConfirmText = element(document, 'p', 'wwc-candidate-card-confirm-discard-text')
  const discardAccept = element(
    document,
    'button',
    'wwc-candidate-card-discard-accept wwc-candidate-card-danger',
  )
  const discardKeep = element(document, 'button', 'wwc-candidate-card-discard-keep')

  const failure = element(document, 'p', 'wwc-candidate-card-error')
  failure.setAttribute('role', 'alert')

  const history = element(document, 'div', 'wwc-candidate-card-history')

  branchCreate.type = 'button'
  branchCreate.textContent = 'Create branch'
  apply.type = 'button'
  apply.textContent = 'Apply…'
  discard.type = 'button'
  discard.textContent = 'Discard'
  download.type = 'button'
  download.textContent = 'Download conflict summary'
  download.hidden = true

  applyBranchInput.id = 'wwc-candidate-card-apply-branch'
  applyBranchInput.name = 'targetBranch'
  applyBranchInput.type = 'text'
  applyBranchInput.autocomplete = 'off'
  applyBranchInput.spellcheck = false
  applyBranchLabel.htmlFor = applyBranchInput.id
  applyBranchLabel.textContent = 'Target branch'
  applyHeadInput.id = 'wwc-candidate-card-apply-head'
  applyHeadInput.name = 'expectedHead'
  applyHeadInput.type = 'text'
  applyHeadInput.autocomplete = 'off'
  applyHeadInput.spellcheck = false
  applyHeadLabel.htmlFor = applyHeadInput.id
  applyHeadLabel.textContent = 'Expected HEAD'
  applyAccept.type = 'button'
  applyAccept.textContent = 'Apply to branch'
  applyKeep.type = 'button'
  applyKeep.textContent = 'Keep candidate'
  applyConfirm.hidden = true

  discardAccept.type = 'button'
  discardAccept.textContent = 'Discard candidate'
  discardKeep.type = 'button'
  discardKeep.textContent = 'Keep candidate'
  discardConfirm.hidden = true

  failure.hidden = true
  history.hidden = true

  applyConfirm.append(
    applyConfirmText,
    applyBranchLabel,
    applyBranchInput,
    applyHeadLabel,
    applyHeadInput,
    applyAccept,
    applyKeep,
  )
  discardConfirm.append(discardConfirmText, discardAccept, discardKeep)
  actions.append(branchCreate, apply, discard, download)
  notice.append(applyConfirm, discardConfirm, failure)
  card.append(stateBadge, refLine, commitLine, branchLine, actions, notice, history)

  function armedAction(interaction: LocalCandidateInteraction): LocalCandidateAction | null {
    if (interaction.kind === 'confirming-apply') return 'apply'
    if (interaction.kind === 'confirming-discard') return 'discard'
    if (interaction.kind === 'failed' && interaction.action !== 'branch') {
      return interaction.action
    }
    return null
  }

  /** The confirmation sentence names the exact branch and expected HEAD. */
  function applyConfirmCopy(): string {
    const target = applyBranchInput.value.trim()
    const head = applyHeadInput.value.trim()
    if (target.length === 0 || head.length === 0) {
      return `Applying rewrites the target branch history. Apply ${currentRef} onto `
        + 'the exact target branch only while its HEAD is still the expected commit.'
    }
    return `Apply ${currentRef} onto branch ${target} only while its HEAD is still ${head}.`
  }

  function refreshApplyConfirm(): void {
    const copy = applyConfirmCopy()
    if (applyConfirmText.textContent !== copy) applyConfirmText.textContent = copy
    const ready = applyBranchInput.value.trim().length > 0
      && applyHeadInput.value.trim().length > 0
    if (ready === applyAccept.disabled) applyAccept.disabled = !ready
  }

  function clearApplyDraft(): void {
    applyBranchInput.value = ''
    applyHeadInput.value = ''
    applyBranchInput.removeAttribute('aria-invalid')
    applyHeadInput.removeAttribute('aria-invalid')
    refreshApplyConfirm()
  }

  function downloadSummary(): void {
    if (closed || currentCandidate === null) return
    const text = options.model.conflictSummary(currentRef)
    if (text === null) return
    if (objectUrl !== null) revokeObjectUrl(objectUrl)
    objectUrl = createObjectUrl(text)
    // The visible control stays a button; the disposable anchor only carries
    // the download to the browser.
    const anchor = document.createElement('a')
    anchor.setAttribute('href', objectUrl)
    anchor.download = `candidate-${currentRef}-conflict-summary.txt`
    anchor.click()
    anchor.remove()
  }

  const onBranchCreate = () => {
    options.model.requestBranch(currentRef)
  }
  const onApply = () => {
    options.model.requestApply(currentRef)
  }
  const onDiscard = () => {
    options.model.requestDiscard(currentRef)
  }
  const onApplyAccept = () => {
    options.model.confirmApply(currentRef, {
      targetBranch: applyBranchInput.value,
      expectedHead: applyHeadInput.value,
    })
  }
  const onApplyKeep = () => {
    clearApplyDraft()
    options.model.dismiss(currentRef)
  }
  const onDiscardAccept = () => {
    options.model.confirmDiscard(currentRef)
  }
  const onDiscardKeep = () => {
    options.model.dismiss(currentRef)
  }
  const onDownload = () => {
    downloadSummary()
  }
  const onDraftEdit = () => {
    refreshApplyConfirm()
  }

  branchCreate.addEventListener('click', onBranchCreate)
  apply.addEventListener('click', onApply)
  discard.addEventListener('click', onDiscard)
  applyAccept.addEventListener('click', onApplyAccept)
  applyKeep.addEventListener('click', onApplyKeep)
  discardAccept.addEventListener('click', onDiscardAccept)
  discardKeep.addEventListener('click', onDiscardKeep)
  download.addEventListener('click', onDownload)
  applyBranchInput.addEventListener('input', onDraftEdit)
  applyHeadInput.addEventListener('input', onDraftEdit)

  function renderHistory(candidate: ControlPlaneCandidateSummary): void {
    const signature = candidate.history
      .map(entry => `${entry.localApplyReceiptId}:${entry.result}`)
      .join('|')
    if (signature === historySignature) return
    historySignature = signature
    // The history is small and rebuilds only when the ledger identity set
    // changes; every other update keeps the existing nodes in place.
    for (const child of [...history.children]) child.remove()
    for (let index = candidate.history.length - 1; index >= 0; index -= 1) {
      const entry = candidate.history[index]
      if (entry === undefined) continue
      const row = element(document, 'p', 'wwc-candidate-card-history-row')
      row.dataset.result = entry.result
      row.dataset.tone = candidateResultTone(entry.result)
      row.textContent = `${candidateResultText(entry.result)} `
        + `(${entry.targetBranch} @ ${shortCommitText(entry.expectedHead)})`
      history.append(row)
    }
    history.hidden = candidate.history.length === 0
  }

  return {
    root: card,
    update(candidate) {
      currentRef = candidate.candidateRef
      currentCandidate = candidate
      const displayState = candidateDisplayState(candidate)
      const stateText = candidateDisplayStateText(displayState)
      card.setAttribute(
        'aria-label',
        `Candidate ${candidate.candidateRef}: ${stateText}`,
      )
      stateBadge.textContent = stateText
      stateBadge.dataset.tone = candidateDisplayStateTone(displayState)
      refLine.textContent = `Ref ${candidate.candidateRef}`
      commitLine.textContent = `Commit ${shortCommitText(candidate.candidateCommit)}`
      commitLine.title = candidate.candidateCommit
      if (candidate.branchName === null) {
        branchLine.hidden = true
        branchLine.textContent = ''
      } else {
        branchLine.hidden = false
        branchLine.textContent = `Branch ${candidate.branchName}`
      }

      const interaction = options.model.interaction(candidate.candidateRef)
      const busy = interaction.kind === 'submitting'
      const busyAction: LocalCandidateAction | null = busy ? interaction.action : null
      actions.setAttribute('aria-busy', busy ? 'true' : 'false')

      const branchApplies = candidateSupportsBranch(candidate)
      branchCreate.hidden = !branchApplies
      branchCreate.textContent = busyAction === 'branch' ? 'Creating…' : 'Create branch'
      branchCreate.disabled = busy

      const applyApplies = candidateSupportsApply(candidate)
      apply.hidden = !applyApplies
      apply.textContent = busyAction === 'apply' ? 'Applying…' : 'Apply…'
      apply.disabled = busy

      const discardApplies = candidateSupportsDiscard(candidate)
      discard.hidden = !discardApplies
      discard.textContent = busyAction === 'discard' ? 'Discarding…' : 'Discard'
      discard.disabled = busy

      download.hidden = displayState !== 'conflict' || busy
      download.disabled = busy

      const armed = armedAction(interaction)
      if (armed === 'apply') {
        applyConfirm.hidden = false
        refreshApplyConfirm()
      } else {
        applyConfirm.hidden = true
      }
      if (armed === 'discard') {
        discardConfirm.hidden = false
        discardConfirmText.textContent = DISCARD_COPY
      } else {
        discardConfirm.hidden = true
      }

      const failureLine = interaction.kind === 'failed' ? failureText(interaction) : null
      if (failureLine === null) {
        failure.hidden = true
        failure.textContent = ''
      } else {
        failure.hidden = false
        failure.textContent = failureLine
      }

      renderHistory(candidate)

      // A settled action clears the typed draft: the confirm inputs survive
      // busy and failed states, and drop once the interaction rests.
      if (interaction.kind === 'rest' && lastInteractionKey !== 'rest') {
        clearApplyDraft()
      }
      lastInteractionKey = interaction.kind === 'submitting'
        ? `submitting:${interaction.action}`
        : interaction.kind
    },
    close() {
      if (closed) return
      closed = true
      branchCreate.removeEventListener('click', onBranchCreate)
      apply.removeEventListener('click', onApply)
      discard.removeEventListener('click', onDiscard)
      applyAccept.removeEventListener('click', onApplyAccept)
      applyKeep.removeEventListener('click', onApplyKeep)
      discardAccept.removeEventListener('click', onDiscardAccept)
      discardKeep.removeEventListener('click', onDiscardKeep)
      download.removeEventListener('click', onDownload)
      applyBranchInput.removeEventListener('input', onDraftEdit)
      applyHeadInput.removeEventListener('input', onDraftEdit)
      if (objectUrl !== null) {
        revokeObjectUrl(objectUrl)
        objectUrl = null
      }
      card.remove()
    },
  }
}
