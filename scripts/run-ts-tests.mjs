#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

// The TypeScript lane exercises the current Client and generated-contract
// boundaries.  Product-process checks (Rust, Server API, and release assets)
// have their own root scripts so this lane stays deterministic and does not
// start a second process-boundary build.
const canonicalTestFiles = Object.freeze([
  'tests/api-production-vertical-runner.test.mjs',
  'tests/architecture-documentation.test.mjs',
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
  'tests/delivery-rust-cutover-gate.test.mjs',
  'tests/delivery-submit-verdict-http-contract.test.mjs',
  'tests/domain-schema.test.mjs',
  'tests/enterprise-application.test.mjs',
  'tests/enterprise-management-view-model.test.mjs',
  'tests/enterprise-operations-page.test.mjs',
  'tests/enterprise-policy-contract.test.mjs',
  'tests/enterprise-resource-page.test.mjs',
  'tests/execution-port-contract.test.mjs',
  'tests/generated-control-plane-client.test.mjs',
  'tests/i18n-embed-fl-reproducibility.test.mjs',
  'tests/local-decisions-client.test.mjs',
  'tests/local-operations-client.test.mjs',
  'tests/open-source-governance.test.mjs',
  'tests/pnpm-pack-report.test.mjs',
  'tests/query-cache.test.mjs',
  'tests/query-cache-view-model.test.mjs',
  'tests/readme-quickstart.test.mjs',
  'tests/release-artifact-contract.test.mjs',
  'tests/release-artifact-security.test.mjs',
  'tests/rust-format-gate.test.mjs',
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
  'tests/strongflow-history.test.mjs',
  'tests/strongflow-page.test.mjs',
  'tests/strongflow-projection-contract.test.mjs',
  'tests/strongflow-role.test.mjs',
  'tests/strongflow-view-model.test.mjs',
  'tests/strongflow-workflow-integration.test.mjs',
  'tests/ui601-keyed-rendering-validation.test.mjs',
  'tests/ui601-strongflow-event-reload.test.mjs',
  'tests/workspace-smoke.test.mjs',
])

for (const path of canonicalTestFiles) {
  if (!existsSync(join(root, path))) {
    throw new Error(`canonical TypeScript test is missing: ${path}`)
  }
}

const result = spawnSync(process.execPath, [
  '--test',
  '--test-concurrency=1',
  ...canonicalTestFiles,
], {
  cwd: root,
  stdio: 'inherit',
})
if (result.error !== undefined) throw result.error
if (result.signal !== null) {
  throw new Error(`canonical TypeScript test runner ended with ${result.signal}`)
}
if (result.status !== 0) process.exit(result.status ?? 1)

process.stdout.write(`canonical TypeScript tests passed: ${canonicalTestFiles.length}\n`)
