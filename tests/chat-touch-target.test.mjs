// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { chromeBinary, DevTools, evaluate, freePort, stopChild } from './fixtures/real-browser-harness.mjs'

// A stylesheet component check, not a substitute for the full Chat browser flow.
test('Chat session buttons render a 48px touch target with keyboard focus', async t => {
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
  const css = ['tokens.css', 'features/chat.css'].map(path =>
    readFileSync(new URL(`../apps/client/src/styles/${path}`, import.meta.url), 'utf8')).join('\n')
  await evaluate(devtools, sessionId, `(() => {
    const style = document.createElement('style'); style.textContent = ${JSON.stringify(css)};
    document.head.append(style);
    document.body.innerHTML = '<div class="wwc-chat-session-list"><button>会话</button></div>';
  })()`)
  await devtools.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 }, sessionId)
  await devtools.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 }, sessionId)
  const result = await evaluate(devtools, sessionId, `(() => {
    const button = document.querySelector('button'); const style = getComputedStyle(button);
    return { height: button.getBoundingClientRect().height, focused: document.activeElement === button,
      outlineWidth: parseFloat(style.outlineWidth), outlineStyle: style.outlineStyle };
  })()`)
  assert.ok(result.height >= 48, `button height is ${result.height}px, expected at least 48px`)
  assert.equal(result.focused, true)
  assert.ok(result.outlineWidth > 0 && result.outlineStyle !== 'none', 'visible keyboard focus')
})
