// SPDX-License-Identifier: Apache-2.0

/** Names are optional presentation data; machine identities stay in API values. */
export const REPOSITORY_NAMES_KEY = 'winwincode.repository-names'
export const REPOSITORY_NAME_CHANGED_EVENT = 'wwc:repository-name-changed'

function readRepositoryNames(browser: Window | null): Record<string, string> {
  if (browser === null) return {}
  try {
    const value: unknown = JSON.parse(browser.localStorage.getItem(REPOSITORY_NAMES_KEY) ?? '{}')
    if (value === null || typeof value !== 'object' || Array.isArray(value)) return {}
    return Object.fromEntries(Object.entries(value).filter(([key, label]) => (
      key.trim() !== '' && typeof label === 'string' && label.trim() !== ''
    )))
  } catch { return {} }
}

export function repositoryDisplayName(name: string, bindingId?: string, browser?: Window | null): string {
  const localName = bindingId === undefined ? undefined : readRepositoryNames(browser ?? null)[bindingId]
  return localName ?? (/^rep_[0-9A-HJKMNP-TV-Z]{26}$/u.test(name) || name.trim() === '' ? '项目名称未设置' : name)
}

export function saveRepositoryDisplayName(browser: Window | null, bindingId: string, name: string): void {
  if (browser === null) throw new Error('当前浏览器存储不可用。')
  const value = name.trim()
  if (value.length === 0 || value.length > 80) throw new Error('项目名称需为 1 到 80 个字符。')
  const names = readRepositoryNames(browser)
  names[bindingId] = value
  browser.localStorage.setItem(REPOSITORY_NAMES_KEY, JSON.stringify(names))
  browser.dispatchEvent(new Event(REPOSITORY_NAME_CHANGED_EVENT))
}

export function clearRepositoryDisplayName(browser: Window | null, bindingId: string): void {
  if (browser === null) throw new Error('当前浏览器存储不可用。')
  const names = readRepositoryNames(browser)
  delete names[bindingId]
  browser.localStorage.setItem(REPOSITORY_NAMES_KEY, JSON.stringify(names))
  browser.dispatchEvent(new Event(REPOSITORY_NAME_CHANGED_EVENT))
}

export function attentionTitle(title: string): string {
  return ({
    'Verification infrastructure must be retried': '验证环境异常，需要重试',
    'Independent verification findings conflict': '审查与验证结论不一致，需要核查',
    'Delivery definition requires clarification': '需要确认任务范围与验收条件',
    'Verification evidence is incomplete': '验收证据不足，需要核查',
    'Acceptance criterion requires bounded rework': '验收未通过，需要修复',
    'Approve the verified candidate': '确认已通过验收的版本',
  } as Readonly<Record<string, string>>)[title] ?? title
}
