import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

test('format check covers every current and future Rust workspace crate', () => {
  const manifest = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8'))

  assert.equal(
    manifest.scripts['format:check'],
    'node scripts/check-format.mjs && cargo fmt --all -- --check',
  )
  assert.doesNotMatch(manifest.scripts['format:check'], /(?:^|\s)--package(?:\s|$)/u)
})
