import { Buffer } from 'node:buffer'

import { Service, type Context } from '@deepseek-ai/cordis'
import {
  credentialRef,
  type CredentialProvider,
  type CredentialRef,
} from '@deepseek-ai/dsh-credentials'
import z from '@deepseek-ai/schemastery'
import {
  parseStrongFlowGitHubProviderOperation,
  type StrongFlowGitHubBranchOperation,
  type StrongFlowGitHubCommitStatusOperation,
  type StrongFlowGitHubIssueCommentOperation,
  type StrongFlowGitHubProviderMutation,
  type StrongFlowGitHubProviderObservation,
  type StrongFlowGitHubProviderOperation,
  type StrongFlowGitHubPublicationProvider,
  type StrongFlowGitHubPullRequestOperation,
} from '@winwincode/strongflow'

export const DEFAULT_GITHUB_CREDENTIAL_REFERENCE = 'GITHUB_TOKEN'
export const DEFAULT_GITHUB_API_BASE_URL = 'https://api.github.com'
export const DEFAULT_GITHUB_API_VERSION = '2022-11-28'

const DEFAULT_REQUEST_TIMEOUT_MILLIS = 30_000
const DEFAULT_MAX_LOOKUP_PAGES = 100
const MAX_RESPONSE_BYTES = 2 * 1_024 * 1_024
const PAGE_SIZE = 100
const PROVIDER_SERVICE = 'winwincodeGitHubPublicationProvider'
const USER_AGENT = 'WinWinCode-GitHub-Publication'
const API_VERSION_PATTERN = /^[0-9]{4}-[0-9]{2}-[0-9]{2}$/u

type FetchPort = typeof fetch

declare module '@deepseek-ai/cordis' {
  interface Context {
    winwincodeGitHubPublicationProvider: DshGitHubPublicationProvider
  }
}

export interface DshGitHubPublicationProviderConfig {
  /** DSH credential reference resolved separately for every remote request. */
  readonly credentialReference?: string
  /** GitHub REST root. Plain HTTP is accepted only for a loopback test server. */
  readonly apiBaseUrl?: string
  /** Value sent as `X-GitHub-Api-Version`. */
  readonly apiVersion?: string
  /** Per-request timeout. */
  readonly requestTimeoutMillis?: number
  /** Maximum pages inspected before lookup returns an unknown outcome. */
  readonly maxLookupPages?: number
}

interface ResolvedConfig {
  readonly credentialReference: CredentialRef
  readonly apiBaseUrl: URL
  readonly apiVersion: string
  readonly requestTimeoutMillis: number
  readonly maxLookupPages: number
}

interface RequestSuccess {
  readonly ok: true
  readonly response: Response
}

interface RequestFailure {
  readonly ok: false
  readonly code: string
}

type RequestResult = RequestSuccess | RequestFailure

interface PageSuccess {
  readonly ok: true
  readonly entries: readonly unknown[]
}

type PageResult = PageSuccess | RequestFailure

type MatchResult =
  | { readonly state: 'current'; readonly resourceRef: string }
  | { readonly state: 'conflict' }
  | { readonly state: 'unrelated' }
  | { readonly state: 'invalid' }

function positiveInteger(value: number | undefined, fallback: number, label: string): number {
  const resolved = value ?? fallback
  if (!Number.isSafeInteger(resolved) || resolved < 1) {
    throw new TypeError(`${label} must be a positive safe integer`)
  }
  return resolved
}

function apiBaseUrl(value: string | undefined): URL {
  let parsed: URL
  try {
    parsed = new URL(value ?? DEFAULT_GITHUB_API_BASE_URL)
  } catch {
    throw new TypeError('GitHub API base URL is invalid')
  }
  const loopback = parsed.hostname === '127.0.0.1'
    || parsed.hostname === 'localhost'
    || parsed.hostname === '[::1]'
  if ((parsed.protocol !== 'https:' && !(parsed.protocol === 'http:' && loopback))
    || parsed.username.length > 0
    || parsed.password.length > 0
    || parsed.search.length > 0
    || parsed.hash.length > 0) {
    throw new TypeError('GitHub API base URL must be credential-free HTTPS or loopback HTTP')
  }
  parsed.pathname = `${parsed.pathname.replace(/\/+$/u, '')}/`
  return parsed
}

