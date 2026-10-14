// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'

const reviewableRules = new Set(['credential.text.assignment', 'credential.text.provider-token'])

export function reviewReleaseFindings({ bytes, target, path, sourceCommit, findings, exceptions }) {
  const binarySha256 = createHash('sha256').update(bytes).digest('hex')
  const reviews = exceptions.filter(entry => entry.sourceCommit === sourceCommit
    && entry.target === target && entry.path === path && entry.binarySha256 === binarySha256)
  const accepted = []
  for (const review of reviews) {
    assert.match(path, /^bin\/(?:winwincode-(?:server|worker|kernel-helper)|wwc)$/u)
    assert.ok(typeof review.reason === 'string' && review.reason.length > 0)
    assert.ok(Array.isArray(review.sourcePaths) && review.sourcePaths.length > 0)
    for (const finding of review.findings) {
      assert.ok(reviewableRules.has(finding.rule), 'this credential rule cannot be excepted')
      assert.match(finding.location, /^byte:\d+$/u)
      accepted.push({ ...finding, reason: review.reason, sourcePaths: review.sourcePaths, binarySha256 })
    }
  }
  const reviewed = findings.flatMap(finding => {
    if (finding.target !== target || finding.path !== path) return []
    const review = accepted.find(entry => entry.rule === finding.rule && entry.location === finding.location)
    return review === undefined ? [] : [{ ...finding, ...review }]
  })
  const pending = findings.filter(finding => !reviewed.some(entry => entry.rule === finding.rule
    && entry.location === finding.location && entry.target === finding.target && entry.path === finding.path))
  return { pending, reviewed }
}
