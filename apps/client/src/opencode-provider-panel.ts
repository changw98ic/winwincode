// SPDX-License-Identifier: Apache-2.0

import {
  DeviceProviderOpenCodeAccountState as AccountState,
  DeviceProviderOpenCodeLoginState as LoginState,
  DeviceProviderOpenCodeOperation as Operation,
  type DeviceProviderOpenCodeCommand,
  type DeviceProviderSnapshot,
} from './generated/contracts.js'

interface OpenCodePanelOptions {
  readonly root: HTMLElement
  readonly snapshot: () => DeviceProviderSnapshot | null
  readonly available: () => boolean
  readonly send: (command: DeviceProviderOpenCodeCommand) => Promise<void>
  readonly report: (message: string) => void
}

/** Account metadata is public. OAuth credentials and device codes never reach this view. */
export function mountOpenCodeProviderPanel(options: OpenCodePanelOptions) {
  const document = options.root.ownerDocument
  let closed = false
  let timer: ReturnType<typeof setTimeout> | undefined
  const node = <K extends keyof HTMLElementTagNameMap>(tag: K, text = ''): HTMLElementTagNameMap[K] => {
    const element = document.createElement(tag)
    element.textContent = text
    return element
  }
  const heading = node('h3', 'OpenCode Go 账号')
  const note = node('p', '请先在 OpenCode 官方控制台开通 Go，并关闭 Use balance。WinWinCode 无法核验此开关。')
  const add = node('button', '登录 OpenCode 账号'); add.type = 'button'
  const content = node('div')
  options.root.append(heading, note, add, content)
  add.addEventListener('click', () => { void options.send({ operation: Operation.BeginOpencodeLogin }) })

  function button(text: string, command: DeviceProviderOpenCodeCommand): HTMLButtonElement {
    const element = node('button', text)
    element.type = 'button'
    element.disabled = !options.available()
    element.addEventListener('click', () => { void options.send(command) })
    return element
  }

  function render(): void {
    if (timer !== undefined) clearTimeout(timer)
    timer = undefined
    if (closed) return
    add.disabled = !options.available()
    content.replaceChildren()
    const snapshot = options.snapshot()
    const accounts = snapshot?.openCodeAccounts ?? []
    const states: Record<AccountState, string> = {
      authorized: '已授权', refresh_in_flight: '正在刷新凭据',
      reauthorization_required: '需要重新授权', logged_out: '已退出',
    }
    for (const account of accounts) {
      const row = node('div')
      row.append(node('p', `${account.email} · ${states[account.state]}`))
      const connections = (snapshot?.providers ?? []).filter(provider => provider.openCode?.accountRef === account.accountRef)
      for (const provider of connections) {
        row.append(node('p', `${provider.config.displayName} · ${provider.openCode?.organizationName} · ${provider.config.enabled ? '已启用' : '已禁用'}`))
      }
      const usage = account.usage
      if (usage === undefined) {
        row.append(node('p', 'Go 用量尚未查询。'))
      } else {
        row.append(node('p', `Go 用量 · 组织 ${usage.organizationId} · 更新于 ${new Date(usage.updatedAtMs).toLocaleString()}`))
        const windows = [['滚动窗口', usage.rolling], ['本周', usage.weekly], ['本月', usage.monthly]] as const
        for (const [label, window] of windows) {
          row.append(node('p', `${label}：${window.percent}% · ${window.status} · 重置时间 ${window.resetsAt}`))
        }
      }
      if (account.state === AccountState.Authorized) {
        const organizations = new Map(connections.map(provider => [provider.openCode!.organizationId, provider.openCode!.organizationName]))
        for (const [organizationId, organizationName] of organizations) {
          row.append(button(`查询 Go 用量 · ${organizationName}`, { operation: Operation.OpencodeUsage, accountRef: account.accountRef, organizationId }))
        }
        row.append(button('退出账号', { operation: Operation.LogoutOpencode, accountRef: account.accountRef }))
      } else if (account.state !== AccountState.RefreshInFlight) {
        row.append(button('重新授权', { operation: Operation.BeginOpencodeLogin }))
      }
      content.append(row)
    }
    for (const login of snapshot?.openCodeLogins ?? []) {
      if (login.state === LoginState.Pending) {
        const row = node('div')
        row.append(node('p', `授权码：${login.userCode ?? '等待设备返回'} · 截止时间 ${new Date(login.expiresAtMs).toLocaleTimeString()}`))
        if (login.verificationUri !== undefined && safeAuthorizationUrl(login.verificationUri)) {
          const link = node('a', '打开 OpenCode 官方授权页')
          link.href = login.verificationUri; link.target = '_blank'; link.rel = 'noopener noreferrer'
          row.append(link)
        }
        row.append(node('p', '请在官方页面确认当前登录的账号。'))
        row.append(button('取消授权', { operation: Operation.CancelOpencodeLogin, loginId: login.loginId }))
        content.append(row)
      } else if (login.state === LoginState.Authorized || login.state === LoginState.Completed) {
        const account = accounts.find(account => account.accountRef === login.accountRef)
        if (account?.state !== AccountState.Authorized) continue
        const row = node('div')
        row.append(node('p', `实际登录账号：${account.email}。请选择组织并添加独立连接。`))
        const organization = node('select'); organization.setAttribute('aria-label', `选择 ${account.email} 的组织`)
        for (const org of login.organizations) { const option = node('option', `${org.name} · ${org.id}`); option.value = org.id; organization.append(option) }
        const name = node('input'); name.value = `OpenCode Go · ${account.email}`; name.maxLength = 200
        name.setAttribute('aria-label', '连接显示名称')
        const model = node('input'); model.maxLength = 128
        model.setAttribute('aria-label', 'Go 模型（可选）'); model.placeholder = '例如 qwen3.8-flash；留空使用默认模型目录'
        const connect = node('button', '添加 Go 连接'); connect.type = 'button'; connect.disabled = !options.available() || login.organizations.length === 0
        connect.addEventListener('click', () => {
          if (name.value.trim() === '') { options.report('请填写连接显示名称。'); return }
          const uuid = (document.defaultView?.crypto ?? globalThis.crypto).randomUUID().replaceAll('-', '')
          void options.send({ operation: Operation.ConnectOpencode, loginId: login.loginId, organizationId: organization.value, providerId: `opencode-go-${uuid}`, displayName: name.value.trim(),
            ...(model.value.trim() ? { modelId: model.value.trim() } : {}) })
        })
        organization.disabled = !options.available(); name.disabled = !options.available()
        model.disabled = !options.available()
        row.append(organization, name, model, connect)
        content.append(row)
      } else if (login.state === LoginState.Failed || login.state === LoginState.Expired || login.state === LoginState.Denied) {
        const labels = { failed: '授权失败，请重新登录。', expired: '授权码已过期，请重新登录。', denied: '官方页面拒绝了本次授权。' }
        content.append(node('p', labels[login.state]))
      }
    }
    const pending = snapshot?.openCodeLogins?.filter(login => login.state === LoginState.Pending)
      .sort((left, right) => left.pollAfterMs - right.pollAfterMs)[0]
    if (pending !== undefined && options.available()) {
      timer = setTimeout(() => {
        if (!closed) void options.send({ operation: Operation.PollOpencodeLogin, loginId: pending.loginId })
      }, Math.max(500, Math.min(60_000, pending.pollAfterMs - Date.now())))
    }
  }

  return { render, close() { closed = true; if (timer !== undefined) clearTimeout(timer); options.root.replaceChildren() } }
}

function safeAuthorizationUrl(value: string): boolean {
  try {
    const url = new URL(value)
    return url.protocol === 'https:' && url.hostname === 'opencode.ai' && url.port === ''
      && url.username === '' && url.password === '' && url.hash === '' && url.pathname.startsWith('/console/')
  } catch { return false }
}
