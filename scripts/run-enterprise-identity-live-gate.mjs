// SPDX-License-Identifier: Apache-2.0

import { accessSync, constants, statSync } from 'node:fs'
import { spawnSync } from 'node:child_process'

const required = Object.freeze([
  'WINWINCODE_LIVE_IDENTITY_VERIFIER_ENDPOINT',
  'WINWINCODE_LIVE_IDENTITY_METADATA_DIRECTORY',
  'WINWINCODE_LIVE_IDENTITY_SECRET_DIRECTORY',
  'WINWINCODE_LIVE_IDENTITY_TLS_ROOT_DER_FILE',
  'WINWINCODE_LIVE_IDENTITY_VERIFIER_CREDENTIAL_REFERENCE_ID',
  'WINWINCODE_LIVE_IDENTITY_ORGANIZATION_ID',
  'WINWINCODE_LIVE_OIDC_ID_TOKEN_FILE',
  'WINWINCODE_LIVE_SAML_RESPONSE_FILE',
  'WINWINCODE_LIVE_SCIM_BEARER_FILE',
  'WINWINCODE_LIVE_IDENTITY_EVIDENCE_FILE',
])

const fileInputs = Object.freeze([
  'WINWINCODE_LIVE_IDENTITY_TLS_ROOT_DER_FILE',
  'WINWINCODE_LIVE_OIDC_ID_TOKEN_FILE',
  'WINWINCODE_LIVE_SAML_RESPONSE_FILE',
  'WINWINCODE_LIVE_SCIM_BEARER_FILE',
])

const directoryInputs = Object.freeze([
  'WINWINCODE_LIVE_IDENTITY_METADATA_DIRECTORY',
  'WINWINCODE_LIVE_IDENTITY_SECRET_DIRECTORY',
])

const present = required.filter((name) => typeof process.env[name] === 'string' && process.env[name].length > 0)
const missing = required.filter((name) => !present.includes(name))

if (missing.length > 0) {
  process.stdout.write(`${JSON.stringify({
    schema: 'winwincode.enterprise-identity-live-gate-preflight.v1',
    status: 'blocked',
    present,
    missing,
  })}\n`)
  process.exitCode = 2
} else {
  const invalid = []
  for (const name of fileInputs) {
    try {
      accessSync(process.env[name], constants.R_OK)
      if (!statSync(process.env[name]).isFile()) invalid.push(name)
    } catch {
      invalid.push(name)
    }
  }
  for (const name of directoryInputs) {
    try {
      accessSync(process.env[name], constants.R_OK | constants.W_OK)
      if (!statSync(process.env[name]).isDirectory()) invalid.push(name)
    } catch {
      invalid.push(name)
    }
  }
  if (invalid.length > 0) {
    process.stdout.write(`${JSON.stringify({
      schema: 'winwincode.enterprise-identity-live-gate-preflight.v1',
      status: 'blocked',
      present,
      invalid: [...new Set(invalid)].sort(),
    })}\n`)
    process.exitCode = 2
  } else {
    const result = spawnSync('cargo', [
      'test',
      '-p',
      'winwincode-control-plane',
      '--test',
      'enterprise_identity_verification',
      '--locked',
      'live_real_oidc_saml_scim_verifier_gate_requires_explicit_file_backed_credentials',
      '--',
      '--ignored',
      '--exact',
    ], {
      cwd: new URL('..', import.meta.url),
      env: process.env,
      stdio: 'inherit',
    })
    if (result.error) {
      process.stderr.write('enterprise identity live gate process failed to start\n')
      process.exitCode = 1
    } else {
      process.exitCode = result.status ?? 1
    }
  }
}
