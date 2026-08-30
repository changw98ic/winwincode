import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { extname } from 'node:path'
import { gunzipSync } from 'node:zlib'

export const CREDENTIAL_LEAK_SCAN_SCHEMA_VERSION = 1

const maximumArchiveBytes = 512 * 1_024 * 1_024
const maximumDepth = 64
const sensitiveKeys = new Set([
  'apikey',
  'authorization',
  'credential',
  'credentials',
  'password',
  'passwd',
  'privatekey',
  'secret',
  'clientsecret',
  'token',
  'accesstoken',
  'refreshtoken',
  'idtoken',
  'sessiontoken',
  'vaultlocator',
  'credentiallocator',
  'providercredential',
  'secretmaterial',
])
const referenceKeys = new Set([
  'credentialreferenceid',
  'credentialreferenceids',
  'credentialref',
])
const safeLiterals = new Set([
  '',
  '[redacted]',
  '<redacted>',
  'redacted',
  'credential-reference',
  'reference-only',
  'dsh-reference-only',
])

export class CredentialLeakGateError extends Error {
  constructor(code, message, report) {
    super(message)
    this.name = 'CredentialLeakGateError'
    this.code = code
    this.report = report
  }
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function finding(label, rule, location = null) {
  return Object.freeze({
    label,
    rule,
    ...(location === null ? {} : { location }),
  })
}

function normalizedKey(key) {
  return key.replaceAll(/[^A-Za-z0-9]/gu, '').toLowerCase()
}

function safeLiteral(value) {
  return value === null
    || (typeof value === 'string' && safeLiterals.has(value.trim().toLowerCase()))
}

function inspectJsonValue(value, label, findings, path = '$', depth = 0) {
  if (depth > maximumDepth) {
    findings.push(finding(label, 'json.maximum-depth', path))
    return
  }
  if (Array.isArray(value)) {
    value.forEach((child, index) => inspectJsonValue(
      child,
      label,
      findings,
      `${path}[${String(index)}]`,
      depth + 1,
    ))
    return
  }
  if (typeof value !== 'object' || value === null) return
  for (const [index, [key, child]] of Object.entries(value).entries()) {
    const normalized = normalizedKey(key)
    const childPath = `${path}.field[${String(index)}]`
    if (normalized === 'secretstate') {
      if (!['available', 'revoked', 'missing', 'unavailable'].includes(child)) {
        findings.push(finding(label, 'json.forbidden-field', childPath))
      }
    } else if (!referenceKeys.has(normalized)
      && sensitiveKeys.has(normalized)
      && !safeLiteral(child)) {
      findings.push(finding(label, 'json.forbidden-field', childPath))
    }
    inspectJsonValue(child, label, findings, childPath, depth + 1)
  }
}

function inspectFingerprints(bytes, label, fingerprints, findings) {
  for (const fingerprint of fingerprints) {
    if (!Number.isSafeInteger(fingerprint?.bytes)
      || fingerprint.bytes < 1
      || !/^[0-9a-f]{64}$/u.test(fingerprint.sha256 ?? '')) {
      findings.push(finding(label, 'fingerprint.invalid'))
      continue
    }
    if (fingerprint.bytes > bytes.length) continue
    for (let offset = 0; offset <= bytes.length - fingerprint.bytes; offset += 1) {
      const candidate = bytes.subarray(offset, offset + fingerprint.bytes)
      if (sha256(candidate) === fingerprint.sha256) {
        findings.push(finding(label, 'fingerprint.exact-secret', `byte:${String(offset)}`))
        break
      }
    }
  }
}

function inspectRecognizedText(bytes, label, findings) {
  const text = bytes.toString('utf8')
  const rules = [
    ['text.private-key', /-----BEGIN (?:RSA |OPENSSH )?PRIVATE KEY-----/gu],
    ['text.bearer', /\bBearer\s+(?!\[REDACTED\]|<redacted>)[A-Za-z0-9._~+/=-]{12,}/giu],
    ['text.basic-auth', /\bBasic\s+[A-Za-z0-9+/]{12,}={0,2}/gu],
    ['text.jwt', /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/gu],
    ['text.provider-token', /\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_-]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,})\b/gu],
    ['text.url-userinfo', /\b(?:https?|wss?):\/\/[^/\s:@]+:[^/\s@]+@/giu],
    ['text.assignment', /\b(?:api[-_]?key|authorization|client[-_]?secret|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|token)(?:(?:\s*[=:]\s*["'](?!\[REDACTED\]|<redacted>|redacted\b)[A-Za-z0-9._~+/=-]{8,}["'])|(?:[=:](?!\[REDACTED\]|<redacted>|redacted\b)[A-Za-z0-9._~+/=-]{8,}))/giu],
  ]
  for (const [rule, pattern] of rules) {
    const match = pattern.exec(text)
    if (match !== null) findings.push(finding(label, rule, `char:${String(match.index)}`))
  }
}

function tarText(header, start, length) {
  return header
    .subarray(start, start + length)
    .toString('utf8')
    .replace(/\0.*$/u, '')
}

function inspectTar(bytes, label, fingerprints, findings) {
  let offset = 0
  let ended = false
  let entryIndex = 0
  while (offset + 512 <= bytes.length) {
    const header = bytes.subarray(offset, offset + 512)
    if (header.every(byte => byte === 0)) {
      ended = true
      break
    }
    const name = [tarText(header, 345, 155), tarText(header, 0, 100)]
      .filter(Boolean)
      .join('/')
    const sizeText = tarText(header, 124, 12).trim()
    const size = Number.parseInt(sizeText || '0', 8)
    const checksum = Number.parseInt(tarText(header, 148, 8).trim() || '0', 8)
    const computedChecksum = header.reduce((sum, byte, index) => (
      sum + (index >= 148 && index < 156 ? 32 : byte)
    ), 0)
    const type = header[156]
    const entryLabel = `${label}!/entry-${String(entryIndex)}-${sha256(Buffer.from(name)).slice(0, 16)}`
    if (name.startsWith('/')
      || name.split('/').includes('..')
      || !Number.isSafeInteger(size)
      || size < 0
      || checksum !== computedChecksum) {
      findings.push(finding(label, 'archive.invalid-entry'))
      return
    }
    const bodyStart = offset + 512
    const bodyEnd = bodyStart + size
    if (bodyEnd > bytes.length) {
      findings.push(finding(label, 'archive.truncated-entry', `entry:${String(entryIndex)}`))
      return
    }
    if (type === 0 || type === 48) {
      inspectBytes(bytes.subarray(bodyStart, bodyEnd), entryLabel, fingerprints, findings)
    } else if (type === 103 || type === 120) {
      inspectRecognizedText(bytes.subarray(bodyStart, bodyEnd), entryLabel, findings)
    } else if (type !== 53) {
      findings.push(finding(label, 'archive.unsupported-entry', `entry:${String(entryIndex)}`))
    }
    offset = bodyStart + Math.ceil(size / 512) * 512
    entryIndex += 1
  }
  if (!ended) findings.push(finding(label, 'archive.missing-terminator'))
}

function inspectBytes(bytes, label, fingerprints, findings) {
  inspectFingerprints(bytes, label, fingerprints, findings)
  inspectRecognizedText(bytes, label, findings)
  if (bytes.length >= 2 && bytes[0] === 0x1f && bytes[1] === 0x8b) {
    try {
      inspectTar(
        gunzipSync(bytes, { maxOutputLength: maximumArchiveBytes }),
        label,
        fingerprints,
        findings,
      )
    } catch {
      findings.push(finding(label, 'archive.invalid-gzip'))
    }
    return
  }
  if (extname(label.split('!/', 1)[0]).toLowerCase() === '.json'
    || extname(label).toLowerCase() === '.json') {
    try {
      inspectJsonValue(JSON.parse(bytes.toString('utf8')), label, findings)
    } catch {
      findings.push(finding(label, 'json.invalid'))
    }
  }
}

/** Deterministically scan bytes without returning matched Credential values. */
export function scanCredentialLeakBytes({
  bytes,
  label = 'output',
  fingerprints = [],
}) {
  if (!Buffer.isBuffer(bytes) && !(bytes instanceof Uint8Array)) {
    throw new TypeError('Credential leak scan bytes must be a Buffer or Uint8Array')
  }
  const input = Buffer.from(bytes)
  const findings = []
  inspectBytes(input, label, fingerprints, findings)
  const unique = [...new Map(findings.map(entry => [JSON.stringify(entry), entry])).values()]
    .toSorted((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right)))
  return Object.freeze({
    schemaVersion: CREDENTIAL_LEAK_SCAN_SCHEMA_VERSION,
    label,
    status: unique.length === 0 ? 'passed' : 'rejected',
    bytes: input.length,
    sha256: sha256(input),
    findings: Object.freeze(unique),
  })
}

export function scanCredentialLeakFile(path, options = {}) {
  return scanCredentialLeakBytes({
    ...options,
    bytes: readFileSync(path),
    label: options.label ?? path,
  })
}

export function assertCredentialLeakFreeFile(path, options = {}) {
  const report = scanCredentialLeakFile(path, options)
  if (report.status !== 'passed') {
    throw new CredentialLeakGateError(
      'CREDENTIAL_LEAK_DETECTED',
      `Credential leak gate rejected ${report.label}`,
      report,
    )
  }
  return report
}
