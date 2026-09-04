// SPDX-License-Identifier: Apache-2.0
//
// UI-608 deterministic visual fingerprints for component states and key pages.
//
// This is the "equivalent visual regression" the decision asked for: instead of
// comparing pixels, which drift with the fonts a runner happens to have, it
// serialises what a rendered surface actually looks like — the tree in paint
// order, the state each element carries, the design-token decision each element
// makes, and where each element sits relative to the captured root.  A committed
// baseline pins that; a diff against a live capture is a reviewable list of
// named differences.
//
// Layering with the functional lanes:
//   * `tests/ui608-visual-regression.test.mjs` — this module, no browser.
//   * `tests/ui608-component-state-visual-browser.test.mjs` — component states.
//   * `tests/ui608-page-visual-browser.test.mjs` — key pages, desktop and narrow.
// A visual failure therefore reports as a UI-608 difference list, never as a
// failed assertion in the middle of a functional scenario.

export const VISUAL_REGRESSION_SCHEMA_VERSION = 1

/** The repository design-token namespace the fingerprints resolve colours to. */
export const VISUAL_REGRESSION_TOKEN_PREFIX = '--wwc-'

/**
 * The one font stack every visual fixture installs before capturing.  Glyph
 * metrics — and therefore every recorded box — then stop depending on whether
 * a runner ships the product font.
 */
export const VISUAL_REGRESSION_FONT_STACK
  = "'wwc-visual-regression', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"

/** Cross-platform text shaping moves a box by less than this. */
export const DEFAULT_GEOMETRY_TOLERANCE_PIXELS = 1

export type VisualCaptureKind = 'component' | 'page' | 'shell'

export type VisualRegressionReasonCode =
  | 'geometry'
  | 'meta'
  | 'missing-node'
  | 'order'
  | 'state'
  | 'style'
  | 'text'
  | 'unexpected-node'

export interface VisualViewport {
  readonly width: number
  readonly height: number
}

export interface VisualRect {
  readonly x: number
  readonly y: number
  readonly width: number
  readonly height: number
}

export interface VisualBox {
  readonly x: number
  readonly y: number
  readonly width: number
  readonly height: number
}

export interface VisualComputedStyles {
  readonly getPropertyValue: (property: string) => string
}

export interface VisualCaptureInspector {
  readonly children: (element: Element) => readonly Element[]
  readonly attributes: (element: Element) => Readonly<Record<string, string>>
  readonly text: (element: Element) => string
  readonly style: (element: Element) => VisualComputedStyles
  readonly rect: (element: Element) => VisualRect
}

export interface VisualNodeSnapshot {
  readonly path: string
  readonly tag: string
  readonly component: string | null
  readonly role: string | null
  readonly text: string
  readonly state: Readonly<Record<string, string>>
  readonly style: Readonly<Record<string, string>>
  readonly box: VisualBox
}

export interface VisualFingerprint {
  readonly schemaVersion: number
  readonly id: string
  readonly kind: VisualCaptureKind
  readonly viewport: VisualViewport
  readonly fontStack: string
  readonly nodes: readonly VisualNodeSnapshot[]
}

export interface VisualDifference {
  readonly reason: VisualRegressionReasonCode
  readonly path: string
  readonly property: string
  readonly baseline: string | null
  readonly actual: string | null
}

export interface VisualComparisonOptions {
  readonly geometryTolerancePixels?: number
}

export interface VisualRegressionReport {
  readonly total: number
  readonly byReason: Readonly<Record<VisualRegressionReasonCode, number>>
  readonly entries: readonly VisualDifference[]
}

export interface VisualCaptureOptions {
  readonly document: Document
  readonly root: Element
  readonly id: string
  readonly kind: VisualCaptureKind
  readonly viewport: VisualViewport
  readonly fontStack: string
  /** Defaults to the design tokens the captured document defines on `:root`. */
  readonly tokenValues?: Readonly<Record<string, string>>
  /** Defaults to the real DOM; browser fixtures and unit tests may inject one. */
  readonly inspector?: VisualCaptureInspector
}

