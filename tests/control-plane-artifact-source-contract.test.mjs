import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'config',
  'contracts',
  'control-plane-artifact-source.rules.json',
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

test('phase 3.1 freezes one implemented Artifact and source authority chain', () => {
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
      schemaVersion: 'winwincode.control-plane-artifact-source-rules.v1',
      status: 'implemented-enforced',
      issueId: 'winwincode-9c4.16.3.1',
      decision: 'config/decisions/0028-control-plane-worker-migration.md',
      documentation: 'config/contracts/control-plane-artifact-source.md',
      implementationCompletionSource: 'rust-black-box-tests-and-beads',
    },
  )
  assert.deepEqual(contract.authorityChain, [
    'generated-artifact-message',
    'durable-execution-job-and-repository-scope',
    'sealed-session-binding-authority',
    'artifact-store-metadata-and-object-bytes',
    'opaque-rebuilt-git-source',
    'settled-successful-worker-outcome',
    'frozen-delivery-candidate',
  ])
  assert.deepEqual(contract.compatibility, {
    callerReportedGitIdentityPathRetained: false,
    secondArtifactStorePathRetained: false,
    enterpriseObjectStorageClaimedComplete: false,
    githubPublicationClaimedComplete: false,
  })
})

test('Artifact identity, recovery, corruption, and deletion rules are closed', () => {
  const artifact = rules().artifact
  assert.deepEqual(artifact.openIdentity, [
    'repositoryScope',
    'messageId',
    'requestId',
    'artifactId',
    'descriptor',
    'executionJobProvenance',
    'retention',
    'createdAt',
  ])
  assert.deepEqual(artifact.chunkIdentity, [
    'repositoryScope',
    'messageId',
    'artifactId',
    'executionJobProvenance',
    'sentAt',
    'sequence',
    'contentType',
    'payloadDigest',
    'decodedBytes',
    'isFinal',
  ])
  assert.equal(artifact.contentAddress, 'sha256')
  assert.equal(artifact.exactReplay, 'duplicate')
  assert.equal(artifact.changedIdentityReuse, 'message-conflict')
  assert.equal(artifact.digestMismatch, 'ARTIFACT_DIGEST_MISMATCH')
  assert.deepEqual(artifact.corruptionCheckBeforeReturn, [
    'object-present',
    'exact-size',
    'exact-sha256',
  ])
  assert.deepEqual(artifact.deletion, {
    incompleteAllowed: false,
    indefiniteRetentionAllowed: false,
    beforeRetentionDeadlineAllowed: false,
    artifactIdReusableAfterDelete: false,
    sharedContentDeletedBeforeLastLiveReference: false,
  })
})

test('source facts are rebuilt from controlled Git and remain opaque', () => {
  const source = rules().gitSource
  assert.deepEqual(source.manifestExactFields, [
    'schemaVersion',
    'candidateCommitId',
  ])
  assert.equal(source.manifestAdditionalProperties, false)
  assert.equal(source.manifestCanonicalBytesRequired, true)
  assert.equal(source.inheritedGitEnvironmentAllowed, false)
  assert.equal(source.replaceRefsAllowed, false)
  assert.equal(source.externalDiffAllowed, false)
  assert.equal(source.textConvAllowed, false)
  assert.equal(source.candidateMustDescendFromBase, true)
  assert.deepEqual(source.rebuiltFacts, [
    'baseCommitId',
    'baseTreeId',
    'candidateCommitId',
    'candidateTreeId',
    'diffSha256',
    'changedPaths',
    'pathObjectIds',
    'changedHunkSha256',
  ])
  assert.equal(source.publicConstructorAllowed, false)
  assert.equal(source.deserializeAllowed, false)

  const gitSource = read(join(
    root,
    'crates',
    'winwincode-storage',
    'src',
    'git_source.rs',
  ))
  assert.match(gitSource, /struct ValidatedGitSourceArtifact \{/u)
  assert.doesNotMatch(
    gitSource,
    /#\[derive\([^\]]*Deserialize[^\]]*\)\]\s*pub struct ValidatedGitSourceArtifact/u,
  )
  assert.match(gitSource, /\.env_clear\(\)/u)
  assert.match(gitSource, /\.env\("GIT_NO_REPLACE_OBJECTS", "1"\)/u)
  assert.match(gitSource, /"--no-ext-diff"/u)
  assert.match(gitSource, /"--no-textconv"/u)
})

test('generated Artifact messages remain bound to durable execution authority', () => {
  const authority = rules().executionAuthority
  assert.deepEqual(authority.required, [
    'durableExecutionJob',
    'exactRepositoryScope',
    'activeDeliveryWorkRunOrProductSessionJob',
    'completeSessionBinding',
    'workerSession',
    'leaseId',
    'attempt',
    'fencingToken',
    'workerId',
    'workerInstanceId',
    'issuedAt',
    'expiresAt',
  ])
  assert.equal(authority.rejectedMessageWritesMetadataOrBytes, false)

  const transaction = read(join(
    root,
    'crates',
    'winwincode-control-plane',
    'src',
    'artifact_transaction.rs',
  ))
  assert.match(transaction, /ArtifactOpenMessage/u)
  assert.match(transaction, /ArtifactChunkMessage/u)
  assert.match(transaction, /load_durable_execution_job/u)
  assert.match(transaction, /validate_current_binding/u)
  assert.match(transaction, /context\.provenance/u)
  assert.match(transaction, /ArtifactDigestMismatch/u)
})

test('the contract gate executes every Artifact and source black-box suite', () => {
  const contract = rules()
  for (const gate of [
    contract.rustGate.storage,
    contract.rustGate.controlPlane,
    contract.rustGate.delivery,
  ]) {
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
