#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

// The TypeScript lane exercises the current Client and generated-contract
// boundaries.  Product-process checks (Rust build, Server API, and release
// assets) have their own root scripts so this lane stays deterministic and
// does not start a second process-boundary build.  The oracle gates at the
// end are Node-authored; the trigger-aware differential executes the already
// built integration target only when its trigger paths exist.
//
// The real-browser suites each rebuild the client into the shared
// `apps/client/dist` tree, and this lane runs files concurrently, so the
// browser harness waits out another suite's rebuild instead of failing on a
// momentarily missing asset (see tests/fixtures/real-browser-harness.mjs).
const canonicalTestFiles = Object.freeze([
  'tests/api-production-vertical-runner.test.mjs',
  'tests/architecture-documentation.test.mjs',
  'tests/attention-center-client.test.mjs',
  'tests/auth-session-client.test.mjs',
  'tests/chat-control-plane-integration.test.mjs',
  'tests/chat-page.test.mjs',
  'tests/chat-view-model.test.mjs',
  'tests/client-editable-draft.test.mjs',
  'tests/client-keyed-collection.test.mjs',
  'tests/client-server-separation.test.mjs',
  'tests/contract-codegen.test.mjs',
  'tests/control-plane-api-coverage.test.mjs',
  'tests/control-plane-client-facade.test.mjs',
  'tests/control-plane-http-contract.test.mjs',
  'tests/control-plane-web-client-preflight.test.mjs',
  'tests/control-plane-websocket-contract.test.mjs',
  'tests/credential-leak-gate.test.mjs',
  'tests/delivery-evidence-verdict-rework-contract.test.mjs',
  'tests/delivery-execution-job-schema.test.mjs',
  'tests/delivery-submit-verdict-http-contract.test.mjs',
  'tests/domain-schema.test.mjs',
  'tests/enterprise-application.test.mjs',
  'tests/enterprise-management-view-model.test.mjs',
  'tests/enterprise-operations-page.test.mjs',
  'tests/enterprise-policy-contract.test.mjs',
  'tests/enterprise-resource-page.test.mjs',
  'tests/execution-port-contract.test.mjs',
  'tests/first-run-strongflow-browser.test.mjs',
  'tests/generated-control-plane-client.test.mjs',
  'tests/i18n-embed-fl-reproducibility.test.mjs',
  'tests/local-decisions-client.test.mjs',
  'tests/local-operations-client.test.mjs',
  'tests/open-source-governance.test.mjs',
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
  'tests/scope-context.test.mjs',
  'tests/scope-selector-application.test.mjs',
  'tests/scope-selector-browser.test.mjs',
  'tests/scope-selector-page.test.mjs',
  'tests/scope-selector-view-model.test.mjs',
  'tests/server-durable-event-hub-contract.test.mjs',
  'tests/session-identity-contract.test.mjs',
  'tests/settings-control-plane-integration.test.mjs',
  'tests/strongflow-canonical-api-contract.test.mjs',
  'tests/strongflow-candidate-files.test.mjs',
  'tests/strongflow-diff-viewer.test.mjs',
  'tests/strongflow-diagram-graph.test.mjs',
  'tests/strongflow-delivery-advance-contract.test.mjs',
  'tests/strongflow-delivery-api.test.mjs',
  'tests/strongflow-delivery-list-page.test.mjs',
  'tests/strongflow-delivery-list-view-model.test.mjs',
  'tests/strongflow-deep-link.test.mjs',
  'tests/strongflow-execution-graph.test.mjs',
  'tests/strongflow-history.test.mjs',
  'tests/strongflow-evidence-browser.test.mjs',
  'tests/strongflow-evidence.test.mjs',
  'tests/strongflow-header.test.mjs',
  'tests/strongflow-header-review-matrix.test.mjs',
  'tests/strongflow-page.test.mjs',
  'tests/strongflow-projection-contract.test.mjs',
  'tests/strongflow-realtime-state-browser.test.mjs',
  'tests/strongflow-role.test.mjs',
  'tests/strongflow-view-model.test.mjs',
  'tests/strongflow-workflow-integration.test.mjs',
  'tests/ui601-keyed-rendering-validation.test.mjs',
  'tests/ui601-strongflow-event-reload.test.mjs',
  'tests/ui604-a11y-audit.test.mjs',
  'tests/ui604-shell-a11y-browser.test.mjs',
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

// The legacy ten-scenario oracle runs after the parallel Node suite and
// reuses the TypeScript build already produced by test:ts.
runTests(['scripts/export-delivery-strongflow-oracle.mjs', '--check'])

// The trigger-aware Rust differential runs last so the generated contract
// and its Rust producer stay one checked path.  Without its trigger paths
// it validates the frozen plan contract-only instead of running Cargo.
runTests(['scripts/run-delivery-strongflow-rust-differential.mjs', '--check'])

process.stdout.write(`canonical TypeScript tests passed: ${canonicalTestFiles.length}\n`)
