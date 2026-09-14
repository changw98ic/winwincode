// SPDX-License-Identifier: Apache-2.0

import { mountPageHeader } from '@winwincode/browser-ui'
import { mountEmptyState, mountTabs } from './components/index.js'

export interface ExtensionsPageOptions {
  readonly root: HTMLElement
}

export interface ExtensionsPage {
  close(): void
}

const EXTENSIONS_TABS = [
  { id: 'plugins', label: '插件', panelId: 'wwc-extensions-plugins-panel' },
  { id: 'skills', label: '技能与指令', panelId: 'wwc-extensions-skills-panel' },
  { id: 'mcp', label: 'MCP 连接', panelId: 'wwc-extensions-mcp-panel' },
] as const

/** Inventory is unavailable until the server exposes verified extension facts. */
export function mountExtensionsPage(options: ExtensionsPageOptions): ExtensionsPage {
  const document = options.root.ownerDocument
  const layout = document.createElement('section')
  layout.className = 'wwc-extensions'
  layout.dataset.wwcPage = 'management'
  const header = mountPageHeader({
    document,
    props: { title: '扩展', headingLevel: 2, className: 'wwc-extensions-heading' },
  })
  const panels = EXTENSIONS_TABS.map(tab => {
    const panel = document.createElement('section')
    panel.id = tab.panelId
    panel.className = 'wwc-extensions-panel'
    panel.setAttribute('role', 'tabpanel')
    panel.setAttribute('aria-label', tab.label)
    panel.tabIndex = 0
    const empty = mountEmptyState({
      document,
      props: {
        title: tab.label + '暂不可用',
        detail: '当前服务器尚未提供此功能的列表与管理操作。',
        headingLevel: 3,
      },
    })
    panel.append(empty.root)
    return { panel, empty }
  })
  function select(id: string): void {
    for (const [index, tab] of EXTENSIONS_TABS.entries()) {
      const entry = panels[index]
      if (entry !== undefined) entry.panel.hidden = tab.id !== id
    }
    tabs.update({
      id: 'wwc-extensions-tabs',
      label: '扩展分类',
      tabs: EXTENSIONS_TABS,
      selectedId: id,
      onSelect: select,
    })
  }
  const tabs = mountTabs({
    document,
    props: {
      id: 'wwc-extensions-tabs',
      label: '扩展分类',
      tabs: EXTENSIONS_TABS,
      selectedId: 'plugins',
      onSelect: select,
    },
  })
  select('plugins')
  layout.append(header.root, tabs.root, ...panels.map(({ panel }) => panel))
  options.root.replaceChildren(layout)
  return {
    close() {
      tabs.close()
      header.close()
      for (const { empty } of panels) empty.close()
      options.root.replaceChildren()
    },
  }
}
