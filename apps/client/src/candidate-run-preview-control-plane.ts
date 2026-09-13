// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  parseControlPlaneServerUrl,
  type ControlPlaneTransportFetch,
} from './community-control-plane-client.js'
import type {
  AuthorizedPreviewSourceFacts,
  CandidatePreviewIdentity,
  CandidatePreviewPort,
} from './candidate-run-preview-view-model.js'
import { isCandidatePreviewIdentity, isSafePreviewOrigin } from './candidate-run-preview-view-model.js'

const CREATE_PATH = '/api/v1/previews'

function isRecord(value: unknown): value is Readonly<Record<string, unknown>> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function failure(kind: 'network' | 'protocol' | 'server', code: string, message: string): ControlPlaneClientError {
  return new ControlPlaneClientError({ kind, code, message, requestId: null, retryable: kind !== 'protocol' })
}

function parseGrant(source: string, identity: CandidatePreviewIdentity): AuthorizedPreviewSourceFacts {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  if (!isRecord(value) || value.schemaVersion !== 'winwincode/v1'
    || typeof value.previewAccessId !== 'string'
    || !/^pva_[0-9a-f]{32}$/u.test(value.previewAccessId)
    || typeof value.previewUrl !== 'string' || !isSafePreviewOrigin(value.previewUrl)
    || typeof value.expiresAt !== 'string' || !Number.isFinite(Date.parse(value.expiresAt))
    || !isRecord(value.source)
    || value.source.sourceId !== identity.sourceId
    || value.source.workerSessionId !== identity.workerSessionId
    || value.source.repositoryBindingId !== identity.repositoryBindingId
    || value.source.mode !== identity.mode
    || (identity.candidateCommit === null
      ? value.source.candidateCommit !== undefined
      : value.source.candidateCommit !== identity.candidateCommit)) {
    throw failure('protocol', 'INVALID_PREVIEW_ACCESS_RESPONSE', 'Server 返回了无效的预览授权。')
  }
  const previewUrl = new URL(value.previewUrl)
  if (previewUrl.search !== '' || previewUrl.hash !== ''
    || previewUrl.username !== '' || previewUrl.password !== ''
    || previewUrl.pathname !== `/p/${value.previewAccessId}/${previewUrl.pathname.split('/')[3] ?? ''}/`
    || !/^[0-9a-f]{64}$/u.test(previewUrl.pathname.split('/')[3] ?? '')) {
    throw failure('protocol', 'INVALID_PREVIEW_ACCESS_RESPONSE', 'Server 返回了无效的预览授权。')
  }
  return Object.freeze({
    previewAccessId: value.previewAccessId,
    sourceId: identity.sourceId,
    mode: identity.mode,
    candidateCommit: identity.candidateCommit,
    previewOrigin: value.previewUrl,
    access: 'authorized',
    accessExpiresAt: value.expiresAt,
  })
}

/** Browser facade for the existing short-lived Server preview access routes. */
export function createControlPlaneCandidatePreviewPort(options: {
  readonly serverUrl: string
  readonly fetch: ControlPlaneTransportFetch | undefined
  readonly identity: CandidatePreviewIdentity
}): CandidatePreviewPort {
  const location = parseControlPlaneServerUrl(options.serverUrl)

  async function request(path: string, method: 'DELETE' | 'POST', body?: string) {
    if (options.fetch === undefined) {
      throw failure('protocol', 'TRANSPORT_UNAVAILABLE', '浏览器 HTTP transport 不可用。')
    }
    try {
      return await Reflect.apply(options.fetch, undefined, [`${location.serverUrl}${path}`, {
        method,
        headers: body === undefined ? {} : { 'content-type': 'application/json' },
        ...(body === undefined ? {} : { body }),
        redirect: 'error',
        cache: 'no-store',
        referrerPolicy: 'no-referrer',
        credentials: 'include',
      }])
    } catch (error) {
      if (error instanceof ControlPlaneClientError) throw error
      throw failure('network', 'NETWORK_ERROR', '无法连接 Server 的预览服务。')
    }
  }

  return Object.freeze({
    async loadIdentity() { return options.identity },
    async authorizePreview(input: { readonly sourceId: string; readonly requestId: string }) {
      if (!isCandidatePreviewIdentity(options.identity) || input.sourceId !== options.identity.sourceId) {
        throw failure('protocol', 'PREVIEW_IDENTITY_INVALID', '预览来源缺少有效运行身份。')
      }
      const response = await request(CREATE_PATH, 'POST', JSON.stringify({
        schemaVersion: 'winwincode/v1',
        clientId: options.identity.clientId,
        sourceId: options.identity.sourceId,
      }))
      const source = await response.text()
      if (!response.ok) throw failure('server', 'PREVIEW_ACCESS_REJECTED', `Server 拒绝预览授权（HTTP ${String(response.status)}）。`)
      if (response.status !== 201) throw failure('protocol', 'INVALID_PREVIEW_ACCESS_STATUS', 'Server 返回了无效的预览授权状态。')
      return parseGrant(source, options.identity)
    },
    async revokePreview(input: { readonly previewAccessId: string; readonly requestId: string }) {
      if (!/^pva_[0-9a-f]{32}$/u.test(input.previewAccessId)) {
        throw failure('protocol', 'PREVIEW_ACCESS_ID_INVALID', '预览授权标识无效。')
      }
      const response = await request(`${CREATE_PATH}/${encodeURIComponent(input.previewAccessId)}`, 'DELETE')
      if (!response.ok) throw failure('server', 'PREVIEW_REVOKE_REJECTED', `Server 拒绝撤销预览（HTTP ${String(response.status)}）。`)
      if (response.status !== 204) throw failure('protocol', 'INVALID_PREVIEW_REVOKE_STATUS', 'Server 返回了无效的预览撤销状态。')
      return Object.freeze({
        previewAccessId: input.previewAccessId,
        sourceId: options.identity.sourceId,
        mode: options.identity.mode,
        candidateCommit: options.identity.candidateCommit,
        previewOrigin: '',
        access: 'revoked',
        accessExpiresAt: null,
      })
    },
  })
}
