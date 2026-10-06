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
} from '../scripts/lib/release-source-contract.mjs'
import {
  assertProductVersion,
  setProductVersion,
} from '../scripts/release/set-product-version.mjs'

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

test('documented pnpm release commands forward script options without a separator token', () => {
  const commands = [
    {
      script: 'release:artifact',
      arguments: [
        '--target',
        'x86_64-pc-windows-msvc',
        '--source-commit',
        '0000000000000000000000000000000000000000',
        '--source-date-epoch',
        '1700000000',
        '--output',
        '/tmp/winwincode-release-artifacts',
      ],
      parsedFailure: 'TARGET_UNSUPPORTED',
    },
    {
      script: 'verify:release-artifacts',
      arguments: [
        '--expected-commit',
        '0000000000000000000000000000000000000000',
        '--source-date-epoch',
        '1700000000',
        '--evidence',
        '/definitely/missing/winwincode-release-artifacts',
        '--output',
        '/tmp/winwincode-release-report.json',
      ],
      parsedFailure: 'ARTIFACT_MISSING',
    },
    {
      script: 'verify:release-artifact-security',
      arguments: [
        '--expected-commit',
        '0000000000000000000000000000000000000000',
        '--source-date-epoch',
        '1700000000',
        '--evidence',
        '/definitely/missing/winwincode-release-artifacts',
        '--output',
        '/tmp/winwincode-release-artifact-security-report.json',
      ],
      parsedFailure: 'ARTIFACT_MISSING',
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

test('release source and package metadata retain the Apache-2.0 project boundary', () => {
  const sourcePaths = releaseSourcePaths(root)
  for (const path of ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES']) {
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
  assert.equal(manifests.length, PRODUCT_PACKAGE_DIRECTORIES.length + 1)
  assert.equal(manifests.every(manifest => manifest.version === version), true)
  assert.equal(manifests.every(manifest => manifest.license === 'Apache-2.0'), true)
  assert.match(read('LICENSE'), /Apache License\s+Version 2\.0/u)
  assert.match(
    read('THIRD_PARTY_NOTICES'),
    /Ratatui and DeepSeek Harness MIT terms/u,
  )
  assert.match(
    read('THIRD_PARTY_NOTICES'),
    /Client incorporates adapted DeepSeek Harness frontend components/u,
  )
  assert.match(read('THIRD_PARTY_NOTICES'), /Permission is hereby granted/u)
})

test('upstream records distinguish current dependencies from historical attribution', () => {
  const sourceLock = JSON.parse(read('upstream/sources.lock.json'))
  assert.equal(Object.hasOwn(sourceLock, 'dsh'), false)
  assert.equal(sourceLock.patches.some(({ id }) => id === 'dsh-winwincode-profile'), false)
  assert.deepEqual(sourceLock.historicalSources, [
    {
      id: 'deepseek-harness-pre-cutover-evaluation',
      repository: 'https://github.com/deepseek-ai/deepseek-harness',
      tag: 'dsh-v0.1.0-rc.8',
      version: '0.1.0-rc.8',
      commit: '141eb6fef83422698aef7a981029e843e8161534',
      archiveSha256: '46fb9d6f103bb7033d066de637069918012a4986aa1f53de793f1f5bdb2d95f1',
      license: 'MIT',
      status: 'historical-attribution-only',
      distributedInCurrentProduct: false,
      workspaceDependency: false,
    },
  ])

  const currentRecords = JSON.stringify({
    codex: sourceLock.codex,
    vendoredCargoSources: sourceLock.vendoredCargoSources,
    patches: sourceLock.patches,
  })
  for (const obsolete of ['apps/host', 'packages/dsh-profile', 'packages/native', 'crates/native']) {
    assert.equal(currentRecords.includes(obsolete), false, obsolete)
  }

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
        ...(index === 1
          ? { dependencies: { '@winwincode/contracts': '0.0.0-dev.0' } }
          : {}),
        ...(index === 0
          ? { devDependencies: { '@winwincode/browser-core': '0.0.0-dev.0' } }
          : {}),
      }, null, 2)}\n`)
    }
    writeFileSync(join(fixture, 'Cargo.toml'), [
      '[workspace.package]',
      'version = "0.0.0-dev.0"',
      '',
      '[workspace.dependencies]',
      'winwincode-domain = { version = "=0.0.0-dev.0", path = "crates/winwincode-domain" }',
      '',
    ].join('\n'))
    mkdirSync(join(fixture, 'crates/winwincode-delivery'), { recursive: true })
    writeFileSync(join(fixture, 'crates/winwincode-delivery/Cargo.toml'), [
      '[dependencies]',
      'winwincode-storage = { version = "0.0.0-dev.0", path = "../winwincode-storage" }',
      '',
    ].join('\n'))
    const updated = setProductVersion(fixture, '1.2.3-rc.1')
    assert.equal(updated.length, PRODUCT_PACKAGE_DIRECTORIES.length + 3)
    for (const directory of directories) {
      const manifest = JSON.parse(
        readFileSync(join(fixture, directory, 'package.json'), 'utf8'),
      )
      assert.equal(manifest.version, '1.2.3-rc.1')
      for (const [name, dependencyVersion] of Object.entries(manifest.dependencies ?? {})) {
        if (name.startsWith('@winwincode/')) {
          assert.equal(dependencyVersion, '1.2.3-rc.1')
        }
      }
      for (const [name, dependencyVersion] of Object.entries(manifest.devDependencies ?? {})) {
        if (name.startsWith('@winwincode/')) {
          assert.equal(dependencyVersion, '1.2.3-rc.1')
        }
      }
    }
    assert.match(
      readFileSync(join(fixture, 'Cargo.toml'), 'utf8'),
      /\[workspace\.package\]\nversion = "1\.2\.3-rc\.1"/u,
    )
    assert.match(
      readFileSync(join(fixture, 'Cargo.toml'), 'utf8'),
      /winwincode-domain = \{ version = "=1\.2\.3-rc\.1"/u,
    )
    assert.match(
      readFileSync(join(fixture, 'crates/winwincode-delivery/Cargo.toml'), 'utf8'),
      /winwincode-storage = \{ version = "1\.2\.3-rc\.1"/u,
    )
  } finally {
    rmSync(fixture, { recursive: true, force: true })
  }
})
