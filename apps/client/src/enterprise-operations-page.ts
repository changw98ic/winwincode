// SPDX-License-Identifier: Apache-2.0

import type {
  CommandRequest,
  CredentialReferenceId,
  EnterpriseAuditProjection,
  EnterpriseFleetProjection,
  EnterpriseFleetUpdatePayload,
  EnterpriseIntegrationId,
  EnterpriseIntegrationProjection,
  EnterpriseIntegrationUpdatePayload,
  EnterprisePolicyId,
  EnterprisePolicyProjection,
  EnterprisePolicyUpdatePayload,
  EnterpriseUsageProjection,
  GitHubRepositorySlug,
  Instant,
  Sha256Digest,
} from './generated/contracts.js'
import type {
  EnterpriseManagementArea,
  EnterpriseManagementCommandContext,
  EnterpriseManagementViewModel,
  EnterpriseManagementViewModelState,
} from './enterprise-management-view-model.js'

export interface EnterpriseOperationsPageOptions {
  readonly root: HTMLElement
  readonly model: EnterpriseManagementViewModel
  /** Browser-host download seam; receives only the public bounded audit projection. */
  readonly onAuditExport?: (filename: string, content: string) => void
  /** Canonical effective instant seam for deterministic browser fixtures. */
  readonly now?: () => Instant
}

export interface EnterpriseOperationsPage {
  exportAuditCsv(): string
  close(): void
}

type MutableOperationsArea = 'policy' | 'fleet' | 'integration'

export interface EnterpriseOperationsSnapshot {
  readonly policies: readonly EnterprisePolicyProjection[]
  readonly fleets: readonly EnterpriseFleetProjection[]
  readonly usage: readonly EnterpriseUsageProjection[]
  readonly audit: readonly EnterpriseAuditProjection[]
  readonly integrations: readonly EnterpriseIntegrationProjection[]
}

export interface EnterpriseOperationsPagePresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
  readonly mutationsDisabled: Readonly<Record<MutableOperationsArea, boolean>>
}

function visibleError(
  state: EnterpriseManagementViewModelState,
): EnterpriseManagementViewModelState['error'] {
  return state.interaction.error
    ?? state.error
    ?? state.areas.policy.error
    ?? state.areas.fleet.error
    ?? state.areas.usage.error
    ?? state.areas.audit.error
    ?? state.areas.integration.error
}

function errorLabel(error: EnterpriseManagementViewModelState['error']): string | null {
  if (error === null) return null
  if (error.code === 'REVISION_CONFLICT') {
    return 'This enterprise setting changed before the update was saved. Review the current snapshot and try again.'
  }
  if (error.code === 'ENTERPRISE_MANAGEMENT_SNAPSHOT_STALE') {
    return 'The update was saved, but the operations snapshot has not reached that revision. Refresh again.'
  }
  if (error.code === 'ENTERPRISE_MANAGEMENT_PERMISSION_REQUIRED') {
    return 'Your current role does not allow this enterprise operation.'
  }
  if (error.kind === 'authentication') return 'Sign in again to view enterprise operations.'
  if (error.kind === 'authorization') return 'Your current role does not allow this enterprise operation.'
  if (error.kind === 'network') return 'The enterprise Control Plane could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The enterprise operation was cancelled.'
  if (error.kind === 'configuration' || error.code === 'INVALID_CLIENT_REQUEST') {
    return 'Check the signed-in identity and enterprise scope, then retry.'
  }
  return 'Enterprise operations could not be updated. Refresh the current snapshot and retry.'
}

function areaMutationDisabled(
  state: EnterpriseManagementViewModelState,
  area: MutableOperationsArea,
  busy: boolean,
): boolean {
  const current = state.areas[area]
  return busy
    || current.permission !== 'allowed'
    || current.revision === null
    || state.status === 'authentication-required'
    || state.status === 'authorization-denied'
    || state.status === 'closed'
}

