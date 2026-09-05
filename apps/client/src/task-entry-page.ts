// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneTaskAnchor } from './control-plane-client.js'
import type {
  TaskEntryFailure,
  TaskEntryState,
  TaskEntryViewModel,
} from './task-entry-view-model.js'

export interface TaskEntryPageOptions {
  readonly root: HTMLElement
  readonly model: TaskEntryViewModel
  /** Raised once per started anchor; the host navigates to the run route. */
  readonly onStarted?: (anchor: ControlPlaneTaskAnchor) => void
}

export interface TaskEntryPage {
  close(): void
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

/** The one copy per form-level failure; the alert role reaches screen readers. */
function failureText(failure: TaskEntryFailure): string {
  switch (failure) {
    case 'no-occupied-client': return 'Occupy a Client first — only an occupied Client can start a task.'
    case 'no-repository': return 'Choose a repository for the task.'
    case 'missing-base-branch': return 'Enter the base branch the task starts from.'
    case 'missing-description': return 'Describe the task for the worker.'
    case 'missing-model-route': return 'Choose a model route.'
    case 'unavailable': return 'Starting the task did not go through. Check the connection and try again.'
  }
}

function optionValue(select: HTMLSelectElement): string | null {
  return select.value === '' ? null : select.value
}

/**
 * Mount the §16.6 new-task form: the occupied-Client select, the repository
 * select driven by the shared repository list, the base branch defaulted to
 * the repository's default branch, the task description, the model route
 * choice, and the submit entry.  The module owns DOM and ARIA only; every
 * change translates into one view-model intent.
 */
export function mountTaskEntryPage(options: TaskEntryPageOptions): TaskEntryPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-task-entry')
  const heading = element(document, 'p', 'wwc-task-entry-heading')
  const intro = element(document, 'p', 'wwc-task-entry-intro')
  const occupiedNotice = element(document, 'p', 'wwc-task-entry-occupied-notice')
  const form = element(document, 'form', 'wwc-task-entry-form')
  const clientLabel = element(document, 'label', 'wwc-task-entry-label')
  const clientSelect = element(document, 'select', 'wwc-task-entry-control wwc-task-entry-client')
  const repositoryLabel = element(document, 'label', 'wwc-task-entry-label')
  const repositorySelect = element(document, 'select', 'wwc-task-entry-control wwc-task-entry-repository')
  const baseLabel = element(document, 'label', 'wwc-task-entry-label')
  const baseInput = element(document, 'input', 'wwc-task-entry-control wwc-task-entry-base')
  const descriptionLabel = element(document, 'label', 'wwc-task-entry-label')
  const descriptionInput = element(document, 'textarea', 'wwc-task-entry-control wwc-task-entry-description')
  const routeLabel = element(document, 'label', 'wwc-task-entry-label')
  const routeSelect = element(document, 'select', 'wwc-task-entry-control wwc-task-entry-route')
  const submit = element(document, 'button', 'wwc-task-entry-submit')
  const failure = element(document, 'p', 'wwc-task-entry-error')
  const status = element(document, 'p', 'wwc-task-entry-status')

  let closed = false
  let reportedAnchor: ControlPlaneTaskAnchor | null = null

  section.setAttribute('aria-label', 'New task')
  heading.id = 'wwc-task-entry-heading'
  heading.textContent = 'New task'
  section.setAttribute('aria-labelledby', heading.id)
  intro.textContent = 'Start a task on a Client you occupy.'
  occupiedNotice.setAttribute('role', 'status')
  occupiedNotice.hidden = true
  failure.setAttribute('role', 'alert')
  failure.hidden = true
  failure.id = 'wwc-task-entry-error'
  status.setAttribute('role', 'status')
  status.hidden = true

  function wireSelect(
    label: HTMLLabelElement,
    select: HTMLSelectElement,
    name: string,
    text: string,
  ): void {
    select.id = `wwc-task-entry-${name}`
    select.name = name
    label.htmlFor = select.id
    label.textContent = text
  }

  wireSelect(clientLabel, clientSelect, 'client', 'Client')
  wireSelect(repositoryLabel, repositorySelect, 'repository', 'Repository')
  wireSelect(routeLabel, routeSelect, 'modelRoute', 'Model route')

  baseInput.id = 'wwc-task-entry-base'
  baseInput.name = 'baseBranch'
  baseInput.type = 'text'
  baseInput.autocomplete = 'off'
  baseInput.spellcheck = false
  baseLabel.htmlFor = baseInput.id
  baseLabel.textContent = 'Base branch'

  descriptionInput.id = 'wwc-task-entry-description'
  descriptionInput.name = 'description'
  descriptionInput.rows = 4
  descriptionInput.spellcheck = true
  descriptionLabel.htmlFor = descriptionInput.id
  descriptionLabel.textContent = 'Task description'

  submit.type = 'submit'
  submit.textContent = 'Start task'

  form.append(
    clientLabel,
    clientSelect,
    repositoryLabel,
    repositorySelect,
    baseLabel,
    baseInput,
    descriptionLabel,
    descriptionInput,
    routeLabel,
    routeSelect,
    submit,
  )
  section.append(heading, intro, occupiedNotice, form, failure, status)
  options.root.replaceChildren(section)

