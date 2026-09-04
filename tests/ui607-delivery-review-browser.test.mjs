import assert from 'node:assert/strict'
import { existsSync, mkdtempSync, readdirSync, rmSync, statSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  certificate,
  chromeBinary,
  closeServer,
  command,
  DevTools,
  evaluate,
  freePort,
  listen,
  staticClientServer,
  stopChild,
  waitForGlobal,
} from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')
const fixturePath = 'tests/fixtures/browser-ui607-delivery-review.mjs'
const publicationSetDigest = `sha256:${'9'.repeat(64)}`
const candidateA = `git-candidate:sha256:${'a'.repeat(64)}`
const candidateC = `git-candidate:sha256:${'c'.repeat(64)}`
const candidateB = `git-candidate:sha256:${'b'.repeat(64)}`
const digestC = `sha256:${'c'.repeat(64)}`
const digestB = `sha256:${'b'.repeat(64)}`

function newestMtimeMillis(directory) {
  let newest = 0
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) {
      newest = Math.max(newest, newestMtimeMillis(path))
    } else {
      newest = Math.max(newest, statSync(path).mtimeMs)
    }
  }
  return newest
}

/**
 * `pnpm test:ts` builds the client bundle before the lane starts, and every
 * browser suite that rebuilds it races on the shared `apps/client/dist` tree.
 * This suite therefore rebuilds only when the bundle is older than the sources,
 * so the lane stays race-free while a standalone run still gets a fresh build.
 */
function ensureClientBundle() {
  const moduleRoot = resolve(root, 'apps/client/dist/module')
  if (
    existsSync(moduleRoot)
    && newestMtimeMillis(moduleRoot) >= newestMtimeMillis(resolve(root, 'apps/client/src'))
  ) return
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
}

