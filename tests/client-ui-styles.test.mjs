import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const stylesRoot = resolve(root, 'apps/client/src/styles')
const sourceFiles = [
  'tokens.css',
  'base.css',
  'shell.css',
  'components.css',
  'features/chat.css',
  'features/strongflow.css',
  'features/settings.css',
  'features/local-operations.css',
  'features/local-decisions.css',
  'features/attention-center.css',
  'features/home.css',
  'features/enterprise.css',
  'features/usage-health.css',
]

function source(path) {
  return readFileSync(resolve(stylesRoot, path), 'utf8')
}

test('Client CSS has one deterministic tokens, base, shell, components, and features entry path', () => {
  const entry = source('client.css')
  assert.deepEqual(
    entry.split('\n').filter(line => line.startsWith('@import ')),
    sourceFiles.map(path => `@import './${path}' layer(${
      path === 'tokens.css'
        ? 'tokens'
        : path === 'base.css'
          ? 'base'
          : path === 'shell.css'
            ? 'shell'
            : path === 'components.css'
              ? 'components'
              : 'features'
    });`),
  )
  assert.equal(existsSync(resolve(root, 'apps/client/public/assets/client.css')), false)
  for (const path of sourceFiles) assert.equal(existsSync(resolve(stylesRoot, path)), true, path)

  const nonTokenColors = sourceFiles
    .filter(path => path !== 'tokens.css')
    .flatMap(path => [...source(path).matchAll(/#[\da-f]{3,8}\b/giu)].map(match => `${path}:${match[0]}`))
  assert.deepEqual(nonTokenColors, [])
  const tokens = source('tokens.css')
  for (const token of [
    '--wwc-color-canvas',
    '--wwc-color-action',
    '--wwc-color-focus',
    '--wwc-color-success-text',
    '--wwc-color-warning-text',
    '--wwc-color-danger-text',
    '--wwc-space-4',
    '--wwc-layer-dialog',
  ]) assert.match(tokens, new RegExp(`${token}:`, 'u'))

  const responsive = `${source('components.css')}\n${source('features/chat.css')}\n${
    source('features/strongflow.css')}`
  assert.match(responsive, /@media \(max-width: 48rem\)/u)
  assert.match(responsive, /@media \(max-width: 64rem\)/u)

  const base = source('base.css')
  const components = source('components.css')
  assert.match(base, /button:focus-visible/u)
  for (const selector of [
    "[data-wwc-component='button']:hover:not(:disabled)",
    "[data-wwc-component='button']:disabled",
    "[data-wwc-component='button'][aria-busy='true']",
    "[data-wwc-component='button'][data-variant='destructive']",
    "[data-wwc-component='form-field'] [aria-invalid='true']",
    "[data-wwc-component='error-state']",
  ]) assert.equal(components.includes(selector), true, selector)

  const build = readFileSync(resolve(root, 'apps/client/scripts/build.mjs'), 'utf8')
  assert.match(build, /src\/styles\/client\.css/u)
  assert.match(build, /dist\/public\/assets\/client\.css/u)
})

test('management pages share panels, bounded text, empty states, and compact layouts', () => {
  const componentCss = source('components.css')
  const featureCss = [
    'features/settings.css',
    'features/local-decisions.css',
    'features/local-operations.css',
    'features/enterprise.css',
  ].map(source).join('\n')
  assert.match(componentCss, /\[data-wwc-page='management'\]/u)
  assert.match(componentCss, /overflow-wrap:\s*anywhere/u)
  assert.match(featureCss, /\[data-state='empty'\]/u)
  assert.match(featureCss, /data-variant='destructive'/u)
  assert.match(featureCss, /@media \(max-width: 48rem\)/u)
  assert.match(featureCss, /@media \(max-width: 64rem\)/u)
})

test('global connection and error states have one responsive non-color presentation', () => {
  const components = source('components.css')
  for (const selector of [
    "[data-wwc-component='connection-bar']",
    "[data-connection-status='reconnecting']",
    "[data-connection-status='offline']",
    "[data-connection-status='refresh-required']",
    "[data-connection-status='authentication-required']",
    "[data-connection-status='permission-denied']",
    "[data-connection-status='version-mismatch']",
    '.wwc-client-error-boundary',
  ]) assert.equal(components.includes(selector), true, selector)
  assert.match(components, /@media \(max-width: 48rem\)[\s\S]+connection-bar/u)
})

test('the Home dashboard keeps one token-driven responsive card layout', () => {
  const home = source('features/home.css')
  assert.match(home, /\[data-wwc-page='home'\]/u)
  assert.match(home, /@media \(max-width: 48rem\)/u)
  for (const className of [
    '.wwc-home-section',
    '.wwc-home-card',
    '.wwc-home-card-context',
    '.wwc-home-unavailable',
  ]) assert.equal(home.includes(className), true, className)
})
