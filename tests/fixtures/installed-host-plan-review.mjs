import { readFileSync } from 'node:fs'

import {
  parseDelivery,
  parseStrongFlowPlanReviewContextText,
} from '@winwincode/contracts'

import {
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
} from './dist/index.js'

const projectionPath = process.argv[2]
if (projectionPath === undefined) throw new Error('projection path is required')
const response = JSON.parse(readFileSync(projectionPath, 'utf8'))
const delivery = parseDelivery(response.result?.delivery)
const reviewStageRunId = 'stage-installed-plan-review'
const attention = createStrongFlowPlanReviewAttention({
  delivery,
  attentionItemId: 'attention-installed-plan-review',
  reviewStageRunId,
  assignedTo: 'installed-reviewer',
  solution: {
    id: 'solution-installed-plan-review',
    summary: 'Exercise the installed Delivery process through its published CLI.',
    approach: [
      'Keep the DSH chat surface as the default entry.',
      'Use the one durable StrongFlow service for the reviewed Delivery.',
    ],
    components: [{
      id: 'component-installed-host',
      label: 'Installed host',
      responsibility: 'Expose the DSH shell and canonical Delivery process.',
      kind: 'component',
      trustBoundary: 'Installed package boundary',
      unresolved: false,
      repositoryPathPrefixes: ['apps/host'],
    }],
    connections: [{
      id: 'connection-installed-host',
      from: 'platform:dsh',
      to: 'component-installed-host',
      label: 'Hosts the reviewed Delivery surface',
    }],
  },
  risks: [],
  unresolvedItems: [],
  preparedAtMillis: Date.now(),
})
const decision = createStrongFlowPlanReviewDecision({
  context: parseStrongFlowPlanReviewContextText(attention.context),
  action: 'approve',
  comments: 'Approve the exact installed-package review set.',
  requestedChanges: [],
})
process.stdout.write(`${JSON.stringify({ attention, decision })}\n`)