/**
 * The computed properties that carry a component's visual identity.  Inherited
 * values are not repeated on a child that only inherits, so a recorded property
 * is exactly the decision made at that element.
 */
const STYLE_PROPERTIES: readonly string[] = Object.freeze([
  'background-color',
  'border-bottom-color',
  'border-bottom-left-radius',
  'border-bottom-right-radius',
  'border-bottom-width',
  'border-left-color',
  'border-left-width',
  'border-right-color',
  'border-right-width',
  'border-top-color',
  'border-top-left-radius',
  'border-top-right-radius',
  'border-top-width',
  'box-shadow',
  'color',
  'cursor',
  'display',
  'font-size',
  'font-weight',
  'opacity',
  'outline-color',
  'outline-style',
  'outline-width',
  'position',
  'text-decoration-line',
  'visibility',
  'z-index',
])

/** Properties whose values may name a design token. */
const TOKENISED_PROPERTIES: ReadonlySet<string> = new Set([
  'background-color',
  'border-bottom-color',
  'border-left-color',
  'border-right-color',
  'border-top-color',
  'color',
  'opacity',
  'outline-color',
  'z-index',
])

/** Properties whose value is one colour, and so may be normalised to a token. */
const COLOR_PROPERTIES: ReadonlySet<string> = new Set([
  'background-color',
  'border-bottom-color',
  'border-left-color',
  'border-right-color',
  'border-top-color',
  'color',
  'outline-color',
])

/** Visual state.  Absent attributes are simply not recorded. */
const STATE_ATTRIBUTES: readonly string[] = Object.freeze([
  'aria-busy',
  'aria-disabled',
  'aria-expanded',
  'aria-hidden',
  'aria-invalid',
  'aria-selected',
  'disabled',
  'hidden',
  'open',
])

/** Presentation markers the components set alongside their state. */
const STATE_DATA_ATTRIBUTES: readonly string[] = Object.freeze([
  'data-align',
  'data-orientation',
  'data-state',
  'data-status',
  'data-tone',
  'data-variant',
])

const COMPONENT_ATTRIBUTE = 'data-wwc-component'
const ROOT_PATH = '.'

/**
 * The one real-DOM inspector.  Browser fixtures use it; unit tests inject a
 * double so the traversal contract is pinned without Chrome.
 */
export function createDomVisualInspector(document: Document): VisualCaptureInspector {
  const readAttributes = (element: Element): Readonly<Record<string, string>> => {
    const attributes: Record<string, string> = {}
    for (const attribute of Array.from(element.attributes)) {
      attributes[attribute.name] = attribute.value
    }
    return attributes
  }
  const inspector: VisualCaptureInspector = {
    children: (element: Element) => Array.from(element.children),
    attributes: readAttributes,
    text: (element: Element) => {
      let text = ''
      for (const child of Array.from(element.childNodes)) {
        if (child.nodeType === child.TEXT_NODE) text = `${text} ${child.nodeValue ?? ''}`
      }
      return text
    },
    style: (element: Element) => {
      const view = document.defaultView
      if (view === null) throw new Error('the captured document has no default view')
      return view.getComputedStyle(element)
    },
    rect: (element: Element) => element.getBoundingClientRect(),
  }
  return Object.freeze(inspector)
}

function normaliseText(value: string): string {
  return value.replace(/\s+/gu, ' ').trim()
}

const HEX_COLOR = /^#[0-9a-f]{3,8}$/iu
const FUNCTION_COLOR = /^([a-z-]+)\(([^)]*)\)$/iu

function channel(value: string): number {
  const raw = value.trim()
  if (raw.endsWith('%')) return Math.round(Number.parseFloat(raw.slice(0, -1)) / 100 * 255)
  return Math.round(Number.parseFloat(raw))
}

function alphaChannel(value: string): number {
  const raw = value.trim()
  if (raw.endsWith('%')) return Number.parseFloat(raw.slice(0, -1)) / 100
  return Number.parseFloat(raw)
}

/** `color(srgb r g b)` carries 0..1 channels instead of 0..255. */
function srgbChannel(value: string): number {
  return Math.round(Number.parseFloat(value.trim()) * 255)
}