export function enterpriseOperationsPagePresentation(
  state: EnterpriseManagementViewModelState,
): EnterpriseOperationsPagePresentation {
  const error = visibleError(state)
  const busy = state.status === 'loading'
    || state.realtime === 'reloading'
    || state.interaction.status === 'submitting'
    || state.interaction.status === 'waiting'
  const statusText = state.interaction.status === 'submitting'
    ? 'Saving enterprise operation…'
    : state.interaction.status === 'waiting'
      ? 'Change accepted · waiting for the current snapshot…'
      : state.status === 'loading'
        ? 'Loading enterprise operations…'
        : state.realtime === 'reloading'
          ? 'Updating enterprise operations…'
          : state.realtime === 'reconnecting'
            ? 'Reconnecting…'
            : state.status === 'authentication-required'
              ? 'Sign in required'
              : state.status === 'authorization-denied'
                ? 'Access denied'
                : state.status === 'cancelled'
                  ? 'Update cancelled'
                  : state.status === 'error'
                    ? 'Enterprise operations unavailable'
                    : state.status === 'closed'
                      ? 'Enterprise operations closed'
                      : 'Enterprise operations ready'
  return Object.freeze({
    statusText,
    errorText: errorLabel(error),
    busy,
    retryVisible: error !== null && state.realtime !== 'reconnecting',
    reconnectVisible: state.realtime === 'reconnecting',
    mutationsDisabled: Object.freeze({
      policy: areaMutationDisabled(state, 'policy', busy),
      fleet: areaMutationDisabled(state, 'fleet', busy),
      integration: areaMutationDisabled(state, 'integration', busy),
    }),
  })
}

export function enterpriseOperationsSnapshot(
  state: EnterpriseManagementViewModelState,
): EnterpriseOperationsSnapshot {
  return Object.freeze({
    policies: Object.freeze(state.areas.policy.pages.flatMap(page => (
      page.query === 'enterprise.policy.list' ? page.result.items : []
    ))),
    fleets: Object.freeze(state.areas.fleet.pages.flatMap(page => (
      page.query === 'enterprise.fleet.list' ? page.result.items : []
    ))),
    usage: Object.freeze(state.areas.usage.pages.flatMap(page => (
      page.query === 'enterprise.usage.list' ? page.result.items : []
    ))),
    audit: Object.freeze(state.areas.audit.pages.flatMap(page => (
      page.query === 'enterprise.audit.list' ? page.result.items : []
    ))),
    integrations: Object.freeze(state.areas.integration.pages.flatMap(page => (
      page.query === 'enterprise.integration.list' ? page.result.items : []
    ))),
  })
}

function csvCell(value: string | number): string {
  const text = String(value)
  return /[",\n\r]/u.test(text) ? `"${text.replaceAll('"', '""')}"` : text
}

/** Export only the closed public audit projection; raw request and tool payloads have no slot. */
export function enterpriseAuditCsv(records: readonly EnterpriseAuditProjection[]): string {
  const rows: readonly (readonly (string | number)[])[] = [
    ['sequence', 'occurredAt', 'category', 'action', 'outcome', 'actorKind', 'actorId', 'revision', 'recordSha256'],
    ...records.map(record => [
      record.sequence,
      record.occurredAt,
      record.category,
      record.action,
      record.outcome,
      record.actor.kind,
      record.actor.id,
      record.revision,
      record.recordSha256,
    ]),
  ]
  return `${rows.map(row => row.map(csvCell).join(',')).join('\n')}\n`
}

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function labelledInput(
  document: Document,
  id: string,
  labelText: string,
  className: string,
  required = true,
): { readonly label: HTMLLabelElement; readonly input: HTMLInputElement } {
  const label = element(document, 'label', `${className}-label`)
  const input = element(document, 'input', className)
  label.htmlFor = id
  label.textContent = labelText
  input.id = id
  input.type = 'text'
  input.required = required
  input.autocomplete = 'off'
  label.append(input)
  return Object.freeze({ label, input })
}

function labelledSelect(
  document: Document,
  id: string,
  labelText: string,
  className: string,
  values: readonly (readonly [string, string])[],
): { readonly label: HTMLLabelElement; readonly select: HTMLSelectElement } {
  const label = element(document, 'label', `${className}-label`)
  const select = element(document, 'select', className)
  label.htmlFor = id
  label.textContent = labelText
  select.id = id
  for (const [value, text] of values) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = text
    select.append(option)
  }
  label.append(select)
  return Object.freeze({ label, select })
}