  function fillSelect(
    select: HTMLSelectElement,
    emptyText: string,
    entries: readonly { readonly value: string; readonly label: string }[],
    selected: string | null,
  ): void {
    if (select.options.length === entries.length + 1
      && select.options[0]?.textContent === emptyText
      && entries.every((entry, index) => select.options[index + 1]?.value === entry.value
        && select.options[index + 1]?.textContent === entry.label)
      && (select.value === (selected ?? ''))) {
      return
    }
    select.replaceChildren()
    const placeholder = document.createElement('option')
    placeholder.value = ''
    placeholder.textContent = emptyText
    select.append(placeholder)
    for (const entry of entries) {
      const option = document.createElement('option')
      option.value = entry.value
      option.textContent = entry.label
      select.append(option)
    }
    select.value = selected ?? ''
  }

  function setFieldError(control: HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement, hasError: boolean): void {
    if (hasError) {
      control.setAttribute('aria-invalid', 'true')
      control.setAttribute('aria-describedby', failure.id)
    } else {
      control.removeAttribute('aria-invalid')
      control.removeAttribute('aria-describedby')
    }
  }

  function render(snapshot: TaskEntryState): void {
    if (closed) return
    const busy = snapshot.status === 'submitting'

    const occupied = snapshot.occupiedDevices
    occupiedNotice.hidden = occupied.length !== 0
    if (occupied.length === 0) {
      occupiedNotice.textContent = snapshot.devicesStatus === 'loading'
        ? 'Checking the Clients you occupy…'
        : 'No Client is occupied by you right now. Connect a device in the Clients area and occupy it to start a task.'
    }

    fillSelect(
      clientSelect,
      'Choose an occupied Client',
      occupied.map(device => ({ value: device.clientId, label: device.displayName })),
      snapshot.selection.clientId,
    )
    fillSelect(
      repositorySelect,
      snapshot.selection.clientId === null
        ? 'Choose a Client first'
        : snapshot.repositoriesStatus === 'loading' && snapshot.repositories.length === 0
          ? 'Loading repositories…'
          : 'Choose a repository',
      snapshot.repositories.map(repository => ({
        value: repository.repositoryBindingId,
        label: repository.displayName,
      })),
      snapshot.selection.repositoryBindingId,
    )
    repositorySelect.disabled = snapshot.selection.clientId === null
    if (baseInput.value !== snapshot.selection.baseBranch) {
      baseInput.value = snapshot.selection.baseBranch
    }
    if (descriptionInput.value !== snapshot.selection.description) {
      descriptionInput.value = snapshot.selection.description
    }
    fillSelect(
      routeSelect,
      'Choose a model route',
      snapshot.modelRouteOptions.map(option => ({
        value: option.routeId,
        label: option.detail === '' ? option.label : `${option.label} — ${option.detail}`,
      })),
      snapshot.selection.modelRouteId,
    )

    submit.textContent = busy ? 'Starting…' : 'Start task'
    submit.disabled = busy || occupied.length === 0
    form.setAttribute('aria-busy', busy ? 'true' : 'false')

    const failureLine = snapshot.status === 'editing' && snapshot.failure !== null
      ? failureText(snapshot.failure)
      : null
    failure.textContent = failureLine ?? ''
    failure.hidden = failureLine === null
    const failureKind = snapshot.status === 'editing' ? snapshot.failure : null
    setFieldError(clientSelect, failureKind === 'no-occupied-client')
    setFieldError(repositorySelect, failureKind === 'no-repository')
    setFieldError(baseInput, failureKind === 'missing-base-branch')
    setFieldError(descriptionInput, failureKind === 'missing-description')
    setFieldError(routeSelect, failureKind === 'missing-model-route')

    const statusLine = busy ? 'Starting the task…' : ''
    status.textContent = statusLine
    status.hidden = statusLine.length === 0

    if (snapshot.status === 'started' && snapshot.anchor !== null) {
      const anchor = snapshot.anchor
      // One navigation per started anchor; a re-render of the same anchor
      // never re-triggers the host route change.
      if (reportedAnchor !== anchor) {
        reportedAnchor = anchor
        options.onStarted?.(anchor)
      }
    }
  }

  const onSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    options.model.submit()
  }
  const onClientChange = () => {
    options.model.selectClient(optionValue(clientSelect))
  }
  const onRepositoryChange = () => {
    options.model.selectRepository(optionValue(repositorySelect))
  }
  const onBaseEdit = () => {
    options.model.setBaseBranch(baseInput.value)
  }
  const onDescriptionEdit = () => {
    options.model.setDescription(descriptionInput.value)
  }
  const onRouteChange = () => {
    options.model.selectModelRoute(optionValue(routeSelect))
  }

  form.addEventListener('submit', onSubmit)
  clientSelect.addEventListener('change', onClientChange)
  repositorySelect.addEventListener('change', onRepositoryChange)
  baseInput.addEventListener('input', onBaseEdit)
  descriptionInput.addEventListener('input', onDescriptionEdit)
  routeSelect.addEventListener('change', onRouteChange)

  const unsubscribe = options.model.subscribe(render)

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      form.removeEventListener('submit', onSubmit)
      clientSelect.removeEventListener('change', onClientChange)
      repositorySelect.removeEventListener('change', onRepositoryChange)
      baseInput.removeEventListener('input', onBaseEdit)
      descriptionInput.removeEventListener('input', onDescriptionEdit)
      routeSelect.removeEventListener('change', onRouteChange)
      options.root.replaceChildren()
    },
  }
}