/**
 * Rewrites a colour into one canonical `rgb()` / `rgba()` form.
 *
 * Chrome hands back the raw text of a custom property (hex) but the resolved
 * form of a computed colour (`rgb()`), and different runners may hand back
 * either, so a palette comparison has to be done over one spelling.  `NaN`
 * falls through to the input, which keeps an unexpected syntax visible in the
 * baseline instead of silently erasing it.
 */
export function normaliseColor(value: string): string {
  const input = value.trim()
  if (HEX_COLOR.test(input)) {
    const digits = input.slice(1)
    const expanded = digits.length <= 4
      ? [...digits].map(digit => digit + digit).join('')
      : digits
    const red = Number.parseInt(expanded.slice(0, 2), 16)
    const green = Number.parseInt(expanded.slice(2, 4), 16)
    const blue = Number.parseInt(expanded.slice(4, 6), 16)
    const alphaHex = expanded.slice(6, 8)
    if ([red, green, blue].some(channel_ => Number.isNaN(channel_))) return input
    if (alphaHex === '') return `rgb(${String(red)}, ${String(green)}, ${String(blue)})`
    const alpha = Number.parseInt(alphaHex, 16) / 255
    if (Number.isNaN(alpha)) return input
    return `rgba(${String(red)}, ${String(green)}, ${String(blue)}, ${String(Number(alpha.toFixed(4)))})`
  }
  const functionMatch = FUNCTION_COLOR.exec(input)
  if (functionMatch === null) return input
  const name = functionMatch[1]
  const body = functionMatch[2]
  if (name === undefined || body === undefined) return input
  const normalisedName = name.toLowerCase()
  if (normalisedName !== 'rgb' && normalisedName !== 'rgba'
    && normalisedName !== 'color' && normalisedName !== 'hsl'
    && normalisedName !== 'hsla') return input
  const parts = body.split('/').map(part => part.trim()).filter(part => part !== '')
  if (parts.length === 0) return input
  const channels = parts[0]?.split(/[\s,]+/u).filter(part => part !== '') ?? []
  if (normalisedName === 'color') {
    // `color(srgb r g b)` carries 0..1 channels; other colour spaces are left
    // alone because converting them needs a profile the capture does not have.
    if (channels[0]?.toLowerCase() !== 'srgb') return input
    const [red, green, blue] = channels.slice(1, 4).map(srgbChannel)
    const alphaText = parts[1]
    const alpha = alphaText === undefined ? 1 : alphaChannel(alphaText)
    if ([red, green, blue, alpha].some(value_ => Number.isNaN(value_))) return input
    return alpha === 1
      ? `rgb(${String(red)}, ${String(green)}, ${String(blue)})`
      : `rgba(${String(red)}, ${String(green)}, ${String(blue)}, ${String(Number(alpha.toFixed(4)))})`
  }
  if (normalisedName === 'hsl' || normalisedName === 'hsla') {
    // Hue, saturation, and lightness convert with trigonometry; tokens in this
    // repository are hex, so leave an hsl value readable rather than wrong.
    return input
  }
  const alphaText = parts[1]
  const alpha = alphaText === undefined ? 1 : alphaChannel(alphaText)
  const [red, green, blue] = channels.slice(0, 3).map(channel)
  if ([red, green, blue, alpha].some(value_ => Number.isNaN(value_))) return input
  return alpha === 1
    ? `rgb(${String(red)}, ${String(green)}, ${String(blue)})`
    : `rgba(${String(red)}, ${String(green)}, ${String(blue)}, ${String(Number(alpha.toFixed(4)))})`
}

function tokenValuesFrom(document: Document): Readonly<Record<string, string>> {
  const view = document.defaultView
  if (view === null) return {}
  const declared = view.getComputedStyle(document.documentElement)
  const values: Record<string, string> = {}
  for (let index = 0; index < declared.length; index += 1) {
    const property = declared.item(index)
    if (property.startsWith(VISUAL_REGRESSION_TOKEN_PREFIX)) {
      values[property] = declared.getPropertyValue(property).trim()
    }
  }
  return values
}

