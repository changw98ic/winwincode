import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

import { runKeylessDeliveryFixture } from '../scripts/run-keyless-delivery-fixture.mjs'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const readmePath = join(root, 'README.md')
const readme = readFileSync(readmePath, 'utf8')
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'))

function relativeMarkdownLinks(text) {
  return [...text.matchAll(/\[[^\]]+\]\(([^)]+)\)/gu)]
    .map(match => match[1])
    .filter(target => !/^(?:https?:|#)/u.test(target))
    .map(target => decodeURIComponent(target.split('#', 1)[0]))
}

test('README exposes one canonical source quickstart and the exact product entrypoints', () => {
  assert.equal((readme.match(/^## 快速开始$/gmu) ?? []).length, 1)
  for (const command of [
    'corepack pnpm install --frozen-lockfile',
    'corepack pnpm build',
    'corepack pnpm fixture:delivery',
    'corepack pnpm start',
    'corepack pnpm start web --no-open --port 3000',
    'corepack pnpm verify:installed-host',
  ]) assert.equal(readme.includes(command), true, `README is missing ${command}`)

  assert.equal(manifest.scripts.start, 'node apps/host/dist/cli.js')
  assert.equal(readme.includes(`\`${manifest.version}\``), true)
  assert.equal(existsSync(join(root, 'docs', 'releases', `${manifest.version}.md`)), true)
  assert.equal(readme.includes(`git clone ${manifest.repository}.git`), true)
  assert.equal(
    manifest.scripts['fixture:delivery'],
    'node scripts/run-keyless-delivery-fixture.mjs',
  )
  assert.match(readme, /默认入口：DSH Chat/u)
  assert.match(readme, /高级入口：StrongFlow/u)
  assert.match(readme, /Windows 尚未进入首发平台/u)
  assert.match(readme, /本机单用户 Host/u)
})

test('README repository-local links resolve', () => {
  const links = relativeMarkdownLinks(readme)
  assert.ok(links.length >= 10)
  for (const target of links) {
    assert.equal(existsSync(resolve(dirname(readmePath), target)), true, target)
  }
})

test('documented keyless fixture reaches human review before execution and ends with evidence', {
  timeout: 90_000,
}, async () => {
  const result = await runKeylessDeliveryFixture()
  assert.equal(result.finalStatus, 'delivered')
  assert.deepEqual(result.humanGate, {
    statusBeforeDecision: 'needs-attention',
    reviewStageStatus: 'waiting',
    executionStageCountBeforeDecision: 0,
    scriptedFirstDecision: 'request_changes',
    revisedPlanApproved: true,
  })
  assert.equal(result.criterionResults.length > 0, true)
  assert.equal(result.criterionResults.every(entry => entry.verdict === 'pass'), true)
  assert.equal(result.deliveryVerdict.status, 'pass')
  assert.deepEqual(result.deliveryVerdict.unresolvedFindings, [])
  assert.equal(result.evidenceCount > 0, true)
  assert.deepEqual(result.credentialNames, [])
})
