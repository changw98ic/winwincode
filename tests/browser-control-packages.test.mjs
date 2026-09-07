import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

function manifest(path) {
  return JSON.parse(readFileSync(resolve(root, path), 'utf8'))
}

test('browser control packages are public canonical sources', () => {
  const control = manifest('packages/control-plane-client/package.json')
  assert.equal(control.name, '@winwincode/control-plane-client')
  assert.equal(control.version, '0.1.0-alpha.1')
  assert.equal(control.publishConfig.access, 'public')
  assert.deepEqual(Object.keys(control.exports).sort(), ['.', './package.json'])

  const browserCore = manifest('packages/browser-core/package.json')
  assert.equal(browserCore.name, '@winwincode/browser-core')
  assert.equal(browserCore.version, '0.1.0-alpha.1')
  assert.equal(browserCore.publishConfig.access, 'public')
  assert.deepEqual(browserCore.dependencies, {
    '@winwincode/contracts': '0.1.0-alpha.1',
  })
  assert.deepEqual(
    Object.keys(browserCore.exports).sort(),
    ['./package.json', './query-cache', './scope-context'],
  )

  assert.equal(existsSync(resolve(root, 'apps/client/src/control-plane-client.ts')), false)
  assert.equal(existsSync(resolve(root, 'apps/client/src/core/query-cache.ts')), false)
  assert.equal(existsSync(resolve(root, 'apps/client/src/core/scope-context.ts')), false)
})

test('published sources consume ports and public contracts, not product generated files', () => {
  for (const path of [
    'packages/control-plane-client/src/index.ts',
    'packages/browser-core/src/query-cache.ts',
    'packages/browser-core/src/scope-context.ts',
  ]) {
    const source = readFileSync(resolve(root, path), 'utf8')
    assert.doesNotMatch(source, /apps\/client|generated\/contracts|generated\/control-plane-client/u)
    assert.doesNotMatch(source, /Enterprise[A-Z]|enterprise\./u)
  }
})
