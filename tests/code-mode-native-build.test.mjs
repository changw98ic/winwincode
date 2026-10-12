import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const driver = fileURLToPath(new URL('../scripts/code-mode-ninja.mjs', import.meta.url))

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'wwc-code-mode-build-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const source = join(root, 'registry/v8-150.4.0')
  const dataPackage = join(root, 'registry/deno_core_icudata-0.77.0')
  const output = join(root, 'target/debug/gn_out')
  const ninja = join(dirname(output), 'ninja_gn_binaries/ninja/ninja')
  mkdirSync(source, { recursive: true })
  mkdirSync(join(dataPackage, 'src'), { recursive: true })
  mkdirSync(output, { recursive: true })
  mkdirSync(dirname(ninja), { recursive: true })
  const data = Buffer.from('fixture ICU data')
  writeFileSync(join(dataPackage, 'src/icudtl.dat'), data)
  writeFileSync(ninja, `#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const args = process.argv.slice(2);
if (args.join('|') !== ['-C', ${JSON.stringify(output)}, '-j', '2', 'rusty_v8'].join('|')) process.exit(2);
if (fs.readFileSync(path.join(args[1], 'icudtl.dat'), 'utf8') !== 'fixture ICU data') process.exit(3);
console.log('native-consumer-read-verified-icu');
`, { mode: 0o700 })
  const dataSha256 = createHash('sha256').update(data).digest('hex')
  return { source, dataPackage, output, dataSha256 }
}

function run({ source, output, dataSha256 }) {
  const script = `import { runNinja } from ${JSON.stringify(new URL('../scripts/code-mode-ninja.mjs', import.meta.url).href)};
process.exitCode = runNinja(${JSON.stringify(['-C', output, '-j', '2', 'rusty_v8'])}, ${JSON.stringify(dataSha256)});`
  return spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    cwd: source,
    encoding: 'utf8',
  })
}

test('V8 snapshot generation receives the verified pinned ICU data before Ninja runs', t => {
  const setup = fixture(t)
  const { output } = setup
  const result = run(setup)
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /native-consumer-read-verified-icu/u)
  assert.equal(readFileSync(join(output, 'icudtl.dat'), 'utf8'), 'fixture ICU data')
})

test('V8 snapshot generation rejects altered ICU data before starting Ninja', t => {
  const setup = fixture(t)
  const { dataPackage } = setup
  writeFileSync(join(dataPackage, 'src/icudtl.dat'), 'altered data')
  const result = run(setup)
  assert.notEqual(result.status, 0)
  assert.match(result.stderr, /ICU data differs from its pinned Cargo source/u)
  assert.doesNotMatch(result.stdout, /native-consumer-read-verified-icu/u)
})

test('the Cargo Ninja entrypoint enforces its pinned ICU digest', t => {
  const { source, output } = fixture(t)
  const result = spawnSync(process.execPath, [driver, '-C', output, '-j', '2', 'rusty_v8'], {
    cwd: source,
    encoding: 'utf8',
  })
  assert.notEqual(result.status, 0)
  assert.match(result.stderr, /ICU data differs from its pinned Cargo source/u)
})
