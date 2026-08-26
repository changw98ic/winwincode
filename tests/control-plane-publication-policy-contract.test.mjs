import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-publication-policy.rules.json',
)
const documentationPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-publication-policy.md',
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

function rustFiles(directory) {
  const files = []
  for (const entry of readdirSync(directory)) {
    const path = join(directory, entry)
    if (statSync(path).isDirectory()) {
      files.push(...rustFiles(path))
    } else if (entry.endsWith('.rs')) {
      files.push(path)
    }
  }
  return files
}

function functionBlock(source, marker) {
  const start = source.indexOf(marker)
  assert.notEqual(start, -1, `missing function marker: ${marker}`)
  const openingBrace = source.indexOf('{', start)
  assert.notEqual(openingBrace, -1, `missing opening brace after: ${marker}`)
  let depth = 0
  for (let index = openingBrace; index < source.length; index += 1) {
    if (source[index] === '{') depth += 1
    if (source[index] === '}') depth -= 1
    if (depth === 0) return source.slice(start, index + 1)
  }
  assert.fail(`missing closing brace after: ${marker}`)
}

function assertOrdered(source, tokens) {
  let previous = -1
  for (const token of tokens) {
    const current = source.indexOf(token)
    assert.ok(current > previous, `${token} is missing or out of order`)
    previous = current
  }
}

test('phase 3.5 freezes one implemented repository Publication policy path', () => {
  const contract = rules()
  assert.deepEqual(Object.keys(contract), [
    'schemaVersion',
    'status',
    'issueId',
    'decision',
    'documentation',
    'implementationCompletionSource',
    'authorityChain',
    'policy',
    'decisionAudit',
    'commandBoundary',
    'publicErrors',
    'providerBoundary',
    'rustGate',
    'compatibility',
  ])
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
      schemaVersion: 'winwincode.control-plane-publication-policy-rules.v1',
      status: 'implemented-enforced',
      issueId: 'winwincode-9c4.16.3.5',
      decision: 'docs/decisions/0028-control-plane-worker-migration.md',
      documentation: 'docs/contracts/control-plane-publication-policy.md',
      implementationCompletionSource: 'rust-black-box-tests-and-beads',
    },
  )
  assert.deepEqual(contract.authorityChain, [
    'generated-publication-publish-command',
    'authenticated-requester-and-exact-repository-scope',
    'current-sealed-publication-authorization-and-policy-evidence',
    'deterministic-repository-publication-policy',
    'immutable-policy-decision-audit',
    'durable-publication-intent',
    'policy-guarded-provider-resume',
  ])
})

test('the first repository policy is closed and explicit deny has fixed priority', () => {
  assert.deepEqual(rules().policy, {
    ownerCrate: 'winwincode-publication',
    policyType: 'RepositoryPublicationPolicy',
    scope: ['organizationId', 'workspaceId', 'projectId', 'repositoryId'],
    requesterKinds: ['user', 'service_account', 'system'],
    permissions: ['allow', 'deny'],
    evidence: [
      'publicationSetSha256',
      'repositoryScopeSha256',
      'independentVerification',
      'artifactExportable',
      'observedAtMillis',
    ],
    repositoryScopeDigest: 'sha256-of-canonical-repository-policy-scope',
    rulePriority: [
      'publication.requester.denied',
      'publication.approver.denied',
      'publication.repository.write-denied',
      'publication.artifact.export-denied',
      'publication.requester.not-allowed',
      'publication.approver.not-allowed',
      'publication.verification.required',
      'publication.artifact.not-exportable',
      'publication.approval.expired',
      'publication.allowed',
    ],
    explicitDenyOverridesAllow: true,
    policyDigestDeterministic: true,
    staleOrForeignFactsAllowed: false,
    callerPolicyTextRetained: false,
  })

  const policySource = read(
    join(root, 'crates', 'winwincode-publication', 'src', 'policy.rs'),
  )
  const evaluate = functionBlock(policySource, '    pub fn evaluate(')
  assert.ok(
    evaluate.includes(
      'evidence.repository_scope_sha256 != self.scope.sha256()',
    ),
    'policy does not bind sealed evidence to the exact canonical repository scope',
  )
  assertOrdered(evaluate, [
    'self.denied_requesters.contains(requester)',
    'self.denied_approvers.contains(&approver)',
    'self.repository_write == PolicyPermission::Deny',
    'self.artifact_export == PolicyPermission::Deny',
    '!self.allowed_requesters.contains(requester)',
    '!self.allowed_approvers.contains(&approver)',
    'self.require_independent_verification && !evidence.independent_verification',
    '!evidence.artifact_exportable',
    '.saturating_sub(authorization.approved_at_millis())',
    'PublicationPolicyRule::Allowed',
  ])
})

