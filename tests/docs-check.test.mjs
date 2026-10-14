import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const script = join(root, 'scripts', 'check-docs.mjs')

function fixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-docs-check-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  mkdirSync(join(directory, 'docs', 'adr', 'history'), { recursive: true })
  mkdirSync(join(directory, 'docs', 'generated'), { recursive: true })
  writeFileSync(join(directory, 'target file.md'), '# Target\n')
  writeFileSync(join(directory, 'docs', 'target.md'), '# Target\n')
  writeFileSync(join(directory, 'docs', 'adr', 'history', 'target.md'), '# History\n')
  writeFileSync(join(directory, 'docs', 'generated', 'ignored.md'), '[missing](absent.md)\n')
  return directory
}

function run(directory) {
  return spawnSync(process.execPath, [script, directory], { encoding: 'utf8' })
}

test('CLI accepts local inline, image, and reference links', (t) => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'README.md'), [
    '[inline](docs/target.md#section "title")',
    '![image](docs/target.md)',
    '![](docs/target.md)',
    '[full][target]',
    '[collapsed][]',
    '[shortcut]',
    '[encoded](<target%20file.md> "title")',
    '[target]: docs/target.md',
    '[collapsed]: docs/target.md',
    '[shortcut]: docs/target.md',
    '',
  ].join('\n'))
  writeFileSync(join(directory, 'docs', 'adr', 'history', 'README.md'), '[history](target.md)\n')

  const result = run(directory)
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /documentation links verified/)
})

test('CLI reports the source file and line for a missing target', (t) => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'README.md'), '# Root\n')
  writeFileSync(join(directory, 'docs', 'broken.md'), '# Broken\n\n[missing](absent.md)\n')

  const result = run(directory)
  assert.equal(result.status, 1)
  assert.match(result.stderr, /docs\/broken\.md:3: missing local link target: absent\.md/)
})

test('CLI ignores links in code and remote or fragment-only targets', (t) => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'README.md'), [
    '`[inline code](missing-inline.md)`',
    '`![](missing-inline.png)`',
    '```md',
    '[sample](missing-backtick.md)',
    '![](missing-backtick.png)',
    '```not-a-closing-fence',
    '[sample](still-missing-backtick.md)',
    '```',
    '~~~md',
    '[sample](missing-tilde.md)',
    '![](missing-tilde.png)',
    '~~~',
    '[web](https://example.com/missing.md)',
    '[mail](mailto:docs@example.com)',
    '[anchor](#section)',
    '',
  ].join('\n'))

  const result = run(directory)
  assert.equal(result.status, 0, result.stderr)
})

test('CLI rejects a missing image with an empty alt label', (t) => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'README.md'), '![](missing.png)\n')

  const result = run(directory)
  assert.equal(result.status, 1, `${result.stdout}${result.stderr}`)
  assert.match(result.stderr, /README\.md:1: missing local link target: missing\.png/)
})

test('CLI rejects the missing first target of duplicate reference definitions', (t) => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'README.md'), [
    '[guide][ref]',
    '',
    '[ref]: missing.md',
    '[ REF ]: docs/target.md',
    '',
  ].join('\n'))

  const result = run(directory)
  assert.equal(result.status, 1, `${result.stdout}${result.stderr}`)
  assert.match(result.stderr, /README\.md:1: missing local link target: missing\.md/)
})

test('CLI accepts the valid first target of duplicate reference definitions', (t) => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'README.md'), [
    '[guide][ref]',
    '',
    '[ref]: docs/target.md',
    '[ REF ]: missing.md',
    '',
  ].join('\n'))

  const result = run(directory)
  assert.equal(result.status, 0, `${result.stdout}${result.stderr}`)
})
