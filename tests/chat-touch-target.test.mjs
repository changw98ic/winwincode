// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { chromeBinary, DevTools, evaluate, freePort, stopChild } from './fixtures/real-browser-harness.mjs'

// A stylesheet component check, not a substitute for the full Chat browser flow.
test('Sidebar session links render a 48px touch target with keyboard focus', async t => {
  const chromePath = chromeBinary()
  assert.ok(chromePath, 'Chrome or Chromium is required')
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-touch-'))
  let browser
  t.after(async () => {
    browser?.devtools.close()
    if (browser) await stopChild(browser.chrome, 'SIGTERM')
    rmSync(directory, { recursive: true, force: true })
  })
  browser = await DevTools.launch({ chromePath, directory, debugPort: await freePort() })
  const { devtools } = browser
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true })
  const css = ['dsh-tokens.css', 'tokens.css', 'base.css', 'shell.css'].map(path =>
    readFileSync(new URL(`../apps/client/src/styles/${path}`, import.meta.url), 'utf8')).join('\n')
  await evaluate(devtools, sessionId, `(() => {
    const style = document.createElement('style'); style.textContent = ${JSON.stringify(css)};
    document.head.append(style);
    document.body.innerHTML = '<div class="wwc-session-browser"><ul class="wwc-sidebar-recent"><li class="wwc-sidebar-recent-item"><a class="wwc-sidebar-recent-item-link" href="#/chat"><span class="wwc-sidebar-recent-item-text">会话</span><small>项目</small></a></li></ul></div>';
  })()`)
  await devtools.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 }, sessionId)
  await devtools.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 }, sessionId)
  const result = await evaluate(devtools, sessionId, `(() => {
    const link = document.querySelector('a'); const style = getComputedStyle(link);
    return { height: link.getBoundingClientRect().height, focused: document.activeElement === link,
      outlineWidth: parseFloat(style.outlineWidth), outlineStyle: style.outlineStyle };
  })()`)
  assert.ok(result.height >= 48, `link height is ${result.height}px, expected at least 48px`)
  assert.equal(result.focused, true)
  assert.ok(result.outlineWidth > 0 && result.outlineStyle !== 'none', 'visible keyboard focus')
})
