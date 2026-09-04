// SPDX-License-Identifier: Apache-2.0
//
// UI-608 unit lane for the deterministic visual-regression fingerprints.
//
// The capture itself needs real layout, so the browser lanes in
// tests/ui608-component-state-visual-browser.test.mjs and
// tests/ui608-page-visual-browser.test.mjs own the rendered fingerprints.
// This lane owns everything that must hold without a browser: the traversal
// contract, the token resolution, the difference reasons a reviewer reads, and
// the registration of the whole visual lane.  Keeping the difference vocabulary
// here is what lets a visual failure be told apart from a functional E2E
// failure instead of failing as a vague assertion deep inside a scenario.

import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm', 'exec', 'tsc',
  '-p', 'apps/client/tsconfig.ui608-visual-tests.json',
  '--pretty', 'false',
  '--incremental', 'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `UI-608 visual regression modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/ui608-visual-tests')
const visual = await import(`${pathToFileURL(resolve(cache, 'visual-regression.js')).href}`)

const {
  DEFAULT_GEOMETRY_TOLERANCE_PIXELS,
  VISUAL_REGRESSION_FONT_STACK,
  VISUAL_REGRESSION_SCHEMA_VERSION,
  VISUAL_REGRESSION_TOKEN_PREFIX,
  captureVisualFingerprint,
  compareVisualFingerprints,
  normaliseColor,
  renderVisualRegressionReport,
  visualRegressionReport,
} = visual

// ---------------------------------------------------------------------------
// A tiny deterministic DOM double.  Real layout is the browser lanes' job;
// this double only has to answer "what is the tree, what does it compute, and
// where does it sit" so the traversal contract is pinned without Chrome.
// ---------------------------------------------------------------------------

const TOKENS = Object.freeze({
  '--wwc-color-danger-border': 'rgb(220, 38, 38)',
  '--wwc-color-danger-surface': 'rgb(254, 242, 242)',
  '--wwc-color-danger-text': 'rgb(153, 27, 27)',
  '--wwc-color-surface': 'rgb(255, 255, 255)',
  '--wwc-color-text': 'rgb(23, 32, 51)',
  '--wwc-opacity-disabled': '0.58',
})

let nodeId = 0

function createNode(tagName, overrides = {}) {
  const node = {
    id: `node-${String(++nodeId)}`,
    tagName: tagName.toUpperCase(),
    children: [],
    attributes: {},
    ownText: '',
    styles: {},
    rect: { x: 0, y: 0, width: 0, height: 0 },
    parent: null,
    append(child) {
      child.parent = node
      node.children.push(child)
      return child
    },
    setAttribute(name, value) {
      node.attributes[name] = value
    },
    appendText(value) {
      node.ownText = node.ownText === '' ? value : `${node.ownText} ${value}`
    },
    ...overrides,
  }
  return node
}

function computedStylesOf(node) {
  return {
    getPropertyValue(property) {
      return node.styles[property] ?? ''
    },
  }
}

function inspectorFor(roots) {
  const byNode = new Map()
  for (const rootNode of roots) {
    const walk = node => {
      byNode.set(node, node)
      for (const child of node.children) walk(child)
    }
    walk(rootNode)
  }
  return {
    children: element => byNode.get(element).children,
    attributes: element => ({ ...byNode.get(element).attributes }),
    text: element => byNode.get(element).ownText,
    style: element => computedStylesOf(byNode.get(element)),
    rect: element => byNode.get(element).rect,
  }
}

function host(roots) {
  return {
    defaultView: null,
    documentElement: roots[0],
  }
}

function capture(rootNode, overrides = {}) {
  return captureVisualFingerprint({
    document: host([rootNode]),
    root: rootNode,
    id: 'gallery/button/default',
    kind: 'component',
    viewport: { width: 1280, height: 800 },
    fontStack: VISUAL_REGRESSION_FONT_STACK,
    tokenValues: TOKENS,
    inspector: inspectorFor([rootNode]),
    ...overrides,
  })
}