function resolvedConfig(config: DshGitHubPublicationProviderConfig): ResolvedConfig {
  const apiVersion = config.apiVersion ?? DEFAULT_GITHUB_API_VERSION
  if (!API_VERSION_PATTERN.test(apiVersion)) {
    throw new TypeError('GitHub API version must use YYYY-MM-DD')
  }
  return Object.freeze({
    credentialReference: credentialRef(
      config.credentialReference ?? DEFAULT_GITHUB_CREDENTIAL_REFERENCE,
    ),
    apiBaseUrl: apiBaseUrl(config.apiBaseUrl),
    apiVersion,
    requestTimeoutMillis: positiveInteger(
      config.requestTimeoutMillis,
      DEFAULT_REQUEST_TIMEOUT_MILLIS,
      'requestTimeoutMillis',
    ),
    maxLookupPages: positiveInteger(
      config.maxLookupPages,
      DEFAULT_MAX_LOOKUP_PAGES,
      'maxLookupPages',
    ),
  })
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function stringValue(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null
}

function nestedRecord(value: Readonly<Record<string, unknown>>, key: string): Record<string, unknown> | null {
  const nested = value[key]
  return isRecord(nested) ? nested : null
}

function sameText(left: unknown, right: string): boolean {
  return typeof left === 'string' && left === right
}

function sameRepository(left: unknown, right: string): boolean {
  return typeof left === 'string' && left.toLowerCase() === right.toLowerCase()
}

function repositoryPath(repository: string): string {
  return repository.split('/').map(encodeURIComponent).join('/')
}

function refPath(branch: string): string {
  return ['heads', ...branch.split('/')].map(encodeURIComponent).join('/')
}

function safeResourceRef(value: unknown, fallback: URL): string | null {
  const candidate = typeof value === 'string' && value.length > 0
    ? value
    : fallback.toString()
  if (candidate.length > 8_192) return null
  try {
    const parsed = new URL(candidate)
    if ((parsed.protocol !== 'https:' && parsed.protocol !== 'http:')
      || parsed.username.length > 0
      || parsed.password.length > 0) return null
    return parsed.toString()
  } catch {
    return null
  }
}

function providerKey(operation: StrongFlowGitHubProviderOperation): string {
  return operation.operationKey.slice(0, operation.operationKey.lastIndexOf(':'))
}

function marker(operation: StrongFlowGitHubProviderOperation): string {
  return `<!-- winwincode-publication:${providerKey(operation)} -->`
}

function observationUnknown(
  operation: StrongFlowGitHubProviderOperation,
  code: string,
): StrongFlowGitHubProviderObservation {
  return Object.freeze({ state: 'unknown', operationKey: operation.operationKey, code })
}

function observationConflict(
  operation: StrongFlowGitHubProviderOperation,
  code: string,
): StrongFlowGitHubProviderObservation {
  return Object.freeze({ state: 'conflict', operationKey: operation.operationKey, code })
}

function observationAbsent(
  operation: StrongFlowGitHubProviderOperation,
): StrongFlowGitHubProviderObservation {
  return Object.freeze({ state: 'absent', operationKey: operation.operationKey })
}

function observationFound(
  operation: StrongFlowGitHubProviderOperation,
  resourceRef: string,
): StrongFlowGitHubProviderObservation {
  return Object.freeze({
    state: 'found',
    operationKey: operation.operationKey,
    requestSha256: operation.requestSha256,
    resourceRef,
  })
}

function mutationUnknown(
  operation: StrongFlowGitHubProviderOperation,
  code: string,
): StrongFlowGitHubProviderMutation {
  return Object.freeze({ state: 'unknown', operationKey: operation.operationKey, code })
}

function mutationRejected(
  operation: StrongFlowGitHubProviderOperation,
  code: string,
): StrongFlowGitHubProviderMutation {
  return Object.freeze({ state: 'rejected', operationKey: operation.operationKey, code })
}

function mutationApplied(
  operation: StrongFlowGitHubProviderOperation,
  resourceRef: string,
  remoteWritePerformed: boolean,
): StrongFlowGitHubProviderMutation {
  return Object.freeze({
    state: 'applied',
    operationKey: operation.operationKey,
    requestSha256: operation.requestSha256,
    resourceRef,
    remoteWritePerformed,
  })
}

function httpCode(status: number): string {
  return `github-http-${String(status)}`
}

function mutationFromObservation(
  operation: StrongFlowGitHubProviderOperation,
  observation: StrongFlowGitHubProviderObservation,
): StrongFlowGitHubProviderMutation {
  if (observation.state === 'found') {
    return mutationApplied(operation, observation.resourceRef, false)
  }
  if (observation.state === 'conflict') {
    return mutationRejected(operation, observation.code)
  }
  return mutationUnknown(
    operation,
    observation.state === 'unknown' ? observation.code : 'github-create-not-confirmed',
  )
}

/** DSH-owned GitHub REST adapter. Resolved credentials never leave one request. */
export class DshGitHubPublicationProvider extends Service
  implements StrongFlowGitHubPublicationProvider {
  static inject = ['credentials']

  static Config = z.object({
    credentialReference: z.string().default(DEFAULT_GITHUB_CREDENTIAL_REFERENCE),
    apiBaseUrl: z.string().default(DEFAULT_GITHUB_API_BASE_URL),
    apiVersion: z.string().default(DEFAULT_GITHUB_API_VERSION),
    requestTimeoutMillis: z.number().step(1).min(1).default(DEFAULT_REQUEST_TIMEOUT_MILLIS),
    maxLookupPages: z.number().step(1).min(1).default(DEFAULT_MAX_LOOKUP_PAGES),
  }) as z<DshGitHubPublicationProviderConfig>

  readonly config: ResolvedConfig
  readonly #credentials: CredentialProvider
  readonly #fetch: FetchPort

  constructor(
    ctx: Context,
    config: DshGitHubPublicationProviderConfig = {},
    fetchPort: FetchPort = fetch,
  ) {
    super(ctx, PROVIDER_SERVICE)
    if (typeof fetchPort !== 'function') throw new TypeError('GitHub fetch adapter is invalid')
    this.config = resolvedConfig(config)
    this.#credentials = ctx.credentials
    this.#fetch = fetchPort
  }

  async lookup(
    operationValue: StrongFlowGitHubProviderOperation,
  ): Promise<StrongFlowGitHubProviderObservation> {
    let operation: StrongFlowGitHubProviderOperation
    try {
      operation = parseStrongFlowGitHubProviderOperation(operationValue)
    } catch {
      return Object.freeze({
        state: 'unknown',
        operationKey: typeof operationValue?.operationKey === 'string'
          ? operationValue.operationKey
          : 'github:pull-request:sha256:invalid:branch',
        code: 'invalid-operation',
      })
    }
    try {
      switch (operation.kind) {
        case 'branch': return await this.#lookupBranch(operation)
        case 'pull-request': return await this.#lookupPullRequest(operation)
        case 'issue-comment': return await this.#lookupIssueComment(operation)
        case 'commit-status': return await this.#lookupCommitStatus(operation)
      }
    } catch {
      return observationUnknown(operation, 'provider-internal-error')
    }
  }

  async apply(
    operationValue: StrongFlowGitHubProviderOperation,
  ): Promise<StrongFlowGitHubProviderMutation> {
    let operation: StrongFlowGitHubProviderOperation
    try {
      operation = parseStrongFlowGitHubProviderOperation(operationValue)
    } catch {
      return Object.freeze({
        state: 'rejected',
        operationKey: typeof operationValue?.operationKey === 'string'
          ? operationValue.operationKey
          : 'github:pull-request:sha256:invalid:branch',
        code: 'invalid-operation',
      })
    }
    try {
      switch (operation.kind) {
        case 'branch': return await this.#applyBranch(operation)
        case 'pull-request': return await this.#applyPullRequest(operation)
        case 'issue-comment': return await this.#applyIssueComment(operation)
        case 'commit-status': return await this.#applyCommitStatus(operation)
      }
    } catch {
      return mutationUnknown(operation, 'provider-internal-error')
    }
  }

  async #credential(): Promise<string | null> {
    try {
      return (await this.#credentials.resolve(this.config.credentialReference))?.value ?? null
    } catch {
      return null
    }
  }

  #url(path: string, query?: Readonly<Record<string, string>>): URL {
    const url = new URL(path.replace(/^\/+/, ''), this.config.apiBaseUrl)
    if (query !== undefined) {
      for (const [key, value] of Object.entries(query)) url.searchParams.set(key, value)
    }
    return url
  }

  async #request(method: 'GET' | 'POST', url: URL, body?: unknown): Promise<RequestResult> {
    const token = await this.#credential()
    if (token === null || token.length === 0) {
      return Object.freeze({ ok: false, code: 'credential-not-configured' })
    }
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), this.config.requestTimeoutMillis)
    try {
      const response = await this.#fetch(url, {
        method,
        redirect: 'error',
        signal: controller.signal,
        headers: {
          Accept: 'application/vnd.github+json',
          Authorization: `Bearer ${token}`,
          'User-Agent': USER_AGENT,
          'X-GitHub-Api-Version': this.config.apiVersion,
          ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      })
      return Object.freeze({ ok: true, response })
    } catch {
      return Object.freeze({ ok: false, code: 'github-transport-unknown' })
    } finally {
      clearTimeout(timer)
    }
  }

  async #json(response: Response): Promise<unknown | null> {
    const contentLength = response.headers.get('content-length')
    if (contentLength !== null
      && (/^[0-9]+$/u.test(contentLength) === false
        || Number(contentLength) > MAX_RESPONSE_BYTES)) return null
    if (response.body === null) return null
    const reader = response.body.getReader()
    const chunks: Uint8Array[] = []
    let bytes = 0
    try {
      while (true) {
        const next = await reader.read()
        if (next.done) break
        bytes += next.value.byteLength
        if (bytes > MAX_RESPONSE_BYTES) {
          await reader.cancel()
          return null
        }
        chunks.push(next.value)
      }
      return JSON.parse(Buffer.concat(chunks).toString('utf8')) as unknown
    } catch {
      return null
    } finally {
      reader.releaseLock()
    }
  }

  async #page(url: URL): Promise<PageResult> {
    const requested = await this.#request('GET', url)
    if (!requested.ok) return requested
    if (requested.response.status !== 200) {
      return Object.freeze({ ok: false, code: httpCode(requested.response.status) })
    }
    const value = await this.#json(requested.response)
    return Array.isArray(value)
      ? Object.freeze({ ok: true, entries: Object.freeze(value) })
      : Object.freeze({ ok: false, code: 'github-response-invalid' })
  }

  async #lookupBranch(
    operation: StrongFlowGitHubBranchOperation,
  ): Promise<StrongFlowGitHubProviderObservation> {
    const url = this.#url(
      `repos/${repositoryPath(operation.payload.repository)}/git/ref/${refPath(operation.payload.branch)}`,
    )
    const requested = await this.#request('GET', url)
    if (!requested.ok) return observationUnknown(operation, requested.code)
    if (requested.response.status === 404) return observationAbsent(operation)
    if (requested.response.status !== 200) {
      return observationUnknown(operation, httpCode(requested.response.status))
    }
    const value = await this.#json(requested.response)
    if (!isRecord(value)) return observationUnknown(operation, 'github-response-invalid')
    const object = nestedRecord(value, 'object')
    const sha = object === null ? null : stringValue(object.sha)
    const resourceRef = safeResourceRef(value.url, url)
    if (sha === null || resourceRef === null) {
      return observationUnknown(operation, 'github-response-invalid')
    }
    return sha === operation.payload.commitId
      ? observationFound(operation, resourceRef)
      : observationConflict(operation, 'branch-ref-conflict')
  }

  #matchPullRequest(
    operation: StrongFlowGitHubPullRequestOperation,
    value: unknown,
    fallback: URL,
  ): MatchResult {
    if (!isRecord(value)) return Object.freeze({ state: 'invalid' })
    const body = typeof value.body === 'string' ? value.body : ''
    const head = nestedRecord(value, 'head')
    const base = nestedRecord(value, 'base')
    const headRepository = head === null ? null : nestedRecord(head, 'repo')
    const baseRepository = base === null ? null : nestedRecord(base, 'repo')
    const sameRoute = sameText(head?.ref, operation.payload.headBranch)
      && sameRepository(headRepository?.full_name, operation.payload.headRepository)
      && sameText(base?.ref, operation.payload.baseBranch)
      && sameRepository(baseRepository?.full_name, operation.payload.repository)
    const ownsMarker = body.includes(marker(operation))
    if (!sameRoute && !ownsMarker) return Object.freeze({ state: 'unrelated' })
    if (!sameRoute
      || !ownsMarker
      || !sameText(value.title, operation.payload.title)
      || body !== operation.payload.body) return Object.freeze({ state: 'conflict' })
    const resourceRef = safeResourceRef(value.html_url ?? value.url, fallback)
    return resourceRef === null
      ? Object.freeze({ state: 'invalid' })
      : Object.freeze({ state: 'current', resourceRef })
  }

  async #lookupPullRequest(
    operation: StrongFlowGitHubPullRequestOperation,
  ): Promise<StrongFlowGitHubProviderObservation> {
    if (!operation.payload.body.includes(marker(operation))) {
      return observationConflict(operation, 'pull-request-marker-missing')
    }
    const [headOwner] = operation.payload.headRepository.split('/')
    for (let page = 1; page <= this.config.maxLookupPages; page += 1) {
      const url = this.#url(`repos/${repositoryPath(operation.payload.repository)}/pulls`, {
        state: 'all',
        head: `${headOwner}:${operation.payload.headBranch}`,
        base: operation.payload.baseBranch,
        per_page: String(PAGE_SIZE),
        page: String(page),
      })
      const result = await this.#page(url)
      if (!result.ok) return observationUnknown(operation, result.code)
      for (const entry of result.entries) {
        const match = this.#matchPullRequest(operation, entry, url)
        if (match.state === 'current') return observationFound(operation, match.resourceRef)
        if (match.state === 'conflict') {
          return observationConflict(operation, 'pull-request-conflict')
        }
        if (match.state === 'invalid') {
          return observationUnknown(operation, 'github-response-invalid')
        }
      }
      if (result.entries.length < PAGE_SIZE) return observationAbsent(operation)
    }
    return observationUnknown(operation, 'lookup-capacity-exceeded')
  }

  #matchIssueComment(
    operation: StrongFlowGitHubIssueCommentOperation,
    value: unknown,
    fallback: URL,
  ): MatchResult {
    if (!isRecord(value) || typeof value.body !== 'string') {
      return Object.freeze({ state: 'invalid' })
    }
    if (!value.body.includes(marker(operation))) return Object.freeze({ state: 'unrelated' })
    if (value.body !== operation.payload.body) return Object.freeze({ state: 'conflict' })
    const resourceRef = safeResourceRef(value.html_url ?? value.url, fallback)
    return resourceRef === null
      ? Object.freeze({ state: 'invalid' })
      : Object.freeze({ state: 'current', resourceRef })
  }

  async #lookupIssueComment(
    operation: StrongFlowGitHubIssueCommentOperation,
  ): Promise<StrongFlowGitHubProviderObservation> {
    if (!operation.payload.body.includes(marker(operation))) {
      return observationConflict(operation, 'issue-comment-marker-missing')
    }
    for (let page = 1; page <= this.config.maxLookupPages; page += 1) {
      const url = this.#url(
        `repos/${repositoryPath(operation.payload.repository)}/issues/${String(operation.payload.issueNumber)}/comments`,
        { per_page: String(PAGE_SIZE), page: String(page) },
      )
      const result = await this.#page(url)
      if (!result.ok) return observationUnknown(operation, result.code)
      for (const entry of result.entries) {
        const match = this.#matchIssueComment(operation, entry, url)
        if (match.state === 'current') return observationFound(operation, match.resourceRef)
        if (match.state === 'conflict') {
          return observationConflict(operation, 'issue-comment-conflict')
        }
        if (match.state === 'invalid') {
          return observationUnknown(operation, 'github-response-invalid')
        }
      }
      if (result.entries.length < PAGE_SIZE) return observationAbsent(operation)
    }
    return observationUnknown(operation, 'lookup-capacity-exceeded')
  }

  #matchCommitStatus(
    operation: StrongFlowGitHubCommitStatusOperation,
    value: unknown,
    fallback: URL,
  ): MatchResult {
    if (!isRecord(value)) return Object.freeze({ state: 'invalid' })
    const context = typeof value.context === 'string' ? value.context : null
    if (context === null) return Object.freeze({ state: 'invalid' })
    if (context.toLowerCase() !== operation.payload.context.toLowerCase()) {
      return Object.freeze({ state: 'unrelated' })
    }
    if (!sameText(value.state, operation.payload.state)
      || !sameText(value.description, operation.payload.description)
      || !sameText(value.target_url, operation.payload.targetUrl)) {
      return Object.freeze({ state: 'conflict' })
    }
    const resourceRef = safeResourceRef(value.url, fallback)
    return resourceRef === null
      ? Object.freeze({ state: 'invalid' })
      : Object.freeze({ state: 'current', resourceRef })
  }

  async #lookupCommitStatus(
    operation: StrongFlowGitHubCommitStatusOperation,
  ): Promise<StrongFlowGitHubProviderObservation> {
    for (let page = 1; page <= this.config.maxLookupPages; page += 1) {
      const url = this.#url(
        `repos/${repositoryPath(operation.payload.repository)}/commits/${encodeURIComponent(operation.payload.commitId)}/statuses`,
        { per_page: String(PAGE_SIZE), page: String(page) },
      )
      const result = await this.#page(url)
      if (!result.ok) return observationUnknown(operation, result.code)
      for (const entry of result.entries) {
        const match = this.#matchCommitStatus(operation, entry, url)
        if (match.state === 'current') return observationFound(operation, match.resourceRef)
        if (match.state === 'conflict') return observationAbsent(operation)
        if (match.state === 'invalid') {
          return observationUnknown(operation, 'github-response-invalid')
        }
      }
      if (result.entries.length < PAGE_SIZE) return observationAbsent(operation)
    }
    return observationUnknown(operation, 'lookup-capacity-exceeded')
  }

  async #applyBranch(
    operation: StrongFlowGitHubBranchOperation,
  ): Promise<StrongFlowGitHubProviderMutation> {
    const url = this.#url(`repos/${repositoryPath(operation.payload.repository)}/git/refs`)
    const requested = await this.#request('POST', url, {
      ref: `refs/heads/${operation.payload.branch}`,
      sha: operation.payload.commitId,
    })
    if (!requested.ok) return mutationUnknown(operation, requested.code)
    if (requested.response.status === 201) {
      const value = await this.#json(requested.response)
      const object = isRecord(value) ? nestedRecord(value, 'object') : null
      const resourceRef = isRecord(value) ? safeResourceRef(value.url, url) : null
      return object !== null
        && sameText(object.sha, operation.payload.commitId)
        && resourceRef !== null
        ? mutationApplied(operation, resourceRef, true)
        : mutationUnknown(operation, 'github-response-invalid')
    }
    if (requested.response.status === 409 || requested.response.status === 422) {
      return mutationFromObservation(operation, await this.#lookupBranch(operation))
    }
    return requested.response.status >= 500 || requested.response.status === 429
      ? mutationUnknown(operation, httpCode(requested.response.status))
      : mutationRejected(operation, httpCode(requested.response.status))
  }

  async #applyPullRequest(
    operation: StrongFlowGitHubPullRequestOperation,
  ): Promise<StrongFlowGitHubProviderMutation> {
    if (!operation.payload.body.includes(marker(operation))) {
      return mutationRejected(operation, 'pull-request-marker-missing')
    }
    const [headOwner = '', headName = ''] = operation.payload.headRepository.split('/')
    const [baseOwner = ''] = operation.payload.repository.split('/')
    const crossRepository = !sameRepository(
      operation.payload.headRepository,
      operation.payload.repository,
    )
    const url = this.#url(`repos/${repositoryPath(operation.payload.repository)}/pulls`)
    const requested = await this.#request('POST', url, {
      title: operation.payload.title,
      body: operation.payload.body,
      head: crossRepository
        ? `${headOwner}:${operation.payload.headBranch}`
        : operation.payload.headBranch,
      base: operation.payload.baseBranch,
      ...(crossRepository && headOwner.toLowerCase() === baseOwner.toLowerCase()
        ? { head_repo: headName }
        : {}),
    })
    if (!requested.ok) return mutationUnknown(operation, requested.code)
    if (requested.response.status === 201) {
      const value = await this.#json(requested.response)
      const match = this.#matchPullRequest(operation, value, url)
      return match.state === 'current'
        ? mutationApplied(operation, match.resourceRef, true)
        : mutationUnknown(operation, 'github-response-invalid')
    }
    if (requested.response.status === 409 || requested.response.status === 422) {
      return mutationFromObservation(operation, await this.#lookupPullRequest(operation))
    }
    return requested.response.status >= 500 || requested.response.status === 429
      ? mutationUnknown(operation, httpCode(requested.response.status))
      : mutationRejected(operation, httpCode(requested.response.status))
  }

  async #applyIssueComment(
    operation: StrongFlowGitHubIssueCommentOperation,
  ): Promise<StrongFlowGitHubProviderMutation> {
    if (!operation.payload.body.includes(marker(operation))) {
      return mutationRejected(operation, 'issue-comment-marker-missing')
    }
    const url = this.#url(
      `repos/${repositoryPath(operation.payload.repository)}/issues/${String(operation.payload.issueNumber)}/comments`,
    )
    const requested = await this.#request('POST', url, { body: operation.payload.body })
    if (!requested.ok) return mutationUnknown(operation, requested.code)
    if (requested.response.status === 201) {
      const value = await this.#json(requested.response)
      const match = this.#matchIssueComment(operation, value, url)
      return match.state === 'current'
        ? mutationApplied(operation, match.resourceRef, true)
        : mutationUnknown(operation, 'github-response-invalid')
    }
    return requested.response.status >= 500 || requested.response.status === 429
      ? mutationUnknown(operation, httpCode(requested.response.status))
      : mutationRejected(operation, httpCode(requested.response.status))
  }

  async #applyCommitStatus(
    operation: StrongFlowGitHubCommitStatusOperation,
  ): Promise<StrongFlowGitHubProviderMutation> {
    const url = this.#url(
      `repos/${repositoryPath(operation.payload.repository)}/statuses/${encodeURIComponent(operation.payload.commitId)}`,
    )
    const requested = await this.#request('POST', url, {
      state: operation.payload.state,
      target_url: operation.payload.targetUrl,
      description: operation.payload.description,
      context: operation.payload.context,
    })
    if (!requested.ok) return mutationUnknown(operation, requested.code)
    if (requested.response.status === 201) {
      const value = await this.#json(requested.response)
      const match = this.#matchCommitStatus(operation, value, url)
      return match.state === 'current'
        ? mutationApplied(operation, match.resourceRef, true)
        : mutationUnknown(operation, 'github-response-invalid')
    }
    return requested.response.status >= 500 || requested.response.status === 429
      ? mutationUnknown(operation, httpCode(requested.response.status))
      : mutationRejected(operation, httpCode(requested.response.status))
  }
}

export default DshGitHubPublicationProvider
