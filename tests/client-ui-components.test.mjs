import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const repositoryRoot = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.ui-components-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: repositoryRoot, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Client UI components did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const { mountButton } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/button.js',
)).href}`)
const { mountStatusBadge } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/status-badge.js',
)).href}`)
const { mountPageHeader } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/page-header.js',
)).href}`)
const { mountPanel } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/panel.js',
)).href}`)
const { mountMetric } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/metric.js',
)).href}`)
const { mountFormField } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/form-field.js',
)).href}`)
const { mountEmptyState } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/empty-state.js',
)).href}`)
const { mountErrorState } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/error-state.js',
)).href}`)
const { mountToolbar } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/toolbar.js',
)).href}`)
const { mountTabs } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/tabs.js',
)).href}`)
const { mountDrawer } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/drawer.js',
)).href}`)
const { mountActionBar } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/action-bar.js',
)).href}`)
const { mountSplitPane } = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/split-pane.js',
)).href}`)
const components = await import(`${pathToFileURL(resolve(
  repositoryRoot,
  '.cache/ui-components-tests/components/index.js',
)).href}`)

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  type = ''
  id = ''
  tabIndex = 0
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) {
    this.children.push(...children)
  }

  replaceChildren(...children) {
    this.children = [...children]
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    this.listeners.set(name, listeners.filter(candidate => candidate !== listener))
  }

  dispatch(name, fields = {}) {
    if (name === 'click' && this.disabled) return
    const event = {
      currentTarget: this,
      target: this,
      key: '',
      defaultPrevented: false,
      preventDefault() { this.defaultPrevented = true },
      ...fields,
    }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
    return event
  }

  focus() {
    this.ownerDocument.activeElement = this
  }

  remove() {
    this.removed = true
  }
}

class FakeDocument {
  activeElement = null

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

test('component barrel exposes the complete first primitive set', () => {
  assert.deepEqual(
    [
      'mountActionBar',
      'mountButton',
      'mountDrawer',
      'mountEmptyState',
      'mountErrorState',
      'mountFormField',
      'mountMetric',
      'mountPageHeader',
      'mountPanel',
      'mountSplitPane',
      'mountStatusBadge',
      'mountTabs',
      'mountToolbar',
    ].filter(name => typeof components[name] !== 'function'),
    [],
  )
})

test('Button keeps one native control across updates and exposes busy and destructive states', () => {
  const document = new FakeDocument()
  const activations = []
  const mounted = mountButton({
    document,
    props: {
      label: 'Save settings',
      variant: 'primary',
      onActivate: () => { activations.push('save') },
    },
  })

  assert.equal(mounted.root.tagName, 'BUTTON')
  assert.equal(mounted.root.className, 'wwc-button')
  assert.equal(mounted.root.dataset.variant, 'primary')
  mounted.root.dispatch('click')
  assert.deepEqual(activations, ['save'])

  const identity = mounted.root
  mounted.update({
    label: 'Save settings',
    busyLabel: 'Saving settings',
    variant: 'destructive',
    busy: true,
    onActivate: () => { activations.push('busy') },
  })
  assert.equal(mounted.root, identity)
  assert.equal(mounted.root.disabled, true)
  assert.equal(mounted.root.getAttribute('aria-busy'), 'true')
  assert.equal(mounted.root.textContent, 'Saving settings')
  assert.equal(mounted.root.dataset.variant, 'destructive')
  mounted.root.dispatch('click')
  assert.deepEqual(activations, ['save'])

  mounted.close()
  assert.equal(mounted.root.removed, true)
  assert.throws(() => {
    mounted.update({ label: 'Again', onActivate() {} })
  }, /closed/u)
})

test('StatusBadge communicates tone with text and a hidden visual icon', () => {
  const document = new FakeDocument()
  const mounted = mountStatusBadge({
    document,
    props: { label: 'Connection restored', tone: 'success', live: 'polite' },
  })

  assert.equal(mounted.root.dataset.tone, 'success')
  assert.equal(mounted.root.getAttribute('role'), 'status')
  assert.equal(mounted.root.getAttribute('aria-live'), 'polite')
  assert.equal(mounted.icon.getAttribute('aria-hidden'), 'true')
  assert.equal(mounted.icon.textContent, '✓')
  assert.equal(mounted.label.textContent, 'Connection restored')

  mounted.update({ label: 'Permission denied', tone: 'danger', live: 'assertive' })
  assert.equal(mounted.root.dataset.tone, 'danger')
  assert.equal(mounted.root.getAttribute('role'), 'alert')
  assert.equal(mounted.icon.textContent, '×')
  assert.equal(mounted.label.textContent, 'Permission denied')
  mounted.close()
})

test('PageHeader updates its copy without replacing heading or description nodes', () => {
  const document = new FakeDocument()
  const mounted = mountPageHeader({
    document,
    props: {
      title: 'Local operations',
      description: 'Repository and Worker diagnostics',
      eyebrow: 'Operations',
      headingLevel: 2,
    },
  })

  const title = mounted.title
  const description = mounted.description
  assert.equal(title.tagName, 'H2')
  assert.equal(title.textContent, 'Local operations')
  assert.equal(description.hidden, false)

  mounted.update({ title: 'Provider settings', headingLevel: 2 })
  assert.equal(mounted.title, title)
  assert.equal(mounted.description, description)
  assert.equal(title.textContent, 'Provider settings')
  assert.equal(description.hidden, true)
  assert.equal(mounted.eyebrow.hidden, true)
  mounted.close()
})

test('Panel exposes one labelled content region and keeps caller content mounted', () => {
  const document = new FakeDocument()
  const mounted = mountPanel({
    document,
    props: { id: 'worker-health', title: 'Worker health', description: 'Current capacity' },
  })
  const callerContent = document.createElement('button')
  mounted.content.append(callerContent)

  assert.equal(mounted.root.getAttribute('aria-labelledby'), 'worker-health-title')
  assert.equal(mounted.title.id, 'worker-health-title')
  mounted.update({ id: 'worker-health', title: 'Worker health', busy: true })
  assert.equal(mounted.content.children[0], callerContent)
  assert.equal(mounted.root.getAttribute('aria-busy'), 'true')
  assert.equal(mounted.description.hidden, true)
  assert.throws(() => {
    mounted.update({ id: 'another-panel', title: 'Moved' })
  }, /cannot change/u)
  mounted.close()
})

test('Metric keeps its label and value explicit when the value changes', () => {
  const document = new FakeDocument()
  const mounted = mountMetric({
    document,
    props: { label: 'Enabled Workers', value: '2', hint: 'of 3 registered', tone: 'success' },
  })
  const value = mounted.value
  assert.equal(mounted.label.textContent, 'Enabled Workers')
  assert.equal(value.textContent, '2')
  assert.equal(mounted.root.dataset.tone, 'success')
  mounted.update({ label: 'Enabled Workers', value: '0', hint: 'capacity unavailable', tone: 'danger' })
  assert.equal(mounted.value, value)
  assert.equal(value.textContent, '0')
  assert.equal(mounted.hint.textContent, 'capacity unavailable')
  mounted.close()
})

test('FormField preserves its control and connects label, help, required, and error semantics', () => {
  const document = new FakeDocument()
  const control = document.createElement('textarea')
  control.value = 'unsent draft'
  const mounted = mountFormField({
    document,
    props: {
      id: 'review-notes',
      label: 'Review notes',
      help: 'Explain the decision.',
      control,
      required: true,
    },
  })

  assert.equal(mounted.control, control)
  assert.equal(mounted.label.htmlFor, 'review-notes-control')
  assert.equal(control.getAttribute('aria-describedby'), 'review-notes-help')
  assert.equal(control.getAttribute('aria-required'), 'true')

  mounted.update({
    id: 'review-notes',
    label: 'Review notes',
    help: 'Explain the decision.',
    error: 'Review notes are required.',
    control,
    required: true,
  })
  assert.equal(control.value, 'unsent draft')
  assert.equal(control.getAttribute('aria-invalid'), 'true')
  assert.equal(control.getAttribute('aria-describedby'), 'review-notes-help review-notes-error')
  assert.equal(mounted.error.getAttribute('role'), 'alert')
  mounted.close()
})

test('EmptyState gives an empty collection a labelled message and optional next action', () => {
  const document = new FakeDocument()
  const create = document.createElement('button')
  create.textContent = 'Create Delivery'
  const mounted = mountEmptyState({
    document,
    props: {
      title: 'No Deliveries yet',
      detail: 'Create the first Delivery for this repository.',
      action: create,
    },
  })
  assert.equal(mounted.root.getAttribute('role'), 'status')
  assert.equal(mounted.title.textContent, 'No Deliveries yet')
  assert.equal(mounted.actions.children[0], create)

  mounted.update({ title: 'No results', detail: 'Change the current filter.' })
  assert.equal(mounted.actions.hidden, true)
  assert.equal(mounted.actions.children.length, 0)
  mounted.close()
})

test('ErrorState announces an error with a non-color signal and preserves recovery actions', () => {
  const document = new FakeDocument()
  const retry = document.createElement('button')
  retry.textContent = 'Retry snapshot'
  const mounted = mountErrorState({
    document,
    props: {
      title: 'Settings could not be loaded',
      message: 'The Server did not respond.',
      actions: [retry],
    },
  })

  assert.equal(mounted.root.getAttribute('role'), 'alert')
  assert.equal(mounted.root.dataset.tone, 'danger')
  assert.equal(mounted.icon.getAttribute('aria-hidden'), 'true')
  assert.equal(mounted.actions.children[0], retry)

  mounted.update({
    title: 'Settings could not be loaded',
    message: 'The Server did not respond.',
    actions: [retry],
    visible: false,
  })
  assert.equal(mounted.root.hidden, true)
  assert.equal(mounted.actions.children[0], retry)
  mounted.close()
})

test('Toolbar is one labelled keyboard stop with arrow, Home, and End navigation', () => {
  const document = new FakeDocument()
  const first = document.createElement('button')
  const disabled = document.createElement('button')
  const last = document.createElement('button')
  disabled.disabled = true
  const mounted = mountToolbar({
    document,
    props: { label: 'Review actions', items: [first, disabled, last] },
  })

  assert.equal(mounted.root.getAttribute('role'), 'toolbar')
  assert.equal(mounted.root.getAttribute('aria-label'), 'Review actions')
  assert.deepEqual([first.tabIndex, disabled.tabIndex, last.tabIndex], [0, -1, -1])
  first.focus()
  const next = mounted.root.dispatch('keydown', { key: 'ArrowRight' })
  assert.equal(next.defaultPrevented, true)
  assert.equal(document.activeElement, last)
  mounted.root.dispatch('keydown', { key: 'Home' })
  assert.equal(document.activeElement, first)
  mounted.root.dispatch('keydown', { key: 'End' })
  assert.equal(document.activeElement, last)
  mounted.close()
})

test('Tabs preserve tab identity and support automatic arrow-key selection', () => {
  const document = new FakeDocument()
  const selections = []
  const tabs = [
    { id: 'overview', label: 'Overview', panelId: 'overview-panel' },
    { id: 'disabled', label: 'Unavailable', panelId: 'disabled-panel', disabled: true },
    { id: 'evidence', label: 'Evidence', panelId: 'evidence-panel' },
  ]
  const mounted = mountTabs({
    document,
    props: {
      id: 'delivery-tabs',
      label: 'Delivery views',
      tabs,
      selectedId: 'overview',
      onSelect: id => { selections.push(id) },
    },
  })
  const overview = mounted.tab('overview')
  assert.equal(overview.getAttribute('role'), 'tab')
  assert.equal(overview.getAttribute('aria-selected'), 'true')
  assert.equal(overview.getAttribute('aria-controls'), 'overview-panel')
  overview.focus()
  overview.dispatch('keydown', { key: 'ArrowRight' })
  assert.deepEqual(selections, ['evidence'])
  assert.equal(document.activeElement, mounted.tab('evidence'))

  mounted.update({
    id: 'delivery-tabs',
    label: 'Delivery views',
    tabs: tabs.map(tab => tab.id === 'overview' ? { ...tab, label: 'Summary' } : tab),
    selectedId: 'evidence',
    onSelect: id => { selections.push(id) },
  })
  assert.equal(mounted.tab('overview'), overview)
  assert.equal(overview.textContent, 'Summary')
  assert.equal(mounted.tab('evidence').getAttribute('aria-selected'), 'true')
  mounted.close()
})

test('Drawer labels its dialog, closes on Escape, and restores the previous focus', () => {
  const document = new FakeDocument()
  const trigger = document.createElement('button')
  const content = document.createElement('div')
  const closes = []
  trigger.focus()
  const mounted = mountDrawer({
    document,
    props: {
      id: 'diagnostics',
      title: 'Diagnostics',
      open: true,
      content,
      onClose: () => { closes.push('escape') },
    },
  })

  assert.equal(mounted.root.getAttribute('role'), 'dialog')
  assert.equal(mounted.root.getAttribute('aria-modal'), 'false')
  assert.equal(mounted.root.getAttribute('aria-labelledby'), 'diagnostics-title')
  assert.equal(document.activeElement, mounted.closeButton)
  mounted.root.dispatch('keydown', { key: 'Escape' })
  assert.deepEqual(closes, ['escape'])

  mounted.update({
    id: 'diagnostics',
    title: 'Diagnostics',
    open: false,
    content,
    onClose: () => { closes.push('closed') },
  })
  assert.equal(mounted.root.hidden, true)
  assert.equal(document.activeElement, trigger)
  mounted.close()
})

test('ActionBar keeps native actions in caller order and exposes one labelled group', () => {
  const document = new FakeDocument()
  const cancel = document.createElement('button')
  const save = document.createElement('button')
  const mounted = mountActionBar({
    document,
    props: { label: 'Settings actions', items: [cancel, save], align: 'end' },
  })
  assert.equal(mounted.root.getAttribute('role'), 'group')
  assert.equal(mounted.root.getAttribute('aria-label'), 'Settings actions')
  assert.deepEqual(mounted.root.children, [cancel, save])
  mounted.update({ label: 'Settings actions', items: [save, cancel], align: 'space-between' })
  assert.deepEqual(mounted.root.children, [save, cancel])
  assert.equal(mounted.root.dataset.align, 'space-between')
  mounted.close()
})

test('SplitPane preserves both labelled regions while layout orientation changes', () => {
  const document = new FakeDocument()
  const primary = document.createElement('div')
  const secondary = document.createElement('div')
  const mounted = mountSplitPane({
    document,
    props: {
      primary,
      primaryLabel: 'Delivery list',
      secondary,
      secondaryLabel: 'Delivery detail',
      orientation: 'horizontal',
    },
  })
  assert.equal(mounted.primary.getAttribute('aria-label'), 'Delivery list')
  assert.equal(mounted.secondary.getAttribute('aria-label'), 'Delivery detail')
  assert.equal(mounted.primary.children[0], primary)
  assert.equal(mounted.secondary.children[0], secondary)
  mounted.update({
    primary,
    primaryLabel: 'Delivery list',
    secondary,
    secondaryLabel: 'Delivery detail',
    orientation: 'vertical',
    secondaryHidden: true,
  })
  assert.equal(mounted.root.dataset.orientation, 'vertical')
  assert.equal(mounted.secondary.hidden, true)
  assert.equal(mounted.primary.children[0], primary)
  mounted.close()
})
