// SPDX-License-Identifier: Apache-2.0

import { mountButton, mountPageHeader } from '@winwincode/browser-ui'
import { mountEmptyState, mountTabs } from './components/index.js'
import type { ControlPlaneClientTransport } from './community-control-plane-client.js'
import { encryptDeviceExtension, type DeviceExtensionMutation } from './device-provider-encryption.js'
import type { DeviceExtensionView } from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'

export interface ExtensionsPageOptions {
  readonly root: HTMLElement
  readonly serverUrl?: string
  readonly fetch?: ControlPlaneClientTransport['fetch']
}
export interface ExtensionsPage { close(): void }

const TABS = [
  { id: 'plugins', label: '插件', panelId: 'wwc-extensions-plugins-panel' },
  { id: 'skills', label: '技能与指令', panelId: 'wwc-extensions-skills-panel' },
  { id: 'mcp', label: 'MCP 连接', panelId: 'wwc-extensions-mcp-panel' },
] as const

/** Manage confirmed Device extensions; each new task loads the latest configuration. */
export function mountExtensionsPage(options: ExtensionsPageOptions): ExtensionsPage {
  const document = options.root.ownerDocument
  const controller = new AbortController()
  const fetcher = options.fetch
  const crypto = document.defaultView?.crypto ?? globalThis.crypto
  let closed = false
  let generation = 0
  let busy = false
  let view: DeviceExtensionView | null = null
  const node = <K extends keyof HTMLElementTagNameMap>(tag: K, text = ''): HTMLElementTagNameMap[K] => {
    const element = document.createElement(tag); element.textContent = text; return element
  }
  const buttons: ReturnType<typeof mountButton>[] = []
  const rowButtons: ReturnType<typeof mountButton>[] = []
  const button = (text: string, action: () => void, row = false): HTMLButtonElement => {
    const view = mountButton({ document, props: { label: text, onActivate: action } })
    ;(row ? rowButtons : buttons).push(view)
    return view.root
  }
  const layout = node('section'); layout.className = 'wwc-extensions'; layout.dataset.wwcPage = 'management'
  const header = mountPageHeader({ document, props: { title: '扩展', headingLevel: 2, className: 'wwc-extensions-heading' } })
  const devices = node('select'); devices.id = 'wwc-extension-device'; devices.className = 'wwc-extensions-scope-select'
  const label = node('label', '配置设备'); label.htmlFor = devices.id
  const refresh = button('刷新设备', () => { void directory() })
  const deviceRow = node('div'); deviceRow.className = 'wwc-extensions-scope-row'; deviceRow.append(label, devices, refresh)
  const status = node('p', '正在读取设备…'); status.setAttribute('role', 'status')
  const note = node('p', '技能和 MCP 保存在所选设备。修改后，同一聊天的下一次任务即可使用；正在执行的任务完成后切换。')
  const panels = TABS.map(tab => {
    const panel = node('section'); panel.id = tab.panelId; panel.className = 'wwc-extensions-panel'
    panel.setAttribute('role', 'tabpanel'); panel.setAttribute('aria-label', tab.label); panel.tabIndex = 0
    return panel
  })
  const pluginEmpty = mountEmptyState({ document, props: { title: '插件暂不可用', detail: '可在技能与指令、MCP 连接中配置设备扩展。', headingLevel: 3 } })
  panels[0]!.append(pluginEmpty.root)
  const managed: (HTMLInputElement | HTMLTextAreaElement | HTMLButtonElement)[] = []
  const forms = (['skill', 'mcp'] as const).map((kind, index) => {
    const panel = panels[index + 1]!
    const list = node('ul'); list.className = 'wwc-settings-provider-rows'
    const form = node('form'); form.className = 'wwc-settings-route-form'; form.hidden = true
    const field = (title: string, suffix: string, multiline = false) => {
      const input = multiline ? node('textarea') : node('input')
      input.id = `wwc-extension-${kind}-${suffix}`; input.maxLength = multiline ? 32768 : suffix === 'source' ? 2048 : 64
      if (input.tagName === 'TEXTAREA') { (input as HTMLTextAreaElement).rows = 10; input.spellcheck = false }
      const label = node('label', title); label.htmlFor = input.id; label.append(input); form.append(label); managed.push(input); return input
    }
    const id = field('标识（字母、数字、下划线或短横线）', 'id') as HTMLInputElement
    id.pattern = '[A-Za-z0-9_-]{1,64}'; id.required = true
    const source = kind === 'skill' ? field('设备上的技能目录（绝对路径）', 'source') : null
    if (source !== null) source.placeholder = '/absolute/path/my-skill'
    const content = field(kind === 'skill' ? '或粘贴 SKILL.md 内容' : 'MCP 配置（JSON）', 'content', true)
    content.required = kind === 'mcp'
    content.placeholder = kind === 'skill' ? '---\nname: my-skill\ndescription: 何时使用此技能\n---\n具体指令…' : '{"command":"node","args":["/absolute/path/server.js"]}'
    const enabled = node('input'); enabled.type = 'checkbox'; enabled.defaultChecked = true; enabled.id = `wwc-extension-${kind}-enabled`
    const enabledLabel = node('label', '启用'); enabledLabel.htmlFor = enabled.id; enabledLabel.append(enabled); form.append(enabledLabel); managed.push(enabled)
    form.append(node('p', kind === 'skill' ? '目录导入会复制 SKILL.md 和配套资源；粘贴内容与目录二选一。' : '支持 stdio 和 Streamable HTTP。密钥可放在 env 或 http_headers 中，配置加密发送到设备。连接测试会启动此服务并读取工具列表。'))
    const saveView = mountButton({ document, props: { label: kind === 'skill' ? '保存技能' : '保存并连接', type: 'submit', variant: 'primary' } }); buttons.push(saveView)
    const save = saveView.root; managed.push(save); form.append(save)
    const add = button(kind === 'skill' ? '添加技能' : '添加 MCP 服务', () => { form.reset(); form.hidden = false; id.focus() }); managed.push(add); add.className = 'wwc-settings-local-save'
    panel.append(add, list, form)
    form.addEventListener('submit', event => {
      event.preventDefault()
      if (!form.reportValidity()) return
      if (kind === 'skill' && (source?.value.trim() === '') === (content.value.trim() === '')) { status.textContent = '请填写技能目录或 SKILL.md 内容，二选一。'; return }
      const mutation: DeviceExtensionMutation = kind === 'skill'
        ? { operation: 'save_skill', id: id.value, enabled: enabled.checked, ...(source?.value.trim() ? { sourcePath: source.value.trim() } : { content: content.value }) }
        : { operation: 'save_mcp', id: id.value, enabled: enabled.checked, configuration: content.value }
      void run(async () => {
        if (await apply(mutation)) {
          content.value = ''; form.hidden = true
          if (kind === 'mcp') await apply({ operation: 'test_mcp', id: mutation.id })
        }
      })
    })
    return { kind, list, form }
  })
  function select(id: string): void {
    panels.forEach((panel, index) => { panel.hidden = TABS[index]?.id !== id })
    tabs.update({ id: 'wwc-extensions-tabs', label: '扩展分类', tabs: TABS, selectedId: id, onSelect: select })
  }
  const tabs = mountTabs({ document, props: { id: 'wwc-extensions-tabs', label: '扩展分类', tabs: TABS, selectedId: 'skills', onSelect: select } })
  select('skills')
  layout.append(header.root, deviceRow, status, note, tabs.root, ...panels); options.root.replaceChildren(layout)

  function show(): void {
    const disabled = busy || view?.online !== true || view.snapshot === null
    devices.disabled = busy; refresh.disabled = busy
    for (const control of managed) control.disabled = disabled
    for (const button of rowButtons.splice(0)) button.close()
    for (const { kind, list } of forms) {
      list.replaceChildren()
      const entries = kind === 'skill' ? view?.snapshot?.skills ?? [] : view?.snapshot?.mcpServers ?? []
      for (const entry of entries) {
        const item = node('li'); item.className = 'wwc-extensions-mcp-row'
        const info = node('div'); info.className = 'wwc-extensions-mcp-info'
        info.append(node('strong', entry.id), node('p', 'name' in entry ? `${entry.name} · ${entry.description}`
          : `${entry.transport} · ${{ untested: '尚未测试连接', ready: '上次连接成功', failed: '连接失败' }[entry.connectionStatus]} · ${entry.toolNames.length} 个工具`))
        if ('toolNames' in entry && entry.toolNames.length > 0) info.append(node('p', entry.toolNames.join('、')))
        const controls = node('div'); controls.className = 'wwc-settings-route-controls'
        controls.append(node('span', entry.enabled ? '已启用' : '已停用'))
        const actions = [button(entry.enabled ? '停用' : '启用', () => { void run(() => apply({ operation: 'set_enabled', kind, id: entry.id, enabled: !entry.enabled })) }, true),
          button('删除', () => { void run(() => apply({ operation: 'delete', kind, id: entry.id })) }, true)]
        if (kind === 'mcp') actions.unshift(button('测试连接', () => { void run(() => apply({ operation: 'test_mcp', id: entry.id })) }, true))
        for (const action of actions) { action.disabled = disabled; controls.append(action) }
        item.append(info, controls); list.append(item)
      }
      if (entries.length === 0) list.append(node('li', kind === 'skill' ? '此设备还没有导入技能。' : '此设备还没有配置 MCP 服务。'))
    }
  }
  async function request(path: string, body?: unknown): Promise<unknown> {
    if (fetcher === undefined || options.serverUrl === undefined) throw new Error('设备设置连接尚未就绪。')
    const response = await fetcher(new URL(path, options.serverUrl).toString(), { method: body === undefined ? 'GET' : 'POST', credentials: 'include', signal: controller.signal,
      headers: body === undefined ? {} : { 'Content-Type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) })
    if (!response.ok) throw new Error(response.status === 403 ? '需要所选设备的管理权限。' : response.status === 409 ? '设备离线或配置已变更，请刷新。' : '设备设置请求失败，请检查连接。')
    return JSON.parse(await response.text()) as unknown
  }
  function parsed(value: unknown): DeviceExtensionView {
    if (!matchesCanonicalSchema('DeviceExtensionView', value)) throw new Error('设备返回了无效的扩展配置。')
    return value as DeviceExtensionView
  }
  async function load(): Promise<void> {
    const current = ++generation; view = null; show()
    if (devices.value === '') { status.textContent = '请先连接一台设备，再配置扩展。'; return }
    try {
      const result = parsed(await request(`/api/v1/clients/${encodeURIComponent(devices.value)}/extensions`))
      if (closed || current !== generation) return
      view = result
      status.textContent = !result.online ? '设备离线，连接后可配置扩展。' : result.snapshot === null ? '等待设备上报扩展配置，请稍后刷新。' : '设备已连接，可配置技能与 MCP。'
      show()
    } catch (error) { if (!closed && current === generation) status.textContent = error instanceof Error ? error.message : '读取失败。' }
  }
  async function directory(): Promise<void> {
    try {
      const result = await request('/api/v1/clients') as { clients: { clientId: string; displayName: string }[] }
      if (closed) return
      const previous = devices.value; devices.replaceChildren()
      for (const device of result.clients) { const option = node('option', `${device.displayName} · ${device.clientId}`); option.value = device.clientId; devices.append(option) }
      if (result.clients.some(device => device.clientId === previous)) devices.value = previous
      await load()
    } catch (error) { if (!closed) status.textContent = error instanceof Error ? error.message : '读取失败。' }
  }
  async function apply(mutation: DeviceExtensionMutation): Promise<boolean> {
    if (view?.online !== true || view.snapshot === null) return false
    const base = `/api/v1/clients/${encodeURIComponent(devices.value)}/extensions`
    const id = `extension_${crypto.randomUUID().replaceAll('-', '')}`
    status.textContent = mutation.operation === 'test_mcp' ? '设备正在连接 MCP 并读取工具…' : '等待设备保存回执…'
    const envelope = await encryptDeviceExtension(view.snapshot, id, mutation, crypto)
    await request(base, envelope)
    const deadline = Date.now() + 90_000
    while (!closed && Date.now() < deadline) {
      const result = parsed(await request(`${base}/receipts/${id}`))
      if (closed) return false
      view = result
      if (result.receipt !== null) {
        const labels = { saved: '已保存到设备，下一次任务生效。', deleted: '已删除，下一次任务生效。', tested: 'MCP 已连接，工具列表已确认。', invalid_request: '设备拒绝了配置，请检查格式、路径和容量限制。', revision_conflict: '配置已变更，请刷新后重试。', connection_failed: 'MCP 连接失败，请检查命令、认证和网络后重新测试。', interrupted: '设备测试被中断，请重新测试。' }
        status.textContent = labels[result.receipt.outcome]
        return ['saved', 'deleted', 'tested'].includes(result.receipt.outcome)
      }
      if (!result.online) throw new Error('设备已离线，尚未收到完成回执。')
      await new Promise(resolve => setTimeout(resolve, 500))
    }
    if (!closed) status.textContent = '尚未收到设备回执，请刷新确认状态。'
    return false
  }
  async function run(action: () => Promise<unknown>): Promise<void> {
    if (busy || view?.online !== true || view.snapshot === null) return
    busy = true; show()
    try { await action() } catch (error) { if (!closed) status.textContent = error instanceof Error ? error.message : '设备设置失败。' }
    finally { busy = false; if (!closed) show() }
  }
  devices.addEventListener('change', () => { for (const { form } of forms) { form.reset(); form.hidden = true }; void load() })
  show(); void directory()
  return { close() { closed = true; controller.abort(); for (const { form } of forms) form.reset(); for (const button of [...buttons, ...rowButtons]) button.close(); tabs.close(); header.close(); pluginEmpty.close(); options.root.replaceChildren() } }
}
