import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

import {
  DELIVERY_STAGES,
  STRONGFLOW_ROLE_IDS,
} from '../packages/contracts/dist/index.js'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const documentPath = resolve(root, 'docs/architecture.md')
const documentText = readFileSync(documentPath, 'utf8')

const canonicalObjects = Object.freeze([
  'Delivery',
  'DeliverySpec',
  'AcceptanceCriterion',
  'DeliveryTask',
  'StageRun',
  'SessionBinding',
  'AttentionItem',
  'EvidenceRef',
  'CriterionResult',
  'DeliveryVerdict',
])

function relativeMarkdownLinks(text) {
  return [...text.matchAll(/\[[^\]]+\]\(([^)]+)\)/gu)]
    .map(match => match[1])
    .filter(target => !/^(?:https?:|#)/u.test(target))
    .map(target => decodeURIComponent(target.split('#', 1)[0]))
}

test('architecture guide names the canonical owners, objects, roles and stages', () => {
  for (const owner of [
    'Codex Core',
    'WinWinCode',
    'TypeScript Presentation Layer',
    'winwincode-server',
    'winwincode-control-plane',
    'winwincode-worker',
    'winwincode-local',
    'winwincode-kernel-helper',
  ]) assert.equal(documentText.includes(owner), true, owner)
  for (const objectName of canonicalObjects) {
    assert.equal(documentText.includes(`| \`${objectName}\` |`), true, objectName)
  }
  assert.equal(canonicalObjects.length, 10)
  for (const role of STRONGFLOW_ROLE_IDS) {
    assert.equal(documentText.includes(`| \`${role}\` |`), true, role)
  }
  for (const stage of DELIVERY_STAGES) {
    assert.equal(documentText.includes(`\`${stage}\``), true, stage)
  }
})

test('architecture guide keeps diagrams, approval boundaries and evidence sources explicit', () => {
  assert.equal((documentText.match(/```mermaid/gu) ?? []).length, 2)
  for (const state of ['before-execution', 'executing', 'execution-finished']) {
    assert.equal(documentText.includes(`\`${state}\``), true, state)
  }
  for (const statement of [
    'Codex Plan',
    'Attention',
    'requestId',
    'expectedRevision',
    'winwincode.independent-verification-result.v1',
    'Agent 的文本回复不是交付证据',
    'attempt+1',
    '旧 attempt 的 runtime、outcome 和 cancel 记录',
  ]) assert.equal(documentText.includes(statement), true, statement)
})

test('architecture guide fixes the accepted Client, Server, Control Plane and Worker target', () => {
  for (const boundary of [
    'TypeScript Presentation Layer',
    'Rust Control Plane',
    'ExecutionPort',
    'Rust Execution Worker',
    'winwincode-server',
    'winwincode-kernel-helper',
    'Codex Core',
  ]) assert.equal(documentText.includes(boundary), true, boundary)

  for (const sessionKind of [
    'ProductSession',
    'WorkerSession',
    'CodexThread',
    'StageRun',
  ]) assert.equal(documentText.includes(sessionKind), true, sessionKind)

  for (const rule of [
    'Server 是唯一公开网络边界',
    '本地部署',
    '企业部署',
    'requestId',
    'expectedRevision',
    'WebSocket',
  ]) assert.equal(documentText.includes(rule), true, rule)

  assert.equal(documentText.includes('0028-control-plane-worker-migration.md'), true)
  assert.equal(documentText.includes('0028-control-plane-worker-migration.inventory.json'), true)
  assert.equal(documentText.includes('0028-control-plane-worker-target-graph.json'), true)
})

test('every repository-local link in the architecture guide resolves', () => {
  const links = relativeMarkdownLinks(documentText)
  assert.ok(links.length >= 30)
  for (const target of links) {
    assert.equal(
      existsSync(resolve(dirname(documentPath), target)),
      true,
      `missing documentation link: ${target}`,
    )
  }
})
