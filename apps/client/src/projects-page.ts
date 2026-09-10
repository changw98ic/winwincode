// SPDX-License-Identifier: Apache-2.0

import {
  type ControlPlaneClientDirectory,
  type ControlPlaneDeviceSummary,
  type ControlPlaneRepositorySummary,
  type ControlPlaneRequestOptions,
} from './community-control-plane-client.js'
import { formatInstant } from './format-instant.js'
import { mountPageHeader } from '@winwincode/browser-ui'

export interface ProjectsPageOptions {
  readonly root: HTMLElement
  readonly clientDirectory: ControlPlaneClientDirectory
  readonly newChatHref: string
  readonly deviceHref: string
  readonly requestOptions?: () => ControlPlaneRequestOptions | undefined
}

export interface ProjectsPage {
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

/**
 * Design page 07: 项目与仓库. One row per repository bound to the connected
 * execution device: name, path/source line, a New-chat entry, and a ⋯ menu
 * placeholder. 添加仓库 routes to the device page where repositories are
 * authorized.
 */
export function renderProjectsPage(options: ProjectsPageOptions): ProjectsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-projects')
  layout.dataset.wwcPage = 'management'

  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '项目',
      description: '已连接执行设备上的项目与仓库。',
      headingLevel: 2,
      className: 'wwc-projects-heading',
    },
  })
  const addButton = element(document, 'a', 'wwc-projects-add')
  addButton.href = options.deviceHref
  addButton.textContent = '添加仓库'

  const headerRow = element(document, 'div', 'wwc-projects-header')
  headerRow.append(pageHeader.root, addButton)

  const status = element(document, 'p', 'wwc-projects-status')
  status.setAttribute('role', 'status')
  status.textContent = '正在加载项目…'

  const list = element(document, 'ul', 'wwc-projects-list')

  function repoRow(repo: ControlPlaneRepositorySummary): HTMLLIElement {
    const row = element(document, 'li', 'wwc-projects-row')
    const info = element(document, 'div', 'wwc-projects-row-info')
    const name = element(document, 'p', 'wwc-projects-row-name')
    name.textContent = repo.displayName
    const source = element(document, 'p', 'wwc-projects-row-source')
    source.textContent = repo.defaultBranch
      ? `${repo.defaultBranch} · ${repo.dirtyState === 'clean' ? '工作区干净' : '有未提交改动'}`
      : ''
    info.append(name, source)
    const newChat = element(document, 'a', 'wwc-projects-row-chat')
    newChat.href = options.newChatHref
    newChat.textContent = '新对话'
    const more = element(document, 'button', 'wwc-projects-row-more')
    more.type = 'button'
    more.textContent = '⋯'
    more.setAttribute('aria-label', `${repo.displayName} 更多操作`)
    row.append(info, newChat, more)
    return row
  }

  async function load(): Promise<void> {
    try {
      const clients = await options.clientDirectory.listClients(
        options.requestOptions?.(),
      )
      const rows: HTMLLIElement[] = []
      for (const client of clients) {
        const repos = await options.clientDirectory.listRepositories(
          { clientId: client.clientId },
          options.requestOptions?.(),
        )
        for (const repo of repos) rows.push(repoRow(repo))
      }
      status.hidden = true
      if (rows.length === 0) {
        status.hidden = false
        status.textContent = '还没有仓库。先连接执行设备并授权仓库。'
      }
      list.replaceChildren(...rows)
    } catch {
      status.hidden = false
      status.textContent = '项目列表读取失败。请检查执行设备连接后重试。'
    }
  }

  layout.append(headerRow, status, list)
  options.root.replaceChildren(layout)
  void load()

  return {
    close() {},
  }
}
