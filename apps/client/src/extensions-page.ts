// SPDX-License-Identifier: Apache-2.0

import { mountButton, mountPageHeader } from '@winwincode/browser-ui'
import { mountEmptyState, mountTabs } from './components/index.js'
import { mountKeyedCollection } from './components/keyed-collection.js'

export interface ExtensionsPageOptions {
  readonly root: HTMLElement
}

export interface ExtensionsPage {
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

type ExtensionsTabId = 'plugins' | 'skills' | 'mcp'

const EXTENSIONS_TABS: readonly {
  readonly id: ExtensionsTabId
  readonly label: string
  readonly panelId: string
}[] = Object.freeze([
  Object.freeze({ id: 'plugins', label: '插件', panelId: 'wwc-extensions-plugins-panel' }),
  Object.freeze({
    id: 'skills',
    label: '技能与指令',
    panelId: 'wwc-extensions-skills-panel',
  }),
  Object.freeze({ id: 'mcp', label: 'MCP 连接', panelId: 'wwc-extensions-mcp-panel' }),
])

/** Design pages 09-11: the header action follows the selected tab. */
const PRIMARY_ACTION_LABEL: Readonly<Record<ExtensionsTabId, string>> = Object.freeze({
  plugins: '安装插件',
  skills: '添加技能',
  mcp: '添加连接',
})

/** Shown while the control plane has no inventory contract for the action. */
const UNAVAILABLE_TITLE = '暂不可用：等待控制面契约。'

interface PluginEntry {
  readonly id: string
  readonly name: string
  readonly description: string
}

interface SkillEntry {
  readonly id: string
  readonly name: string
  readonly command: string
  readonly description: string
}

interface McpEntry {
  readonly id: string
  readonly name: string
  readonly connected: boolean
}

interface InstructionEntry {
  readonly id: string
  readonly name: string
  readonly description: string
}

/**
 * Page-local inventory models. The control plane does not expose plugin,
 * skill, MCP, or project-instruction contracts yet, so every list starts
 * empty and the render path below is the only writer: no sample rows.
 */
const PLUGINS: readonly PluginEntry[] = Object.freeze([])
const SKILLS: readonly SkillEntry[] = Object.freeze([])
const MCP_CONNECTIONS: readonly McpEntry[] = Object.freeze([])
const PROJECT_INSTRUCTIONS: readonly InstructionEntry[] = Object.freeze([])

const PLUGIN_STATE_STORAGE_KEY = 'winwincode.extensions.plugin-state.v1'

function pluginStorage(view: Document): Storage | null {
  try {
    return view.defaultView?.localStorage ?? null
  } catch {
    return null
  }
}

function readPluginState(storage: Storage | null): Map<string, boolean> {
  const enabled = new Map<string, boolean>()
  if (storage === null) return enabled
  try {
    const raw = storage.getItem(PLUGIN_STATE_STORAGE_KEY)
    if (raw === null) return enabled
    const parsed: unknown = JSON.parse(raw)
    if (parsed === null || typeof parsed !== 'object') return enabled
    for (const [id, value] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof value === 'boolean') enabled.set(id, value)
    }
  } catch {
    // A blocked or corrupted store falls back to the default switch state.
  }
  return enabled
}

function writePluginState(storage: Storage | null, enabled: Map<string, boolean>): void {
  if (storage === null) return
  try {
    storage.setItem(PLUGIN_STATE_STORAGE_KEY, JSON.stringify(
      Object.fromEntries(enabled),
    ))
  } catch {
    // Local switch state is cosmetic; a blocked store never breaks the page.
  }
}

/**
 * Community extensions hub (design pages 09-11): one surface with 插件 /
 * 技能与指令 / MCP 连接 tabs. The control plane has no inventory contract for
 * these tabs yet: the toolbar, table structure, and empty states are complete,
 * the lists render from the page-local models above, and actions that would
 * need a backend degrade to a disabled state explained by a title.
 */
