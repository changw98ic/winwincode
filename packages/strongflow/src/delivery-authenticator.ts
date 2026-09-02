import { createHash, timingSafeEqual } from 'node:crypto'

import type {
  StrongFlowDeliveryAuthentication,
  StrongFlowDeliveryChannel,
} from '@winwincode/contracts'

export interface StrongFlowDeliveryAuthenticationRequest {
  readonly channel: StrongFlowDeliveryChannel
  readonly authentication: StrongFlowDeliveryAuthentication
}

export interface AuthenticatedDeliveryActor {
  readonly actorId: string
}

/** Resolves a transport proof to the human identity stored on a business decision. */
export interface StrongFlowDeliveryAuthenticator {
  authenticate(
    request: StrongFlowDeliveryAuthenticationRequest,
  ): Promise<AuthenticatedDeliveryActor | undefined>
}

export interface StrongFlowDeliveryLocalProofAuthenticatorOptions {
  readonly localSessionProof?: string
  readonly localPeerProof?: string
  readonly localSessionActorId?: string
  readonly localPeerActorId?: string
}

const ACTOR_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u

function actorId(value: string, label: string): string {
  if (!ACTOR_ID_PATTERN.test(value)) {
    throw new TypeError(`${label} must be a portable identity`)
  }
  return value
}

function configuredProof(value: string | undefined, label: string): string | undefined {
  if (value === undefined) return undefined
  if (value.length < 16 || value.length > 8_192) {
    throw new TypeError(`${label} must contain between 16 and 8192 characters`)
  }
  return value
}

function constantTimeEqual(left: string, right: string): boolean {
  const leftDigest = createHash('sha256').update(left).digest()
  const rightDigest = createHash('sha256').update(right).digest()
  return timingSafeEqual(leftDigest, rightDigest)
}

/** Exact local proof matcher used by the packaged DSH Host and CLI process. */
export function createStrongFlowDeliveryLocalProofAuthenticator(
  options: StrongFlowDeliveryLocalProofAuthenticatorOptions,
): StrongFlowDeliveryAuthenticator {
  const localSessionProof = configuredProof(options.localSessionProof, 'localSessionProof')
  const localPeerProof = configuredProof(options.localPeerProof, 'localPeerProof')
  const localSessionActorId = actorId(
    options.localSessionActorId ?? 'local-ui-reviewer',
    'localSessionActorId',
  )
  const localPeerActorId = actorId(
    options.localPeerActorId ?? 'local-cli-reviewer',
    'localPeerActorId',
  )
  return Object.freeze({
    async authenticate(request: StrongFlowDeliveryAuthenticationRequest) {
      if (request.channel === 'local-ui'
        && request.authentication.scheme === 'local-session'
        && localSessionProof !== undefined
        && constantTimeEqual(request.authentication.proof, localSessionProof)) {
        return Object.freeze({ actorId: localSessionActorId })
      }
      if (request.channel === 'cli'
        && request.authentication.scheme === 'local-peer'
        && localPeerProof !== undefined
        && constantTimeEqual(request.authentication.proof, localPeerProof)) {
        return Object.freeze({ actorId: localPeerActorId })
      }
      return undefined
    },
  })
}
