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
  'control-plane-github-publication-adapter.rules.json',
)
const documentationPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-github-publication-adapter.md',
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

test('phase 3.3 freezes one implemented GitHub Publication adapter', () => {
  const contract = rules()
  assert.deepEqual(
    {
      schemaVersion: contract.schemaVersion,
      status: contract.status,
      issueId: contract.issueId,
      documentation: contract.documentation,
      ownerCrate: contract.adapter.ownerCrate,
      implementation: contract.adapter.implementation,
      providerPort: contract.adapter.providerPort,
    },
    {
      schemaVersion: 'winwincode.control-plane-github-publication-adapter-rules.v1',
      status: 'implemented-enforced',
      issueId: 'winwincode-9c4.16.3.3',
      documentation: 'docs/contracts/control-plane-github-publication-adapter.md',
      ownerCrate: 'winwincode-publication',
      implementation: 'GitHubPublicationAdapter',
      providerPort: 'PublicationPort',
    },
  )
  assert.deepEqual(contract.adapter.operationOrder, [
    'branch',
    'pull-request',
    'issue-comment',
    'commit-status',
  ])
  assert.equal(contract.adapter.releaseOperation, 'not-in-canonical-v1')

  const manifest = read(join(root, 'crates', 'winwincode-publication', 'Cargo.toml'))
  assert.match(
    manifest,
    /ureq = \{ version = "=3\.1\.4", default-features = false, features = \["rustls", "json"\] \}/u,
  )
})

test('credential references resolve per request and secret material stays outside durable facts', () => {
  const contract = rules()
  assert.equal(contract.credential.referenceType, 'CredentialReferenceId')
  assert.equal(contract.credential.resolveFrequency, 'every-http-request')
  assert.equal(contract.credential.requiredProviderId, 'github')
  assert.equal(contract.credential.serializableSecretType, false)
  assert.equal(contract.credential.cloneableSecretType, false)
  assert.equal(contract.credential.debugSecretOutput, 'redacted')
  assert.deepEqual(contract.credential.forbiddenDurableLocations, [
    'PublicationOperation',
    'publicationState',
    'aggregateJournalRecord',
    'scopedCommandReceipt',
    'outboxEvent',
    'PublicationResultFact',
    'error',
  ])
  assert.deepEqual(contract.credential.failureCodes, [
    'credential-not-configured',
    'credential-resolution-denied',
    'credential-resolution-unavailable',
    'credential-provider-mismatch',
  ])

  const source = read(join(root, 'crates', 'winwincode-publication', 'src', 'github.rs'))
  assert.match(source, /pub trait GitHubCredentialResolver \{/u)
  assert.match(source, /\.resolve\(&self\.config\.credential_reference_id\)/u)
  assert.match(source, /\.field\("secret", &"\[REDACTED\]"\)/u)
  assert.doesNotMatch(
    source,
    /#\[derive\([^\]]*(?:Serialize|Deserialize|Clone)[^\]]*\)\]\s*pub struct GitHubCredential/u,
  )
})

test('GitHub HTTP effects use exact lookup joins and closed failure classes', () => {
  const contract = rules()
  assert.deepEqual(contract.http.allowedBaseUrls, [
    'https-remote',
    'http-loopback-only',
  ])
  assert.equal(contract.http.redirects, 0)
  assert.equal(contract.http.responseLimitBytes, 2_097_152)
  assert.equal(contract.http.apiVersion, '2022-11-28')
  assert.deepEqual(contract.http.failureClasses, {
    authentication: 'github-authentication-failed',
    permission: 'github-permission-denied',
    rateLimit: 'github-rate-limited',
    service: 'github-service-unavailable',
    transport: 'github-transport-unknown',
  })
  assert.equal(contract.http.remoteDiagnosticPersisted, false)
  assert.deepEqual(contract.reconciliation.duplicateStatuses, [409, 422])
  assert.equal(contract.reconciliation.duplicatePolicy, 'exact-lookup-only')
  assert.equal(contract.reconciliation.lostResponsePolicy, 'durable-operation-lookup-after-restart')
  assert.equal(contract.reconciliation.laterOperationsAfterRejectedStep, false)

  const source = read(join(root, 'crates', 'winwincode-publication', 'src', 'github.rs'))
  for (const token of [
    'lookup_branch',
    'lookup_pull_request',
    'lookup_issue_comment',
    'lookup_commit_status',
    'apply_branch',
    'apply_pull_request',
    'apply_issue_comment',
    'apply_commit_status',
    'github-rate-limited',
    'github-authentication-failed',
    'github-permission-denied',
    'github-service-unavailable',
    'github-transport-unknown',
  ]) {
    assert.ok(source.includes(token), `missing GitHub adapter token: ${token}`)
  }
})

test('the contract gate executes the loopback GitHub and SQLite recovery suite', () => {
  const contract = rules()
  const output = run(contract.rustGate.command)
  for (const requiredTest of contract.rustGate.requiredTests) {
    assert.match(
      output,
      new RegExp(`test ${requiredTest} \\.\\.\\. ok`, 'u'),
      `GitHub adapter gate did not execute ${requiredTest}`,
    )
  }
})

test('the documentation states the same credential, recovery, and protocol boundary', () => {
  const documentation = read(documentationPath)
  for (const statement of [
    'GitHubPublicationAdapter + PublicationCoordinator + PublicationLedger',
    '每次 HTTP 请求',
    'branch → pull-request → issue-comment → commit-status',
    '409 或 422',
    'github-rate-limited',
    '不会调用 comment 或 status',
    'Release 尚未进入 canonical v1 operation protocol',
    'WINWINCODE_GITHUB_LIVE_TEST=1',
  ]) {
    assert.ok(documentation.includes(statement), `missing documentation statement: ${statement}`)
  }
})