function flatten(fingerprint) {
  return Object.fromEntries(fingerprint.nodes.map(node => [node.path, node]))
}

// ---------------------------------------------------------------------------

test('visual fingerprints carry the schema version, capture identity, and the fixed font contract', () => {
  const button = createNode('button', {
    styles: { color: TOKENS['--wwc-color-text'] },
  })
  const fingerprint = capture(button)

  assert.equal(fingerprint.schemaVersion, VISUAL_REGRESSION_SCHEMA_VERSION)
  assert.equal(fingerprint.id, 'gallery/button/default')
  assert.equal(fingerprint.kind, 'component')
  assert.deepEqual(fingerprint.viewport, { width: 1280, height: 800 })
  assert.equal(fingerprint.fontStack, VISUAL_REGRESSION_FONT_STACK)
  assert.equal(fingerprint.nodes.length, 1)
  assert.equal(fingerprint.nodes[0].tag, 'BUTTON')
})

test('the token prefix is the repository design-token namespace', () => {
  assert.equal(VISUAL_REGRESSION_TOKEN_PREFIX, '--wwc-')
})

test('colours are normalised so a token matches whichever spelling the runner reports', () => {
  assert.equal(normaliseColor('#dc2626'), 'rgb(220, 38, 38)')
  assert.equal(normaliseColor('#dc2626cc'), 'rgba(220, 38, 38, 0.8)')
  assert.equal(normaliseColor('rgb(220, 38, 38)'), 'rgb(220, 38, 38)')
  assert.equal(normaliseColor('rgb(220 38 38)'), 'rgb(220, 38, 38)')
  assert.equal(normaliseColor('rgb(15 23 42 / 45%)'), 'rgba(15, 23, 42, 0.45)')
  assert.equal(normaliseColor('color(srgb 0.8627 0.149 0.149)'), 'rgb(220, 38, 38)')
  assert.equal(normaliseColor('#fff'), 'rgb(255, 255, 255)')
  assert.equal(normaliseColor('none'), 'none')
  assert.equal(normaliseColor('0.58'), '0.58')
  assert.equal(normaliseColor('10px'), '10px')
})

test('a token declared in hex still resolves on an element that reports rgb', () => {
  const button = createNode('button', { styles: { color: 'rgb(153, 27, 27)' } })
  const fingerprint = capture(button, {
    tokenValues: { '--wwc-color-danger-text': '#991b1b' },
  })
  assert.equal(fingerprint.nodes[0].style.color, 'var(--wwc-color-danger-text)')
})

test('computed colors are recorded as design tokens so a palette change reads as a palette change', () => {
  const button = createNode('button', {
    styles: {
      'background-color': TOKENS['--wwc-color-danger-surface'],
      'border-top-color': TOKENS['--wwc-color-danger-border'],
      color: TOKENS['--wwc-color-danger-text'],
    },
  })
  const style = capture(button).nodes[0].style

  assert.equal(style['background-color'], 'var(--wwc-color-danger-surface)')
  assert.equal(style['border-top-color'], 'var(--wwc-color-danger-border)')
  assert.equal(style.color, 'var(--wwc-color-danger-text)')
})

test('a colour outside the token system stays a raw value instead of being silently normalised', () => {
  const button = createNode('button', { styles: { color: 'rgb(1, 2, 3)' } })
  assert.equal(capture(button).nodes[0].style.color, 'rgb(1, 2, 3)')
})

test('state attributes are captured so busy, disabled, and error states are part of the baseline', () => {
  const button = createNode('button')
  button.setAttribute('disabled', '')
  button.setAttribute('aria-busy', 'true')
  button.setAttribute('data-variant', 'destructive')
  button.setAttribute('data-tone', 'danger')

  const node = capture(button).nodes[0]
  assert.equal(node.component, null, 'only data-wwc-component maps to component')
  assert.deepEqual(node.state, {
    'aria-busy': 'true',
    'data-tone': 'danger',
    'data-variant': 'destructive',
    disabled: '',
  })
})

