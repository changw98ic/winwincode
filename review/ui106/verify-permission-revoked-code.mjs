// UI-106 review repro: observeControlPlaneClient passes 'PERMISSION_REVOKED'
// which is not in connection-state PUBLIC_CODES, so safeCode substitutes
// CLIENT_FAILURE. A stable permission-revocation code never reaches the UI.
// Run from the candidate worktree root (ui-workbench-xhigh) with:
//   node <review-worktree>/review/ui106/verify-permission-revoked-code.mjs
import assert from 'node:assert/strict'

const cache = new URL('./.cache/ui-components-tests/core/connection-state.js', `file://${process.argv[2] ?? '/Volumes/ORICO/winwincode-worktrees/ui-workbench-xhigh'}/`)
const { createConnectionMonitor, observeControlPlaneClient } = await import(cache)

const monitor = createConnectionMonitor()
let rawOptions = null
const rawClient = {
  serverUrl: 'https://control.localhost',
  async restore() { return {} },
  async login() { return {} },
  async logout() {},
  async command() { throw new Error('unused') },
  async query() { throw new Error('unused') },
  subscribe(options) {
    rawOptions = options
    return { cursor: null, resume() {}, reconnect() {}, close() {} }
  },
  close() {},
}
const observed = observeControlPlaneClient({ client: rawClient, monitor, online: () => true })
const subscription = observed.client.subscribe({
  subscriptionId: 'sub_00000000000000000000000001',
  subscription: {},
  async onEvent() {},
})
await rawOptions.onAuthorizationRevoked(null)

assert.equal(monitor.state.status, 'permission-denied')
console.log('status:', monitor.state.status)
console.log('code: ', monitor.state.code)
assert.notEqual(monitor.state.code, 'PERMISSION_REVOKED')
console.log('CONFIRMED: PERMISSION_REVOKED is rewritten to', monitor.state.code)
subscription.close()
monitor.close()
