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
const canonicalTestFiles = Object.freeze([
  'tests/api-production-vertical-runner.test.mjs',
  'tests/architecture-documentation.test.mjs',
  'tests/auth-session-client.test.mjs',
  'tests/chat-control-plane-integration.test.mjs',
  'tests/chat-page.test.mjs',
  'tests/chat-view-model.test.mjs',
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
  'tests/generated-control-plane-client.test.mjs',
  'tests/i18n-embed-fl-reproducibility.test.mjs',
  'tests/local-decisions-client.test.mjs',
  'tests/local-operations-client.test.mjs',
  'tests/open-source-governance.test.mjs',
  'tests/pnpm-pack-report.test.mjs',
  'tests/readme-quickstart.test.mjs',
  'tests/release-artifact-contract.test.mjs',
  'tests/release-artifact-security.test.mjs',
  'tests/server-durable-event-hub-contract.test.mjs',
  'tests/session-identity-contract.test.mjs',
  'tests/settings-control-plane-integration.test.mjs',
  'tests/strongflow-canonical-api-contract.test.mjs',
  'tests/strongflow-delivery-advance-contract.test.mjs',
  'tests/strongflow-delivery-api.test.mjs',
  'tests/strongflow-projection-contract.test.mjs',
  'tests/strongflow-role.test.mjs',
  'tests/strongflow-view-model.test.mjs',
  'tests/strongflow-workflow-integration.test.mjs',
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
