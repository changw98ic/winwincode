// SPDX-License-Identifier: Apache-2.0

import {
  type ControlPlaneClientDirectory,
  type ControlPlaneDeviceSummary,
  type ControlPlaneRequestOptions,
} from './community-control-plane-client.js'
import { formatInstant } from './format-instant.js'
import { mountPageHeader } from '@winwincode/browser-ui'

export interface DevicePageOptions {
  readonly root: HTMLElement
  readonly clientDirectory: ControlPlaneClientDirectory
  readonly homeHref: string
  readonly projectsHref: string
  readonly requestOptions?: () => ControlPlaneRequestOptions | undefined
}

export interface DevicePage {
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
 * Design page 08: 执行设备. The connected execution device card, its
 * accessible directories, and collapsed 运行中的任务 / 连接记录 rows. With no
 * device connected the page degrades to the pairing hint (design page 02
 * carries the full onboarding flow).
 */
export function renderDevicePage(options: DevicePageOptions): DevicePage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-device')
  layout.dataset.wwcPage = 'management'

  // Design page 08: the display title stands alone, the 更多 ∨ menu on the
  // same row's right end.
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '执行设备',
      headingLevel: 2,
      className: 'wwc-device-heading',
    },
  })
  const more = element(document, 'button', 'wwc-device-more')
  more.type = 'button'
  more.textContent = '更多 ∨'
  const headerRow = element(document, 'div', 'wwc-device-header')
  headerRow.append(pageHeader.root, more)

  const status = element(document, 'p', 'wwc-device-status')
  status.setAttribute('role', 'status')
  status.textContent = '正在读取执行设备…'

  const body = element(document, 'div', 'wwc-device-body')

  layout.append(headerRow, status, body)
  options.root.replaceChildren(layout)

  async function load(): Promise<void> {
    try {
      const clients = await options.clientDirectory.listClients(
        options.requestOptions?.(),
      )
      body.replaceChildren()
      if (clients.length === 0) {
        const hint = element(document, 'p', 'wwc-device-empty')
        hint.textContent = '还没有连接执行设备。在执行任务的电脑上启动客户端，使用配对码连接。'
        const connect = element(document, 'a', 'wwc-device-empty-connect')
        connect.href = options.homeHref
        connect.textContent = '连接设备'
        body.append(hint, connect)
        status.hidden = true
        return
      }
      for (const client of clients) {
        body.append(deviceCard(client))
        const directories = element(document, 'section', 'wwc-device-directories')
        const directoriesHeading = element(document, 'h3', 'wwc-device-subheading')
        directoriesHeading.textContent = '可访问目录'
        const directoryList = element(document, 'ul', 'wwc-device-directory-list')
        const repos = await options.clientDirectory.listRepositories(
          { clientId: client.clientId },
          options.requestOptions?.(),
        )
        for (const repo of repos) {
          const row = element(document, 'li', 'wwc-device-directory-row')
          const icon = element(document, 'span', 'wwc-device-directory-icon')
          icon.textContent = '📁'
          const label = element(document, 'span', 'wwc-device-directory-label')
          label.textContent = repo.displayName
          row.append(icon, label)
          directoryList.append(row)
        }
        if (repos.length === 0) {
          const empty = element(document, 'li', 'wwc-device-directory-row')
          empty.textContent = '此设备尚未授权任何目录。'
          directoryList.append(empty)
        }
        const manage = element(document, 'a', 'wwc-device-manage')
        manage.href = options.projectsHref
        manage.textContent = '管理目录'
        directories.append(directoriesHeading, directoryList, manage)
        body.append(directories)
      }
      const rows = element(document, 'div', 'wwc-device-rows')
      const running = element(document, 'button', 'wwc-device-row')
      running.type = 'button'
      running.textContent = '运行中的任务'
      const history = element(document, 'button', 'wwc-device-row')
      history.type = 'button'
      history.textContent = '连接记录'
      rows.append(running, history)
      body.append(rows)
      status.hidden = true
    } catch {
      body.replaceChildren()
      status.hidden = false
      status.textContent = '执行设备读取失败。请检查连接后重试。'
    }
  }

  function deviceCard(client: ControlPlaneDeviceSummary): HTMLElement {
    const card = element(document, 'section', 'wwc-device-card')
    const icon = element(document, 'span', 'wwc-device-card-icon')
    icon.textContent = '🖥️'
    const info = element(document, 'div', 'wwc-device-card-info')
    const name = element(document, 'p', 'wwc-device-card-name')
    name.textContent = client.displayName
    const presence = element(document, 'p', 'wwc-device-card-presence')
    presence.textContent = client.presence === 'online'
      ? `🟢 已连接 · 最近心跳 ${formatInstant(client.lastHeartbeatAt)}`
      : client.presence === 'offline' ? '⚪ 离线' : '🔒 已锁定'
    info.append(name, presence)
    card.append(icon, info)
    return card
  }

  void load()

  return {
    close() {},
  }
}