function shortValue(value: string): string {
  return `…${value.slice(-8)}`
}

function areaLabel(
  state: EnterpriseManagementViewModelState,
  area: EnterpriseManagementArea,
): string {
  const current = state.areas[area]
  if (current.permission === 'denied') return 'Access denied for this section.'
  if (current.status === 'loading' || current.status === 'refreshing') return 'Updating this section…'
  if (current.status === 'revision-conflict') return 'This section changed. Review it before retrying.'
  if (current.status === 'error') return 'This section is unavailable. Refresh and retry.'
  return current.revision === null ? 'No current snapshot.' : `Revision ${String(current.revision)}`
}

function policyCommand(
  context: EnterpriseManagementCommandContext,
  payload: EnterprisePolicyUpdatePayload,
): CommandRequest {
  return {
    schemaVersion: 'winwincode/v1',
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    command: 'enterprise.policy.update',
    expectedRevision: context.expectedRevision,
    payload,
  }
}

function fleetCommand(
  context: EnterpriseManagementCommandContext,
  payload: EnterpriseFleetUpdatePayload,
): CommandRequest {
  return {
    schemaVersion: 'winwincode/v1',
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    command: 'enterprise.fleet.update',
    expectedRevision: context.expectedRevision,
    payload,
  }
}

function integrationCommand(
  context: EnterpriseManagementCommandContext,
  payload: EnterpriseIntegrationUpdatePayload,
): CommandRequest {
  return {
    schemaVersion: 'winwincode/v1',
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    command: 'enterprise.integration.update',
    expectedRevision: context.expectedRevision,
    payload,
  }
}

interface OperationsSection<T> {
  readonly section: HTMLElement
  render(
    items: readonly T[],
    state: EnterpriseManagementViewModelState,
    disabled: boolean,
  ): void
}

/** Mount Policy, fleet, usage/quota, audit, and Integration operations. */
export function mountEnterpriseOperationsPage(
  options: EnterpriseOperationsPageOptions,
): EnterpriseOperationsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'main', 'wwc-enterprise-operations')
  const heading = element(document, 'h1', 'wwc-enterprise-operations-heading')
  const status = element(document, 'p', 'wwc-enterprise-operations-status')
  const error = element(document, 'div', 'wwc-enterprise-operations-error')
  const errorText = element(document, 'span', 'wwc-enterprise-operations-error-text')
  const retry = element(document, 'button', 'wwc-enterprise-operations-retry')
  const reconnect = element(document, 'button', 'wwc-enterprise-operations-reconnect')
  const policy = createPolicySection(
    document,
    options.model,
    options.now ?? (() => new Date().toISOString()),
  )
  const fleet = createFleetSection(document, options.model)
  const usage = createUsageSection(document, options.model)
  const audit = createAuditSection(document, options.onAuditExport)
  const integration = createIntegrationSection(document, options.model)
  let latestAudit: readonly EnterpriseAuditProjection[] = Object.freeze([])
  let closed = false

  heading.textContent = 'Enterprise governance and operations'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  retry.type = 'button'
  retry.textContent = 'Retry snapshot'
  reconnect.type = 'button'
  reconnect.textContent = 'Reconnect events'
  error.append(errorText, retry, reconnect)
  layout.append(
    heading,
    status,
    error,
    policy.section,
    fleet.section,
    usage.section,
    audit.section,
    integration.section,
  )
  options.root.replaceChildren(layout)

  function render(state: EnterpriseManagementViewModelState): void {
    if (closed) return
    const presentation = enterpriseOperationsPagePresentation(state)
    const snapshot = enterpriseOperationsSnapshot(state)
    latestAudit = snapshot.audit
    status.textContent = presentation.statusText
    layout.setAttribute('aria-busy', String(presentation.busy))
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    policy.render(snapshot.policies, state, presentation.mutationsDisabled.policy)
    fleet.render(snapshot.fleets, state, presentation.mutationsDisabled.fleet)
    usage.render(snapshot.usage, state, false)
    audit.render(snapshot.audit, state, false)
    integration.render(
      snapshot.integrations,
      state,
      presentation.mutationsDisabled.integration,
    )
  }

  retry.addEventListener('click', () => { void options.model.refresh() })
  reconnect.addEventListener('click', () => { options.model.reconnect() })
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    exportAuditCsv() { return enterpriseAuditCsv(latestAudit) },
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}

