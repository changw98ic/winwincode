import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  certificate, chromeBinary, closeServer, command, DevTools, evaluate,
  freePort, listen, staticClientServer, stopChild, waitForGlobal,
} from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')

test('a real browser reviews bounded artifacts without executing HTML or SVG', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-review-'))
  const server = staticClientServer({
    root,
    certificateFiles: certificate(root, directory),
    fixturePath: 'tests/fixtures/browser-strongflow-review.mjs',
    configuration: () => ({}),
  })
  const origin = `https://client.localhost:${String(await listen(server))}`
  let chrome = null
  let devtools = null
  t.after(async () => {
    devtools?.close()
    await Promise.all([...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]), closeServer(server)])
    rmSync(directory, { recursive: true, force: true })
  })
  const launched = await DevTools.launch({ chromePath, directory, debugPort: await freePort() })
  chrome = launched.chrome
  devtools = launched.devtools
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Security.setIgnoreCertificateErrors', { ignore: true }, sessionId)
  await devtools.send('Page.navigate', { url: origin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'reviewReady')
  const result = await evaluate(devtools, sessionId, 'globalThis.exerciseReview()')

  assert.equal(result.fileClass, 'executable-document')
  assert.match(result.fileDegraded, /不与管理界面同源渲染/u)
  assert.equal(result.artifactClass, 'executable-document')
  assert.match(result.artifactDegraded, /不与管理界面同源执行/u)
  assert.equal(result.scriptCount, 0)
  assert.equal(result.pwned, false)
  assert.match(result.citation, /runtime:command:42/u)
  assert.match(result.history, /不能授权当前候选/u)
  assert.equal(result.currentAuthorization, 'false')
  assert.match(result.progress, /执行计划（模型工作步骤）.*共 2 项.*已完成 1 项.*进行中 1 项/u)
  assert.match(result.progress, /验收条件（Controller 判定）.*共 2 项.*通过 1 项.*未判定 1 项/u)
  assert.match(result.progress, /已发生返工 0 次 · 预算 2 次/u)
  assert.doesNotMatch(result.progress, /%/u)
  assert.deepEqual(result.criteria.map(criterion => [criterion.id, criterion.result]), [
    ['criterion-one', 'pass'],
    ['criterion-two', 'pending'],
  ])
  assert.match(result.criteria[0].text, /command.*WorkRun/u)
  assert.match(result.criteria[1].text, /未执行、未映射都不是通过/u)
  assert.match(result.report, /剩余风险.*criterion-two：未判定/u)
  assert.match(result.report, /自动检查只证明对应断言/u)
  assert.match(result.solution, /为什么需要你：.*自动检查已完成/u)
  assert.match(result.solution, /批准方案.*要求修改.*拒绝方案/u)
  assert.equal(result.solutionStatus, 'approved')
  assert.match(result.solutionSettled, /审核已结束.*历史页面不能再次授权/u)
  assert.equal(result.solutionActionsDisabled, true)
  assert.equal(result.decision.length, 1)
  assert.equal(result.decision[0].command, 'delivery.resolve_attention')
  assert.equal(result.decision[0].expectedRevision, 2)
  assert.equal(result.decision[0].payload.resolution.action, 'approve')
  assert.equal(result.decision[0].payload.resolution.reviewSetSha256, `sha256:${'a'.repeat(64)}`)
  assert.deepEqual(result.downloads[0], {
    fileName: 'report.html',
    text: '<script>globalThis.pwned=true</script>review html',
    mediaType: 'text/html',
  })
  assert.match(result.downloads[1].fileName, /^delivery-dlv_.*-report\.txt$/u)
  assert.match(result.downloads[1].text, /2\. required; result=pending; evidence=0; verification=unmapped/u)
  assert.doesNotMatch(result.downloads[1].text, /Tests pass|Review passes|runtime:command/u)
  assert.ok(result.queryNames.includes('evidence.artifact.content.get'))
})
