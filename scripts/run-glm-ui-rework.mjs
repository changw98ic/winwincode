#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdirSync, writeFileSync, readFileSync, readdirSync } from 'node:fs'
import { resolve, join } from 'node:path'
import { runApiProductionVertical, workItemCreatePayload } from './run-api-production-vertical.mjs'

const directory = resolve('test-results/glm-ui-rework', new Date().toISOString().replaceAll(':', '-'))
mkdirSync(directory, { recursive: true, mode: 0o700 })
const deliveryId = 'dlv_01J00000000000000000000001'
const targetPath = 'apps/client/src/styles/features/chat.css'
const sourcePaths = execFileSync('git', ['ls-files', '-z', '--',
  'apps/client/src', 'apps/client/public', 'packages/browser-ui/src',
  'packages/browser-core/src'], { encoding: 'utf8' }).split('\0').filter(Boolean)
const files = Object.fromEntries(sourcePaths.map(path => [path, readFileSync(path, 'utf8')]))
// An explicit captured baseline permits replay after the generated CSS was applied.
// It changes only the verification checkout, never the application's source file.
const replayArguments = process.argv.slice(2)
assert.ok(replayArguments.length === 0 || (replayArguments.length === 2 && replayArguments[0] === '--baseline-css'), 'Usage: run-glm-ui-rework.mjs [--baseline-css captured-before-chat.css]')
if (replayArguments.length) files[targetPath] = readFileSync(resolve(replayArguments[1]), 'utf8')
for (const path of ['tests/chat-touch-target.test.mjs', 'tests/fixtures/real-browser-harness.mjs']) {
  files[path] = readFileSync(path, 'utf8')
}
files['package.json'] = JSON.stringify({ private: true, type: 'module', scripts: { verify: 'node --test tests/chat-touch-target.test.mjs' } })
const baselineCss = files[targetPath]
assert.ok(baselineCss?.includes('.wwc-chat'), 'The existing Chat source is required')
const report = { model: process.env.ZHIPU_MODEL, baselineCssSource: replayArguments[1] ?? targetPath, steps: [], complete: false }
const save = () => writeFileSync(join(directory, 'result.json'), JSON.stringify(report, null, 2))
function preservePages(root) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (entry.name.startsWith('.') || entry.name === 'provider-secrets' || entry.name === 'publication-secrets') continue
    const path = join(root, entry.name)
    if (entry.isDirectory()) preservePages(path)
    else if (path.endsWith('/' + targetPath)) {
      const content = readFileSync(path, 'utf8')
      if (content !== baselineCss) writeFileSync(join(directory, 'after-chat.css'), content)
    }
  }
}
for (const name of ['ZHIPU_API_KEY', 'ZHIPU_BASE_URL', 'ZHIPU_MODEL']) assert.ok(process.env[name], `${name} is required`)
const url = new URL(process.env.ZHIPU_BASE_URL)
assert.equal(url.protocol, 'https:')
assert.ok(!url.username && !url.password && !url.search && !url.hash)
const endpoint = url.href.replace(/\/$/u, '') + '/v1/messages'
writeFileSync(join(directory, 'before-chat.css'), baselineCss)
writeFileSync(join(directory, 'source-paths.json'), JSON.stringify(sourcePaths, null, 2))
try {
  await runApiProductionVertical({
    directory,
    restart: false,
    repeat: false,
    serverEnvironment: {
      WWC_SERVER_EXECUTION_LEASE_SECONDS: '600',
      WWC_DEBUG_RUNTIME: '1',
      WWC_DEBUG_RUNTIME_LOG: join(directory, 'runtime.log'),
      WWC_SERVER_MODEL_PROVIDER_ID: 'zhipu-glm',
      WWC_SERVER_MODEL_ID: process.env.ZHIPU_MODEL,
      WWC_SERVER_MODEL_ANTHROPIC_ENDPOINT: endpoint,
      WWC_SERVER_MODEL_API_KEY: process.env.ZHIPU_API_KEY,
    },
    timeoutMillis: 600_000,
    scenario: {
      files,
      async run({ api, baseline }) {
        const get = async () => (await api.query('delivery.get', { deliveryId })).result
        const aggregate = async () => (await api.query('workrun.get', { deliveryId, workItemId: null, atCursor: null })).result
        const command = async (name, payload) => {
          const current = await get()
          const result = await api.command(name, current.deliveryRevision, payload)
          assert.equal(result.outcome, 'completed', name)
          report.steps.push({ command: name, revision: result.currentRevision }); save()
          return result
        }
        const created = await api.command('delivery.create', 0, {
          deliveryId, spec: {
            title: 'GLM 页面优化与精确返工验证',
            goal: '完成现有 DSH Chat 页面的一项小型 UI 优化：会话列表按钮在手机上触控面积偏小。只编辑 apps/client/src/styles/features/chat.css，将现有 .wwc-chat-session-list button 基础规则中的 min-height 调整为 calc(var(--wwc-control-min-height) + var(--wwc-space-1))，即48px。不要重复添加声明，保留其他样式和所有交互，不创建 HTML、不安装依赖、不改 TypeScript。一次读取目标 CSS 及 tokens.css 确认变量后，用 Python 原地修改，运行 git --no-pager diff --check 和 git --no-pager diff 查看唯一变更，然后直接结束并汇报。环境没有 apply_patch 命令。不要启动开发服务器、浏览器或后台进程，页面验证由调用方完成。',
            scope: [targetPath], constraints: ['只修改现有 Chat CSS；保留会话、模型选择、发送取消、转交 StrongFlow 等功能和样式变量'], outOfScope: ['其他文件、依赖、业务逻辑'],
            baseRevision: baseline, repositoryId: 'rep_01J00000000000000000000000', publicationTarget: null, sourceProductSessionId: null,
            acceptanceCriteria: [{ id: 'ui-layout', required: true, title: '会话列表按钮浏览器中会话按钮高度至少 48px，键盘焦点可见；运行仓库 verify 脚本测量当前候选的实际样式。360px/1440px 完整 Chat 交互检查由调用方另行执行并保存证据。' }],
          },
        })
        assert.equal(created.outcome, 'completed')
        const payload = workItemCreatePayload(await aggregate(), created.currentRevision)
        payload.items[0].title = '优化现有 DSH Chat 页面'
        payload.items[0].goal = (await aggregate()).contract.objective
        assert.ok(payload.items[0].goal.includes(targetPath), 'The accepted page goal must reach WorkContract')
        await command('workitems.create', payload)
        await command('workrun.start', { deliveryId, dispatchProfile: 'executor', rework: null })
        const deadline = Date.now() + 600_000
        let runs
        do {
          try {
            runs = await aggregate()
            report.workRun = runs; report.delivery = await get()
          } catch (error) {
            if (error.message !== 'HTTP request timed out') throw error
            report.lastPollingError = error.message
          }
          preservePages(directory); save()
          if (!runs) continue
          if (runs.runs.some(run => ['failed', 'cancelled'].includes(run.state))) throw Error('GLM execution failed; see result.json')
          if (runs.runs.some(run => ['candidate_ready', 'settled'].includes(run.state))) break
          await new Promise(r => setTimeout(r, 1000))
        } while (Date.now() < deadline)
        preservePages(directory); save()
        assert.ok(runs.runs.some(run => ['candidate_ready', 'settled'].includes(run.state)), 'GLM writer timed out')
        for (const profile of ['reviewer', 'verifier']) {
          const previous = new Set(runs.runs.map(run => run.id))
          await command('workrun.start', { deliveryId, dispatchProfile: profile, rework: null })
          const deadline = Date.now() + 600_000
          let consumer
          do {
            try {
              runs = await aggregate()
              consumer = runs.runs.find(run => !previous.has(run.id))
              report.workRun = runs; report.delivery = await get(); save()
            } catch (error) {
              if (error.message !== 'HTTP request timed out') throw error
              report.lastPollingError = error.message; save()
            }
            if (consumer && ['settled', 'failed', 'cancelled'].includes(consumer.state)) break
            await new Promise(r => setTimeout(r, 1000))
          } while (Date.now() < deadline)
          assert.equal(consumer?.state, 'settled', `${profile} must settle its own read-only run`)
        }
        const candidate = (await get()).currentCandidate
        assert.ok(candidate, 'A real frozen candidate is required')
        await command('delivery.submit_verdict', { deliveryId, candidateDigest: candidate.candidateRef.replace('git-candidate:', '') })
        report.delivery = await get(); report.verdictSubmitted = true; report.verificationComplete = report.delivery.verdict?.status === 'pass'; save()
        assert.equal(report.delivery.verdict?.status, 'pass', 'Independent checks must support a passing verdict')
        throw Error('Independent verification completed; precise rework and continuation remain unverified')
      },
    },
  })
} catch (error) {
  report.error = String(error.message).replaceAll(process.env.ZHIPU_API_KEY, '<redacted>')
  preservePages(directory)
  save()
  console.error(JSON.stringify({ directory, complete: report.complete, error: report.error }))
  process.exitCode = 1
}
