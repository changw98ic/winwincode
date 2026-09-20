// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClientDirectory,
  type ControlPlaneDeviceSummary,
  type ControlPlaneRepositorySummary,
  type ControlPlaneRequestOptions,
  type ControlPlaneRepositoryRegistration,
  type ControlPlaneManagedAppTemplate,
  type ControlPlaneManagedAppTemplatePort,
} from './community-control-plane-client.js'
import { encryptDeviceRepository } from './device-provider-encryption.js'
import { mountPageHeader } from '@winwincode/browser-ui'
import { clearRepositoryDisplayName, repositoryDisplayName, saveRepositoryDisplayName } from './display-labels.js'

export interface ProjectsPageOptions {
  readonly root: HTMLElement
  readonly clientDirectory: ControlPlaneClientDirectory
  readonly newChatHref: (clientId: string, repository: ControlPlaneRepositorySummary) => string
  readonly deviceHref: string
  readonly requestOptions?: () => ControlPlaneRequestOptions | undefined
  readonly registration?: ControlPlaneRepositoryRegistration
  readonly managedAppTemplate?: ControlPlaneManagedAppTemplatePort
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

let managedAppFormSequence = 0

function mountManagedAppSettings(
  document: Document,
  repository: ControlPlaneRepositorySummary,
  port: ControlPlaneManagedAppTemplatePort | undefined,
): HTMLElement {
  const details = element(document, 'details', 'wwc-projects-row-settings')
  const summary = element(document, 'summary', 'wwc-projects-row-settings-summary')
  summary.textContent = '运行设置'
  details.append(summary)
  if (port === undefined) return details
  const templatePort = port

  const form = element(document, 'form', 'wwc-projects-template-form')
  const formId = `wwc-projects-template-${++managedAppFormSequence}`
  form.id = formId
  const mode = document.createElement('select')
  mode.required = true
  const liveOption = document.createElement('option')
  liveOption.value = 'live'; liveOption.textContent = '实时项目'
  const candidateOption = document.createElement('option')
  candidateOption.value = 'frozen-candidate'; candidateOption.textContent = '候选版本'
  mode.append(liveOption, candidateOption)
  const cwd = document.createElement('input')
  cwd.required = true; cwd.value = '.'; cwd.placeholder = '例如：.'; cwd.autocomplete = 'off'
  const argv = document.createElement('textarea')
  argv.required = true; argv.rows = 3; argv.value = 'pnpm\ndev'; argv.spellcheck = false
  const listenPort = document.createElement('input')
  listenPort.type = 'number'; listenPort.required = true; listenPort.min = '1'; listenPort.max = '65535'; listenPort.value = '3000'
  const healthPath = document.createElement('input')
  healthPath.required = true; healthPath.value = '/'; healthPath.placeholder = '例如：/health'; healthPath.autocomplete = 'off'
  const fields: Array<[string, HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement]> = [
    ['运行模式', mode], ['仓库相对工作目录', cwd], ['启动命令与参数（每行一个）', argv], ['监听端口', listenPort], ['健康检查路径', healthPath],
  ]
  for (const [labelText, input] of fields) {
    const label = element(document, 'label', 'wwc-projects-template-field')
    label.textContent = labelText
    label.append(input)
    form.append(label)
  }
  const error = element(document, 'p', 'wwc-projects-template-error')
  error.id = `${formId}-error`
  error.setAttribute('role', 'alert')
  error.hidden = true
  const note = element(document, 'p', 'wwc-projects-template-note')
  note.textContent = '启动设置保存在项目中；敏感环境变量不在此页面填写。'
  const actions = element(document, 'div', 'wwc-projects-template-actions')
  const save = element(document, 'button', 'wwc-projects-template-save')
  save.type = 'submit'; save.textContent = '保存运行设置'
  const status = element(document, 'p', 'wwc-projects-template-status')
  status.setAttribute('role', 'status')
  actions.append(save)
  form.append(error, note, actions, status)
  details.append(form)

  const controls = fields.map(([, input]) => input)
  function clearError(): void {
    error.hidden = true
    error.textContent = ''
    for (const control of controls) {
      control.removeAttribute('aria-invalid')
      control.removeAttribute('aria-describedby')
    }
  }
  function showError(message: string, invalidControls: readonly HTMLElement[] = []): void {
    error.textContent = message
    error.hidden = false
    for (const control of invalidControls) {
      control.setAttribute('aria-invalid', 'true')
      control.setAttribute('aria-describedby', error.id)
    }
    invalidControls[0]?.focus()
  }

  let loaded = false
  let busy = false
  async function load(): Promise<void> {
    if (loaded || busy) return
    busy = true
    clearError()
    status.textContent = '正在读取运行设置…'
    try {
      const snapshot = await templatePort.load(repository.repositoryBindingId)
      if (snapshot.template !== null) {
        mode.value = snapshot.template.mode
        cwd.value = snapshot.template.cwd
        argv.value = snapshot.template.argv.join('\n')
        listenPort.value = String(snapshot.template.listenPort)
        healthPath.value = snapshot.template.healthCheck.path
        status.textContent = `已加载运行设置 · 第 ${snapshot.revision ?? 1} 版`
      } else status.textContent = '尚未配置运行设置。'
      loaded = true
    } catch (loadError) {
      status.textContent = ''
      showError(loadError instanceof Error ? loadError.message : '运行设置读取失败，请稍后重试。')
    } finally { busy = false }
  }
  details.addEventListener('toggle', () => { if (details.open) void load() })
  form.addEventListener('submit', event => {
    event.preventDefault()
    if (busy) return
    clearError()
    const nativeInvalid = controls.filter(control => !control.checkValidity())
    if (nativeInvalid.length > 0) {
      showError('请修正标记的字段后再保存。', nativeInvalid)
      return
    }
    const relativeCwd = cwd.value.trim()
    const args = argv.value.split('\n').map(value => value.trim()).filter(Boolean)
    const portNumber = Number(listenPort.value)
    const customInvalid: Array<HTMLElement> = []
    const invalidLabels: string[] = []
    if (relativeCwd.startsWith('/') || relativeCwd.split('/').some(part => part === '..' || part.length === 0)) {
      customInvalid.push(cwd); invalidLabels.push('仓库相对工作目录')
    }
    if (args.length === 0) {
      customInvalid.push(argv); invalidLabels.push('启动命令')
    }
    if (!Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535) {
      customInvalid.push(listenPort); invalidLabels.push('监听端口')
    }
    if (!healthPath.value.trim().startsWith('/') || healthPath.value.includes('..')) {
      customInvalid.push(healthPath); invalidLabels.push('健康检查路径')
    }
    if (customInvalid.length > 0) {
      showError(`请修正：${invalidLabels.join('、')}。`, customInvalid)
      return
    }
    busy = true; save.disabled = true; status.textContent = '正在保存运行设置…'
    const template: ControlPlaneManagedAppTemplate = {
      schemaVersion: 'winwincode/managed-app-template-v1',
      mode: mode.value as ControlPlaneManagedAppTemplate['mode'],
      cwd: relativeCwd,
      argv: args,
      healthCheck: { path: healthPath.value.trim(), timeoutMs: 5000 },
      listenPort: portNumber,
    }
    void templatePort.save(repository.repositoryBindingId, template).then(revision => {
      loaded = true
      clearError()
      status.textContent = `运行设置已保存 · 第 ${revision} 版`
    }).catch(saveError => {
      status.textContent = ''
      showError(saveError instanceof Error ? saveError.message : '运行设置保存失败，请稍后重试。')
    }).finally(() => { busy = false; save.disabled = false })
  })
  return details
}

/**
 * Design page 07: 项目与仓库. One row per repository bound to the connected
 * execution device. Registration checks the selected device's local path
 * before exposing a project-specific Chat entry.
 */
export function renderProjectsPage(options: ProjectsPageOptions): ProjectsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-projects')
  layout.dataset.wwcPage = 'management'

