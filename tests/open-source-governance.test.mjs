import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

import {
  PRODUCT_PACKAGE_DIRECTORIES,
  releaseSourcePaths,
  verifyReleaseLegalBoundary,
} from '../scripts/release-source-contract.mjs'
import {
  assertProductVersion,
  setProductVersion,
} from '../scripts/set-product-version.mjs'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')

function read(path) {
  return readFileSync(join(root, path), 'utf8')
}

function relativeMarkdownLinks(text) {
  return [...text.matchAll(/\[[^\]]+\]\(([^)]+)\)/gu)]
    .map(match => match[1])
    .filter(target => !/^(?:https?:|#)/u.test(target))
    .map(target => decodeURIComponent(target.split('#', 1)[0]))
}

test('public contribution, security, conduct, release, and upstream guides are linked', () => {
  const documents = [
    'README.md',
    'CONTRIBUTING.md',
    'SECURITY.md',
    'CODE_OF_CONDUCT.md',
    'docs/releasing.md',
    'docs/releases/0.1.0-alpha.1.md',
    'docs/upstream-updates.md',
    '.github/pull_request_template.md',
  ]
  for (const document of documents) assert.equal(existsSync(join(root, document)), true, document)
  for (const document of documents) {
    const text = read(document)
    for (const target of relativeMarkdownLinks(text)) {
      assert.equal(
        existsSync(resolve(dirname(join(root, document)), target)),
        true,
        `${document}: ${target}`,
      )
    }
  }

  const readme = read('README.md')
  for (const target of documents.slice(1, 7)) {
    assert.equal(readme.includes(`](${target})`), true, `README does not link ${target}`)
  }
})

test('contribution guide provides the Beads path, exact checks, and one Delivery migration path', () => {
  const guide = read('CONTRIBUTING.md')
  for (const command of [
    'bd prime',
    'bd ready',
    'bd show ISSUE_ID',
    'bd update ISSUE_ID --claim',
    'corepack pnpm install --frozen-lockfile',
    'corepack pnpm typecheck',
    'corepack pnpm test',
    'corepack pnpm lint',
    'corepack pnpm build',
    'corepack pnpm verify',
  ]) assert.equal(guide.includes(command), true, command)
  for (const policy of [
    'DELIVERY_SCHEMA_VERSION = 3',
    '离线、一次性迁移程序',
    '只接收上一受支持版本并输出当前版本',
    '原副本保留为回滚点',
    '不保留双读、双写、静默回退或长期适配器',
  ]) assert.equal(guide.includes(policy), true, policy)
})

test('upstream guide has independent Codex and DSH checks with concrete rollback points', () => {
  const guide = read('docs/upstream-updates.md')
  for (const command of [
    '--codex-root third_party/codex',
    '--codex-archive "$CODEX_ARCHIVE"',
    '--dsh-root "$DSH_CANDIDATE"',
    '--dsh-archive "$DSH_ARCHIVE"',
    'corepack pnpm verify:upstream',
    'corepack pnpm verify:installed-host',
    'corepack pnpm fixture:delivery',
    'corepack pnpm verify',
  ]) assert.equal(guide.includes(command), true, command)
  assert.match(guide, /回滚点 A/gu)
  assert.match(guide, /回滚点 B/gu)
  assert.match(guide, /回滚点 C/gu)
  for (const path of [
    'third_party/codex/',
    'third_party/codex.UPSTREAM.json',
    'packages/dsh-profile/cordis.patch.yml',
    'pnpm-lock.yaml',
    'upstream/sources.lock.json',
  ]) assert.equal(guide.includes(path), true, path)
})

test('release guide fixes one version, four native lanes, ten package order, and rollback', () => {
  const guide = read('docs/releasing.md')
  for (const marker of [
    'corepack pnpm version:set 0.1.0-alpha.1',
    'aarch64-apple-darwin',
    'x86_64-apple-darwin',
    'aarch64-unknown-linux-gnu',
    'x86_64-unknown-linux-gnu',
    'corepack pnpm verify:release',
    '@winwincode/contracts',
    '@winwincode/native-darwin-arm64',
    '@winwincode/native-darwin-x64',
    '@winwincode/native-linux-arm64',
    '@winwincode/native-linux-x64',
    '@winwincode/native',
    '@winwincode/strongflow',
    '@winwincode/dsh-profile',
    '@winwincode/client',
    '10. winwincode',
    'npm deprecate PACKAGE@VERSION',
  ]) assert.equal(guide.includes(marker), true, marker)
})

test('documented pnpm release commands forward script options without a separator token', () => {
  const commands = [
    {
      script: 'evaluate:live',
      arguments: [
        '--live',
        '--config',
        '/definitely/missing/winwincode-live.json',
        '--output',
        '/tmp/winwincode-live-output',
      ],
      parsedFailure: '"code":"ENOENT"',
    },
    {
      script: 'measure:evaluation',
      arguments: [
        '--result',
        '/definitely/missing/winwincode-result.json',
        '--check',
      ],
      parsedFailure: 'ENOENT',
    },
    {
      script: 'verify:release',
      arguments: [
        '--expected-commit',
        '0000000000000000000000000000000000000000',
        '--native-evidence',
        '/definitely/missing/winwincode-native',
        '--live-evaluation',
        '/definitely/missing/winwincode-live-result.json',
        '--output',
        '/tmp/winwincode-release-report.json',
      ],
      parsedFailure: '"code":"RELEASE_GATE_FAILED"',
    },
  ]

  for (const command of commands) {
    const result = spawnSync(
      'corepack',
      ['pnpm', command.script, ...command.arguments],
      { cwd: root, encoding: 'utf8' },
    )
    assert.equal(result.error, undefined, command.script)
    assert.equal(result.status, 1, command.script)
    const output = `${result.stdout}${result.stderr}`
    assert.equal(output.includes('unexpected argument: --'), false, output)
    assert.equal(output.includes(command.parsedFailure), true, output)
  }
})

test('current release notes match the package version and public alpha scope', () => {
  const manifest = JSON.parse(read('package.json'))
  const path = `docs/releases/${manifest.version}.md`
  assert.equal(existsSync(join(root, path)), true)
  const notes = read(path)
  for (const marker of [
    `# WinWinCode ${manifest.version}`,
    'DELIVERY_SCHEMA_VERSION = 3',
    'aarch64-apple-darwin',
    'x86_64-apple-darwin',
    'aarch64-unknown-linux-gnu',
    'x86_64-unknown-linux-gnu',
    '私有漏洞报告',
  ]) assert.equal(notes.includes(marker), true, marker)
})

test('security policy directs reports to the enabled private advisory channel', () => {
  const policy = read('SECURITY.md')
  assert.equal(
    policy.includes('https://github.com/changw98ic/winwincode/security/advisories/new'),
    true,
  )
  assert.match(policy, /已经启用 Private Vulnerability Reporting/u)
  assert.match(policy, /不要通过公开 Issue、Pull Request、Discussion/u)
  assert.match(policy, /不需要先公开问题/u)
})

test('release source and package metadata retain the Apache-2.0 project boundary', () => {
  const sourcePaths = releaseSourcePaths(root)
  for (const path of ['CONTRIBUTING.md', 'SECURITY.md', 'CODE_OF_CONDUCT.md']) {
    assert.equal(sourcePaths.includes(path), true, path)
  }
  assert.deepEqual(verifyReleaseLegalBoundary(root), [])
  const manifests = [
    JSON.parse(read('package.json')),
    ...PRODUCT_PACKAGE_DIRECTORIES.map(directory => (
      JSON.parse(read(`${directory}/package.json`))
    )),
  ]
  const version = manifests[0].version
  assert.equal(manifests.length, 11)
  assert.equal(manifests.every(manifest => manifest.version === version), true)
  assert.equal(manifests.every(manifest => manifest.license === 'Apache-2.0'), true)
  assert.match(read('LICENSE'), /Apache License\s+Version 2\.0/u)
  assert.match(read('THIRD_PARTY_NOTICES.md'), /DeepSeek Harness and Ratatui MIT terms/u)
  assert.match(read('THIRD_PARTY_NOTICES.md'), /Permission is hereby granted/u)
})

test('product version command updates every manifest and rejects invalid versions', () => {
  assert.doesNotThrow(() => assertProductVersion('1.2.3'))
  assert.doesNotThrow(() => assertProductVersion('0.4.0-rc.2+build.7'))
  assert.throws(() => assertProductVersion('01.2.3'), /invalid semantic version/u)
  assert.throws(() => assertProductVersion('1.2'), /invalid semantic version/u)

  const fixture = mkdtempSync(join(tmpdir(), 'winwincode-version-'))
  try {
    const directories = ['.', ...PRODUCT_PACKAGE_DIRECTORIES]
    for (const [index, directory] of directories.entries()) {
      mkdirSync(join(fixture, directory), { recursive: true })
      writeFileSync(join(fixture, directory, 'package.json'), `${JSON.stringify({
        name: index === 0 ? '@winwincode/workspace' : `fixture-${String(index)}`,
        version: '0.0.0-dev.0',
        license: 'Apache-2.0',
      }, null, 2)}\n`)
    }
    const updated = setProductVersion(fixture, '1.2.3-rc.1')
    assert.equal(updated.length, 11)
    for (const directory of directories) {
      const manifest = JSON.parse(
        readFileSync(join(fixture, directory, 'package.json'), 'utf8'),
      )
      assert.equal(manifest.version, '1.2.3-rc.1')
    }
  } finally {
    rmSync(fixture, { recursive: true, force: true })
  }
})