function createPolicySection(
  document: Document,
  model: EnterpriseManagementViewModel,
  now: () => Instant,
): OperationsSection<EnterprisePolicyProjection> {
  const section = element(document, 'section', 'wwc-enterprise-policies')
  const heading = element(document, 'h2', 'wwc-enterprise-section-heading')
  const areaStatus = element(document, 'p', 'wwc-enterprise-policy-status')
  const list = element(document, 'ul', 'wwc-enterprise-policy-list')
  const fieldset = element(document, 'fieldset', 'wwc-enterprise-policy-fields')
  const legend = element(document, 'legend', 'wwc-enterprise-form-heading')
  const form = element(document, 'form', 'wwc-enterprise-policy-form')
  const id = labelledInput(document, 'wwc-enterprise-policy-id', 'Policy ID', 'wwc-enterprise-policy-id')
  const kind = labelledSelect(document, 'wwc-enterprise-policy-kind', 'Policy kind', 'wwc-enterprise-policy-kind', [
    ['repository', 'Repository'], ['model', 'Model'], ['provider', 'Provider'],
    ['tool', 'Tool'], ['network', 'Network'], ['approval', 'Approval'],
    ['verifier', 'Verifier'], ['worker_placement', 'Worker placement'],
    ['publication', 'Publication'], ['retention', 'Retention'], ['integration', 'Integration'],
  ])
  const mode = labelledSelect(document, 'wwc-enterprise-policy-mode', 'Mode', 'wwc-enterprise-policy-mode', [
    ['audit', 'Audit dry-run'], ['enforce', 'Enforce'],
  ])
  const state = labelledSelect(document, 'wwc-enterprise-policy-state', 'State', 'wwc-enterprise-policy-state', [
    ['draft', 'Draft'], ['active', 'Active'], ['retired', 'Retired'],
  ])
  const defaultEffect = labelledSelect(document, 'wwc-enterprise-policy-default', 'Default effect', 'wwc-enterprise-policy-default', [
    ['deny', 'Deny'], ['allow', 'Allow'],
  ])
  const inheritanceMode = labelledSelect(document, 'wwc-enterprise-policy-inheritance', 'Inheritance mode', 'wwc-enterprise-policy-inheritance', [
    ['tighten', 'Tighten inherited Policy'], ['override', 'Authorized override'],
  ])
  const childOverrideMode = labelledSelect(document, 'wwc-enterprise-policy-child-override', 'Child override mode', 'wwc-enterprise-policy-child-override', [
    ['tighten_only', 'Tighten only'],
    ['allow_explicit_relaxation', 'Allow explicit relaxation'],
  ])
  const ruleKind = labelledSelect(document, 'wwc-enterprise-policy-rule-kind', 'Rule kind', 'wwc-enterprise-policy-rule-kind', [
    ['repository', 'Repository'], ['model', 'Model'], ['provider', 'Provider'],
    ['tool', 'Tool'], ['network', 'Network'], ['approval', 'Approval'],
    ['verifier', 'Verifier'], ['worker_placement', 'Worker placement'],
    ['publication', 'Publication'], ['retention', 'Retention'], ['integration', 'Integration'],
  ])
  const ruleEffect = labelledSelect(document, 'wwc-enterprise-policy-rule-effect', 'Rule effect', 'wwc-enterprise-policy-rule-effect', [
    ['deny', 'Deny'], ['allow', 'Allow'],
  ])
  const resourcePattern = labelledInput(document, 'wwc-enterprise-policy-resource', 'Resource pattern', 'wwc-enterprise-policy-resource')
  const conditionDigest = labelledInput(document, 'wwc-enterprise-policy-condition-digest', 'Condition SHA-256', 'wwc-enterprise-policy-condition-digest')
  const definitionDigest = labelledInput(document, 'wwc-enterprise-policy-definition-digest', 'Definition SHA-256', 'wwc-enterprise-policy-definition-digest')
  const save = element(document, 'button', 'wwc-enterprise-policy-save')
  heading.id = 'wwc-enterprise-policies-heading'
  heading.textContent = 'Policy and dry-run'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  list.setAttribute('aria-live', 'polite')
  legend.textContent = 'Save a closed Policy definition'
  id.input.pattern = 'pol_[0-9A-HJKMNP-TV-Z]{26}'
  conditionDigest.input.pattern = 'sha256:[0-9a-f]{64}'
  definitionDigest.input.pattern = 'sha256:[0-9a-f]{64}'
  save.type = 'submit'
  save.textContent = 'Save Policy'
  form.addEventListener('submit', event => {
    event.preventDefault()
    const policyKind = kind.select.value as EnterprisePolicyProjection['policyKind']
    const payload: EnterprisePolicyUpdatePayload = {
      policyId: id.input.value as EnterprisePolicyId,
      policyKind,
      mode: mode.select.value as EnterprisePolicyProjection['mode'],
      state: state.select.value as EnterprisePolicyProjection['state'],
      effectiveAt: now(),
      inheritanceMode: inheritanceMode.select.value as 'tighten' | 'override',
      baseVersion: null,
      definitionSha256: definitionDigest.input.value as Sha256Digest,
      definition: {
        childOverrideMode: childOverrideMode.select.value as (
          'tighten_only' | 'allow_explicit_relaxation'
        ),
        defaultEffect: defaultEffect.select.value as 'allow' | 'deny',
        rules: [{
          kind: ruleKind.select.value as EnterprisePolicyProjection['policyKind'],
          effect: ruleEffect.select.value as 'allow' | 'deny',
          resourcePattern: resourcePattern.input.value,
          conditionSha256: conditionDigest.input.value as Sha256Digest,
        }],
      },
    }
    void model.execute('policy', context => policyCommand(context, payload))
  })
  form.append(
    id.label,
    kind.label,
    mode.label,
    state.label,
    inheritanceMode.label,
    childOverrideMode.label,
    defaultEffect.label,
    ruleKind.label,
    ruleEffect.label,
    resourcePattern.label,
    conditionDigest.label,
    definitionDigest.label,
    save,
  )
  fieldset.append(legend, form)
  section.append(heading, areaStatus, list, fieldset)
  return {
    section,
    render(items, viewState, disabled) {
      areaStatus.textContent = areaLabel(viewState, 'policy')
      fieldset.disabled = disabled
      list.replaceChildren(...items.map(item => {
        const row = element(document, 'li', 'wwc-enterprise-policy')
        const title = element(document, 'h3', 'wwc-enterprise-resource-title')
        const summary = element(document, 'p', 'wwc-enterprise-resource-summary')
        const audit = element(document, 'button', 'wwc-enterprise-policy-dry-run')
        title.textContent = `${item.policyKind} Policy ${shortValue(item.id)}`
        summary.textContent = `${item.state} · ${item.mode === 'audit' ? 'audit dry-run' : 'enforced'} · version ${String(item.version)} · definition ${shortValue(item.definitionSha256)}`
        audit.type = 'button'
        audit.textContent = 'Prepare audit dry-run'
        audit.disabled = disabled
        audit.addEventListener('click', () => {
          id.input.value = item.id
          kind.select.value = item.policyKind
          mode.select.value = 'audit'
          state.select.value = item.state
          inheritanceMode.select.value = item.inheritanceMode
          definitionDigest.input.value = item.definitionSha256
        })
        row.append(title, summary, audit)
        return row
      }))
      if (items.length === 0) list.append(emptyRow(
        document,
        viewState.areas.policy.permission === 'denied'
          ? 'Policy data is not available for your current role.'
          : 'No Policies in the current snapshot.',
        'wwc-enterprise-policy-empty',
      ))
    },
  }
}