/**
 * Maps a resolved value back to the design token that produced it, so a palette
 * change reads as a palette change in review instead of as a mystery rgb value.
 * A value shared by several tokens stays raw: naming one of them would make the
 * baseline depend on which token the resolver happened to meet first.
 */
function tokenResolver(
  tokenValues: Readonly<Record<string, string>>,
): ReadonlyMap<string, string> {
  const names = Object.keys(tokenValues).filter(name => name.startsWith(VISUAL_REGRESSION_TOKEN_PREFIX))
    .sort((left, right) => left.localeCompare(right))
  const owners = new Map<string, string>()
  for (const name of names) {
    const raw = tokenValues[name]
    if (raw === undefined) continue
    // Chrome reports a custom property's raw text (hex) but a computed
    // colour's resolved form, so every candidate value goes through the same
    // normaliser before it can be recognised again on an element.
    const value = normaliseColor(raw.trim())
    if (value === '' || value === 'none') continue
    if (owners.has(value)) owners.set(value, '')
    else owners.set(value, name)
  }
  const resolved = new Map<string, string>()
  for (const [value, name] of owners) {
    if (name !== undefined && name !== '') resolved.set(value, `var(${String(name)})`)
  }
  return resolved
}

function resolvedStyleValue(
  property: string,
  value: string,
  tokens: ReadonlyMap<string, string>,
): string {
  if (!TOKENISED_PROPERTIES.has(property)) return value
  const normalised = COLOR_PROPERTIES.has(property) ? normaliseColor(value) : value
  return tokens.get(normalised) ?? normalised
}

function readStyles(
  element: Element,
  inspector: VisualCaptureInspector,
  tokens: ReadonlyMap<string, string>,
): Readonly<Record<string, string>> {
  const computed = inspector.style(element)
  const styles: Record<string, string> = {}
  for (const property of STYLE_PROPERTIES) {
    const value = computed.getPropertyValue(property)
    if (value === '') continue
    styles[property] = resolvedStyleValue(property, value, tokens)
  }
  return styles
}

function childPath(parentPath: string, tag: string, indexWithinTag: number): string {
  return `${parentPath}/${tag}[${String(indexWithinTag)}]`
}

export function captureVisualFingerprint(options: VisualCaptureOptions): VisualFingerprint {
  const inspector = options.inspector ?? createDomVisualInspector(options.document)
  const tokens = tokenResolver(options.tokenValues ?? tokenValuesFrom(options.document))
  const rootRect = inspector.rect(options.root)
  const nodes: VisualNodeSnapshot[] = []

  const walk = (
    element: Element,
    path: string,
    inherited: Readonly<Record<string, string>>,
    origin: VisualRect,
    isRoot: boolean,
  ): void => {
    const styles = readStyles(element, inspector, tokens)
    // A display:none subtree paints nothing and holds no space, so its absence
    // from the baseline is the correct picture of it.
    if (styles.display === 'none') return

    const attributes = inspector.attributes(element)
    const state: Record<string, string> = {}
    for (const name of STATE_ATTRIBUTES) {
      const value = attributes[name]
      if (value !== undefined) state[name] = value
    }
    for (const name of STATE_DATA_ATTRIBUTES) {
      const value = attributes[name]
      if (value !== undefined) state[name] = value
    }

    const recorded: Record<string, string> = {}
    for (const property of STYLE_PROPERTIES) {
      const value = styles[property]
      // The root states its whole presentation; a descendant states only what
      // it decides differently from the parent it would otherwise inherit.
      if (value === undefined) continue
      if (!isRoot && value === inherited[property]) continue
      recorded[property] = value
    }

    const rect = inspector.rect(element)
    const orderedState: Record<string, string> = {}
    for (const name of Object.keys(state).sort((left, right) => left.localeCompare(right))) {
      orderedState[name] = state[name] ?? ''
    }
    const orderedStyle: Record<string, string> = {}
    for (const name of Object.keys(recorded).sort((left, right) => left.localeCompare(right))) {
      orderedStyle[name] = recorded[name] ?? ''
    }
    nodes.push({
      path,
      tag: element.tagName,
      component: attributes[COMPONENT_ATTRIBUTE] ?? null,
      role: attributes.role ?? null,
      text: normaliseText(inspector.text(element)),
      state: orderedState,
      style: orderedStyle,
      box: {
        x: Math.round(rect.x - origin.x),
        y: Math.round(rect.y - origin.y),
        width: Math.round(rect.width),
        height: Math.round(rect.height),
      },
    })

    const counts = new Map<string, number>()
    for (const child of inspector.children(element)) {
      const tag = child.tagName
      // Paths read like selectors and stay stable across HTML's upper-cased
      // tagName and the mixed case SVG keeps.
      const pathTag = tag.toLowerCase()
      const indexWithinTag = (counts.get(tag) ?? 0) + 1
      counts.set(tag, indexWithinTag)
      walk(child, childPath(path, pathTag, indexWithinTag), styles, origin, false)
    }
  }

  walk(options.root, ROOT_PATH, {}, rootRect, true)

  return {
    schemaVersion: VISUAL_REGRESSION_SCHEMA_VERSION,
    id: options.id,
    kind: options.kind,
    viewport: { ...options.viewport },
    fontStack: options.fontStack,
    nodes,
  }
}

