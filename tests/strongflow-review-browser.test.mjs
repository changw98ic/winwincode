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

  assert.doesNotMatch(result.initialVisible, /wrn_|psn_|evd_|git-candidate|criterion-one/u)
  assert.match(result.initialVisible, /修复加法计算并保留现有调用方式/u)
  assert.match(result.initialVisible, /修正 sum 函数.*验证正数与负数相加/su)
  assert.doesNotMatch(result.initialVisible, /判定说明与证据|运行日志|预算 2 次/u)
  assert.deepEqual(result.initialTabs.map(tab => tab.label), ['任务内容', '代码变更 2', '验收结果 1/2'])
  assert.equal(result.initialTabs[0].selected, 'true')
  assert.equal(result.acceptanceVisible, true)
  assert.equal(result.taskHidden, true)
  const removed = result.diff.find(line => line.kind === 'removed')
  const added = result.diff.find(line => line.kind === 'added')
  assert.notEqual(removed.background, added.background)
  assert.notEqual(added.background, 'rgba(0, 0, 0, 0)')
  assert.deepEqual([removed.before, removed.after, added.before, added.after], ['1', '', '', '1'])
  assert.match(removed.text, /a - b/u)
  assert.match(added.text, /a \+ b/u)
  assert.equal(result.expandedAfterRefresh, true, 'opening evidence must preserve the acceptance explanation')
  assert.equal(result.fileClass, 'executable-document')
  assert.match(result.fileDegraded, /不与管理界面同源渲染/u)
  assert.equal(result.artifactClass, 'executable-document')
  assert.match(result.artifactDegraded, /不与管理界面同源执行/u)
  assert.equal(result.scriptCount, 0)
  assert.equal(result.pwned, false)
  assert.equal(result.citation, '错误引用已绑定当前执行记录')
  assert.equal(result.evidenceResult, '已核验执行结果：失败')
  assert.match(result.history, /不能授权当前候选/u)
  assert.equal(result.currentAuthorization, 'false')
  assert.match(result.progress, /执行计划（模型工作步骤）.*共 2 项.*已完成 1 项.*进行中 1 项/u)
  assert.match(result.progress, /验收条件.*共 2 项.*通过 1 项.*未判定 1 项/u)
  assert.match(result.progress, /已发生返工 0 次 · 预算 2 次/u)
  assert.doesNotMatch(result.progress, /%/u)
  assert.deepEqual(result.criteria.map(criterion => [criterion.id, criterion.result]), [
    ['1', 'pass'],
    ['2', 'pending'],
  ])
  assert.match(result.criteria[0].text, /命令执行 · 失败 · 附件可用 · 1 个附件/u)
  assert.match(result.criteria[0].text, /查看第 1 条证据/u)
  assert.match(result.criteria[1].text, /尚未获得该项的独立验证结果/u)
  assert.match(result.report, /剩余风险.*Review passes：未判定/u)
  assert.match(result.solution, /请检查目标、执行范围.*包含 1 项执行任务/u)
  assert.match(result.solution, /批准方案.*要求修改.*拒绝方案/u)
  assert.equal(result.solutionStatus, 'approved')
  assert.match(result.solutionSettled, /审核已结束.*审核意见已记录/u)
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
