// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClientDirectory,
  type ControlPlaneRequestOptions,
} from './community-control-plane-client.js'
import { createClientsViewModel } from './clients-view-model.js'
import { mountClientsPage } from './clients-page.js'
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

export function renderDevicePage(options: DevicePageOptions): DevicePage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-device')
  layout.dataset.wwcPage = 'management'

  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '执行设备',
      headingLevel: 2,
      className: 'wwc-device-heading',
    },
  })
  const more = element(document, 'a', 'wwc-device-more')
  more.href = options.homeHref
  more.textContent = '连接设备'
  const headerRow = element(document, 'div', 'wwc-device-header')
  headerRow.append(pageHeader.root, more)

  const status = element(document, 'p', 'wwc-device-status')
  status.setAttribute('role', 'status')
  status.textContent = '正在读取执行设备…'

  const body = element(document, 'div', 'wwc-device-body')
  const clientsRoot = element(document, 'div', 'wwc-device-clients')
  const clientsModel = createClientsViewModel({ client: options.clientDirectory })
  const clientsPage = mountClientsPage({ root: clientsRoot, model: clientsModel })
  clientsPage.setVisible(true)
  let closed = false
  const retry = element(document, 'button', 'wwc-device-retry')
  retry.type = 'button'
  retry.textContent = '重试'
  retry.hidden = true
  retry.addEventListener('click', () => { void load() })

  layout.append(headerRow, clientsRoot, status, retry, body)
  options.root.replaceChildren(layout)

  async function load(): Promise<void> {
    retry.hidden = true
    status.hidden = false
    status.textContent = '正在读取执行设备…'
    try {
      await clientsModel.refresh()
      if (closed) return
      if (clientsModel.state.devicesStatus === 'unavailable') throw new Error('Device list unavailable')
      const clients = clientsModel.state.devices
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
        const directories = element(document, 'section', 'wwc-device-directories')
        const directoriesHeading = element(document, 'h3', 'wwc-device-subheading')
        directoriesHeading.textContent = `${client.displayName} · 可访问目录`
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
    } catch (error) {
      body.replaceChildren()
      status.hidden = false
      status.textContent = error instanceof ControlPlaneClientError && error.code === 'RESOURCE_NOT_FOUND'
        ? '当前服务器未提供设备管理。请确认已启用远程执行设备服务。'
        : '执行设备读取失败。请检查连接后重试。'
      retry.hidden = false
    }
  }

  void load()

  return {
    close() {
      closed = true
      clientsPage.close()
      clientsModel.close()
    },
  }
}