function compareBoxes(
  path: string,
  baseline: VisualBox,
  actual: VisualBox,
  tolerance: number,
  differences: VisualDifference[],
): void {
  for (const field of ['x', 'y', 'width', 'height'] as const) {
    const expected = baseline[field]
    const received = actual[field]
    if (Math.abs(expected - received) <= tolerance) continue
    differences.push({
      reason: 'geometry',
      path,
      property: `box.${field}`,
      baseline: String(expected),
      actual: String(received),
    })
  }
}

function compareRecords(
  reason: VisualRegressionReasonCode,
  prefix: string,
  path: string,
  baseline: Readonly<Record<string, string>>,
  actual: Readonly<Record<string, string>>,
  differences: VisualDifference[],
): void {
  const names = new Set([...Object.keys(baseline), ...Object.keys(actual)])
  for (const name of [...names].sort((left, right) => left.localeCompare(right))) {
    const expected = baseline[name]
    const received = actual[name]
    if (expected === received) continue
    differences.push({
      reason,
      path,
      property: `${prefix}${name}`,
      baseline: expected ?? null,
      actual: received ?? null,
    })
  }
}

export function compareVisualFingerprints(
  baseline: VisualFingerprint,
  actual: VisualFingerprint,
  options: VisualComparisonOptions = {},
): readonly VisualDifference[] {
  const tolerance = options.geometryTolerancePixels ?? DEFAULT_GEOMETRY_TOLERANCE_PIXELS
  const differences: VisualDifference[] = []

  if (baseline.schemaVersion !== actual.schemaVersion) {
    differences.push({
      reason: 'meta',
      path: ROOT_PATH,
      property: 'schemaVersion',
      baseline: String(baseline.schemaVersion),
      actual: String(actual.schemaVersion),
    })
  }
  if (baseline.kind !== actual.kind) {
    differences.push({
      reason: 'meta',
      path: ROOT_PATH,
      property: 'kind',
      baseline: baseline.kind,
      actual: actual.kind,
    })
  }
  if (baseline.viewport.width !== actual.viewport.width
    || baseline.viewport.height !== actual.viewport.height) {
    differences.push({
      reason: 'meta',
      path: ROOT_PATH,
      property: 'viewport',
      baseline: `${String(baseline.viewport.width)}x${String(baseline.viewport.height)}`,
      actual: `${String(actual.viewport.width)}x${String(actual.viewport.height)}`,
    })
  }
  if (baseline.fontStack !== actual.fontStack) {
    differences.push({
      reason: 'meta',
      path: ROOT_PATH,
      property: 'fontStack',
      baseline: baseline.fontStack,
      actual: actual.fontStack,
    })
  }

  const baselineNodes = new Map(baseline.nodes.map(node => [node.path, node]))
  const actualNodes = new Map(actual.nodes.map(node => [node.path, node]))

  for (const [path, node] of baselineNodes) {
    const received = actualNodes.get(path)
    if (received !== undefined) continue
    differences.push({
      reason: 'missing-node',
      path,
      property: 'presence',
      baseline: node.tag,
      actual: null,
    })
  }
  for (const [path, node] of actualNodes) {
    if (baselineNodes.has(path)) continue
    differences.push({
      reason: 'unexpected-node',
      path,
      property: 'presence',
      baseline: null,
      actual: node.tag,
    })
  }

  // Membership is unchanged but the paint order moved: report the first
  // position that diverged rather than every position after it.
  if (baselineNodes.size === actualNodes.size
    && differences.every(difference => difference.reason !== 'missing-node')
    && differences.every(difference => difference.reason !== 'unexpected-node')) {
    for (let index = 0; index < baseline.nodes.length; index += 1) {
      const expected = baseline.nodes[index]
      const received = actual.nodes[index]
      if (expected === undefined || received === undefined) break
      if (expected.path === received.path) continue
      differences.push({
        reason: 'order',
        path: expected.path,
        property: 'order',
        baseline: expected.path,
        actual: received.path,
      })
      break
    }
  }

  for (const [path, node] of baselineNodes) {
    const received = actualNodes.get(path)
    if (received === undefined) continue
    if (node.tag !== received.tag) {
      differences.push({
        reason: 'meta',
        path,
        property: 'tag',
        baseline: node.tag,
        actual: received.tag,
      })
    }
    if (node.component !== received.component) {
      differences.push({
        reason: 'state',
        path,
        property: 'component',
        baseline: node.component,
        actual: received.component,
      })
    }
    if (node.role !== received.role) {
      differences.push({
        reason: 'state',
        path,
        property: 'role',
        baseline: node.role,
        actual: received.role,
      })
    }
    if (node.text !== received.text) {
      differences.push({
        reason: 'text',
        path,
        property: 'text',
        baseline: node.text === '' ? null : node.text,
        actual: received.text === '' ? null : received.text,
      })
    }
    compareRecords('state', 'state.', path, node.state, received.state, differences)
    compareRecords('style', 'style.', path, node.style, received.style, differences)
    compareBoxes(path, node.box, received.box, tolerance, differences)
  }

  return differences
}