test('the component marker comes from data-wwc-component', () => {
  const badge = createNode('span')
  badge.setAttribute('data-wwc-component', 'status-badge')
  assert.equal(capture(badge).nodes[0].component, 'status-badge')
})

test('own text is captured and whitespace-normalised; descendant text stays with its own node', () => {
  const root_ = createNode('section')
  const title = root_.append(createNode('h2'))
  title.appendText('  Provider   settings ')
  const detail = root_.append(createNode('p'))
  detail.appendText('unavailable')

  const nodes = flatten(capture(root_))
  assert.equal(nodes['./h2[1]'].text, 'Provider settings')
  assert.equal(nodes['./p[1]'].text, 'unavailable')
  assert.equal(nodes['.'].text, '')
})

test('paths name the element order among same-tag siblings so reordering is a visible difference', () => {
  const root_ = createNode('div')
  root_.append(createNode('span'))
  root_.append(createNode('p'))
  root_.append(createNode('span'))

  assert.deepEqual(
    capture(root_).nodes.map(node => node.path),
    ['.', './span[1]', './p[1]', './span[2]'],
  )
})

test('geometry is recorded relative to the capture root and rounded to whole pixels', () => {
  const root_ = createNode('div', { rect: { x: 100.4, y: 50.6, width: 400, height: 200 } })
  const child = root_.append(createNode('span', { rect: { x: 132.6, y: 82.4, width: 33.5, height: 19.5 } }))

  const fingerprint = capture(root_)
  assert.deepEqual(fingerprint.nodes[0].box, { x: 0, y: 0, width: 400, height: 200 })
  assert.deepEqual(flatten(fingerprint)['./span[1]'].box, { x: 32, y: 32, width: 34, height: 20 })
  assert.equal(child.parent, root_)
})

test('display:none subtrees are absent from the fingerprint', () => {
  const root_ = createNode('div')
  const visible = root_.append(createNode('span'), )
  const hidden = root_.append(createNode('span'))
  hidden.styles.display = 'none'
  const insideHidden = hidden.append(createNode('em'))
  insideHidden.styles.display = 'inline'
  assert.equal(visible.parent, root_)

  assert.deepEqual(
    capture(root_).nodes.map(node => node.path),
    ['.', './span[1]'],
  )
})

test('visibility:hidden nodes stay in the fingerprint because they still hold layout space', () => {
  const root_ = createNode('div')
  const ghost = root_.append(createNode('span'))
  ghost.styles.visibility = 'hidden'
  ghost.rect = { x: 0, y: 24, width: 80, height: 20 }

  const nodes = flatten(capture(root_))
  assert.equal(nodes['./span[1]'].style.visibility, 'hidden')
  assert.deepEqual(nodes['./span[1]'].box, { x: 0, y: 24, width: 80, height: 20 })
})

test('a descendant that only inherits records no style decision of its own', () => {
  const root_ = createNode('div', { styles: { color: TOKENS['--wwc-color-text'] } })
  const child = root_.append(createNode('span', { styles: { color: TOKENS['--wwc-color-text'] } }))
  child.rect = { x: 0, y: 24, width: 40, height: 20 }

  const nodes = flatten(capture(root_))
  assert.equal(nodes['.'].style.color, 'var(--wwc-color-text)')
  assert.equal(nodes['./span[1]'].style.color, undefined)
})

test('two captures of the same tree are identical', () => {
  const build = () => {
    const root_ = createNode('button', { styles: { color: TOKENS['--wwc-color-text'] } })
    root_.setAttribute('data-wwc-component', 'button')
    const label = root_.append(createNode('span'))
    label.appendText('Publish')
    label.rect = { x: 8, y: 8, width: 60, height: 20 }
    return root_
  }
  assert.deepEqual(capture(build()), capture(build()))
})

test('comparing an unchanged fingerprint reports no differences', () => {
  const build = () => {
    const root_ = createNode('button')
    root_.appendText('Publish')
    return root_
  }
  const baseline = capture(build())
  assert.deepEqual(compareVisualFingerprints(baseline, capture(build())), [])
})

