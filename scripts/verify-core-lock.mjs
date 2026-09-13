#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

/**
 * REPO-SPLIT-000.3: verify a Community core lock before a consumer build.
 *
 * Schema-valid locks can still be operationally wrong. This verifier enforces
 * the contract rules beyond JSON Schema: one core version, matching sourceTag,
 * protocol winwincode/v1, and optional byte/digest checks against a local
 * artifact directory. Any failure exits non-zero so the consumer build stops.
 */

import { createHash } from 'node:crypto'
import { readFileSync, existsSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import Ajv2020 from 'ajv/dist/2020.js'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
export const CORE_LOCK_SCHEMA_PATH = join(root, 'schema/winwincode/core-lock.schema.json')
export const CORE_PROTOCOL_VERSION = 'winwincode/v1'
export const CORE_LOCK_KIND = 'winwincode.community-core-lock.v1'

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function loadSchemaValidator() {
  const schema = JSON.parse(readFileSync(CORE_LOCK_SCHEMA_PATH, 'utf8'))
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  return { schema, validate: ajv.compile(schema) }
}

function collectArtifacts(core) {
  const artifacts = []
  const push = (label, artifact) => {
    if (isRecord(artifact)) artifacts.push({ label, artifact })
  }
  push('core.releaseManifest', core.releaseManifest)
  for (const [index, item] of (core.cargo ?? []).entries()) {
    push(`core.cargo[${String(index)}]`, item)
  }
  for (const [index, item] of (core.npm ?? []).entries()) {
    push(`core.npm[${String(index)}]`, item)
  }
  push('core.contracts', core.contracts)
  for (const [index, item] of (core.workerRuntimes ?? []).entries()) {
    push(`core.workerRuntimes[${String(index)}]`, item)
  }
  push('core.sbom', core.sbom)
  push('core.licenseManifest', core.licenseManifest)
  return artifacts
}

function sha256Hex(buffer) {
  return createHash('sha256').update(buffer).digest('hex')
}

/**
 * Beyond-schema rules from ADR-0031 coreLockManifest.validationRulesBeyondSchema.
 * Returns { ok, errors }. Never mutates the lock.
 */
export function verifyCoreLock(lock, options = {}) {
  const errors = []
  if (!isRecord(lock)) {
    return { ok: false, errors: ['core lock must be a JSON object'] }
  }
  const { validate } = loadSchemaValidator()
  if (!validate(lock)) {
    for (const error of validate.errors ?? []) {
      errors.push(`schema:${error.instancePath || '/'} ${error.message ?? 'invalid'}`)
    }
  }
  const core = lock.core
  if (!isRecord(core)) {
    return { ok: false, errors }
  }

  if (core.kind !== undefined && lock.kind !== CORE_LOCK_KIND) {
    errors.push(`kind must be ${CORE_LOCK_KIND}`)
  }
  if (core.protocolVersion !== CORE_PROTOCOL_VERSION) {
    errors.push(`core.protocolVersion must be ${CORE_PROTOCOL_VERSION}`)
  }
  if (typeof core.version === 'string' && core.sourceTag !== `core-v${core.version}`) {
    errors.push('core.sourceTag must equal core-v followed by core.version')
  }

  const versions = new Set()
  const names = []
  for (const { label, artifact } of collectArtifacts(core)) {
    if (typeof artifact.version === 'string') versions.add(artifact.version)
    if (typeof artifact.name === 'string') names.push({ label, name: artifact.name })
    if (typeof core.version === 'string' && artifact.version !== core.version) {
      errors.push(`${label}.version must equal core.version`)
    }
    if (typeof artifact.uri === 'string' && !artifact.uri.startsWith('https://')) {
      errors.push(`${label}.uri must be https`)
    }
  }
  if (versions.size > 1) {
    errors.push(`mixed core versions are rejected: ${[...versions].sort().join(', ')}`)
  }

  // Optional local artifact verification: bytes and sha256 must both match.
  if (typeof options.artifactDir === 'string') {
    const dir = options.artifactDir
    for (const { label, artifact } of collectArtifacts(core)) {
      if (typeof artifact.uri !== 'string') continue
      const fileName = decodeURIComponent(artifact.uri.split('/').at(-1) ?? '')
      if (fileName.length === 0) continue
      const localPath = join(dir, fileName)
      if (!existsSync(localPath)) {
        errors.push(`${label}: missing local artifact ${fileName}`)
        continue
      }
      const bytes = readFileSync(localPath)
      if (statSync(localPath).isFile() === false) {
        errors.push(`${label}: local artifact is not a file`)
        continue
      }
      if (typeof artifact.bytes === 'number' && bytes.length !== artifact.bytes) {
        errors.push(`${label}: byte length mismatch`)
      }
      if (typeof artifact.sha256 === 'string' && sha256Hex(bytes) !== artifact.sha256) {
        errors.push(`${label}: sha256 mismatch`)
      }
    }
  }

  return { ok: errors.length === 0, errors }
}

/**
 * Upgrade rule: a lock update replaces the complete core object. Mixing one
 * package from an older core into a newer lock is rejected.
 */
export function verifyCoreLockUpgrade(previous, next) {
  const errors = []
  if (!isRecord(previous) || !isRecord(next)) {
    return { ok: false, errors: ['both locks must be JSON objects'] }
  }
  const previousVersion = previous.core?.version
  const nextVersion = next.core?.version
  if (typeof previousVersion === 'string' && typeof nextVersion === 'string') {
    if (previousVersion === nextVersion) {
      // Same version: digests of shared package names must not drift.
      const previousByName = new Map(
        collectArtifacts(previous.core).map(({ artifact }) => [artifact.name, artifact]),
      )
      for (const { label, artifact } of collectArtifacts(next.core)) {
        const before = previousByName.get(artifact.name)
        if (
          before !== undefined
          && typeof before.sha256 === 'string'
          && typeof artifact.sha256 === 'string'
          && before.sha256 !== artifact.sha256
        ) {
          errors.push(`${label}: same core version cannot change artifact digest`)
        }
      }
    }
  }
  const nextCheck = verifyCoreLock(next)
  return { ok: errors.length === 0 && nextCheck.ok, errors: [...errors, ...nextCheck.errors] }
}

function parseArgv(argv) {
  const values = {}
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (!argument.startsWith('--')) throw new Error(`unexpected argument ${argument}`)
    const key = argument.slice(2)
    const value = argv[index + 1]
    if (value === undefined || value.startsWith('--')) throw new Error(`--${key} requires a value`)
    values[key] = value
    index += 1
  }
  if (values.lock === undefined) throw new Error('--lock is required')
  return values
}

function main() {
  const values = parseArgv(process.argv.slice(2))
  const lockPath = resolve(root, values.lock)
  const lock = JSON.parse(readFileSync(lockPath, 'utf8'))
  const result = verifyCoreLock(lock, {
    ...(values['artifact-dir'] === undefined
      ? {}
      : { artifactDir: resolve(root, values['artifact-dir']) }),
  })
  if (!result.ok) {
    process.stderr.write(`${JSON.stringify({
      ok: false,
      lockPath,
      protocolVersion: CORE_PROTOCOL_VERSION,
      errors: result.errors,
    }, null, 2)}\n`)
    process.exitCode = 1
    return
  }
  process.stdout.write(`${JSON.stringify({
    ok: true,
    lockPath,
    consumerRepository: lock.consumerRepository,
    coreVersion: lock.core.version,
    protocolVersion: lock.core.protocolVersion,
    sourceTag: lock.core.sourceTag,
  }, null, 2)}\n`)
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main()
}