const REASON_LABELS: Readonly<Record<VisualRegressionReasonCode, string>> = Object.freeze({
  geometry: 'geometry moved',
  meta: 'capture identity changed',
  'missing-node': 'a rendered element disappeared',
  order: 'paint order changed',
  state: 'component state changed',
  style: 'presentation changed',
  text: 'rendered text changed',
  'unexpected-node': 'a new element is rendered',
})

export function visualRegressionReport(
  differences: readonly VisualDifference[],
): VisualRegressionReport {
  const byReason = {} as Record<VisualRegressionReasonCode, number>
  for (const difference of differences) {
    byReason[difference.reason] = (byReason[difference.reason] ?? 0) + 1
  }
  const sorted = {} as Record<VisualRegressionReasonCode, number>
  for (const reason of Object.keys(byReason).sort((left, right) => left.localeCompare(right))) {
    sorted[reason as VisualRegressionReasonCode] = byReason[reason as VisualRegressionReasonCode]
  }
  return { total: differences.length, byReason: sorted, entries: [...differences] }
}

export function renderVisualRegressionReport(
  differences: readonly VisualDifference[],
  identity: { readonly id: string },
): string {
  const report = visualRegressionReport(differences)
  if (report.total === 0) return `${identity.id}: no visual differences`
  const lines = [
    `UI-608 visual regression failed for ${identity.id}: `
      + `${String(report.total)} visual differences`,
    ...Object.entries(report.byReason).map(([reason, count]) => (
      `  ${reason} (${REASON_LABELS[reason as VisualRegressionReasonCode]}): ${String(count)}`
    )),
    ...report.entries.map(entry => (
      `  [${entry.reason}] ${entry.path} ${entry.property}`
        + ` baseline=${entry.baseline === null ? '<absent>' : entry.baseline}`
        + ` actual=${entry.actual === null ? '<absent>' : entry.actual}`
    )),
  ]
  return lines.join('\n')
}
