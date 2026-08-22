import {
  AttemptId,
  CandidateId,
  DiagramId,
  HumanReviewId,
  JobId,
  KernelSessionId,
  RequirementId,
  SolutionId,
  StageRunId,
  createStrongFlowJobEvent,
} from '../../packages/contracts/dist/index.js'
import {
  StrongFlowController,
  StrongFlowHumanReviewGate,
  StrongFlowJobStore,
} from '../../packages/strongflow/dist/index.js'

const [command, home, jobIdInput, payloadInput] = process.argv.slice(2)

function payload() {
  if (payloadInput === undefined) return {}
  return JSON.parse(Buffer.from(payloadInput, 'base64url').toString('utf8'))
}

function writeReport(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`)
}

async function main() {
  if (command === undefined || home === undefined || jobIdInput === undefined) {
    throw new Error('command, home, and jobId are required')
  }
  const jobId = JobId(jobIdInput)
  const store = await StrongFlowJobStore.open(home, jobId)
  const initial = await store.read()
  let now = initial.snapshot.lastOccurredAtMillis
  let providerCalls = 0

  const providers = Object.freeze([
    'REQUIREMENTS',
    'SOLUTION',
    'DIAGRAMS',
    'PLANNING',
    'EXECUTION',
    'VERIFICATION',
    'REMEDIATION',
    'DELIVERY',
  ].map(stage => Object.freeze({
    stage,
    roleId: `process-role-${stage.toLowerCase()}`,
    async run(context) {
      providerCalls += 1
      if (command === 'crash-during-stage') process.exit(23)
      const revision = context.snapshot.definitionRevision
      const requirementId = context.snapshot.definition.requirementId
        ?? RequirementId(`process-requirement-r${revision}`)
      const solutionId = context.snapshot.definition.solutionId
        ?? SolutionId(`process-solution-r${revision}`)
      const candidateId = context.snapshot.candidateId
        ?? CandidateId(`process-candidate-r${revision}`)
      const common = {
        kernelSessionId: KernelSessionId(`process-kernel-${context.attemptId}`),
      }
      switch (stage) {
        case 'REQUIREMENTS':
          return { ...common, output: { requirementId } }
        case 'SOLUTION':
          return { ...common, output: { requirementId, solutionId } }
        case 'DIAGRAMS':
          return {
            ...common,
            output: {
              definition: {
                requirementId,
                solutionId,
                systemArchitectureDiagramId: DiagramId(
                  `process-architecture-r${revision}`,
                ),
                processFlowDiagramId: DiagramId(`process-flow-r${revision}`),
              },
            },
          }
        case 'PLANNING':
          return { ...common, output: {} }
        case 'EXECUTION':
          return { ...common, output: { candidateId } }
        case 'VERIFICATION':
          return { ...common, output: { candidateId, outcome: 'passed' } }
        case 'REMEDIATION':
        case 'DELIVERY':
          return { ...common, output: { candidateId } }
        default:
          throw new Error(`unexpected process stage ${stage}`)
      }
    },
  })))

  const controller = new StrongFlowController({
    store,
    providers,
    completionGate: {
      authority: 'program',
      async evaluate() {
        return { outcome: 'passed' }
      },
    },
    controllerId: 'process-controller',
    clock: () => ++now,
    stageRunIdFactory: (operation, snapshot) => StageRunId(
      `process-run-${operation.toLowerCase()}-${snapshot.sequence}`,
    ),
    attemptIdFactory: (stage, snapshot) => AttemptId(
      `process-attempt-${stage.toLowerCase()}-${snapshot.sequence}`,
    ),
  })

  if (command === 'advance' || command === 'crash-during-stage') {
    const result = await controller.advance()
    writeReport({ ok: true, command, providerCalls, result })
    return
  }

  if (command === 'review') {
    const request = payload()
    const gate = new StrongFlowHumanReviewGate({
      store,
      authenticator: {
        async authenticate(authentication) {
          return authentication.authentication === 'process-authentication'
            ? { reviewerId: 'process-reviewer' }
            : undefined
        },
      },
      clock: () => ++now,
      reviewIdFactory: () => HumanReviewId(
        `process-review-${initial.snapshot.sequence}`,
      ),
    })
    const receipt = await gate.submit({
      ...request,
      channel: 'cli',
      authentication: 'process-authentication',
    })
    writeReport({ ok: true, command, providerCalls, receipt })
    return
  }

  if (command === 'interrupt-active') {
    const snapshot = initial.snapshot
    if (snapshot.activeStage === undefined) throw new Error('no active stage to interrupt')
    const event = createStrongFlowJobEvent({
      jobId,
      sequence: (BigInt(snapshot.sequence) + 1n).toString(),
      occurredAtMillis: ++now,
      source: { kind: 'system', actorId: 'process-recovery' },
      kind: 'job.interrupted',
      data: {
        reason: 'The prior process exited during an active stage.',
        stageRunId: snapshot.activeStage.stageRunId,
      },
    })
    const result = await store.append(event)
    writeReport({ ok: true, command, providerCalls, result })
    return
  }

  if (command === 'resume') {
    const result = await controller.resume()
    writeReport({ ok: true, command, providerCalls, result })
    return
  }

  throw new Error(`unknown command ${command}`)
}

main().catch(error => {
  writeReport({
    ok: false,
    error: {
      name: error instanceof Error ? error.name : 'Error',
      code: typeof error?.code === 'string' ? error.code : undefined,
      message: error instanceof Error ? error.message : 'unknown process failure',
    },
  })
  process.exitCode = 2
})
