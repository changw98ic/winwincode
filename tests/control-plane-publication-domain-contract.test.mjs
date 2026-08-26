import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-publication-domain.rules.json',
)
const documentationPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-publication-domain.md',
)

function read(path) {
  return readFileSync(path, 'utf8')
}

function rules() {
  return JSON.parse(read(rulesPath))
}

function run(command) {
  const [program, ...commandArguments] = command
  const result = spawnSync(program, commandArguments, {
    cwd: root,
    encoding: 'utf8',
    env: process.env,
    timeout: 300_000,
  })
  assert.equal(
    result.status,
    0,
    [result.stdout, result.stderr, result.error?.stack].filter(Boolean).join('\n'),
  )
  return `${result.stdout}\n${result.stderr}`
}

test('phase 3.2 freezes one implemented Publication authority and effect path', () => {
  const contract = rules()
  assert.deepEqual(
    {
      schemaVersion: contract.schemaVersion,
      status: contract.status,
      issueId: contract.issueId,
      decision: contract.decision,
      documentation: contract.documentation,
      implementationCompletionSource: contract.implementationCompletionSource,
    },
    {
      schemaVersion: 'winwincode.control-plane-publication-domain-rules.v1',
      status: 'implemented-enforced',
      issueId: 'winwincode-9c4.16.3.2',
      decision: 'docs/decisions/0028-control-plane-worker-migration.md',
      documentation: 'docs/contracts/control-plane-publication-domain.md',
      implementationCompletionSource: 'rust-black-box-tests-and-beads',
    },
  )
  assert.deepEqual(contract.authorityChain, [
    'current-delivered-delivery',
    'current-frozen-candidate-and-artifact',
    'current-passing-verdict',
    'resolved-exact-human-publication-approval',
    'immutable-publication-target',
    'durable-publication-intent',
    'ordered-provider-operations',
    'secret-safe-publication-result',
  ])
})

test('Publication commands and trusted authority remain separate and closed', () => {
  const contract = rules()
  assert.deepEqual(contract.command.publishFields, [
    'publicationId',
    'deliveryId',
    'candidateDigest',
    'target',
  ])
  assert.deepEqual(contract.authorization.requiredCurrentFacts, [
    'deliveredDeliveryRevision',
    'deliverySpecIdentity',
    'frozenCandidateAndDiff',
    'candidateArtifactIdentityAndDigest',
    'passingVerdict',
    'resolvedHumanApprovalAndReviewSet',
    'sourceIssue',
    'publicationTarget',
    'repositoryScope',
  ])
  assert.equal(contract.authorization.callerJsonCanConstruct, false)
  assert.equal(contract.authorization.staleOrIncompleteAllowed, false)
  assert.equal(
    contract.authorization.targetDigestShape,
    'Delivery GitHubPullRequestTargetRef schema v3',
  )
  assert.equal(contract.authorization.approvalActor, 'canonical-user-id-only')
  assert.equal(
    contract.authorization.publicationSetSha256Source,
    'durable-publication-authorization',
  )

  const facts = read(join(
    root,
    'crates',
    'winwincode-publication',
    'src',
    'facts.rs',
  ))
  assert.match(facts, /pub struct PublicationAuthorization \{/u)
  assert.doesNotMatch(
    facts,
    /#\[derive\([^\]]*Deserialize[^\]]*\)\]\s*pub struct PublicationAuthorization/u,
  )
  const sources = read(join(
    root,
    'crates',
    'winwincode-control-plane',
    'src',
    'strongflow_projection',
    'sources.rs',
  ))
  assert.match(sources, /pub use winwincode_publication::\{/u)
  assert.doesNotMatch(sources, /pub struct PublicationFactBinding \{/u)
  assert.doesNotMatch(sources, /pub struct PublicationResultFact \{/u)

  const mapping = read(join(
    root,
    'crates',
    'winwincode-control-plane',
    'src',
    'strongflow_projection',
    'mapping.rs',
  ))
  assert.match(
    mapping,
    /publication_set_sha256: result\.publication_set_sha256\(\)\.clone\(\)/u,
  )
})

test('the durable intent and provider protocol have one exact recovery policy', () => {
  const contract = rules()
  assert.deepEqual(contract.ledger.atomicMembers, [
    'publicationState',
    'aggregateJournalRecord',
    'scopedCommandReceipt',
    'internalOutboxEvent',
  ])
  assert.equal(contract.ledger.intentBeforeProviderCall, true)
  assert.equal(contract.ledger.exactReplayReadsCurrentStateOrJournal, false)
  assert.deepEqual(contract.provider.operationOrder, [
    'branch',
    'pull-request',
    'issue-comment',
    'commit-status',
  ])
  assert.equal(contract.provider.lookupBeforeApply, true)
  assert.equal(contract.provider.operationSchemaVersion, 1)
  assert.equal(
    contract.provider.operationProtocol,
    'winwincode.github-provider-operation.v1',
  )
  assert.equal(contract.provider.unknownResult, 'durable-and-recoverable')
  assert.equal(contract.provider.rejectedResult, 'terminal-failed')
  assert.equal(contract.provider.successCanBeDowngraded, false)
  assert.equal(contract.provider.credentialsInOperationOrResult, false)
  assert.equal(contract.cancel.changesDeliveryVerdict, false)
})

test('the contract gate executes the Publication and eligibility black-box suites', () => {
  const contract = rules()
  for (const gate of [contract.rustGate.publication, contract.rustGate.controlPlane]) {
    const output = run(gate.command)
    for (const requiredTest of gate.requiredTests) {
      assert.match(
        output,
        new RegExp(`test ${requiredTest} \\.\\.\\. ok`, 'u'),
        `${gate.package} did not execute ${requiredTest}`,
      )
    }
  }
})

test('the documentation states the same path and links the implemented GitHub adapter', () => {
  const documentation = read(documentationPath)
  for (const statement of [
    'PublicationCoordinator + PublicationLedger + PublicationPort',
    'branch → pull-request → issue-comment → commit-status',
    '任何 provider 调用之前',
    '不会重复创建 PR',
    'Cancel 只改变 Publication',
    'GitHub HTTP 与 credential reference adapter 已由阶段 3.3',
    'control-plane-github-publication-adapter.md',
  ]) {
    assert.ok(documentation.includes(statement), `missing documentation statement: ${statement}`)
  }
})