function fingerprintWith(mutate) {
  const build = () => {
    const root_ = createNode('button', { styles: { color: TOKENS['--wwc-color-text'] } })
    root_.setAttribute('data-wwc-component', 'button')
    const label = root_.append(createNode('span'))
    label.appendText('Publish')
    label.rect = { x: 8, y: 8, width: 60, height: 20 }
    mutate(root_, label)
    return root_
  }
  return capture(build())
}

test('changed text is reported per node with both renderings', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith((_root_, label) => {
    label.appendText('Save')
  })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.deepEqual(differences.map(difference => [difference.reason, difference.property]), [
    ['text', 'text'],
  ])
  assert.equal(differences[0].path, './span[1]')
  assert.equal(differences[0].baseline, 'Publish')
  assert.equal(differences[0].actual, 'Publish Save')
})

test('a node that disappears from the tree is reported with the missing-node reason', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith(root_ => {
    root_.children.pop()
  })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.equal(differences.length, 1)
  assert.equal(differences[0].reason, 'missing-node')
  assert.equal(differences[0].path, './span[1]')
  assert.equal(differences[0].property, 'presence')
  assert.equal(differences[0].actual, null)
})

test('a node that appears in the tree is reported with the unexpected-node reason', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith(root_ => {
    root_.append(createNode('em')).appendText('new')
  })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.equal(differences.length, 1)
  assert.equal(differences[0].reason, 'unexpected-node')
  assert.equal(differences[0].path, './em[1]')
  assert.equal(differences[0].baseline, null)
})

test('a reordered tree is reported with the order reason instead of a wall of presence noise', () => {
  const baseline = fingerprintWith(root_ => {
    root_.append(createNode('em')).appendText('first')
  })
  const actual = fingerprintWith(root_ => {
    const label = root_.children[0]
    root_.children.length = 0
    const moved = createNode('em')
    moved.appendText('first')
    root_.children.push(moved, label)
    label.parent = root_
  })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.deepEqual(differences.map(difference => [difference.reason, difference.property]), [
    ['order', 'order'],
  ])
  assert.equal(differences[0].baseline, './span[1]')
  assert.equal(differences[0].actual, './em[1]')
})

test('state changes are reported per attribute', () => {
  const baseline = fingerprintWith(root_ => root_.setAttribute('data-variant', 'primary'))
  const actual = fingerprintWith(root_ => root_.setAttribute('data-variant', 'destructive'))

  const differences = compareVisualFingerprints(baseline, actual)
  assert.equal(differences.length, 1)
  assert.equal(differences[0].reason, 'state')
  assert.equal(differences[0].property, 'state.data-variant')
  assert.equal(differences[0].baseline, 'primary')
  assert.equal(differences[0].actual, 'destructive')
})

test('style changes are reported per property with the token names a reviewer can trace', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith(root_ => {
    root_.styles.color = TOKENS['--wwc-color-danger-text']
  })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.equal(differences.length, 1)
  assert.equal(differences[0].reason, 'style')
  assert.equal(differences[0].property, 'style.color')
  assert.equal(differences[0].baseline, 'var(--wwc-color-text)')
  assert.equal(differences[0].actual, 'var(--wwc-color-danger-text)')
})

test('geometry within the tolerance is not a difference and the default tolerance is one pixel', () => {
  assert.equal(DEFAULT_GEOMETRY_TOLERANCE_PIXELS, 1)

  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith((root_, label) => {
    label.rect = { x: 8.6, y: 8, width: 60.9, height: 20 }
  })

  assert.deepEqual(compareVisualFingerprints(baseline, actual), [])
})

test('geometry beyond the tolerance is reported per box field', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith((root_, label) => {
    label.rect = { x: 8, y: 40, width: 60, height: 20 }
  })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.deepEqual(differences.map(difference => difference.property), ['box.y'])
  assert.equal(differences[0].reason, 'geometry')
  assert.equal(differences[0].baseline, '8')
  assert.equal(differences[0].actual, '40')
})

