// SPDX-License-Identifier: Apache-2.0

// Secret-safe diagnostics for the first-run browser vertical.  The browser
// fixture returns one unfiltered observation of what the run did (including the
// bootstrap proof it typed and the planted vault locator), and this module is
// the only place that turns that observation into an on-disk artifact.  Every
// value is either an identifier, a known safe word, a number, or a boolean;
// everything else is replaced, so a failed run leaves a bounded artifact that
// can be shared without leaking material.

import { createHash } from 'node:crypto'

import { scanCredentialLeakBytes } from '../../scripts/credential-leak-gate.mjs'

export const FIRST_RUN_DIAGNOSTIC_SCHEMA_VERSION = 'winwincode.first-run-strongflow-browser.v1'

const IDENTIFIER = /^[a-z]{3}_[0-9A-HJKMNP-TV-Z]{26}$/u
const CONTRACT_IDENTIFIER = /^(?:req|sub|psn|dlv|run|crd|org|wsp|prj|rep|usr|evt|msg|wrk|job|cdx|wsn|pub|apr)_[0-9A-HJKMNP-TV-Z]{26}$/u
const CONTRACT_NAME = /^[a-z][a-z0-9]*(?:\.[a-z][a-z0-9]*)+$/u
const BASELINE_REVISION = /^[0-9a-f]{40}$/u
const CRITERION_ID = /^criterion:[0-9]+$/u
// Keys the canonical credential gate treats as sensitive; their value never
// reaches the artifact even when the value itself looks harmless.
const SENSITIVE_KEYS = new Set([
  'apikey',
  'authorization',
  'clientsecret',
  'credential',
  'credentials',
  'credentiallocator',
  'password',
  'passwd',
  'privatekey',
  'providercredential',
  'secret',
  'secretmaterial',
  'token',
  'vaultlocator',
])
const SAFE_WORDS = new Set([
  'available',
  'cancelled',
  'clarifying',
  'completed',
  'delivery',
  'disabled',
  'draft',
  'enabled',
  'failed',
  'idle',
  'missing',
  'organization',
  'pending',
  'product-session',
  'project',
  'ready',
  'repository',
  'revoked',
  'running',
  'unavailable',
  'user',
  'workspace',
])
const BOUNDED_TEXT = 240
const MAXIMUM_DEPTH = 5

/** Fingerprint the exact secret bytes so the canonical leak gate can detect them. */
export function secretFingerprints(values) {
  return values.map(value => {
    const bytes = Buffer.from(value)
    return { bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') }
  })
}

function safeScalar(value) {
  if (typeof value === 'number' || typeof value === 'boolean' || value === null) return value
  if (typeof value !== 'string') return `typeof:${typeof value}`
  if (CONTRACT_IDENTIFIER.test(value) || CONTRACT_NAME.test(value)) return value
  if (BASELINE_REVISION.test(value) || CRITERION_ID.test(value)) return value
  if (IDENTIFIER.test(value)) return value
  return SAFE_WORDS.has(value) ? value : '<redacted>'
}

function summarize(value, depth = 0, key = '') {
  if (depth > MAXIMUM_DEPTH) return '<bounded>'
  if (SENSITIVE_KEYS.has(key.replaceAll(/[^a-z]/gu, ''))) return '<redacted>'
  if (Array.isArray(value)) return { length: value.length }
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([childKey, child]) => [
      childKey,
      Array.isArray(child) ? { length: child.length } : summarize(child, depth + 1, childKey),
    ]))
  }
  return safeScalar(value)
}

function summarizeCalls(calls) {
  return (Array.isArray(calls) ? calls : []).slice(-40).map(call => ({
    requestId: safeScalar(call?.requestId),
    ...(call?.command === undefined ? {} : { command: safeScalar(call.command) }),
    ...(call?.query === undefined ? {} : { query: safeScalar(call.query) }),
    ...(call?.expectedRevision === undefined ? {} : {
      expectedRevision: safeScalar(call.expectedRevision),
    }),
    scope: summarize(call?.scope ?? null),
    ...(call?.payload === undefined ? {} : { payload: summarize(call.payload) }),
    ...(call?.parameters === undefined ? {} : { parameters: summarize(call.parameters) }),
    ...(call?.subscription === undefined ? {} : { subscription: summarize(call.subscription) }),
  }))
}

function boundedLines(value) {
  return (Array.isArray(value) ? value : [])
    .map(line => String(line))
    .map(line => line.length > BOUNDED_TEXT ? line.slice(0, BOUNDED_TEXT) : line)
    .slice(-40)
}

function normalizedUrl(value, clientOrigin) {
  const text = String(value ?? '')
  return typeof clientOrigin === 'string' && clientOrigin.length > 0
    ? text.replaceAll(clientOrigin, 'CLIENT_ORIGIN')
    : text
}

/**
 * Build the on-disk diagnostic from one unfiltered browser observation.
 * The observation may contain secret material; the result may not.  Free text
 * (failure messages, console lines) is bounded and scrubbed against the exact
 * secret values the run knows about.
 */
export function buildFirstRunDiagnostic({
  phase,
  failure = null,
  observation,
  clientOrigin,
  assertions = [],
  secretValues = [],
}) {
  const secrets = secretValues.filter(value => typeof value === 'string' && value.length > 0)
  function scrub(value) {
    return secrets.reduce((text, secret) => text.replaceAll(secret, '[redacted]'), String(value))
  }
  return {
    schemaVersion: FIRST_RUN_DIAGNOSTIC_SCHEMA_VERSION,
    phase: scrub(phase),
    ...(failure === null ? {} : { failure: { message: scrub(failure).slice(0, 400) } }),
    page: {
      url: scrub(normalizedUrl(observation?.page?.url, clientOrigin)),
      hash: observation?.page?.hash === undefined ? null : scrub(observation.page.hash),
      title: observation?.page?.title === undefined ? null : scrub(observation.page.title),
    },
    identity: summarize(observation?.identity ?? null),
    workspace: summarize(observation?.workspace ?? null),
    commands: summarizeCalls(observation?.commands),
    queries: summarizeCalls(observation?.queries),
    subscriptions: summarizeCalls(observation?.subscriptions),
    console: boundedLines(observation?.console).map(scrub),
    assertions: assertions.map(line => scrub(line)),
  }
}

/** Scan one artifact with the canonical credential leak gate plus exact-secret fingerprints. */
export function scanFirstRunDiagnostic(bytes, { label, secretValues }) {
  return scanCredentialLeakBytes({
    bytes,
    label,
    fingerprints: secretFingerprints(secretValues),
  })
}