function createFleetSection(
  document: Document,
  model: EnterpriseManagementViewModel,
): OperationsSection<EnterpriseFleetProjection> {
  const section = element(document, 'section', 'wwc-enterprise-fleets')
  const heading = element(document, 'h2', 'wwc-enterprise-section-heading')
  const areaStatus = element(document, 'p', 'wwc-enterprise-fleet-status')
  const list = element(document, 'ul', 'wwc-enterprise-fleet-list')
  heading.id = 'wwc-enterprise-fleets-heading'
  heading.textContent = 'Remote Worker fleets'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  list.setAttribute('aria-live', 'polite')
  section.append(heading, areaStatus, list)
  return {
    section,
    render(items, viewState, disabled) {
      areaStatus.textContent = areaLabel(viewState, 'fleet')
      list.replaceChildren(...items.map(item => {
        const row = element(document, 'li', 'wwc-enterprise-fleet')
        const title = element(document, 'h3', 'wwc-enterprise-resource-title')
        const summary = element(document, 'p', 'wwc-enterprise-resource-summary')
        const drain = element(document, 'button', 'wwc-enterprise-fleet-drain')
        const enable = element(document, 'button', 'wwc-enterprise-fleet-enable')
        title.textContent = item.displayName
        summary.textContent = `${item.state} · ${String(item.registeredWorkers)} Workers · ${String(item.activeLeases)} active leases · ${String(item.availableCapacity)} available`
        drain.type = 'button'
        drain.textContent = 'Drain remote Worker pool'
        drain.disabled = disabled || item.state === 'draining' || item.state === 'offline'
        enable.type = 'button'
        enable.textContent = 'Enable remote Worker pool'
        enable.disabled = disabled || item.state === 'healthy'
        drain.addEventListener('click', () => {
          void model.execute('fleet', context => fleetCommand(context, {
            workerPoolId: item.id,
            action: 'drain',
            reason: 'Requested from the enterprise operations page.',
          }))
        })
        enable.addEventListener('click', () => {
          void model.execute('fleet', context => fleetCommand(context, {
            workerPoolId: item.id,
            action: 'enable',
            reason: 'Requested from the enterprise operations page.',
          }))
        })
        row.append(title, summary, drain, enable)
        return row
      }))
      if (items.length === 0) list.append(emptyRow(
        document,
        viewState.areas.fleet.permission === 'denied'
          ? 'Fleet data is not available for your current role.'
          : 'No remote Worker pools in the current snapshot.',
        'wwc-enterprise-fleet-empty',
      ))
    },
  }
}

