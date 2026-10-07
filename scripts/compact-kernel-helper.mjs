import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { chmodSync, copyFileSync, statSync } from 'node:fs'
import { resolve } from 'node:path'

const MAX_HELPER_BYTES = 64 * 1024 * 1024

// Development products use the same authenticated image limit as releases.
export function prepareCompactKernelHelper({ root, targetDirectory, environment, offline = false }) {
  const helper = resolve(targetDirectory, 'debug/winwincode-kernel-helper')
  if (statSync(helper).size > MAX_HELPER_BYTES) {
    const args = ['build', '--release', '-p', 'winwincode-kernel-helper', '--locked']
    if (offline) args.push('--offline')
    const result = spawnSync('cargo', args, { cwd: root, env: environment, stdio: 'inherit' })
    if (result.error !== undefined) throw result.error
    assert.equal(result.status, 0, 'compact Kernel helper build failed')
    const compact = resolve(targetDirectory, 'release/winwincode-kernel-helper')
    assert.ok(statSync(compact).size <= MAX_HELPER_BYTES, 'Kernel helper exceeds the authenticated image limit')
    copyFileSync(compact, helper)
  }
  chmodSync(helper, 0o755)
  return helper
}