test('publish replays first and every new publish or resume audits before effects', () => {
  assert.deepEqual(rules().decisionAudit, {
    requiredPort: 'PublicationPolicyAudit',
    adapter: 'ControlPlanePolicyAudit-to-AuditStore',
    actionCategory: 'policy',
    stateDigest: 'policySha256',
    results: {
      allow: 'policy.allowed',
      deny: 'policy.denied',
    },
    retainedDecisionFields: [
      'effect',
      'ruleId',
      'policySha256',
      'requester',
      'scope',
      'requestId',
      'origin',
      'deliveryId',
      'publicationId',
      'occurredAtMillis',
      'decisionSha256',
    ],
    rawPolicyTextAllowed: false,
    auditFailure: 'fail-closed-before-intent-or-provider',
  })

  const coordinator = read(
    join(root, 'crates', 'winwincode-publication', 'src', 'coordinator.rs'),
  )
  assert.match(
    coordinator,
    /audit: Box<dyn PublicationPolicyAudit \+ 'audit>/u,
  )
  assert.match(
    coordinator,
    /pub fn new\([\s\S]*audit: Box<dyn PublicationPolicyAudit \+ 'audit>/u,
  )
  assertOrdered(functionBlock(coordinator, '    pub fn publish('), [
    '.replay(',
    'validate_publish(',
    'self.authorize(',
    'Publication::initial(',
    'self.ledger.create(',
  ])
  assertOrdered(functionBlock(coordinator, '    fn authorize('), [
    '.evaluate(',
    '.record(&decision)',
    'decision.effect() == PublicationPolicyEffect::Deny',
  ])
  assertOrdered(functionBlock(coordinator, '    pub fn resume('), [
    'self.ledger.load(',
    'self.authorize_resume(',
    'PublicationState::Pending',
    'self.port.lookup(',
    'self.port.apply(',
  ])
})

test('Control Plane owns the generated command, audit adapter, and public errors', () => {
  const contract = rules()
  assert.deepEqual(contract.commandBoundary, {
    command: 'publication.publish',
    generatedRustType: 'PublicationPublishCommand',
    generatedPayloadFields: [
      'publicationId',
      'deliveryId',
      'candidateDigest',
      'target',
    ],
    publishReceiptReplay: 'before-current-policy-facts-and-audit',
    recoveryResume: 'typed-rust-application-seam',
    httpResumeAdded: false,
    auditQueryAdded: false,
  })
  assert.deepEqual(contract.publicErrors, {
    denied: {
      code: 'PERMISSION_DENIED',
      retryable: false,
      detailFields: ['ruleId', 'repositoryId', 'publicationId'],
    },
    auditUnavailable: {
      code: 'SERVICE_UNAVAILABLE',
      retryable: true,
      detailFields: [],
    },
  })

  const source = read(
    join(root, 'crates', 'winwincode-control-plane', 'src', 'publication_policy.rs'),
  )
  for (const token of [
    'PublicationPublishCommand as ApiPublicationPublishCommand',
    'pub fn commit_publication_publish(',
    'pub fn resume_publication(',
    'ErrorCode::PermissionDenied',
    'ErrorCode::ServiceUnavailable',
    '"ruleId"',
    '"repositoryId"',
    '"publicationId"',
    'ControlPlanePolicyAudit',
    'AuditState::unchanged(Some(decision.policy_sha256().clone()))',
  ]) {
    assert.ok(source.includes(token), `missing Control Plane policy token: ${token}`)
  }
})

test('GitHub and Worker code cannot construct a second Publication effect path', () => {
  assert.deepEqual(rules().providerBoundary, {
    providerPort: 'PublicationPort',
    githubAdapterRole: 'provider-lookup-and-apply-only',
    workerRole: 'call-control-plane-resume-only',
    productionCoordinatorConstructors: [
      'crates/winwincode-control-plane/src/publication_policy.rs',
    ],
    testSupportConstructor: 'crates/winwincode-publication/src/test_support.rs',
    providerMayPersistIntent: false,
    providerMaySkipPolicyAudit: false,
  })

  const constructorFiles = rustFiles(join(root, 'crates'))
    .filter(path => read(path).includes('PublicationCoordinator::new'))
    .map(path => relative(root, path))
    .sort()
  assert.deepEqual(constructorFiles, [
    'crates/winwincode-control-plane/src/publication_policy.rs',
    'crates/winwincode-publication/src/test_support.rs',
  ])

  const github = read(
    join(root, 'crates', 'winwincode-publication', 'src', 'github.rs'),
  )
  assert.match(
    github,
    /impl<Resolver: GitHubCredentialResolver> PublicationPort for GitHubPublicationAdapter<Resolver>/u,
  )
  assert.doesNotMatch(github, /PublicationCoordinator|PublicationLedger/u)
})

test('the gate executes every real Control Plane policy and audit tracer', () => {
  const gate = rules().rustGate
  const output = run(gate.command)
  for (const requiredTest of gate.requiredTests) {
    assert.match(
      output,
      new RegExp(`test ${requiredTest} \\.\\.\\. ok`, 'u'),
      `${gate.package} did not execute ${requiredTest}`,
    )
  }
})

test('documentation states the same fail-closed policy and adapter boundary', () => {
  const documentation = read(documentationPath)
  for (const statement of [
    'generated `publication.publish`',
    '显式 deny 固定优先于 allow',
    'canonical\n`RepositoryPolicyScope` JSON 的 SHA-256',
    '任何 Publication intent 之前',
    '任何 provider lookup 或 apply 之前',
    '先写入不可变 AuditStore',
    '`PERMISSION_DENIED`',
    '`SERVICE_UNAVAILABLE`',
    'exact receipt replay',
    '`GitHubPublicationAdapter` 只实现 `PublicationPort`',
    '不新增 HTTP resume 或 Audit query',
  ]) {
    assert.ok(
      documentation.includes(statement),
      `documentation is missing: ${statement}`,
    )
  }
})