function createUsageSection(
  document: Document,
  model: EnterpriseManagementViewModel,
): OperationsSection<EnterpriseUsageProjection> {
  const section = element(document, 'section', 'wwc-enterprise-usage')
  const heading = element(document, 'h2', 'wwc-enterprise-section-heading')
  const areaStatus = element(document, 'p', 'wwc-enterprise-usage-status')
  const summary = element(document, 'p', 'wwc-enterprise-usage-summary')
  const refresh = element(document, 'button', 'wwc-enterprise-usage-refresh')
  const list = element(document, 'ul', 'wwc-enterprise-usage-list')
  heading.id = 'wwc-enterprise-usage-heading'
  heading.textContent = 'Usage and quota evidence'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  refresh.type = 'button'
  refresh.textContent = 'Refresh usage and quota evidence'
  refresh.addEventListener('click', () => { void model.refresh('usage') })
  list.setAttribute('aria-live', 'polite')
  section.append(heading, areaStatus, summary, refresh, list)
  return {
    section,
    render(items, viewState) {
      areaStatus.textContent = areaLabel(viewState, 'usage')
      refresh.disabled = viewState.areas.usage.permission !== 'allowed'
      const totals = items.reduce((current, item) => ({
        operations: current.operations + item.operationCount,
        inputTokens: current.inputTokens + item.inputTokens,
        outputTokens: current.outputTokens + item.outputTokens,
        runtimeMillis: current.runtimeMillis + item.runtimeMillis,
        storageBytes: current.storageBytes + item.storageBytes,
        costMicros: current.costMicros + item.costMicros,
      }), { operations: 0, inputTokens: 0, outputTokens: 0, runtimeMillis: 0, storageBytes: 0, costMicros: 0 })
      summary.textContent = `${String(totals.operations)} operations · ${String(totals.inputTokens + totals.outputTokens)} tokens · ${String(totals.runtimeMillis)} runtime ms · ${String(totals.storageBytes)} storage bytes · ${String(totals.costMicros)} cost micros`
      list.replaceChildren(...items.map(item => {
        const row = element(document, 'li', 'wwc-enterprise-usage-bucket')
        row.textContent = `${item.sourceKind} · ${item.bucketStart} to ${item.bucketEnd} · ${String(item.operationCount)} operations · ${String(item.costMicros)} cost micros`
        return row
      }))
      if (items.length === 0) list.append(emptyRow(
        document,
        viewState.areas.usage.permission === 'denied'
          ? 'Usage and quota evidence is not available for your current role.'
          : 'No usage buckets in the current snapshot.',
        'wwc-enterprise-usage-empty',
      ))
    },
  }
}