export function mountExtensionsPage(options: ExtensionsPageOptions): ExtensionsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-extensions')
  layout.dataset.wwcPage = 'management'

  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '扩展',
      headingLevel: 2,
      className: 'wwc-extensions-heading',
    },
  })
  const heading = pageHeader.root

  let selectedTab: ExtensionsTabId = 'plugins'
  const primaryActionButton = mountButton({
    document,
    props: {
      label: PRIMARY_ACTION_LABEL[selectedTab],
      variant: 'primary',
      className: 'wwc-extensions-primary-action',
      onActivate() {
        // No install/add contract exists yet: present the disabled state
        // instead of pretending the action succeeded.
        primaryActionButton.update({
          label: PRIMARY_ACTION_LABEL[selectedTab],
          variant: 'primary',
          className: 'wwc-extensions-primary-action',
          disabled: true,
        })
        primaryActionButton.root.title = UNAVAILABLE_TITLE
      },
    },
  })
  const primaryAction = primaryActionButton.root

  const headerRow = element(document, 'div', 'wwc-extensions-header')
  headerRow.append(heading, primaryAction)

  const tabs = mountTabs({
    document,
    props: {
      id: 'wwc-extensions-tabs',
      label: '扩展分类',
      tabs: EXTENSIONS_TABS.map(tab => ({
        id: tab.id,
        label: tab.label,
        panelId: tab.panelId,
      })),
      selectedId: selectedTab,
      onSelect(id: string) {
        if (id === 'plugins' || id === 'skills' || id === 'mcp') show(id)
      },
    },
  })

  /** A section head row: bold label, optional count, hairline underneath. */
  function sectionHead(className: string, labelText: string, count: number | null): HTMLElement {
    const head = element(document, 'div', className)
    const label = element(document, 'h3', `${className}-label`)
    label.textContent = count === null ? labelText : `${labelText} · ${String(count)}`
    head.append(label)
    return head
  }

  /** A collapsed disclosure row: bold text with a chevron that rotates open. */
  function collapsedRow(
    id: string,
    labelText: string,
    content: HTMLElement,
    expanded: boolean,
  ): { readonly root: HTMLElement; readonly button: HTMLButtonElement } {
    const root = element(document, 'div', 'wwc-extensions-collapsed')
    const button = element(document, 'button', 'wwc-extensions-collapsed-button')
    button.type = 'button'
    button.id = `${id}-button`
    button.setAttribute('aria-expanded', expanded ? 'true' : 'false')
    button.setAttribute('aria-controls', `${id}-content`)
    const label = element(document, 'span', 'wwc-extensions-collapsed-label')
    label.textContent = labelText
    const chevron = element(document, 'span', 'wwc-extensions-collapsed-chevron')
    chevron.setAttribute('aria-hidden', 'true')
    chevron.textContent = '›'
    button.append(label, chevron)
    content.id = `${id}-content`
    content.hidden = !expanded
    root.append(button, content)
    button.addEventListener('click', () => {
      const next = content.hidden
      content.hidden = !next
      button.setAttribute('aria-expanded', next ? 'true' : 'false')
    })
    return { root, button }
  }

  /** An action that only becomes real with a backend contract. */
  function unavailableAction(className: string, label: string): HTMLButtonElement {
    const button = element(document, 'button', className)
    button.type = 'button'
    button.dataset.wwcComponent = 'button'
    button.dataset.variant = 'default'
    button.textContent = label
    button.title = UNAVAILABLE_TITLE
    button.addEventListener('click', () => {
      button.disabled = true
      button.title = UNAVAILABLE_TITLE
    })
    return button
  }

  function moreAction(className: string): HTMLButtonElement {
    const button = element(document, 'button', className)
    button.type = 'button'
    button.dataset.wwcComponent = 'button'
    button.dataset.variant = 'default'
    button.setAttribute('aria-label', '更多操作')
    button.textContent = '⋯'
    button.title = UNAVAILABLE_TITLE
    button.addEventListener('click', () => {
      button.disabled = true
      button.title = UNAVAILABLE_TITLE
    })
    return button
  }

  // --- 插件 (design page 09) -------------------------------------------------

  const pluginsPanel = element(document, 'section', 'wwc-extensions-panel wwc-extensions-plugins')
  pluginsPanel.id = 'wwc-extensions-plugins-panel'
  pluginsPanel.setAttribute('role', 'tabpanel')
  pluginsPanel.setAttribute('aria-label', '插件')
  pluginsPanel.tabIndex = -1

  const pluginsHead = sectionHead('wwc-extensions-plugins-head', '已安装', PLUGINS.length)
  const pluginList = element(document, 'ul', 'wwc-extensions-plugin-list')
  const pluginsEmpty = mountEmptyState({
    document,
    props: {
      title: '尚未安装插件',
      detail: '插件由执行设备上报。在执行设备安装后显示在这里。',
      headingLevel: 3,
      className: 'wwc-extensions-plugins-empty',
    },
  })
  pluginsPanel.append(pluginsHead, pluginList, pluginsEmpty.root)

  const pluginStore = pluginStorage(document)
  const pluginEnabled = readPluginState(pluginStore)
  const pluginRows = new WeakMap<HTMLLIElement, {
    readonly name: HTMLElement
    readonly description: HTMLElement
    readonly toggle: HTMLButtonElement
    readonly onToggle: () => void
  }>()
  const pluginCollection = mountKeyedCollection<PluginEntry, string, HTMLLIElement>({
    parent: pluginList,
    key: plugin => plugin.id,
    create(plugin: PluginEntry) {
      const item = element(document, 'li', 'wwc-extensions-plugin-row')
      const info = element(document, 'div', 'wwc-extensions-plugin-info')
      const name = element(document, 'p', 'wwc-extensions-plugin-name')
      const description = element(document, 'p', 'wwc-extensions-plugin-description')
      info.append(name, description)
      const actions = element(document, 'div', 'wwc-extensions-plugin-actions')
      const configure = unavailableAction(
        'wwc-extensions-plugin-configure',
        '配置',
      )
      const toggle = element(document, 'button', 'wwc-extensions-switch')
      toggle.type = 'button'
      toggle.setAttribute('role', 'switch')
      const knob = element(document, 'span', 'wwc-extensions-switch-knob')
      knob.setAttribute('aria-hidden', 'true')
      toggle.append(knob)
      const onToggle = () => {
        const next = toggle.getAttribute('aria-checked') !== 'true'
        toggle.setAttribute('aria-checked', next ? 'true' : 'false')
        toggle.setAttribute('aria-label', `${name.textContent ?? plugin.name} 已${next ? '启用' : '停用'}`)
        pluginEnabled.set(plugin.id, next)
        writePluginState(pluginStore, pluginEnabled)
      }
      toggle.addEventListener('click', onToggle)
      actions.append(configure, toggle)
      item.append(info, actions)
      pluginRows.set(item, { name, description, toggle, onToggle })
      return item
    },
    update(item, plugin: PluginEntry) {
      const row = pluginRows.get(item)
      if (row === undefined) return
      row.name.textContent = plugin.name
      row.description.textContent = plugin.description
      const enabled = pluginEnabled.get(plugin.id) ?? true
      row.toggle.setAttribute('aria-checked', enabled ? 'true' : 'false')
      row.toggle.setAttribute('aria-label', `${plugin.name} 已${enabled ? '启用' : '停用'}`)
    },
    remove(item) {
      const row = pluginRows.get(item)
      if (row === undefined) return
      row.toggle.removeEventListener('click', row.onToggle)
      pluginRows.delete(item)
    },
  })

  // --- 技能与指令 (design page 10) -------------------------------------------

  const skillsPanel = element(document, 'section', 'wwc-extensions-panel wwc-extensions-skills')
  skillsPanel.id = 'wwc-extensions-skills-panel'
  skillsPanel.setAttribute('role', 'tabpanel')
  skillsPanel.setAttribute('aria-label', '技能与指令')
  skillsPanel.tabIndex = -1

  const scopeRow = element(document, 'div', 'wwc-extensions-scope-row')
  const scopeLabel = element(document, 'label', 'wwc-extensions-scope-label')
  scopeLabel.htmlFor = 'wwc-extensions-scope'
  scopeLabel.textContent = '作用范围'
  const scopeSelect = element(document, 'select', 'wwc-extensions-scope-select')
  scopeSelect.id = 'wwc-extensions-scope'
  const scopeOption = element(document, 'option', '')
  // The community client is single-scope; the option label mirrors the design.
  scopeOption.value = 'winwincode'
  scopeOption.textContent = 'winwincode'
  scopeSelect.append(scopeOption)
  scopeRow.append(scopeLabel, scopeSelect)

  const skillsHead = element(document, 'div', 'wwc-extensions-table-head')
  for (const column of ['名称', '指令', '描述', '操作']) {
    const cell = element(document, 'span', 'wwc-extensions-table-head-cell')
    cell.textContent = column
    skillsHead.append(cell)
  }

  const skillList = element(document, 'ul', 'wwc-extensions-skill-list')
  const skillsEmpty = mountEmptyState({
    document,
    props: {
      title: '暂无技能或项目指令',
      detail: '技能与指令来自当前仓库。在执行设备安装后显示在这里。',
      headingLevel: 3,
      className: 'wwc-extensions-skills-empty',
    },
  })
  const skillRows = new WeakMap<HTMLLIElement, {
    readonly name: HTMLElement
    readonly command: HTMLElement
    readonly description: HTMLElement
  }>()
  const skillCollection = mountKeyedCollection<SkillEntry, string, HTMLLIElement>({
    parent: skillList,
    key: skill => skill.id,
    create(skill: SkillEntry) {
      const item = element(document, 'li', 'wwc-extensions-skill-row')
      const name = element(document, 'p', 'wwc-extensions-skill-name')
      const command = element(document, 'code', 'wwc-extensions-skill-command')
      const description = element(document, 'p', 'wwc-extensions-skill-description')
      const actions = element(document, 'div', 'wwc-extensions-skill-actions')
      actions.append(
        unavailableAction('wwc-extensions-skill-edit', '编辑'),
        moreAction('wwc-extensions-skill-more'),
      )
      item.append(name, command, description, actions)
      skillRows.set(item, { name, command, description })
      return item
    },
    update(item, skill: SkillEntry) {
      const row = skillRows.get(item)
      if (row === undefined) return
      row.name.textContent = skill.name
      row.command.textContent = skill.command
      row.description.textContent = skill.description
    },
    remove(item) {
      skillRows.delete(item)
    },
  })

  const instructionsContent = element(document, 'div', 'wwc-extensions-instructions')
  const instructionList = element(document, 'ul', 'wwc-extensions-instruction-list')
  const instructionsEmpty = element(document, 'p', 'wwc-extensions-instructions-empty')
  instructionsEmpty.textContent = '项目指令来自仓库根目录的 AGENTS.md。在执行设备安装后显示在这里。'
  const instructionRows = new WeakMap<HTMLLIElement, {
    readonly name: HTMLElement
    readonly description: HTMLElement
  }>()
  const instructionCollection = mountKeyedCollection<InstructionEntry, string, HTMLLIElement>({
    parent: instructionList,
    key: instruction => instruction.id,
    create(instruction: InstructionEntry) {
      const item = element(document, 'li', 'wwc-extensions-instruction-row')
      const name = element(document, 'p', 'wwc-extensions-instruction-name')
      const description = element(document, 'p', 'wwc-extensions-instruction-description')
      const actions = element(document, 'div', 'wwc-extensions-instruction-actions')
      actions.append(moreAction('wwc-extensions-instruction-more'))
      item.append(name, description, actions)
      instructionRows.set(item, { name, description })
      return item
    },
    update(item, instruction: InstructionEntry) {
      const row = instructionRows.get(item)
      if (row === undefined) return
      row.name.textContent = instruction.name
      row.description.textContent = instruction.description
    },
    remove(item) {
      instructionRows.delete(item)
    },
  })
  instructionsContent.append(instructionList, instructionsEmpty)
  const instructionsRow = collapsedRow(
    'wwc-extensions-instructions',
    '项目指令 · AGENTS.md',
    instructionsContent,
    false,
  )

  skillsPanel.append(scopeRow, skillsHead, skillList, skillsEmpty.root, instructionsRow.root)

  // --- MCP 连接 (design page 11) ---------------------------------------------

  const mcpPanel = element(document, 'section', 'wwc-extensions-panel wwc-extensions-mcp')
  mcpPanel.id = 'wwc-extensions-mcp-panel'
  mcpPanel.setAttribute('role', 'tabpanel')
  mcpPanel.setAttribute('aria-label', 'MCP 连接')
  mcpPanel.tabIndex = -1

  const mcpList = element(document, 'ul', 'wwc-extensions-mcp-list')
  const mcpEmpty = mountEmptyState({
    document,
    props: {
      title: '尚未配置 MCP 连接',
      detail: 'MCP 连接由执行设备上报。在执行设备安装后显示在这里。',
      headingLevel: 3,
      className: 'wwc-extensions-mcp-empty',
    },
  })
  const mcpRows = new WeakMap<HTMLLIElement, {
    readonly icon: HTMLElement
    readonly name: HTMLElement
    readonly state: HTMLElement
  }>()
  function createMcpRow(connection: McpEntry): HTMLLIElement {
    const item = element(document, 'li', 'wwc-extensions-mcp-row')
    const icon = element(document, 'span', 'wwc-extensions-mcp-icon')
    icon.setAttribute('aria-hidden', 'true')
    const info = element(document, 'div', 'wwc-extensions-mcp-info')
    const name = element(document, 'p', 'wwc-extensions-mcp-name')
    const state = element(document, 'p', 'wwc-extensions-mcp-state')
    info.append(name, state)
    const actions = element(document, 'div', 'wwc-extensions-mcp-actions')
    actions.append(
      unavailableAction('wwc-extensions-mcp-tools', '查看工具'),
      moreAction('wwc-extensions-mcp-more'),
    )
    item.append(icon, info, actions)
    mcpRows.set(item, { icon, name, state })
    return item
  }
  function updateMcpRow(item: HTMLLIElement, connection: McpEntry): void {
    const row = mcpRows.get(item)
    if (row === undefined) return
    row.icon.textContent = connection.name.charAt(0).toUpperCase()
    row.name.textContent = connection.name
    row.state.textContent = connection.connected ? '🟢 已连接' : '⚪ 已停用'
    item.dataset.connected = connection.connected ? 'true' : 'false'
  }
  function removeMcpRow(item: HTMLLIElement): void {
    mcpRows.delete(item)
  }
  const mcpCollection = mountKeyedCollection<McpEntry, string, HTMLLIElement>({
    parent: mcpList,
    key: connection => connection.id,
    create: createMcpRow,
    update: updateMcpRow,
    remove: removeMcpRow,
  })

  const disabledConnectionsContent = element(document, 'div', 'wwc-extensions-disabled-mcp')
  const disabledMcpList = element(document, 'ul', 'wwc-extensions-disabled-mcp-list')
  const disabledMcpEmpty = element(document, 'p', 'wwc-extensions-disabled-mcp-empty')
  disabledMcpEmpty.textContent = '没有已停用的 MCP 连接。'
  disabledConnectionsContent.append(disabledMcpList, disabledMcpEmpty)
  const disabledMcpCollection = mountKeyedCollection<McpEntry, string, HTMLLIElement>({
    parent: disabledMcpList,
    key: connection => connection.id,
    create: createMcpRow,
    update: updateMcpRow,
    remove: removeMcpRow,
  })
  const disabledMcpRow = collapsedRow(
    'wwc-extensions-disabled-mcp',
    `已停用 · ${String(MCP_CONNECTIONS.filter(connection => !connection.connected).length)}`,
    disabledConnectionsContent,
    false,
  )

  mcpPanel.append(mcpList, mcpEmpty.root, disabledMcpRow.root)

  // --- shared render path -----------------------------------------------------

  function renderLists(): void {
    pluginCollection.update(PLUGINS)
    skillCollection.update(SKILLS)
    instructionCollection.update(PROJECT_INSTRUCTIONS)
    mcpCollection.update(MCP_CONNECTIONS)
    disabledMcpCollection.update(MCP_CONNECTIONS.filter(connection => !connection.connected))
    pluginList.hidden = PLUGINS.length === 0
    pluginsEmpty.root.hidden = PLUGINS.length !== 0
    skillList.hidden = SKILLS.length === 0
    skillsEmpty.root.hidden = SKILLS.length !== 0
    instructionList.hidden = PROJECT_INSTRUCTIONS.length === 0
    instructionsEmpty.hidden = PROJECT_INSTRUCTIONS.length !== 0
    mcpList.hidden = MCP_CONNECTIONS.length === 0
    mcpEmpty.root.hidden = MCP_CONNECTIONS.length !== 0
    disabledMcpList.hidden = MCP_CONNECTIONS.every(connection => connection.connected)
    disabledMcpEmpty.hidden = MCP_CONNECTIONS.some(connection => !connection.connected)
  }

  function show(next: ExtensionsTabId): void {
    selectedTab = next
    tabs.update({
      id: 'wwc-extensions-tabs',
      label: '扩展分类',
      tabs: EXTENSIONS_TABS.map(tab => ({
        id: tab.id,
        label: tab.label,
        panelId: tab.panelId,
      })),
      selectedId: next,
      onSelect(id: string) {
        if (id === 'plugins' || id === 'skills' || id === 'mcp') show(id)
      },
    })
    // A fresh tab gets a fresh action; the disabled state never survives it.
    primaryActionButton.update({
      label: PRIMARY_ACTION_LABEL[next],
      variant: 'primary',
      className: 'wwc-extensions-primary-action',
      disabled: false,
    })
    primaryAction.title = ''
    for (const tab of EXTENSIONS_TABS) {
      const panel = tab.id === 'plugins'
        ? pluginsPanel
        : tab.id === 'skills' ? skillsPanel : mcpPanel
      panel.hidden = tab.id !== next
    }
  }

  renderLists()
  show(selectedTab)

  layout.append(headerRow, tabs.root, pluginsPanel, skillsPanel, mcpPanel)
  options.root.replaceChildren(layout)

  return {
    close() {
      tabs.close()
      pluginCollection.close()
      skillCollection.close()
      instructionCollection.close()
      mcpCollection.close()
      disabledMcpCollection.close()
      pluginsEmpty.close()
      skillsEmpty.close()
      mcpEmpty.close()
      primaryActionButton.close()
      pageHeader.close()
      options.root.replaceChildren()
    },
  }
}
