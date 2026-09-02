const CREDENTIAL_VALUE_KEY = /^(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)$/iu
const CREDENTIAL_ASSIGNMENT = /\b(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s,;]+))/giu
const CREDENTIAL_PROPERTY = /(?:(?:^|[{,]\s*)(?:"[^"]+"|'[^']+')|(?:[{,]\s*)[A-Za-z0-9_-]+)\s*:\s*(?:"([^"]*)"|'([^']*)'|(\[[^\]]+\]|[^\s,}\]]+))/giu
const PRIVATE_KEY = /-----BEGIN [^-\r\n]*PRIVATE KEY-----[\s\S]*?-----END [^-\r\n]*PRIVATE KEY-----/u
const JSON_WEB_TOKEN = /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/u
const BEARER_VALUE = /\bBearer\s+(?!\[REDACTED\])[A-Za-z0-9._~+/=-]+/iu
const BASIC_AUTH_VALUE = /\bBasic\s+[A-Za-z0-9+/]{12,}={0,2}\b/iu
const PROVIDER_SECRET_VALUE = /\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,})\b/u
const URL_USERINFO_VALUE = /\b(?:https?|wss?):\/\/[^/\s:@]+:[^/\s@]+@/iu
const SAFE_CREDENTIAL_LITERAL = /^(?:\[REDACTED(?: [A-Z ]+)?\]|<redacted>|redacted|null|undefined|none|dsh-reference-only|credential-reference|reference-only)$/iu
const CREDENTIAL_PLACEHOLDER = /^(?:\$\{?|<)?(?:api_?key|apikey|auth(?:orization)?|credential|password|private_?key|secret|token)(?:\}|>)?$/iu

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function safeCredentialLiteral(value: unknown): boolean {
  if (value === null || value === undefined || value === false || value === '') return true
  return typeof value === 'string'
    && (SAFE_CREDENTIAL_LITERAL.test(value.trim()) || CREDENTIAL_PLACEHOLDER.test(value.trim()))
}

function capturedCredentialValue(match: RegExpMatchArray): string {
  return match[1] ?? match[2] ?? match[3] ?? ''
}

function textContainsCredentialMaterial(
  value: string,
  sensitiveValues: readonly string[],
): boolean {
  if (sensitiveValues.some(sensitive => sensitive.length > 0 && value.includes(sensitive))) {
    return true
  }
  if (PRIVATE_KEY.test(value)
    || JSON_WEB_TOKEN.test(value)
    || BEARER_VALUE.test(value)
    || BASIC_AUTH_VALUE.test(value)
    || PROVIDER_SECRET_VALUE.test(value)
    || URL_USERINFO_VALUE.test(value)) return true
  for (const match of value.matchAll(CREDENTIAL_ASSIGNMENT)) {
    if (!safeCredentialLiteral(capturedCredentialValue(match))) return true
  }
  const propertyText = value.replaceAll(/\bBearer\s+\[REDACTED\]/giu, '[REDACTED]')
  for (const match of propertyText.matchAll(CREDENTIAL_PROPERTY)) {
    const property = match[0].split(':', 1)[0]
      ?.replace(/^[{,]\s*/u, '')
      .replaceAll(/["']/gu, '')
      .trim()
    if (property !== undefined
      && CREDENTIAL_VALUE_KEY.test(property)
      && !safeCredentialLiteral(capturedCredentialValue(match))) return true
  }
  return false
}

/** Detect raw secrets before a Delivery fact can enter durable storage or a response. */
export function containsRawCredentialMaterial(
  value: unknown,
  sensitiveValues: readonly string[] = [],
): boolean {
  const seen = new WeakSet<object>()
  const walk = (input: unknown, key = '', depth = 0): boolean => {
    if (depth > 64) return true
    if (CREDENTIAL_VALUE_KEY.test(key) && !safeCredentialLiteral(input)) return true
    if (typeof input === 'string') {
      return textContainsCredentialMaterial(input, sensitiveValues)
    }
    if (typeof input === 'number'
      || typeof input === 'boolean'
      || typeof input === 'bigint'
      || input === null
      || input === undefined) return false
    if (typeof input !== 'object') return true
    if (input instanceof Uint8Array) {
      return textContainsCredentialMaterial(Buffer.from(input).toString('utf8'), sensitiveValues)
    }
    if (seen.has(input)) return true
    seen.add(input)
    if (Array.isArray(input)) return input.some(entry => walk(entry, '', depth + 1))
    if (!isRecord(input)) return true
    return Object.entries(input).some(([childKey, child]) => walk(child, childKey, depth + 1))
  }
  return walk(value)
}