function createAuditSection(
  document: Document,
  onExport: EnterpriseOperationsPageOptions['onAuditExport'],
): OperationsSection<EnterpriseAuditProjection> {
  const section = element(document, 'section', 'wwc-enterprise-audit')
  const heading = element(document, 'h2', 'wwc-enterprise-section-heading')
  const areaStatus = element(document, 'p', 'wwc-enterprise-audit-status')
  const exportStatus = element(document, 'p', 'wwc-enterprise-audit-export-status')
  const exportButton = element(document, 'button', 'wwc-enterprise-audit-export')
  const list = element(document, 'ol', 'wwc-enterprise-audit-list')
  let currentItems: readonly EnterpriseAuditProjection[] = Object.freeze([])
  heading.id = 'wwc-enterprise-audit-heading'
  heading.textContent = 'Audit trail'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  exportStatus.setAttribute('role', 'status')
  exportStatus.setAttribute('aria-live', 'polite')
  exportButton.type = 'button'
  exportButton.textContent = 'Export bounded audit CSV'
  exportButton.addEventListener('click', () => {
    const content = enterpriseAuditCsv(currentItems)
    onExport?.('winwincode-enterprise-audit.csv', content)
    exportStatus.textContent = `Audit export ready · ${String(currentItems.length)} records`
  })
  list.setAttribute('aria-live', 'polite')
  section.append(heading, areaStatus, exportButton, exportStatus, list)
  return {
    section,
    render(items, viewState) {
      currentItems = items
      areaStatus.textContent = areaLabel(viewState, 'audit')
      exportButton.disabled = viewState.areas.audit.permission !== 'allowed'
      list.replaceChildren(...items.map(item => {
        const row = element(document, 'li', 'wwc-enterprise-audit-record')
        row.textContent = `${item.occurredAt} · ${item.category} · ${item.action} · ${item.outcome} · actor ${shortValue(item.actor.id)} · record ${shortValue(item.recordSha256)}`
        return row
      }))
      if (items.length === 0) list.append(emptyRow(
        document,
        viewState.areas.audit.permission === 'denied'
          ? 'Audit data is not available for your current role.'
          : 'No audit records in the current snapshot.',
        'wwc-enterprise-audit-empty',
      ))
    },
  }
}

