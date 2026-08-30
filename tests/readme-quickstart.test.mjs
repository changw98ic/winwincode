import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

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

test('README exposes the one Client/Server/Worker/Local/Helper quickstart', () => {
  assert.equal((readme.match(/^## 快速开始$/gmu) ?? []).length, 1)
  for (const command of [
    'corepack pnpm install --frozen-lockfile',
    'corepack pnpm build',
    'corepack pnpm start',
    'corepack pnpm verify:api-production-vertical',
    'corepack pnpm contracts:check',
    'corepack pnpm format:check',
  ]) assert.equal(readme.includes(command), true, `README is missing ${command}`)

  assert.equal(readme.includes(`\`${manifest.version}\``), true)
  assert.equal(existsSync(join(root, 'docs', 'releases', `${manifest.version}.md`)), true)
  assert.equal(readme.includes(`git clone ${manifest.repository}.git`), true)
  for (const path of [
    'apps/client',
    'crates/winwincode-server',
    'crates/winwincode-control-plane',
    'crates/winwincode-worker',
    'crates/winwincode-local',
    'crates/helper',
  ]) assert.equal(readme.includes(path), true, `README is missing ${path}`)
  for (const oldPath of [
    'apps/host',
    'apps/web',
    'packages/dsh-profile',
    'packages/native',
    'crates/native',
  ]) assert.equal(readme.includes(oldPath), false, oldPath)
  assert.doesNotMatch(readme, /DSH|Cordis|N-API/iu)
})

test('README repository-local links resolve', () => {
  const links = relativeMarkdownLinks(readme)
  assert.ok(links.length >= 10)
  for (const target of links) {
    assert.equal(existsSync(resolve(dirname(readmePath), target)), true, target)
  }
})
