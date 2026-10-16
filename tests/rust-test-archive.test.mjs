import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { cpSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { consumeArchive, laneFilter, produceArchive, testIds } from '../scripts/rust-test-archive.mjs'
import { sha256 } from '../scripts/product-build-contract.mjs'

const suite = (id, names) => ({ 'binary-id': id, 'package-name': id, testcases: Object.fromEntries(names.map(name => [name, { 'filter-match': { status: 'matches' } }])) })

test('archive inventory retains the original heavy and standard selectors and every matching test ID', () => {
  assert.equal(laneFilter('e2e'), 'binary(production_vertical) | binary(worker_lifecycle) | binary(code_mode_kernel)')
  assert.equal(laneFilter('test'), `not (${laneFilter('e2e')})`)
  const list = { 'rust-suites': { a: suite('standard', ['one', 'two']), b: suite('winwincode-kernel-helper', ['helper']) } }
  assert.deepEqual(testIds(list, true), ['standard::one', 'standard::two'])
  assert.equal(testIds(list).length, 3)
})

test('archive consumer fails on missing manifest before any compilation fallback', t => {
  const root = mkdtempSync(join(tmpdir(), 'wwc-missing-archive-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  assert.throws(() => consumeArchive(root, root, 'test'), /ENOENT/u)
})

if (process.env.WWC_NEXTEST_INTEGRATION === '1') {
  test('real cold/hot archive, relocated checkout, no compiler consumers, missing helper and identity rejection', t => {
    const base = mkdtempSync(join(tmpdir(), 'wwc-archive-integration-'))
    t.after(() => rmSync(base, { recursive: true, force: true }))
    const root = join(base, 'producer')
    const output = join(base, 'archive')
    mkdirSync(root)
    function file(path, value, options) {
      mkdirSync(join(root, path, '..'), { recursive: true })
      writeFileSync(join(root, path), value, options)
    }
    file('Cargo.toml', '[workspace]\nmembers=["helper","fixture"]\nresolver="2"\n')
    file('helper/Cargo.toml', '[package]\nname="winwincode-kernel-helper"\nversion="0.1.0"\nedition="2024"\n')
    file('helper/src/main.rs', 'fn main() { println!("fixture-helper"); }\n#[test] fn helper_test() {}\n')
    file('fixture/Cargo.toml', '[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
    file('fixture/src/lib.rs', '#[test] fn reads_source_asset() { assert_eq!(std::fs::read_to_string(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("asset")).unwrap(), "fixture"); }\n')
    file('fixture/asset', 'fixture')
    for (const binary of ['production_vertical', 'worker_lifecycle', 'code_mode_kernel']) {
      file(`fixture/tests/${binary}.rs`, '#[test] fn uses_helper() { let p=std::env::var_os("WWC_WORKER_HELPER_EXECUTABLE").unwrap(); assert!(std::process::Command::new(p).status().unwrap().success()); }\n')
    }
    file('.config/nextest.toml', '[profile.default]\narchive.include=[{path="debug/winwincode-kernel-helper",relative-to="target",on-missing="error"}]\n')
    const env = { ...process.env, CARGO_TARGET_DIR: join(root, 'target'), CARGO_BUILD_BUILD_DIR: join(root, 'intermediates') }
    function run(command, args, cwd = root, environment = env) {
      const result = spawnSync(command, args, { cwd, env: environment, encoding: 'utf8' })
      assert.equal(result.status, 0, `${result.stdout}${result.stderr}`)
      return result
    }
    run('cargo', ['generate-lockfile'])
    run('git', ['init', '-q'])
    run('git', ['add', 'Cargo.toml', 'Cargo.lock', 'helper', 'fixture', '.config'])
    run('git', ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture'])
    const start = performance.now()
    produceArchive(root, output, env)
    const cold = performance.now() - start
    const warmStart = performance.now()
    produceArchive(root, output, env)
    const hot = performance.now() - warmStart
    process.stdout.write(`fixture archive cold=${cold.toFixed(0)}ms hot=${hot.toFixed(0)}ms bytes=${readFileSync(join(output, 'tests.tar.zst')).length}\n`)
    const moved = join(base, 'consumer')
    cpSync(root, moved, { recursive: true, filter: path => !['target', 'intermediates'].some(name => path === join(root, name)) })
    renameSync(root, join(base, 'old-producer-unavailable'))
    const tools = join(base, 'no-compilers')
    mkdirSync(tools)
    for (const command of ['cargo', 'rustc']) writeFileSync(join(tools, command), '#!/bin/sh\necho unexpected-compiler-call >&2\nexit 91\n', { mode: 0o700 })
    const consumerEnv = { ...env, PATH: `${tools}:${env.PATH}` }
    consumeArchive(moved, output, 'test', consumerEnv)
    consumeArchive(moved, output, 'e2e', consumerEnv)
    assert.throws(() => consumeArchive(moved, output, 'test', { ...consumerEnv, GITHUB_RUN_ID: 'wrong-run' }), /archive run mismatch/u)
    const manifest = JSON.parse(readFileSync(join(output, 'manifest.json')))
    manifest.baseline.test.push('missing::test')
    writeFileSync(join(output, 'manifest.json'), JSON.stringify(manifest))
    assert.throws(() => consumeArchive(moved, output, 'test', consumerEnv), /test IDs differ/u)
    manifest.baseline.test.pop()
    const old = join(base, 'old-producer-unavailable')
    writeFileSync(join(old, 'no-extra.toml'), '[profile.default]\n')
    const missing = join(output, 'tests.tar.zst')
    run('cargo-nextest', ['nextest', 'archive', '--workspace', '--all-features', '--locked',
      '--config-file', join(old, 'no-extra.toml'), '--archive-file', missing], old,
    { ...env, CARGO_TARGET_DIR: join(old, 'target'), CARGO_BUILD_BUILD_DIR: join(old, 'intermediates') })
    manifest.sha256 = sha256(missing)
    writeFileSync(join(output, 'manifest.json'), JSON.stringify(manifest))
    assert.throws(() => consumeArchive(moved, output, 'e2e', consumerEnv), /ENOENT/u)
  })
}
