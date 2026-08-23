import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

import {
  DELIVERY_STAGES,
  STRONGFLOW_ROLE_IDS,
} from '../packages/contracts/dist/index.js'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const documentPath = join(root, 'docs', 'architecture.md')
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

test('architecture guide names the canonical owners, objects, roles, and stages', () => {
  for (const owner of ['Codex Core', 'DSH', 'WinWinCode']) {
    assert.equal(documentText.includes(owner), true)
  }
  for (const objectName of canonicalObjects) {
    assert.equal(documentText.includes(`| \`${objectName}\` |`), true)
  }
  assert.equal(canonicalObjects.length, 10)
  for (const role of STRONGFLOW_ROLE_IDS) {
    assert.equal(documentText.includes(`| \`${role}\` |`), true)
  }
  for (const stage of DELIVERY_STAGES) {
    assert.equal(documentText.includes(`\`${stage}\``), true)
  }
})

test('architecture guide keeps diagrams, approval boundaries, and evidence sources explicit', () => {
  assert.equal((documentText.match(/```mermaid/gu) ?? []).length, 2)
  for (const state of ['before-execution', 'executing', 'execution-finished']) {
    assert.equal(documentText.includes(`\`${state}\``), true)
  }
  for (const statement of [
    'Codex Plan',
    '业务 Attention',
    '执行审批',
    'requestId',
    '预期 Delivery revision',
    'winwincode.independent-verification-result.v1',
    'Agent 的“已经完成”消息本身不构成交付证据',
  ]) assert.equal(documentText.includes(statement), true)
})

test('every repository-local link in the architecture guide resolves', () => {
  const links = relativeMarkdownLinks(documentText)
  assert.ok(links.length >= 40)
  for (const target of links) {
    assert.equal(
      existsSync(resolve(dirname(documentPath), target)),
      true,
      `missing documentation link: ${target}`,
    )
  }
})
