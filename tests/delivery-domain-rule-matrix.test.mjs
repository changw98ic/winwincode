import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
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
  'session_binding.required_product_and_job_identities',
  'session_binding.matches_delivery_stage_run_and_task',
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

function assertPublicSymbol(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.match(
    source,
    new RegExp(`export\\s+(?:const|type|class|interface|function)\\s+${mapping.name}\\b`, 'u'),
    `${mapping.path} does not export ${mapping.name}`,
  )
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')
}

test('Delivery Domain rule matrix covers the transitional TypeScript and canonical Rust seams', () => {
  const matrix = json(matrixPath)
  assert.equal(matrix.schemaVersion, 'winwincode.delivery-domain-rule-matrix.v1')
  assert.equal(matrix.issueId, 'winwincode-9c4.16.2.2')
  assert.equal(matrix.status, 'implemented-enforced')
  assert.equal(matrix.transitionalTypescriptSeam, 'packages/contracts/src/delivery.ts')
  assert.equal(matrix.canonicalRustAuthority.crate, 'crates/winwincode-delivery')
  assert.equal(matrix.canonicalRustAuthority.status, 'implemented-enforced')
  assert.equal(
    matrix.canonicalRustAuthority.decision,
    'docs/decisions/0023-canonical-delivery-ownership.md',
  )
  const decision = readFileSync(repositoryPath(matrix.canonicalRustAuthority.decision), 'utf8')
  assert.match(decision, /规范 Rust Delivery 领域与存储/u)
  assert.match(decision, /过渡期 TypeScript 产品路径/u)
  assert.match(decision, /winwincode-9c4\.16\.6\.3/u)
  assert.match(decision, /winwincode-9c4\.16\.6\.6/u)
  assert.equal(Object.hasOwn(matrix, 'typescriptSourceOfTruth'), false)
  assert.equal(Object.hasOwn(matrix, 'rustTarget'), false)

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
      for (const mapping of rule.typescript.tests) {
        assert.equal(existsSync(repositoryPath(mapping.path)), true, mapping.path)
        assert.ok(mapping.name.length > 0, `${rule.id} needs a TypeScript test name`)
      }
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
  const identifierFinding = matrix.contractFindings.find(finding => (
    finding.id === 'contract.identifier_format'
  ))
  assert.equal(identifierFinding.status, 'closed')
  assert.ok(identifierFinding.evidence.length >= 4)
  for (const path of identifierFinding.evidence) {
    assert.equal(existsSync(repositoryPath(path)), true, path)
  }
})

test('transitional TypeScript seam executes every mapped behavior test', () => {
  const matrix = json(matrixPath)
  const mappings = matrix.rules.flatMap(rule => rule.typescript.tests)
  const paths = [...new Set(mappings.map(mapping => repositoryPath(mapping.path)))]
  const environment = { ...process.env }
  delete environment.NODE_TEST_CONTEXT
  const result = spawnSync(process.execPath, [
    '--test',
    '--test-reporter=spec',
    ...paths,
  ], {
    cwd: root,
    encoding: 'utf8',
    env: environment,
    maxBuffer: 32 * 1_024 * 1_024,
  })
  const output = `${result.stdout ?? ''}${result.stderr ?? ''}`
  assert.equal(result.error, undefined, output)
  assert.equal(result.signal, null, output)
  assert.equal(result.status, 0, output)
  for (const mapping of mappings) {
    assert.match(
      output,
      new RegExp(`✔ ${escapeRegExp(mapping.name)} \\(`, 'u'),
      `${mapping.path} test ${mapping.name} did not execute successfully`,
    )
  }
})

test('canonical Rust Delivery authority executes every named rule test', () => {
  const matrix = json(matrixPath)
  const crateRoot = repositoryPath(matrix.canonicalRustAuthority.crate)
  assert.equal(existsSync(crateRoot), true)

  for (const rule of matrix.rules) {
    assert.equal(
      existsSync(join(crateRoot, 'src', rule.rust.module)),
      true,
      `${rule.id} target module ${rule.rust.module} is missing`,
    )
  }

  const result = spawnSync('cargo', [
    'test',
    '-p',
    'winwincode-delivery',
    '--lib',
    '--locked',
    '--',
    '--test-threads=1',
  ], {
    cwd: root,
    encoding: 'utf8',
    env: process.env,
    maxBuffer: 32 * 1_024 * 1_024,
  })
  const output = `${result.stdout ?? ''}${result.stderr ?? ''}`
  assert.equal(result.error, undefined, output)
  assert.equal(result.signal, null, output)
  assert.equal(result.status, 0, output)
  for (const rule of matrix.rules) {
    assert.match(
      output,
      new RegExp(`test [^\\n]*\\b${escapeRegExp(rule.rust.testName)} \\.\\.\\. ok`, 'u'),
      `${rule.id} Rust test ${rule.rust.testName} did not execute successfully`,
    )
  }
})
