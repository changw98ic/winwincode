import { mountEnterpriseOperationsPage } from '/module/enterprise-operations-page.js'
import { mountEnterpriseResourcePage } from '/module/enterprise-resource-page.js'

function area(permission = 'allowed') {
  return {
    status: permission === 'allowed' ? 'ready' : 'permission-denied',
    permission,
    revision: permission === 'allowed' ? 1 : null,
    pages: [],
    error: null,
  }
}

function enterpriseState(busy = false) {
  return {
    status: busy ? 'loading' : 'partial',
    realtime: 'subscribed',
    interaction: {
      status: busy ? 'submitting' : 'idle',
      area: busy ? 'members' : null,
      error: null,
    },
    error: null,
    areas: {
      organization: area('denied'),
      members: area(),
      projects: area(),
      policy: area(),
      fleet: area(),
      usage: area(),
      audit: area(),
      integration: area('denied'),
    },
  }
}

function fixtureModel() {
  let state = enterpriseState()
  const listeners = new Set()
  return {
    get state() { return state },
    subscribe(listener) {
      listeners.add(listener)
      listener(state)
      return () => { listeners.delete(listener) }
    },
    async start() {},
    async refresh() {},
    async execute() {},
    reconnect() {},
    close() {},
    setBusy(busy) {
      state = enterpriseState(busy)
      for (const listener of listeners) listener(state)
    },
  }
}

const host = document.querySelector('[data-winwincode-client-root]')
const resourcesRoot = document.createElement('div')
const operationsRoot = document.createElement('div')
resourcesRoot.dataset.enterpriseFixture = 'resources'
operationsRoot.dataset.enterpriseFixture = 'operations'
host.replaceChildren(resourcesRoot, operationsRoot)

const resourceModel = fixtureModel()
const operationsModel = fixtureModel()
mountEnterpriseResourcePage({ root: resourcesRoot, model: resourceModel })
mountEnterpriseOperationsPage({ root: operationsRoot, model: operationsModel })

globalThis.setEnterpriseBusy = busy => {
  resourceModel.setBusy(busy)
  operationsModel.setBusy(busy)
}

globalThis.inspectEnterpriseManagement = () => {
  const resources = document.querySelector('.wwc-enterprise-resources')
  const operations = document.querySelector('.wwc-enterprise-operations')
  const organization = document.querySelector('.wwc-enterprise-organizations')
  const members = document.querySelector('.wwc-enterprise-members')
  const status = document.querySelector('.wwc-enterprise-resources-status')
  const statusIcon = status.querySelector('.wwc-status-badge-icon')
  const organizationRect = organization.getBoundingClientRect()
  const membersRect = members.getBoundingClientRect()
  return {
    resourcesPage: resources.dataset.wwcPage,
    operationsPage: operations.dataset.wwcPage,
    resourcePanels: resources.querySelectorAll('[data-wwc-component="panel"]').length,
    operationsPanels: operations.querySelectorAll('[data-wwc-component="panel"]').length,
    resourceEmpty: resources.querySelectorAll('[data-state="empty"]').length,
    operationsEmpty: operations.querySelectorAll('[data-state="empty"]').length,
    deniedFieldsetDisabled: document.querySelector('.wwc-enterprise-organization-fields').disabled,
    availableFieldsetDisabled: document.querySelector('.wwc-enterprise-member-fields').disabled,
    statusIcon: statusIcon.textContent,
    statusIconHidden: statusIcon.getAttribute('aria-hidden'),
    statusRole: status.getAttribute('role'),
    busy: resources.getAttribute('aria-busy'),
    noHorizontalOverflow: document.documentElement.scrollWidth <= document.documentElement.clientWidth,
    panelsShareRow: Math.abs(organizationRect.top - membersRect.top) < 1,
    panelsStacked: membersRect.top >= organizationRect.bottom,
  }
}

globalThis.inspectEnterpriseFocus = () => {
  const control = document.querySelector('.wwc-enterprise-member-id')
  control.focus()
  const style = getComputedStyle(control)
  return {
    active: document.activeElement === control,
    outlineStyle: style.outlineStyle,
    outlineWidth: style.outlineWidth,
  }
}