  // Design page 07: the display title stands alone, the accent 添加仓库 entry
  // on the same row's right end.
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '项目',
      headingLevel: 2,
      className: 'wwc-projects-heading',
    },
  })
  const addButton = element(document, 'button', 'wwc-projects-add')
  addButton.type = 'button'
  addButton.textContent = '添加仓库'
  let closed = false
  let busy = false
  const controller = new AbortController()
  const form = element(document, 'form', 'wwc-projects-register')
  form.hidden = true
  const deviceSelect = document.createElement('select')
  deviceSelect.id = 'wwc-project-device'
  deviceSelect.required = true
  const deviceLabel = document.createElement('label')
  deviceLabel.htmlFor = deviceSelect.id
  deviceLabel.textContent = '执行设备'
  deviceLabel.append(deviceSelect)
  const path = document.createElement('input')
  path.id = 'wwc-project-path'
  path.required = true
  path.maxLength = 4096
  path.autocomplete = 'off'
  path.spellcheck = false
  const pathLabel = document.createElement('label')
  pathLabel.htmlFor = path.id
  pathLabel.textContent = '设备上的完整目录路径'
  pathLabel.append(path)
  const initialize = document.createElement('input')
  initialize.type = 'checkbox'
  const initializeLabel = document.createElement('label')
  initializeLabel.append(initialize, document.createTextNode('如果目录还不是 Git 仓库，允许初始化 Git'))
  const submit = document.createElement('button')
  submit.type = 'submit'
  submit.textContent = '检查并接入项目'
  const registrationStatus = document.createElement('p')
  registrationStatus.setAttribute('role', 'status')
  const startChat = document.createElement('a')
  startChat.textContent = '开始对话'
  startChat.hidden = true
  const connect = document.createElement('a')
  connect.href = options.deviceHref
  connect.textContent = '连接执行设备'
  const actions = element(document, 'div', 'wwc-projects-register-actions')
  actions.append(submit, connect)
  form.append(deviceLabel, pathLabel, initializeLabel, actions, registrationStatus, startChat)
  addButton.addEventListener('click', () => {
    form.hidden = !form.hidden
    addButton.setAttribute('aria-expanded', String(!form.hidden))
    if (!form.hidden) path.focus()
  })

  async function register(): Promise<void> {
    if (busy || !form.reportValidity()) return
    busy = true
    for (const control of [deviceSelect, path, initialize, submit]) control.disabled = true
    startChat.hidden = true
    registrationStatus.textContent = '正在检查设备上的目录、读写权限和 Git 状态…'
    const clientId = deviceSelect.value
    try {
      if (options.registration === undefined) throw new Error('项目接入服务不可用。')
      const view = await options.registration.readDevice(clientId, controller.signal)
      if (!view.online || view.snapshot === null) throw new Error('设备尚未就绪，请连接设备后重试。')
      const id = `repository_${(document.defaultView?.crypto ?? crypto).randomUUID().replaceAll('-', '')}`
      const envelope = await encryptDeviceRepository(view.snapshot, id, { path: path.value.trim(), confirmGitInit: initialize.checked }, document.defaultView?.crypto ?? crypto)
      await options.registration.submit(clientId, envelope, controller.signal)
      const deadline = Date.now() + 120_000
      while (!closed && Date.now() < deadline) {
        const result = await options.registration.receipt(clientId, id, controller.signal)
        if (closed) return
        if (result.receipt !== null && result.receipt !== undefined) {
          const receipt = result.receipt
          const labels: Record<string, string> = { invalid_git: '目录不是有效的 Git 仓库。可勾选允许初始化后重试。', permission_denied: '设备没有该目录的读写权限。', moved: '设备上找不到该目录。', scan_failed: 'Git 检查失败，请检查设备上的仓库。', unavailable: '目录不可用，请检查路径。', invalid_request: '设备拒绝了路径，请填写该设备上的完整目录路径。' }
          if (receipt.outcome !== 'registered') throw new Error(labels[receipt.outcome] ?? '项目接入失败。')
          await load()
          const repositories = await options.clientDirectory.listRepositories({ clientId })
          const repository = repositories.find(item => item.repositoryBindingId === receipt.repositoryBindingId)
          if (repository === undefined) throw new Error('设备已接入项目，但列表尚未更新，请刷新确认。')
          registrationStatus.textContent = `已接入 ${repositoryDisplayName(repository.displayName, repository.repositoryBindingId, document.defaultView)} · ${repository.defaultBranch} · ${repository.dirtyState === 'clean' ? '工作区干净' : '有未提交改动'}`
          startChat.href = options.newChatHref(clientId, repository)
          startChat.hidden = false
          path.value = ''
          return
        }
        if (result.online !== true) throw new Error('设备已离线，尚未收到检查结果。')
        await new Promise(resolve => setTimeout(resolve, 500))
      }
      if (!closed) registrationStatus.textContent = '尚未收到设备结果，请刷新项目列表确认。'
    } catch (error) {
      if (!closed) registrationStatus.textContent = '项目接入失败，请检查设备连接和目录后重试。'
    } finally {
      busy = false
      if (!closed) for (const control of [deviceSelect, path, initialize, submit]) control.disabled = false
    }
  }
  form.addEventListener('submit', event => { event.preventDefault(); void register() })

  const headerRow = element(document, 'div', 'wwc-projects-header')
  headerRow.append(pageHeader.root, addButton)

  const status = element(document, 'p', 'wwc-projects-status')
  status.setAttribute('role', 'status')
  status.textContent = '正在加载项目…'

  const list = element(document, 'ul', 'wwc-projects-list')
  const retry = element(document, 'button', 'wwc-projects-retry')
  retry.type = 'button'
  retry.textContent = '重试'
  retry.hidden = true
  retry.addEventListener('click', () => { void load() })

  function repoRow(client: ControlPlaneDeviceSummary, repo: ControlPlaneRepositorySummary): HTMLLIElement {
    const row = element(document, 'li', 'wwc-projects-row')
    const info = element(document, 'div', 'wwc-projects-row-info')
    const name = element(document, 'p', 'wwc-projects-row-name')
    const browser = document.defaultView
    const setName = (value = repositoryDisplayName(repo.displayName, repo.repositoryBindingId, browser)) => { name.textContent = value }
    setName()
    const source = element(document, 'p', 'wwc-projects-row-source')
    source.textContent = `${client.displayName} · ${repo.defaultBranch} · ${repo.dirtyState === 'clean' ? '工作区干净' : '有未提交改动'}`
    info.append(name, source)
    const newChat = element(document, 'a', 'wwc-projects-row-chat')
    newChat.href = options.newChatHref(client.clientId, repo)
    newChat.textContent = '新对话'
    const rename = element(document, 'button', 'wwc-projects-row-rename')
    rename.type = 'button'; rename.textContent = '改名'
    rename.addEventListener('click', () => {
      const input = element(document, 'input', 'wwc-projects-row-name-input')
      input.value = repositoryDisplayName(repo.displayName, repo.repositoryBindingId, browser)
      input.maxLength = 80; input.required = true; input.setAttribute('aria-label', '项目显示名称')
      const save = element(document, 'button', 'wwc-projects-row-name-save'); save.type = 'button'; save.textContent = '保存'
      const restore = element(document, 'button', 'wwc-projects-row-name-restore'); restore.type = 'button'; restore.textContent = '恢复原名'
      const cancel = element(document, 'button', 'wwc-projects-row-name-cancel'); cancel.type = 'button'; cancel.textContent = '取消'
      const help = element(document, 'span', 'wwc-projects-row-name-help'); help.textContent = '此名称保存在当前浏览器'
      const feedback = element(document, 'span', 'wwc-projects-row-name-feedback'); feedback.setAttribute('role', 'status')
      const editor = element(document, 'div', 'wwc-projects-row-name-editor'); editor.append(input, save, restore, cancel, help, feedback)
      name.replaceWith(editor); rename.hidden = true
      const finish = () => { editor.replaceWith(name); rename.hidden = false; rename.focus() }
      cancel.addEventListener('click', finish)
      restore.addEventListener('click', () => {
        try { clearRepositoryDisplayName(browser, repo.repositoryBindingId); setName(); finish() }
        catch { feedback.textContent = '保存失败，请重试。' }
      })
      save.addEventListener('click', () => {
        try { saveRepositoryDisplayName(browser, repo.repositoryBindingId, input.value); setName(); finish() }
        catch { feedback.textContent = '保存失败，请重试。' }
      })
      input.focus(); input.select()
    })
    const settings = mountManagedAppSettings(document, repo, options.managedAppTemplate)
    const main = element(document, 'div', 'wwc-projects-row-main')
    const actions = element(document, 'div', 'wwc-projects-row-actions'); actions.append(rename, newChat)
    main.append(info, actions)
    row.append(main, settings)
    return row
  }

  async function load(): Promise<void> {
    retry.hidden = true
    status.hidden = false
    status.textContent = '正在加载项目…'
    try {
      const clients = await options.clientDirectory.listClients(
        options.requestOptions?.(),
      )
      if (closed) return
      if (!busy) {
        const selected = deviceSelect.value
        deviceSelect.replaceChildren(...clients.map(client => {
          const option = document.createElement('option')
          option.value = client.clientId
          option.textContent = client.displayName
          return option
        }))
        if (clients.some(client => client.clientId === selected)) deviceSelect.value = selected
      }
      const rows: HTMLLIElement[] = []
      for (const client of clients) {
        const repos = await options.clientDirectory.listRepositories(
          { clientId: client.clientId },
          options.requestOptions?.(),
        )
        for (const repo of repos) rows.push(repoRow(client, repo))
      }
      status.hidden = true
      if (rows.length === 0) {
        status.hidden = false
        status.textContent = '还没有项目。点击“添加仓库”，选择设备并填写目录路径。'
      }
      list.replaceChildren(...rows)
    } catch (error) {
      status.hidden = false
      status.textContent = error instanceof ControlPlaneClientError && error.code === 'RESOURCE_NOT_FOUND'
        ? '当前服务器未提供设备仓库列表。请确认已启用远程执行设备服务。'
        : '项目列表读取失败。请检查执行设备连接后重试。'
      retry.hidden = false
    }
  }

  layout.append(headerRow, form, status, retry, list)
  options.root.replaceChildren(layout)
  void load()

  return {
    close() { closed = true; controller.abort(); path.value = '' },
  }
}