test('a wider tolerance absorbs small cross-platform font drift', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith((root_, label) => {
    label.rect = { x: 8, y: 8, width: 62, height: 21 }
  })

  assert.deepEqual(
    compareVisualFingerprints(baseline, actual, { geometryTolerancePixels: 2 }),
    [],
  )
})

test('capture identity is part of the comparison so a snapshot cannot be replayed against another viewport', () => {
  const button = createNode('button')
  const baseline = capture(button)
  const actual = capture(button, { viewport: { width: 420, height: 860 } })

  const differences = compareVisualFingerprints(baseline, actual)
  assert.equal(differences.length, 1)
  assert.equal(differences[0].reason, 'meta')
  assert.equal(differences[0].property, 'viewport')
})

test('the report groups differences by reason so the failing lane names its own cause', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith(root_ => {
    root_.styles.color = TOKENS['--wwc-color-danger-text']
    root_.children.pop()
    root_.append(createNode('em')).appendText('new')
  })

  const report = visualRegressionReport(compareVisualFingerprints(baseline, actual))
  assert.equal(report.total, 3)
  assert.deepEqual(report.byReason, {
    'missing-node': 1,
    'style': 1,
    'unexpected-node': 1,
  })
  assert.deepEqual(Object.keys(report.byReason), [
    'missing-node',
    'style',
    'unexpected-node',
  ])
})

test('the rendered report names the fingerprint, the reason, and both sides of every difference', () => {
  const baseline = fingerprintWith(() => {})
  const actual = fingerprintWith(root_ => {
    root_.styles.color = TOKENS['--wwc-color-danger-text']
  })

  const text = renderVisualRegressionReport(
    compareVisualFingerprints(baseline, actual),
    { id: baseline.id },
  )
  assert.match(text, /gallery\/button\/default/u)
  assert.match(text, /1 visual difference/u)
  assert.match(text, /style\.color/u)
  assert.match(text, /var\(--wwc-color-text\)/u)
  assert.match(text, /var\(--wwc-color-danger-text\)/u)
  assert.match(text, /UI-608 visual regression/u)
})

test('an empty difference list renders as a clean report', () => {
  assert.equal(
    renderVisualRegressionReport([], { id: 'gallery/button/default' }),
    'gallery/button/default: no visual differences',
  )
})

test('the visual lane is registered exactly once in the canonical TypeScript lane', () => {
  const runner = readFileSync(resolve(root, 'scripts/run-ts-tests.mjs'), 'utf8')
  for (const path of [
    'tests/ui608-visual-regression.test.mjs',
    'tests/ui608-component-state-visual-browser.test.mjs',
    'tests/ui608-page-visual-browser.test.mjs',
  ]) {
    assert.equal(
      runner.split(`'${path}'`).length - 1,
      1,
      `${path} must be registered exactly once in the canonical TypeScript lane`,
    )
  }
})

test('the visual module and both browser suites are listed in the decision inventory', () => {
  const inventory = JSON.parse(readFileSync(
    resolve(root, 'docs/decisions/0028-control-plane-worker-migration.inventory.json'),
    'utf8',
  ))
  const listed = new Set(inventory.surfaces.flatMap(surface => surface.sourcePaths))
  assert.equal(listed.has('apps/client/src/visual-regression.ts'), true)

  const baselines = new Map(inventory.behaviorBaselines.map(entry => [entry.id, entry]))
  for (const id of ['ui608-component-state-visual', 'ui608-page-visual']) {
    assert.equal(baselines.has(id), true, `${id} must be a behaviour baseline`)
    assert.match(baselines.get(id).testFile, /^tests\/ui608-.*\.test\.mjs$/u)
  }
})

test('the committed baselines exist and are credential free', () => {
  for (const name of [
    'component-states.baseline.json',
    'pages.baseline.json',
  ]) {
    const path = resolve(root, 'tests/fixtures/visual-regression', name)
    assert.equal(existsSync(path), true, `${path} must exist`)
  }
})
