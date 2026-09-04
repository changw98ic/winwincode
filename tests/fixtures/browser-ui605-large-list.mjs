// SPDX-License-Identifier: Apache-2.0

import { mountStrongFlowDeliveryList } from '/module/strongflow-delivery-list-page.js'

// UI-605 performance fixture: one real browser mounts the real Delivery list
// page over an enterprise-sized corpus and measures the recorded budgets.
const DELIVERY_COUNT = 5000
const SCROLL_STEPS = 40

const STATUSES = [
  'draft',
  'clarifying',
  'ready',
  'planning',
  'plan-review',
  'executing',
  'verifying',
  'reworking',
  'ready-to-deliver',
  'delivered',
]

function deliveryId(index) {
  return `dlv_${String(index).padStart(22, '0')}`
}

function corpus(count) {
  return Array.from({ length: count }, (_, index) => ({
    deliveryId: deliveryId(index + 1),
    revision: (index % 7) + 1,
    status: STATUSES[index % STATUSES.length],
    title: `Enterprise delivery ${String(index + 1)} — ${
      index % 3 === 0 ? 'kernel' : index % 3 === 1 ? 'control plane' : 'client'
    } workstream`,
    openAttentionCount: index % 11 === 0 ? 1 : 0,
  }))
}

const state = {
  status: 'ready',
  filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
  visible: corpus(DELIVERY_COUNT),
  loadedCount: DELIVERY_COUNT,
  hasMore: true,
  loadingMore: false,
  moreFailure: null,
  error: null,
  advance: { deliveryId: null, failure: null },
}

const model = {
  state,
  listener: null,
  calls: [],
  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  },
  publish(next) {
    this.state = next
    this.listener?.(next)
  },
  async start() {}
  ,
  async refresh() { this.calls.push(['refresh']) },
  async loadMore() { this.calls.push(['loadMore']) },
  setSearch(value) { this.calls.push(['setSearch', value]) },
  async setStatusFilter(value) { this.calls.push(['setStatusFilter', value]) },
  setAttentionOnly() {},
  setOrder() {},
  async advanceDelivery() {},
  close() {},
}

function styleViewport() {
  const style = document.createElement('style')
  style.textContent = [
    'html, body { margin: 0; }',
    '[data-winwincode-client-root] { width: 480px; }',
  ].join('\n')
  document.head.append(style)
}

function nextFrame() {
  return new Promise(resolve => requestAnimationFrame(() => resolve()))
}

function countNodes(root) {
  return root.querySelectorAll('*').length
}

async function scrollSteps(scroller, steps) {
  const maximum = scroller.scrollHeight - scroller.clientHeight
  const started = performance.now()
  for (let step = 1; step <= steps; step += 1) {
    scroller.scrollTop = Math.round(maximum * (step / steps))
    scroller.dispatchEvent(new Event('scroll'))
    // Every step waits for the frame the browser would paint, so the measured
    // time is the time a real scroll would cost.
    await nextFrame()
  }
  return performance.now() - started
}

globalThis.runUi605LargeListScenario = async function runUi605LargeListScenario() {
  styleViewport()
  const root = document.querySelector('[data-winwincode-client-root]')
  const css = document.createElement('link')
  css.rel = 'stylesheet'
  css.href = '/assets/client.css'
  document.head.append(css)

  const mountStarted = performance.now()
  const page = mountStrongFlowDeliveryList({ root, model, view: 'list' })
  await nextFrame()
  const mounted = performance.now()

  const scroller = root.querySelector('.wwc-window-scroller')
  const rowsAtMount = scroller.querySelectorAll('li').length
  const nodesAtMount = countNodes(root)

  // The first interaction: focus the search field and type into it, which must
  // reach the model and repaint within the recorded budget.
  const search = root.querySelector('.wwc-delivery-search')
  const interactionStarted = performance.now()
  search.focus()
  search.value = 'control plane'
  search.dispatchEvent(new Event('input', { bubbles: true }))
  const searchAccepted = model.calls.at(-1)[0] === 'setSearch'
  const firstInteractionMillis = performance.now() - interactionStarted
  const nodesAfterSearch = countNodes(root)

  model.publish({
    ...state,
    filters: { ...state.filters, search: 'control plane' },
    visible: state.visible.filter(row => row.title.includes('control plane')),
  })
  await nextFrame()
  const filteredRows = scroller.querySelectorAll('li').length
  const nodesFiltered = countNodes(root)
  const note = root.querySelector('.wwc-delivery-loaded-note').textContent

  // Return to the unfiltered corpus, then scroll the whole loaded list.
  model.publish(state)
  await nextFrame()
  const scrollMillis = await scrollSteps(scroller, SCROLL_STEPS)
  const nodesAfterScroll = countNodes(root)
  const rowsAfterScroll = scroller.querySelectorAll('li').length

  // A deep link to a Delivery outside the rendered window.
  const deepStarted = performance.now()
  page.setActive({
    deliveryId: deliveryId(4_500),
    revision: 1,
    status: 'executing',
    title: 'Enterprise delivery 4500 — client workstream',
    openAttentionCount: 0,
  })
  await nextFrame()
  const deepLinkMillis = performance.now() - deepStarted
  const deepLinked = scroller.querySelector(`[data-delivery-id="${deliveryId(4_500)}"]`)
  const deepLinkRendered = deepLinked !== null
    && deepLinked.getAttribute('aria-current') === 'page'

  const result = {
    corpus: DELIVERY_COUNT,
    rowsAtMount,
    nodesAtMount,
    nodesAfterSearch,
    nodesFiltered,
    nodesAfterScroll,
    rowsAfterScroll,
    filteredRows,
    note,
    searchAccepted,
    deepLinkRendered,
    deepLinkMillis,
    firstInteractionMillis,
    scrollMillis,
    mountedMillis: mounted - mountStarted,
    scrollSteps: SCROLL_STEPS,
  }
  page.close()
  return result
}
