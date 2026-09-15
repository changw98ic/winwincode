// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientTransport } from './community-control-plane-client.js'

import { encryptDeviceProvider, type DeviceProviderMutation } from './device-provider-encryption.js'
import { DeviceProviderProtocol, type DeviceProviderConfig, type DeviceProviderView } from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'

interface Device { readonly clientId: string; readonly displayName: string }

export interface DeviceProviderPanelOptions {
  readonly root: HTMLElement
  readonly serverUrl: string
  readonly readOnly?: boolean
  readonly fetch?: ControlPlaneClientTransport['fetch']
}

export function mountDeviceProviderPanel(options: DeviceProviderPanelOptions) {
  const document = options.root.ownerDocument
  const browser = document.defaultView
  const fetcher = options.fetch
  const controller = new AbortController()
  let view: DeviceProviderView | null = null
  let busy = false
  let generation = 0
  let closed = false
  const node = <K extends keyof HTMLElementTagNameMap>(tag: K, text = ''): HTMLElementTagNameMap[K] => {
    const element = document.createElement(tag)
    element.textContent = text
    return element
  }
  const devices = node('select')
  devices.id = 'wwc-provider-device'
  const deviceLabel = node('label', '配置设备')
  deviceLabel.htmlFor = devices.id
  const refresh = node('button', '刷新设备')
  refresh.type = 'button'
  const status = node('p', '正在读取设备…')
  status.setAttribute('role', 'status')
  const list = node('ul')
  list.className = 'wwc-settings-provider-rows'
  const form = node('form')
  form.className = 'wwc-settings-route-form'
  const field = (id: string, title: string, type = 'text') => {
    const label = node('label', title)
    const input = node('input')
    input.id = `wwc-device-provider-${id}`
    input.type = type
    input.required = id !== 'key'
    input.maxLength = id === 'key' ? 8192 : id === 'endpoint' ? 2048 : 200
    label.htmlFor = input.id
    label.append(input)
    form.append(label)
    return input
  }
  const provider = field('id', '服务商 ID')
  const name = field('name', '显示名称')
  const endpoint = field('endpoint', 'API 地址', 'url')
  endpoint.placeholder = 'https://open.bigmodel.cn/api/anthropic/v1/messages'
  const protocol = node('select')
  protocol.id = 'wwc-device-provider-protocol'
  for (const [value, label] of [[DeviceProviderProtocol.AnthropicMessages, 'Anthropic Messages'], [DeviceProviderProtocol.Canonical, 'Canonical SSE']] as const) {
    const option = node('option', label); option.value = value; protocol.append(option)
  }
  const protocolLabel = node('label', '接口协议'); protocolLabel.htmlFor = protocol.id; protocolLabel.append(protocol); form.append(protocolLabel)
  const models = field('models', '模型 ID（多个用逗号分隔）')
  models.placeholder = 'glm-5.3-flash'
  const key = field('key', 'API Key', 'password')
  key.autocomplete = 'new-password'
  key.spellcheck = false
  const note = node('p', '配置保存在所选设备。编辑时留空 API Key，可继续使用设备中的密钥。')
  const enabled = node('input'); enabled.type = 'checkbox'; enabled.checked = true; enabled.id = 'wwc-device-provider-enabled'
  const enabledLabel = node('label', '启用此服务商'); enabledLabel.htmlFor = enabled.id; enabledLabel.append(enabled)
  const save = node('button', '保存到设备'); save.type = 'submit'
  const test = node('button', '测试连接'); test.type = 'button'
  const remove = node('button', '删除'); remove.type = 'button'
  const clear = node('button', '添加服务商'); clear.type = 'button'
  const controls = node('div'); controls.className = 'wwc-settings-route-controls'; controls.append(save, test, remove, clear)
  form.append(enabledLabel, note, controls)
  options.root.append(deviceLabel, devices, refresh, status, list, form)

  async function request(path: string, method: 'GET' | 'POST' | 'DELETE' = 'GET', body?: unknown): Promise<unknown> {
    if (fetcher === undefined) throw new Error('设备设置连接尚未就绪。')
    const response = await fetcher(new URL(path, options.serverUrl).toString(), { method, headers: {}, credentials: 'include', signal: controller.signal,
      ...(body === undefined ? {} : { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }) })
    if (!response.ok) {
      if (response.status === 403) throw new Error('需要所选设备的管理权限。')
      if (response.status === 409) throw new Error('设备离线或配置已变更，请刷新后重试。')
      throw new Error('无法读取设备设置，请检查连接。')
    }
    return JSON.parse(await response.text()) as unknown
  }
  function lock(): void {
    devices.disabled = busy
    refresh.disabled = busy
    for (const control of [provider, name, endpoint, protocol, models, key, enabled, save, test, remove, clear]) {
      control.disabled = busy || options.readOnly === true || view?.online !== true || view.snapshot === null
    }
  }
  function providerView(value: unknown): DeviceProviderView {
    if (!matchesCanonicalSchema('DeviceProviderView', value)) throw new Error('设备返回了无效的配置。')
    return value as DeviceProviderView
  }
  function edit(config: DeviceProviderConfig): void {
    provider.value = config.providerId; name.value = config.displayName; endpoint.value = config.endpoint
    protocol.value = config.protocol; models.value = config.modelIds.join(', '); enabled.checked = config.enabled; key.value = ''
  }
  function show(): void {
    list.replaceChildren()
    for (const entry of view?.snapshot?.providers ?? []) {
      const item = node('li'); const editButton = node('button', '编辑'); editButton.type = 'button'
      editButton.disabled = busy || options.readOnly === true
      editButton.addEventListener('click', () => edit(entry.config))
      item.append(node('span', `${entry.config.displayName} · ${entry.config.modelIds.join(', ')} · ${entry.credentialConfigured ? '已配置密钥' : '待配置'}`), editButton)
      list.append(item)
    }
    lock()
  }
  async function load(): Promise<void> {
    const current = ++generation
    key.value = ''; view = null; lock()
    if (devices.value === '') { status.textContent = '请先在设备页面连接一台设备，再设置服务商。'; return }
    try {
      const result = providerView(await request(`/api/v1/clients/${encodeURIComponent(devices.value)}/providers`))
      if (closed || generation !== current) return
      view = result
      status.textContent = !result.online ? '设备离线，连接后可设置服务商。' : result.snapshot === null ? '等待设备上报 Provider 配置。' : '设备已连接，可以保存和测试服务商。'
      show()
    } catch (error) { if (!closed && current === generation) status.textContent = error instanceof Error ? error.message : '设备读取失败。' }
  }
  async function directory(): Promise<void> {
    try {
      const result = await request('/api/v1/clients') as { clients: Device[] }
      if (closed) return
      const previous = devices.value; devices.replaceChildren()
      for (const device of result.clients) { const option = node('option', `${device.displayName} · ${device.clientId}`); option.value = device.clientId; devices.append(option) }
      if (result.clients.some(device => device.clientId === previous)) devices.value = previous
      await load()
    } catch (error) { if (!closed) status.textContent = error instanceof Error ? error.message : '设备读取失败。' }
  }
  async function submit(operation: DeviceProviderMutation['operation']): Promise<void> {
    if (options.readOnly === true || busy || view?.online !== true || view.snapshot === null || !form.reportValidity()) return
    const snapshot = view.snapshot
    const selected = devices.value
    const secret = key.value
    const mutation: DeviceProviderMutation = { operation, config: { providerId: provider.value.trim(), displayName: name.value.trim(), endpoint: endpoint.value.trim(),
      protocol: protocol.value as DeviceProviderProtocol, modelIds: models.value.split(',').map(value => value.trim()).filter(Boolean), enabled: enabled.checked }, ...(secret === '' ? {} : { apiKey: secret }) }
    busy = true; show(); status.textContent = operation === 'test' ? '等待设备测试连接…' : '等待设备保存回执…'
    try {
      const id = `provider_${(browser?.crypto ?? crypto).randomUUID().replaceAll('-', '')}`
      const encrypted = await encryptDeviceProvider(snapshot, id, mutation, browser?.crypto ?? crypto)
      key.value = ''
      await request(`/api/v1/clients/${encodeURIComponent(selected)}/providers`, 'POST', encrypted)
      const deadline = Date.now() + 120_000
      while (!closed && Date.now() < deadline) {
        const result = providerView(await request(`/api/v1/clients/${encodeURIComponent(selected)}/providers/receipts/${id}`))
        if (closed) return
        view = result
        if (result.receipt !== null) {
          const labels: Record<string, string> = { saved: '已保存到设备。', deleted: '已从设备删除。', tested: '设备已成功调用模型，连接测试通过。', invalid_request: '设备拒绝了配置，请检查 API 地址、模型和密钥。', revision_conflict: '设备配置已变更，请刷新后重试。', provider_unavailable: '设备调用失败，请检查服务商配置和网络。', interrupted: '设备测试被中断，请重新测试。' }
          status.textContent = labels[result.receipt.outcome] ?? '设备返回了未知结果。'
          show(); return
        }
        if (!result.online) { status.textContent = '设备已离线，尚未收到完成回执。'; return }
        await new Promise(resolve => setTimeout(resolve, 500))
      }
      status.textContent = '尚未收到设备回执，请刷新确认配置。'
    } catch (error) { if (!closed) status.textContent = error instanceof Error ? error.message : '设备设置失败。' }
    finally { busy = false; if (!closed) show() }
  }
  form.addEventListener('submit', event => { event.preventDefault(); void submit('save') })
  test.addEventListener('click', () => { void submit('test') })
  remove.addEventListener('click', () => { void submit('delete') })
  clear.addEventListener('click', () => { form.reset(); provider.focus() })
  devices.addEventListener('change', () => { form.reset(); void load() })
  refresh.addEventListener('click', () => { void directory() })
  lock(); void directory()
  return { close() { closed = true; controller.abort(); key.value = ''; options.root.replaceChildren() },
    refresh: directory, get online() { return view?.online === true }, get configured() { return (view?.snapshot?.providers.length ?? 0) > 0 } }
}
