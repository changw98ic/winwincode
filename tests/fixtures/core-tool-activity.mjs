// SPDX-License-Identifier: Apache-2.0
export function sharedCoreActivity() {
  return {
    callId: 'core:thread:request:8', activityType: 'tool', command: 'public_smoke',
    status: 'cancelled', outcome: null, exitCode: null, sourceRef: 'core:thread:request:8',
    coreTool: {
      sourceThreadId: 'thread', sourceSequence: 20, kind: 'waiter_cancellation',
      call: {
        requestSequence: 8, logicalId: 'logical-8', toolName: 'public_smoke',
        parentCallId: 'outer-exec', parentRequestSequence: 1, cellId: 'cell-1',
        attemptId: null, execution: null, disposition: 'accepted', delivery: 'offered',
        cancelled: true, inputValidation: 'verified',
      },
      cell: null, wait: null, diagnosis: null, recovery: null,
      sharing: { kind: 'merged', sourceRequestSequence: 7, sourceAttemptId: 'attempt-7' },
    },
  }
}
export function diagnosticCoreActivity() {
  const activity = sharedCoreActivity()
  return {
    ...activity, callId: 'diagnostic-1', status: 'completed',
    coreTool: {
      ...activity.coreTool, call: null, sharing: null, kind: 'diagnostic',
      diagnosis: {
        diagnosticId: 'diagnostic-1', evidenceVersion: 2, kind: 'wait_cycle', delivery: 'offered',
        question: '这些等待是否成环？<script>window.pwned=true</script>',
        evidence: [{ requestSequence: 8, logicalId: 'logical-8', toolName: 'public_smoke', parentCallId: 'outer-exec', cellId: 'cell-1' }],
        waitGraph: [agentWaitEdge()],
      },
    },
  }
}

function agentWaitEdge() {
  return {
    treeId: 'tree-1', requestSequence: 8, logicalId: 'logical-8',
    source: { kind: 'cell', threadId: 'thread', ownerId: 'owner-1', cellId: 'cell-1', scopeId: 'scope-1' },
    targets: [{ kind: 'thread', threadId: 'agent-2', ownerId: 'owner-2', cellId: null, scopeId: null }],
    targetCount: 1, deadlineUnixMs: 1893456000000,
  }
}

export function agentWaitCoreActivity() {
  const activity = sharedCoreActivity()
  return {
    ...activity, callId: 'agent-wait-8', status: 'unknown', command: 'Agent completion wait',
    coreTool: {
      ...activity.coreTool, call: null, sharing: null, kind: 'agent_wait',
      agentWait: { edge: agentWaitEdge(), state: 'waiting' },
    },
  }
}
