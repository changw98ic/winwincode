// SPDX-License-Identifier: Apache-2.0
// Theme validation adapted from DeepSeek Harness (c) 2026 DeepSeek, MIT.

const THEME_PREFERENCES = ['light', 'dark', 'system'] as const
export type ThemePreference = typeof THEME_PREFERENCES[number]
export interface Preferences {
  displayName: string
  appearance: ThemePreference
  sendKey: 'enter' | 'mod-enter'
}
export const PREFERENCES_KEY = 'winwincode.preferences'
const changed = 'wwc:preferences-changed'

export function isThemePreference(value: unknown): value is ThemePreference {
  return THEME_PREFERENCES.some(preference => preference === value)
}

export function readPreferences(browser: Window | null): Preferences {
  let value: Partial<Preferences> = {}
  try { value = JSON.parse(browser?.localStorage.getItem(PREFERENCES_KEY) ?? '{}') ?? {} } catch { /* Browser storage may be unavailable. */ }
  return {
    displayName: typeof value.displayName === 'string' ? value.displayName.slice(0, 80) : '',
    appearance: isThemePreference(value.appearance) ? value.appearance : 'system',
    sendKey: value.sendKey === 'mod-enter' ? 'mod-enter' : 'enter',
  }
}

export function savePreferences(browser: Window | null, value: Preferences): void {
  if (browser === null) throw new Error('Browser storage unavailable')
  browser.localStorage.setItem(PREFERENCES_KEY, JSON.stringify(value))
  browser.dispatchEvent(new Event(changed))
}

export function mountPreferences(browser: Window, document: Document): () => void {
  const system = browser.matchMedia?.('(prefers-color-scheme: dark)')
  const apply = () => {
    const { appearance } = readPreferences(browser)
    const dark = appearance === 'dark' || (appearance === 'system' && system?.matches === true)
    document.documentElement?.setAttribute('data-theme', dark ? 'dark' : 'light')
  }
  apply()
  browser.addEventListener(changed, apply)
  browser.addEventListener('storage', apply)
  system?.addEventListener('change', apply)
  return () => {
    browser.removeEventListener(changed, apply)
    browser.removeEventListener('storage', apply)
    system?.removeEventListener('change', apply)
  }
}
