#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

const canonicalTestFiles = Object.freeze([
  'tests/api-production-vertical-runner.test.mjs',
  'tests/api-production-device-prerequisites.test.mjs',
  'tests/network-request.test.mjs',
  'tests/network-diagnostics.test.mjs',
  'tests/diagnostics-boundary.test.mjs',
  'tests/device-diagnostics-export.test.mjs',
  'tests/device-agent-environment.test.mjs',
  'tests/device-task-runtime-unified.test.mjs',
  'tests/device-extension-mutation.test.mjs',
  'tests/approval-settlement-export.test.mjs',
  'tests/approval-settlement-runtime.test.mjs',
  'tests/benchmark-supervisor.test.mjs',
  'tests/native-fusion-benchmark.test.mjs',
  'tests/device-fixture-shutdown.test.mjs',
  'tests/device-task-runtime.test.mjs',
  'tests/device-provider-structured-output.test.mjs',
  'tests/device-provider-credentials.test.mjs',
  'tests/candidate-run-preview-ui.test.mjs',
  'tests/okqq-sidebar-board-wiring.test.mjs',
  'tests/wwx-gate-x3.test.mjs',
  'tests/wwx-gate-x4.test.mjs',
  'tests/page-annotation-ui.test.mjs',
  'tests/core-lock-verify.test.mjs',
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
  'tests/client-server-separation.test.mjs',
  'tests/codex-handoff-patch-replay.test.mjs',
  'tests/code-mode-cancellation-evidence.test.mjs',
  'tests/code-mode-patch-replay.test.mjs',
  'tests/code-mode-dispatch-patch-replay.test.mjs',
  'tests/code-mode-native-build.test.mjs',
  'tests/native-kernel-acceptance-runner.test.mjs',
  'tests/compact-kernel-helper.test.mjs',
  'tests/engineering-runtime-contract.test.mjs',
  'tests/contract-codegen.test.mjs',
  'tests/contextual-decision.test.mjs',
  'tests/contextual-decision-view-model.test.mjs',
  'tests/decision-clarification-ui.test.mjs',
  'tests/control-plane-client-facade.test.mjs',
  'tests/control-plane-http-contract.test.mjs',
  'tests/control-plane-websocket-contract.test.mjs',
  'tests/credential-leak-gate.test.mjs',
  'tests/data-export-semantic-conformance.test.mjs',
  'tests/delivery-execution-job-schema.test.mjs',
  'tests/delivery-submit-verdict-http-contract.test.mjs',
  'tests/domain-schema.test.mjs',
  'tests/execution-port-contract.test.mjs',
  'tests/home-dashboard-browser.test.mjs',
  'tests/home-dashboard-client.test.mjs',
  'tests/generated-control-plane-client.test.mjs',
  'tests/i18n-embed-fl-reproducibility.test.mjs',
  'tests/jev-session-replay.test.mjs',
  'tests/open-source-governance.test.mjs',
  'tests/pnpm-pack-report.test.mjs',
  'tests/query-cache.test.mjs',
  'tests/query-cache-view-model.test.mjs',
  'tests/repository-display-labels.test.mjs',
  'tests/readiness-application.test.mjs',
  'tests/readiness-browser.test.mjs',
  'tests/readiness-page.test.mjs',
  'tests/readiness-view-model.test.mjs',
  'tests/real-task-benchmark.test.mjs',
  'tests/benchmark-public-smoke.test.mjs',
  'tests/benchmark-device-adapter.test.mjs',
  'tests/real-task-benchmark-runner.test.mjs',
  'tests/publish-benchmark-submission.test.mjs',
  'tests/release-artifact-contract.test.mjs',
  'tests/release-artifact-security.test.mjs',
  'tests/rusqlite-savepoint-backport.test.mjs',
  'tests/scope-context.test.mjs',
  'tests/server-durable-event-hub-contract.test.mjs',
  'tests/session-identity-contract.test.mjs',
  'tests/settings-control-plane-integration.test.mjs',
  'tests/codex-login-settings.test.mjs',
  'tests/strongflow-canonical-api-contract.test.mjs',
  'tests/workrun-single-path-source-gate.test.mjs',
  'tests/strongflow-review-ui.test.mjs',
  'tests/strongflow-review-browser.test.mjs',
  'tests/strongflow-role.test.mjs',
  'tests/ui601-keyed-rendering-validation.test.mjs',
  'tests/ui604-a11y-audit.test.mjs',
  'tests/ui604-shell-a11y-browser.test.mjs',
  'tests/ui605-large-list-virtualization.test.mjs',
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

runTests(['--experimental-test-module-mocks', '--test', '--test-concurrency=1', ...canonicalTestFiles])

process.stdout.write(`canonical TypeScript tests passed: ${canonicalTestFiles.length}\n`)
