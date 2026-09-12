#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

// The TypeScript lane exercises the current Client and generated-contract
// boundaries.  Product-process checks (Rust build, Server API, and release
// assets) have their own root scripts so this lane stays deterministic and
// does not start a second process-boundary build.
//
// The real-browser suites each rebuild the client into the shared
// `apps/client/dist` tree, and this lane runs files concurrently, so the
// browser harness waits out another suite's rebuild instead of failing on a
// momentarily missing asset (see tests/fixtures/real-browser-harness.mjs).
const canonicalTestFiles = Object.freeze([
  'tests/api-production-vertical-runner.test.mjs',
  'tests/architecture-documentation.test.mjs',
  'tests/attention-notifications-client.test.mjs',
  'tests/auth-session-client.test.mjs',
  'tests/chat-control-plane-integration.test.mjs',
  'tests/chat-page.test.mjs',
  'tests/chat-view-model.test.mjs',
  'tests/client-clients.test.mjs',
  'tests/client-control-contract.test.mjs',
  'tests/client-editable-draft.test.mjs',
  'tests/client-keyed-collection.test.mjs',
  'tests/client-login.test.mjs',
  'tests/client-repositories.test.mjs',
  'tests/my-work-ui.test.mjs',
  'tests/task-entry-ui.test.mjs',
  'tests/backup-restore.test.mjs',
  'tests/my-work-clients-consistency.test.mjs',
  'tests/client-occupancy-card-detail.test.mjs',
  'tests/browser-control-packages.test.mjs',
  'tests/browser-ui-package.test.mjs',
  'tests/client-users-facade.test.mjs',
  'tests/client-server-separation.test.mjs',
  'tests/community-persistence-ports.test.mjs',
  'tests/engineering-runtime-backlog.test.mjs',
  'tests/engineering-runtime-design.test.mjs',
  'tests/engineering-runtime-contract.test.mjs',
  'tests/contract-codegen.test.mjs',
  'tests/contextual-decision.test.mjs',
  'tests/contextual-decision-view-model.test.mjs',
  'tests/control-plane-api-coverage.test.mjs',
  'tests/control-plane-client-facade.test.mjs',
  'tests/control-plane-http-contract.test.mjs',
  'tests/control-plane-web-client-preflight.test.mjs',
  'tests/control-plane-websocket-contract.test.mjs',
  'tests/credential-leak-gate.test.mjs',
  'tests/data-export-semantic-conformance.test.mjs',
  'tests/delivery-evidence-verdict-rework-contract.test.mjs',
  'tests/delivery-execution-job-schema.test.mjs',
  'tests/delivery-submit-verdict-http-contract.test.mjs',
  'tests/domain-schema.test.mjs',
  'tests/execution-port-contract.test.mjs',
  'tests/home-dashboard-browser.test.mjs',
  'tests/home-dashboard-client.test.mjs',
  'tests/generated-control-plane-client.test.mjs',
  'tests/i18n-embed-fl-reproducibility.test.mjs',
  'tests/open-source-governance.test.mjs',
  'tests/product-repository-boundary.test.mjs',
  'tests/pnpm-pack-report.test.mjs',
  'tests/query-cache.test.mjs',
  'tests/query-cache-view-model.test.mjs',
  'tests/readiness-application.test.mjs',
  'tests/readiness-browser.test.mjs',
  'tests/readiness-page.test.mjs',
  'tests/readiness-view-model.test.mjs',
  'tests/readme-quickstart.test.mjs',
  'tests/release-artifact-contract.test.mjs',
  'tests/release-artifact-security.test.mjs',
  'tests/rusqlite-savepoint-backport.test.mjs',
  'tests/scope-context.test.mjs',
  'tests/scope-selector-application.test.mjs',
  'tests/scope-selector-browser.test.mjs',
  'tests/scope-selector-page.test.mjs',
  'tests/scope-selector-view-model.test.mjs',
  'tests/server-durable-event-hub-contract.test.mjs',
  'tests/session-identity-contract.test.mjs',
  'tests/settings-control-plane-integration.test.mjs',
  'tests/strongflow-canonical-api-contract.test.mjs',
  'tests/workrun-single-path-source-gate.test.mjs',
  'tests/strongflow-projection-contract.test.mjs',
  'tests/strongflow-review-ui.test.mjs',
  'tests/strongflow-review-browser.test.mjs',
  'tests/strongflow-role.test.mjs',
  'tests/ui601-keyed-rendering-validation.test.mjs',
  'tests/ui604-a11y-audit.test.mjs',
  'tests/ui604-shell-a11y-browser.test.mjs',
  'tests/ui605-large-list-virtualization.test.mjs',
  // 用户裁定(2026-09-10):界面与设计稿的差异一律以真实渲染 + 视觉评审判断,
  // 不再做指纹基线式的自动化样式断言。ui608 三个泳道文件保留在仓库作参考,
  // 不再进入默认测试清单(bd winwincode-oq7r 收口记录)。
  'tests/usage-health-browser.test.mjs',
  'tests/usage-health-client.test.mjs',
  'tests/workspace-smoke.test.mjs',
])

for (const path of canonicalTestFiles) {
  if (!existsSync(join(root, path))) {
    throw new Error(`canonical TypeScript test is missing: ${path}`)
  }
}

function runTests(arguments_) {
  const result = spawnSync(process.execPath, arguments_, {
    cwd: root,
    stdio: 'inherit',
  })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null) {
    throw new Error(`Node test runner ended with ${result.signal}`)
  }
  if (result.status !== 0) process.exit(result.status ?? 1)
}

runTests(['--test', '--test-concurrency=4', ...canonicalTestFiles])

process.stdout.write(`canonical TypeScript tests passed: ${canonicalTestFiles.length}\n`)
