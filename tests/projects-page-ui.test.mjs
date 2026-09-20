import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const page = readFileSync(resolve(root, 'apps/client/src/projects-page.ts'), 'utf8')
const styles = readFileSync(resolve(root, 'apps/client/src/styles/features/projects.css'), 'utf8')

test('project runtime settings use a full-width row and accessible validation', () => {
  assert.match(page, /const main = element\(document, 'div', 'wwc-projects-row-main'\)/u)
  assert.match(page, /row\.append\(main, settings\)/u)
  assert.match(page, /error\.setAttribute\('role', 'alert'\)/u)
  assert.match(page, /control\.setAttribute\('aria-invalid', 'true'\)/u)
  assert.match(page, /invalidControls\[0\]\?\.focus\(\)/u)
})

test('project runtime settings keep DSH row layout and collapse to one column', () => {
  assert.match(styles, /\.wwc-projects-row-settings\s*\{[\s\S]*?width: 100%/u)
  assert.match(styles, /\.wwc-projects-template-field\s*\{[\s\S]*?grid-template-columns: minmax\(8rem, 14rem\) minmax\(0, 1fr\)/u)
  assert.match(styles, /@media \(max-width: 48rem\)[\s\S]*?\.wwc-projects-template-field\s*\{[\s\S]*?grid-template-columns: minmax\(0, 1fr\)/u)
  const formBlock = styles.match(/\.wwc-projects-template-form\s*\{([^}]*)\}/u)?.[1] ?? ''
  assert.doesNotMatch(formBlock, /background:/u)
})