function createIntegrationSection(
  document: Document,
  model: EnterpriseManagementViewModel,
): OperationsSection<EnterpriseIntegrationProjection> {
  const section = element(document, 'section', 'wwc-enterprise-integrations')
  const heading = element(document, 'h2', 'wwc-enterprise-section-heading')
  const areaStatus = element(document, 'p', 'wwc-enterprise-integration-status')
  const list = element(document, 'ul', 'wwc-enterprise-integration-list')
  const fieldset = element(document, 'fieldset', 'wwc-enterprise-integration-fields')
  const legend = element(document, 'legend', 'wwc-enterprise-form-heading')
  const form = element(document, 'form', 'wwc-enterprise-integration-form')
  const id = labelledInput(document, 'wwc-enterprise-integration-id', 'Integration ID', 'wwc-enterprise-integration-id')
  const kind = labelledSelect(document, 'wwc-enterprise-integration-kind', 'Integration kind', 'wwc-enterprise-integration-kind', [
    ['github', 'GitHub'], ['oidc', 'OIDC'], ['saml', 'SAML'], ['scim', 'SCIM'],
    ['webhook', 'Webhook'], ['custom', 'Custom'],
  ])
  const name = labelledInput(document, 'wwc-enterprise-integration-name', 'Display name', 'wwc-enterprise-integration-name')
  const state = labelledSelect(document, 'wwc-enterprise-integration-state', 'State', 'wwc-enterprise-integration-state', [
    ['enabled', 'Enabled'], ['disabled', 'Disabled'],
  ])
  const endpoint = labelledInput(document, 'wwc-enterprise-integration-endpoint', 'HTTPS endpoint origin, optional', 'wwc-enterprise-integration-endpoint', false)
  const tenant = labelledInput(document, 'wwc-enterprise-integration-tenant', 'Tenant, optional', 'wwc-enterprise-integration-tenant', false)
  const repository = labelledInput(document, 'wwc-enterprise-integration-repository', 'GitHub owner/repository, optional', 'wwc-enterprise-integration-repository', false)
  const audience = labelledInput(document, 'wwc-enterprise-integration-audience', 'Audience, optional', 'wwc-enterprise-integration-audience', false)
  const configurationDigest = labelledInput(document, 'wwc-enterprise-integration-digest', 'Configuration SHA-256', 'wwc-enterprise-integration-digest')
  const credentialReference = labelledInput(document, 'wwc-enterprise-integration-credential', 'Credential reference ID, optional', 'wwc-enterprise-integration-credential', false)
  const save = element(document, 'button', 'wwc-enterprise-integration-save')
  heading.id = 'wwc-enterprise-integrations-heading'
  heading.textContent = 'Integrations'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  list.setAttribute('aria-live', 'polite')
  legend.textContent = 'Save secret-free Integration settings'
  id.input.pattern = 'int_[0-9A-HJKMNP-TV-Z]{26}'
  endpoint.input.pattern = 'https://[^/?#]+(?::[0-9]{1,5})?'
  repository.input.pattern = '[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+'
  configurationDigest.input.pattern = 'sha256:[0-9a-f]{64}'
  credentialReference.input.pattern = 'crd_[0-9A-HJKMNP-TV-Z]{26}'
  save.type = 'submit'
  save.textContent = 'Save Integration status'
  form.addEventListener('submit', event => {
    event.preventDefault()
    const payload: EnterpriseIntegrationUpdatePayload = {
      integrationId: id.input.value as EnterpriseIntegrationId,
      kind: kind.select.value as EnterpriseIntegrationProjection['kind'],
      displayName: name.input.value,
      state: state.select.value as 'enabled' | 'disabled',
      configuration: {
        endpointOrigin: endpoint.input.value || null,
        tenant: tenant.input.value || null,
        repository: repository.input.value
          ? repository.input.value as GitHubRepositorySlug
          : null,
        audience: audience.input.value || null,
      },
      configurationSha256: configurationDigest.input.value as Sha256Digest,
      credentialReferenceId: credentialReference.input.value
        ? credentialReference.input.value as CredentialReferenceId
        : null,
    }
    void model.execute('integration', context => integrationCommand(context, payload))
  })
  form.append(
    id.label,
    kind.label,
    name.label,
    state.label,
    endpoint.label,
    tenant.label,
    repository.label,
    audience.label,
    configurationDigest.label,
    credentialReference.label,
    save,
  )
  fieldset.append(legend, form)
  section.append(heading, areaStatus, list, fieldset)
  return {
    section,
    render(items, viewState, disabled) {
      areaStatus.textContent = areaLabel(viewState, 'integration')
      fieldset.disabled = disabled
      list.replaceChildren(...items.map(item => {
        const row = element(document, 'li', 'wwc-enterprise-integration')
        const title = element(document, 'h3', 'wwc-enterprise-resource-title')
        const summary = element(document, 'p', 'wwc-enterprise-resource-summary')
        title.textContent = item.displayName
        summary.textContent = `${item.kind} · ${item.state} · last sync ${item.lastSyncAt ?? 'never'} · configuration ${shortValue(item.configurationSha256)}`
        row.append(title, summary)
        return row
      }))
      if (items.length === 0) list.append(emptyRow(
        document,
        viewState.areas.integration.permission === 'denied'
          ? 'Integration data is not available for your current role.'
          : 'No Integrations in the current snapshot.',
        'wwc-enterprise-integration-empty',
      ))
    },
  }
}

function emptyRow(document: Document, label: string, className: string): HTMLLIElement {
  const row = element(document, 'li', className)
  row.textContent = label
  return row
}
