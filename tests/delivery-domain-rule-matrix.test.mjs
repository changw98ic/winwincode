import assert from 'node:assert/strict'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { extname, join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const matrixPath = join(root, 'docs', 'contracts', 'delivery-domain-rules.v1.json')

const REQUIRED_RULE_IDS = Object.freeze([
  'attention.delivered_rejects_open_blocker',
  'attention.needs_attention_requires_open_blocker',
  'attention.pass_rejects_open_non_approval_blocker',
  'evidence.current_candidate',
  'evidence.current_spec_revision',
  'evidence.current_stage_run',
  'evidence.current_stage_run_session_binding',
  'evidence.not_before_run_or_binding',
  'spec.required_acceptance_criterion',
  'stage_run.rework_attempt_limit',
  'stage_run.rework_requires_codex_remediator',
  'stage_run.status_finish_time',
  'status.ready_or_delivered_requires_completed_tasks',
  'status.ready_or_delivered_requires_pass',
  'store.concurrent_revision_publish',
  'store.corruption_rejected',
  'store.digest_chain',
  'store.expected_revision',
  'store.pending_record_recovery',
  'store.request_id_idempotency',
  'task.acceptance_criteria_current',
  'task.dependency_acyclic',
  'task.dependency_exists',
  'task.dependency_not_self',
  'verdict.all_criteria_exactly_once',
  'verdict.pass_or_fail_requires_evidence',
  'verdict.required_result_fold',
  'session_binding.has_session_identity',
  'session_binding.matches_delivery_run_and_actor',
].sort())

const REQUIRED_FINDING_IDS = Object.freeze([
  'contract.error_code_mapping',
  'contract.identifier_format',
  'contract.missing_internal_ids',
  'contract.schema_version',
  'contract.session_identities',
  'contract.timestamp_format',
].sort())

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(relativePath) {
  assert.equal(relativePath.startsWith('/'), false, `${relativePath} must be repository-relative`)
  assert.equal(relativePath.split('/').includes('..'), false, `${relativePath} must not escape the repository`)
  return join(root, relativePath)
}

function rustFiles(directory) {
  const result = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) result.push(...rustFiles(path))
    else if (entry.isFile() && extname(entry.name) === '.rs') result.push(path)
  }
  return result
}

function assertPublicSymbol(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.match(
    source,
    new RegExp(`export\\s+(?:const|type|class|interface|function)\\s+${mapping.name}\\b`, 'u'),
    `${mapping.path} does not export ${mapping.name}`,
  )
}

function assertTestCase(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.equal(
    source.includes(`test('${mapping.name}'`) || source.includes(`test("${mapping.name}"`),
    true,
    `${mapping.path} does not define test ${mapping.name}`,
  )
}

test('Delivery Domain rule matrix covers the frozen TypeScript and target Rust seams', () => {
  const matrix = json(matrixPath)
  assert.equal(matrix.schemaVersion, 'winwincode.delivery-domain-rule-matrix.v1')
  assert.equal(matrix.issueId, 'winwincode-9c4.16.2.2')
  assert.equal(matrix.typescriptSourceOfTruth, 'packages/contracts/src/delivery.ts')
  assert.equal(matrix.rustTarget.crate, 'crates/winwincode-delivery')

  const ruleIds = matrix.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), REQUIRED_RULE_IDS)

  for (const rule of matrix.rules) {
    assert.match(rule.id, /^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$/u)
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    assert.ok(rule.adrRefs.length > 0, `${rule.id} needs an accepted-decision reference`)
    for (const path of rule.adrRefs) assert.equal(existsSync(repositoryPath(path)), true)

    assert.ok(rule.typescript.publicSymbols.length > 0, `${rule.id} needs a public TypeScript seam`)
    for (const mapping of rule.typescript.publicSymbols) assertPublicSymbol(mapping)
    assert.ok(
      ['covered', 'partial', 'implementation_only'].includes(rule.typescript.coverage),
      `${rule.id} has an unknown TypeScript coverage state`,
    )
    if (rule.typescript.coverage === 'implementation_only') {
      assert.deepEqual(rule.typescript.tests, [])
      assert.ok(rule.typescript.gap.length > 0, `${rule.id} must explain its missing old test`)
    } else {
      assert.ok(rule.typescript.tests.length > 0, `${rule.id} needs an existing TypeScript test`)
      for (const mapping of rule.typescript.tests) assertTestCase(mapping)
    }

    assert.match(rule.rust.module, /^(?:domain\/[a-z_]+|store)\.rs$/u)
    assert.match(rule.rust.testName, /^[a-z][a-z0-9_]+$/u)
  }
})

test('Delivery Domain contract differences stay explicit instead of changing phase-one schemas', () => {
  const matrix = json(matrixPath)
  const findingIds = matrix.contractFindings.map(finding => finding.id)
  assert.equal(new Set(findingIds).size, findingIds.length)
  assert.deepEqual([...findingIds].sort(), REQUIRED_FINDING_IDS)
  for (const finding of matrix.contractFindings) {
    assert.ok(['conflict', 'missing_definition', 'mapping_required'].includes(finding.kind))
    assert.ok(finding.current.length > 0)
    assert.ok(finding.target.length > 0)
    assert.ok(finding.action.length > 0)
    for (const path of finding.refs) assert.equal(existsSync(repositoryPath(path)), true)
  }
})

test('merged Rust Delivery crate implements every named rule test in its assigned module', () => {
  const matrix = json(matrixPath)
  const crateRoot = repositoryPath(matrix.rustTarget.crate)
  if (!existsSync(crateRoot)) {
    assert.equal(matrix.rustTarget.scanWhenPresent, true)
    return
  }

  const sources = rustFiles(crateRoot)
  const allRust = sources.map(path => readFileSync(path, 'utf8')).join('\n')
  for (const rule of matrix.rules) {
    assert.equal(
      existsSync(join(crateRoot, 'src', rule.rust.module)),
      true,
      `${rule.id} target module ${rule.rust.module} is missing`,
    )
    assert.match(
      allRust,
      new RegExp(`\\bfn\\s+${rule.rust.testName}\\s*\\(`, 'u'),
      `${rule.id} target test ${rule.rust.testName} is missing`,
    )
  }
})
