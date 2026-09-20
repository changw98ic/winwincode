// SPDX-License-Identifier: Apache-2.0

/**
 * Browser-facing redaction boundary. The credential patterns mirror the
 * StrongFlow credential-boundary package; this extra pass is required because
 * runtime artifacts are not proven to have been redacted by their producer.
 */
const PRIVATE_KEY = /-----BEGIN [^-\r\n]*PRIVATE KEY-----[\s\S]*?-----END [^-\r\n]*PRIVATE KEY-----/gu
const JWT = /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/gu
const BEARER = /\bBearer\s+(?!\[REDACTED\])[A-Za-z0-9._~+/=-]+/giu
const BASIC = /\bBasic\s+[A-Za-z0-9+/]{12,}={0,2}\b/giu
const PROVIDER_SECRET = /\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,})\b/gu
const URL_CREDENTIALS = /\b(?:https?|wss?):\/\/[^/\s:@]+:[^/\s@]+@/giu
const CREDENTIAL_ASSIGNMENT = /\b(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)\s*=\s*(?:"[^"]*"|'[^']*'|Bearer\s+[^\s,;]+|[^\s,;]+)/giu
const CREDENTIAL_PROPERTY = /((?:"|')?(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)(?:"|')?\s*:\s*)(?:"[^"]*"|'[^']*'|\[[^\]]*\]|Bearer\s+[^\s,}\]]+|[^\s,}\]]+)/giu
const ABSOLUTE_PATH = /(?:^|(?<=[\s("'=]))(?:\/(?:Users|Volumes|private|tmp|var|opt|home|Applications|Library)\/[^\s"'<>]+|[A-Za-z]:[\\/][^\s"'<>]+)/gu
const CANDIDATE_REF = /\bgit-candidate:sha256:[0-9a-f]{64}\b/gu
const INTERNAL_ID = /\b(?:agt|art|att|binding|call|cfg|crd|crt|dlv|evd|evt|hum|job|lease|lse|org|prj|psn|pva|pvs|rb|rbd|rep|req|rpo|run|sys|thr|wit|wki|wrs|wrk|wsp|wsn|wss|wct|wrn|usr)_[A-Za-z0-9-]+\b/gu
const SOURCE_REF = /\b(?:sourceRef|source_ref)\s*[:=]\s*[^\s,;]+/giu
const RUNTIME_SOURCE = /\bruntime(?::\/\/|:)[^\s,;]+/gu

/** Redacts credentials, local paths, and internal runtime identities in text. */
export function redactPublicText(value: string): string {
  return value
    .replace(PRIVATE_KEY, '[REDACTED PRIVATE KEY]')
    .replace(JWT, '[REDACTED TOKEN]')
    .replace(BASIC, 'Basic [REDACTED]')
    .replace(PROVIDER_SECRET, '[REDACTED SECRET]')
    .replace(URL_CREDENTIALS, '[REDACTED URL]')
    .replace(CREDENTIAL_ASSIGNMENT, match => `${match.split('=', 1)[0]?.trim() ?? 'secret'}=[REDACTED]`)
    .replace(CREDENTIAL_PROPERTY, (match, prefix: string) => {
      const rawValue = match.slice(prefix.length).trim()
      return /authorization/iu.test(prefix) && /^bearer\s+/iu.test(rawValue)
        ? `${prefix}Bearer [REDACTED]`
        : `${prefix}"[REDACTED]"`
    })
    .replace(BEARER, 'Bearer [REDACTED]')
    .replace(ABSOLUTE_PATH, '[REDACTED PATH]')
    .replace(CANDIDATE_REF, '[CANDIDATE]')
    .replace(SOURCE_REF, '[SOURCE REF]')
    .replace(RUNTIME_SOURCE, '[SOURCE REF]')
    .replace(INTERNAL_ID, '[INTERNAL ID]')
}

/** Converts transport and server errors to fixed browser-facing copy. */
export function publicErrorText(error: unknown, phase: string): string {
  const code = error !== null && typeof error === 'object'
    && typeof (error as { readonly code?: unknown }).code === 'string'
    ? (error as { readonly code: string }).code
    : null
  switch (code) {
    case 'AUTHENTICATION_REQUIRED': return '登录状态已失效，请重新登录。'
    case 'PERMISSION_DENIED': return '当前账号没有执行此操作的权限。'
    case 'RESOURCE_NOT_FOUND': return '当前内容已不存在，请刷新后重试。'
    case 'CANDIDATE_STALE':
    case 'STALE_CANDIDATE_FILE_PAGE':
    case 'STALE_CANDIDATE_FILE_CONTENT':
    case 'STALE_CANDIDATE_DIFF_CHUNK': return '候选版本已变化，请刷新后重试。'
    case 'REVISION_CONFLICT': return '内容已更新，请刷新后重试。'
    case 'WRONG_STATE': return '当前状态不允许此操作，请刷新后重试。'
    case 'DEVICE_SESSION_REQUIRED': return '请先连接并启动 Device 后再发送消息。'
    case 'DEVICE_MODEL_UNAVAILABLE': return '所选模型在当前 Device 上不可用，请检查 Device 的 Provider 配置。'
    default: return `${phase}，请重试。`
  }
}