test('a real browser reviews a candidate, approves bounded rework, and reads one publication receipt', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(
    chromePath,
    null,
    'Chrome or Chromium is required for the UI-607 review vertical',
  )
  ensureClientBundle()
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-ui607-review-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath,
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  let chrome = null
  let devtools = null
  const exceptions = []
  t.after(async () => {
    devtools?.close()
    await Promise.all([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
    assert.deepEqual(exceptions, [], 'the browser must run the vertical without throwing')
  })
  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  chrome = launched.chrome
  devtools = launched.devtools
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 1000,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', { url: `${clientOrigin}/#/strongflow` }, sessionId)
  await waitForGlobal(devtools, sessionId, 'ui607Ready')

  // ── Scenario 1: read the real Candidate Diff and its Evidence. Two required
  // criteria did not pass, and no final Delivery approval is offered.
  const review = await evaluate(devtools, sessionId, 'globalThis.ui607ReviewCandidate()')

  assert.equal(review.diff.fileQueries >= 1, true, JSON.stringify(review.diff))
  assert.equal(review.diff.diffQueries, 1)
  assert.match(review.diff.content, /diff --git a\/src\/export\/gateway\.ts/u)
  assert.match(review.diff.content, /return input\.replace\(\/internal-\[a-z\]\+\/u/u)
  assert.match(review.diff.summary, /3 files loaded/u)
  assert.equal(review.diff.candidateRef, candidateA)

  assert.equal(review.evidence.evidenceQueries, 1)
  assert.equal(review.evidence.evidenceId, 'evd_00000000000000000000000002')
  assert.match(review.evidence.candidate, /current candidate/u)
  assert.match(review.evidence.outcome, /Failed/u)
  assert.match(review.evidence.hash, /evidence=evd_00000000000000000000000002/u)
  assert.match(review.evidence.hash, /tab=evidence/u)

  assert.equal(review.gate.deliveryStatus, 'needs-attention')
  assert.equal(review.gate.predicateUnmet, true)
  assert.equal(review.gate.predicateAdvance, false)
  assert.equal(review.gate.advancePresent, true)
  assert.equal(review.gate.advanceHidden, true)
  assert.equal(review.gate.blockedHidden, true, 'the authority agrees, so no note is needed')
  assert.equal(review.gate.blockedText, '')
  assert.match(review.gate.criteriaText, /criterion:2 · required/u)
  assert.match(review.gate.criteriaText, /infra_error/u)
  assert.deepEqual(review.gate.criterionOutcomes, [
    'pass', 'fail', 'infra_error', 'not_evaluated',
  ])
  assert.match(review.gate.conclusion, /Verdict failed/u)
  assert.equal(review.gate.receiptReads, 0, 'no receipt is read before a Publication exists')
  assert.deepEqual(review.commands, [])

  // ── Scenario 2: even a read model that wrongly claims `ready-to-deliver`
  // cannot be turned into a final approval, and the command path refuses on its
  // own once the hidden control is activated.
  const hostile = await evaluate(devtools, sessionId, 'globalThis.ui607HostileReadyClaim()')

  assert.equal(hostile.deliveryStatus, 'ready-to-deliver', JSON.stringify(hostile))
  assert.equal(hostile.candidate, candidateA)
  assert.equal(hostile.predicateUnmet, true)
  assert.equal(hostile.predicateAdvance, false)
  assert.equal(hostile.advancePresent, true)
  assert.equal(hostile.advanceHidden, true)
  assert.equal(hostile.blockedHidden, false)
  assert.match(hostile.blockedText, /criterion:2, criterion:3 did not pass/u)
  assert.match(hostile.blockedText, /Approve a bounded rework and recompute the Verdict/u)
  assert.equal(hostile.advanceCommands, 0)
  assert.equal(hostile.advanceCommandsAfterClick, 0, 'no delivery.advance left the browser')
  assert.match(hostile.errorText, /criterion:2, criterion:3 must pass/u)
  assert.equal(hostile.restoredStatus, 'needs-attention')
  assert.equal(hostile.restoredCandidate, candidateA)

  // ── Scenario 3: a Candidate superseded under the reviewer marks the note
  // stale, keeps the draft, and submits nothing onto the new Candidate.
  const stale = await evaluate(devtools, sessionId, 'globalThis.ui607StaleCandidate()')

  assert.equal(stale.before.notes.length, 1, JSON.stringify(stale.before))
  assert.equal(stale.before.notes[0].note, 'The redaction gate drops this host from every export.')
  assert.equal(stale.before.candidate, candidateA)
  assert.equal(stale.stale.notes.length, 1, 'the stale draft must survive')
  assert.equal(stale.stale.notes[0].note, stale.before.notes[0].note)
  assert.equal(stale.stale.notes[0].anchor, stale.before.notes[0].anchor)
  assert.equal(stale.stale.submitDisabled, true)
  assert.match(stale.stale.notes[0].staleText, /candidate changed/u)
  assert.equal(stale.stale.candidateBefore, candidateA)
  assert.equal(stale.stale.candidateNow, candidateC)
  assert.equal(stale.stale.resolveCommands, 0, 'a stale note is never submitted')
  assert.equal(stale.afterDiscard.notes.length, 0)
  assert.equal(stale.afterDiscard.resolveCommands, 0)

  // ── Scenario 4: the staged notes compose into exactly one bounded rework
  // command, keep the draft across a revision conflict, and bind to the
  // Candidate the reviewer actually read.
  const rework = await evaluate(devtools, sessionId, 'globalThis.ui607ApproveBoundedRework()')

  assert.equal(rework.scope.attentionId, 'att_0000000000000000000000000c')
  assert.match(rework.scope.attention, /Verification blocked/u)
  assert.match(rework.scope.node, /Export gateway/u)
  assert.match(rework.scope.task, /Ship the redacted export/u)
  assert.match(rework.finalScope.join(' '), /Bounded rework scope · node:export-gateway/u)
  assert.match(rework.finalScope.join(' '), new RegExp(`Candidate · ${digestC}`, 'u'))
  assert.match(
    rework.conflict.errorText,
    /This Delivery changed before the decision was saved/u,
  )
  assert.equal(rework.conflict.notes.length, 1, 'a revision conflict must keep the draft')
  assert.deepEqual(rework.conflict.notes, rework.conflict.stagedBeforeSubmit)
  assert.equal(rework.conflict.commands, 1)
  assert.equal(rework.command.name, 'delivery.resolve_attention')
  assert.equal(rework.command.attentionItemId, 'att_0000000000000000000000000c')
  assert.equal(rework.command.decision, 'resolve')
  assert.equal(rework.command.remediationNode, 'node:export-gateway')
  assert.equal(rework.command.remediationTask, 'task:export')
  assert.equal(rework.command.remediationDigest, digestC)
  assert.equal(rework.command.readCandidate, digestC)
  assert.equal(rework.command.carriesNotes, true)
  assert.equal(rework.notesAfterSuccess, 0)
  assert.deepEqual(rework.commands, [
    'delivery.resolve_attention',
    'delivery.resolve_attention',
  ])

  // ── Scenario 5: the reworked Candidate compares against the rework baseline,
  // the Verdict is computed, the final approval opens, and the Publication
  // receipt is followed while the browser never writes to the provider.
  const receipt = await evaluate(devtools, sessionId, 'globalThis.ui607VerdictAndReceipt()')

  // The comparison workbench offers the baseline side plus every frozen
  // Candidate of this Delivery, and reports the compared Verdict.
  assert.equal(receipt.comparison.from, 'baseline')
  assert.equal(receipt.comparison.candidateChoices.includes('baseline'), true)
  assert.equal(receipt.comparison.candidateChoices.length >= 2, true)
  assert.equal(receipt.comparison.alertHidden, true)
  assert.equal(receipt.comparison.candidateListQueries, 1)
  assert.match(receipt.comparison.verdict, /Verdict/u)
  assert.equal(receipt.verdictCommand.name, 'delivery.submit_verdict')
  assert.equal(receipt.verdictCommand.candidateDigest, digestB)
  assert.equal(
    receipt.verdictCommand.expectedRevision,
    receipt.advanceCommand.expectedRevision - 1,
  )
  assert.equal(receipt.advanceCommand.name, 'delivery.advance')
  assert.equal(receipt.gate.advanceHidden, false)
  assert.equal(receipt.gate.blockedHidden, true)
  assert.equal(receipt.gate.predicateAdvance, true)
  assert.equal(receipt.gate.deliveryStatus, 'ready-to-deliver')
  assert.match(receipt.gate.conclusion, /Verdict passed · Publication not created/u)

  assert.equal(receipt.receipt.queries, 1, 'the receipt is read once and then replayed')
  assert.match(receipt.receipt.text, /pub_00000000000000000000000002/u)
  assert.match(receipt.receipt.text, new RegExp(publicationSetDigest, 'u'))

  assert.match(receipt.expired.text, /publication\.approval\.expired/u)
  assert.match(receipt.expired.steps, /pull_request rejected \(publication\.approval\.expired\)/u)
  assert.equal(receipt.expired.retryable, 'no')
  assert.match(receipt.retried.text, /RESOURCE_CONFLICT/u)
  assert.equal(receipt.retried.retryable, 'yes')
  assert.match(receipt.published.text, /winwincode\/browser-fixture #21/u)
  assert.match(receipt.published.text, /Statepublished/u)
  assert.match(receipt.published.conclusion, /Publication published/u)
  assert.equal(receipt.published.publicationQueries >= 2, true)

  assert.deepEqual(receipt.writeCommands, [], 'only the Publication coordinator may write')
  assert.deepEqual(
    [...new Set(receipt.commands)],
    ['delivery.resolve_attention', 'delivery.submit_verdict', 'delivery.advance'],
  )
  // The receipt, the comparison, the Diff, the Evidence, and the staged review
  // notes introduce no live region of their own: the page keeps exactly one.
  assert.equal(
    receipt.collectionLiveRegions,
    0,
    'no review collection announces its own re-renders',
  )
})
