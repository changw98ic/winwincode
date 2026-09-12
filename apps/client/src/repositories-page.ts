// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection } from './components/keyed-collection.js'
import type {
  ControlPlaneRepositoryPermissions,
  ControlPlaneRepositorySummary,
} from './community-control-plane-client.js'
import type {
  RepositoriesViewModel,
  RepositoriesViewModelState,
} from './repositories-view-model.js'
import {
  repositoryAvailabilityText,
  repositoryAvailabilityTone,
  repositoryDirtyText,
  repositoryDirtyTone,
  repositoryHeadShortText,
} from './repositories-view-model.js'

export interface RepositoriesPageOptions {
  readonly root: HTMLElement
  readonly model: RepositoriesViewModel
}

export interface RepositoriesPage {
  close(): void
  /** Show or hide the area; selection state survives visibility changes. */
  setVisible(visible: boolean): void
}

interface RepositoryCardRefs {
  readonly card: HTMLElement
  readonly name: HTMLElement
  readonly availability: HTMLElement
  readonly branch: HTMLElement
  readonly dirty: HTMLElement
  readonly head: HTMLElement
  readonly permissions: HTMLElement
  readonly grantForm: HTMLFormElement
  readonly grantUser: HTMLInputElement
  readonly grantPermissions: HTMLSelectElement
  readonly grantStatus: HTMLElement
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

/** §16.5: the unavailable read keeps the shown cards and explains itself. */
const UNAVAILABLE_TEXT
  = '暂时无法列出仓库，请检查连接后重试。'

/**
 * Mount the signed-in repository area for the selected Client device. The
 * area renders the same shell audit constraints as the Clients area: titles
 * as styled paragraphs and one polite alert channel of its own.
 */
export function mountRepositoriesPage(options: RepositoriesPageOptions): RepositoriesPage {
  const document = options.root.ownerDocument
  const region = element(document, 'section', 'wwc-repositories')
  const heading = element(document, 'p', 'wwc-repositories-heading')
  const hint = element(document, 'p', 'wwc-repositories-hint')
  const error = element(document, 'p', 'wwc-repositories-error')
  const empty = element(document, 'p', 'wwc-repositories-empty')
  const list = element(document, 'div', 'wwc-repositories-list')
  const cards = new WeakMap<HTMLElement, RepositoryCardRefs>()
  let closed = false

  region.setAttribute('aria-label', '仓库')
  region.hidden = true
  heading.textContent = '仓库'
  heading.id = 'wwc-repositories-heading'
  region.setAttribute('aria-labelledby', heading.id)
  hint.hidden = true
  error.setAttribute('role', 'alert')
  error.hidden = true
  empty.hidden = true

  region.append(heading, hint, error, empty, list)
  options.root.replaceChildren(region)

  const cardCollection = mountKeyedCollection<ControlPlaneRepositorySummary, string, HTMLElement>({
    parent: list,
    key: repository => repository.repositoryBindingId,
    create(repository) {
      const card = element(document, 'article', 'wwc-repositories-card')
      const name = element(document, 'p', 'wwc-repositories-card-name')
      const availability = element(document, 'span', 'wwc-repositories-card-availability')
      const meta = element(document, 'p', 'wwc-repositories-card-meta')
      const branch = element(document, 'span', 'wwc-repositories-card-branch')
      const dirty = element(document, 'span', 'wwc-repositories-card-dirty')
      const head = element(document, 'span', 'wwc-repositories-card-head')
      const permissions = element(document, 'span', 'wwc-repositories-card-permissions')
      const grantForm = element(document, 'form', 'wwc-repositories-card-grant') as HTMLFormElement
      const grantUser = element(document, 'input', 'wwc-repositories-card-grant-user') as HTMLInputElement
      const grantPermissions = element(document, 'select', 'wwc-repositories-card-grant-permissions') as HTMLSelectElement
      const grantStatus = element(document, 'p', 'wwc-repositories-card-grant-status')
      grantUser.type = 'text'
      grantUser.placeholder = '用户 ID'
      grantUser.autocomplete = 'off'
      grantUser.setAttribute('aria-label', '用户 ID')
      grantPermissions.setAttribute('aria-label', '仓库权限')
      grantStatus.setAttribute('aria-live', 'polite')
      for (const [value, label] of [['use', '使用'], ['use+manage', '使用和管理']] as const) {
        const option = element(document, 'option', '')
        option.setAttribute('value', value)
        option.textContent = label
        grantPermissions.append(option)
      }
      const grantSubmit = element(document, 'button', 'wwc-repositories-card-grant-submit')
      grantSubmit.type = 'submit'
      grantSubmit.textContent = '授予访问'
      grantForm.append(grantUser, grantPermissions, grantSubmit, grantStatus)
      meta.append(branch, dirty, head, permissions)
      card.append(name, availability, meta, grantForm)
      const refs: RepositoryCardRefs = {
        card, name, availability, branch, dirty, head, permissions,
        grantForm, grantUser, grantPermissions, grantStatus,
      }
      cards.set(card, refs)
      grantForm.addEventListener('submit', event => {
        event.preventDefault()
        grantSubmit.disabled = true
        grantStatus.textContent = ''
        void options.model.grantRepositoryAccess({
          repositoryBindingId: repository.repositoryBindingId,
          userId: grantUser.value.trim(),
          permissions: grantPermissions.value as ControlPlaneRepositoryPermissions,
        }).then(() => {
          grantUser.value = ''
          grantStatus.textContent = '仓库访问已授予。'
        }).catch(error => {
          grantStatus.textContent = error instanceof Error
            ? error.message : '无法授予仓库访问权限。'
        }).finally(() => {
          grantSubmit.disabled = false
        })
      })
      updateCard(refs, repository)
      return card
    },
    update(node, repository) {
      const refs = cards.get(node)
      if (refs === undefined) return
      updateCard(refs, repository)
    },
  })

  function updateCard(refs: RepositoryCardRefs, repository: ControlPlaneRepositorySummary): void {
    refs.card.setAttribute('aria-label', `${repository.displayName} (${repository.defaultBranch})`)
    refs.name.textContent = repository.displayName
    // §16.5: no absolute path is ever rendered; only Server-owned display
    // names, branches, dirty state, and the short HEAD hash appear.
    const availabilityText = repositoryAvailabilityText(repository)
    refs.availability.textContent = availabilityText ?? ''
    refs.availability.dataset.tone = repositoryAvailabilityTone(repository)
    refs.availability.hidden = availabilityText === null
    refs.branch.textContent = repository.defaultBranch
    refs.dirty.textContent = repositoryDirtyText(repository)
    refs.dirty.dataset.tone = repositoryDirtyTone(repository)
    refs.head.textContent = repositoryHeadShortText(repository)
    refs.permissions.textContent = repository.permissions === 'use+manage'
      ? '权限：使用和管理' : '权限：使用'
    refs.grantForm.hidden = !repository.canGrantAccess
  }

  function render(snapshot: RepositoriesViewModelState): void {
    if (closed) return
    if (snapshot.clientId === null) {
      hint.textContent = '先选择上方设备以查看它的仓库。'
      hint.hidden = false
    } else {
      hint.textContent = ''
      hint.hidden = true
    }
    const unavailable = snapshot.status === 'unavailable'
    error.textContent = unavailable ? UNAVAILABLE_TEXT : ''
    error.hidden = !unavailable
    cardCollection.update(snapshot.repositories)
    const loading = snapshot.status === 'loading' && snapshot.repositories.length === 0
    empty.textContent = '该设备还没有被授权的仓库。'
    // An unavailable read explains itself through the alert channel and never
    // claims the empty state, mirroring the Clients area semantics.
    empty.hidden = snapshot.clientId === null
      || loading
      || unavailable
      || snapshot.repositories.length !== 0
    list.hidden = snapshot.repositories.length === 0
  }

  const unsubscribe = options.model.subscribe(render)

  return {
    setVisible(visible) {
      if (closed) return
      region.hidden = !visible
    },
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      cardCollection.close()
      options.root.replaceChildren()
    },
  }
}
