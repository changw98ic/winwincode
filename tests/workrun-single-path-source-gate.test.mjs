import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import test from 'node:test'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

const retiredContracts = [
  'delivery-candidate.ts',
  'delivery.ts',
  'strongflow-workrun-start.ts',
  'strongflow-delivery-api.ts',
  'strongflow-diagram-execution.ts',
  'strongflow-github-publication.ts',
  'strongflow-plan-review.ts',
  'strongflow-runtime-execution.ts',
]

const retiredStrongFlowModules = [
  'acceptance-verification.ts',
  'candidate-evidence.ts',
  'delivery-attention.ts',
  'delivery-authenticator.ts',
  'delivery-invoker.ts',
  'delivery-runtime-projection.ts',
  'delivery-service.ts',
  'delivery-store.ts',
  'delivery-verdict.ts',
  'diagram-execution-projection.ts',
  'evaluation-measures.ts',
  'execution-source.ts',
  'github-publication.ts',
  'independent-verification.ts',
  'local-git-delivery-workspace.ts',
  'plan-review.ts',
  'runtime-execution-projection.ts',
]

const retiredTestsAndFixtures = [
  'docs/contracts/delivery-domain-rules.v1.json',
  'docs/contracts/delivery-solution-review-authority.md',
  'docs/contracts/delivery-solution-review-authority.rules.json',
  'docs/contracts/session-worker-fault-differential.rules.json',
  'docs/live-evaluation.md',
  'scripts/evaluation-measures.mjs',
  'scripts/run-evaluation-measures.mjs',
  'scripts/run-keyless-delivery-fixture.mjs',
  'scripts/run-session-worker-fault-differential.mjs',
  'tests/acceptance-verification.test.mjs',
  'tests/candidate-evidence.test.mjs',
  'tests/control-plane-contract-integration.test.mjs',
  'tests/delivery-attention.test.mjs',
  'tests/delivery-contract.test.mjs',
  'tests/delivery-domain-rule-matrix.test.mjs',
  'tests/delivery-fixture-testkit.test.mjs',
  'tests/delivery-full-keyless.test.mjs',
  'tests/delivery-recovery.test.mjs',
  'tests/delivery-restart-idempotency.test.mjs',
  'tests/delivery-runtime-projection.test.mjs',
  'tests/delivery-service.test.mjs',
  'tests/delivery-solution-review-authority-contract.test.mjs',
  'tests/delivery-store.test.mjs',
  'tests/delivery-verdict.test.mjs',
  'tests/diagram-execution-projection.test.mjs',
  'tests/dsh-agent-factory.test.mjs',
  'tests/evaluation-measures.test.mjs',
  'tests/github-publication.test.mjs',
  'tests/independent-verification.test.mjs',
  'tests/live-evaluation-runner.test.mjs',
  'tests/local-git-delivery-workspace.test.mjs',
  'tests/navigation-capability-application.test.mjs',
  'tests/plan-review.test.mjs',
  'tests/session-worker-fault-differential-gate.test.mjs',
  'tests/strongflow-delivery-adapters.test.mjs',
  'tests/fixtures/delivery-service-checkpoint.mjs',
  'tests/fixtures/delivery-service-testkit.mjs',
  'tests/fixtures/dsh-profile/delivery-recovery.mjs',
  'tests/fixtures/full-delivery-scenario.mjs',
  'tests/fixtures/installed-host-plan-review.mjs',
  'tests/fixtures/strongflow-cli.mjs',
]

test('published TypeScript packages contain no retired StageRun surface', () => {
  for (const file of retiredContracts) {
    assert.equal(existsSync(resolve(root, 'packages/contracts/src', file)), false, file)
  }
  for (const file of retiredStrongFlowModules) {
    assert.equal(existsSync(resolve(root, 'packages/strongflow/src', file)), false, file)
  }
  for (const file of retiredTestsAndFixtures) {
    assert.equal(existsSync(resolve(root, file)), false, file)
  }

  const contracts = readFileSync(resolve(root, 'packages/contracts/src/index.ts'), 'utf8')
  const strongflow = readFileSync(resolve(root, 'packages/strongflow/src/index.ts'), 'utf8')
  const chatDeliveryCreator = readFileSync(
    resolve(root, 'apps/client/src/chat-delivery-creator.ts'),
    'utf8',
  )
  assert.doesNotMatch(contracts, /delivery|stage/iu)
  assert.doesNotMatch(strongflow, /delivery|stage/iu)
  assert.match(chatDeliveryCreator, /CommandName\.DeliveryCreate/u)
  assert.doesNotMatch(chatDeliveryCreator, /WorkRunStart|dispatchProfile|planner/u)
})
