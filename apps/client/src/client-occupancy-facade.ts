// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  controlPlaneOccupancyFailure,
  createControlPlaneClientOccupancy,
  type ControlPlaneClient,
  type ControlPlaneClientTransport,
  type ControlPlaneOccupancyClaimInput,
  type ControlPlaneOccupancyFailure,
  type ControlPlaneOccupancyForceReleaseOutcome,
  type ControlPlaneOccupancyHolderView,
  type ControlPlaneOccupancyReleaseMode,
  type ControlPlaneOccupancyReleaseOutcome,
  type ControlPlaneOccupancyStatus,
  type ControlPlaneOccupancyStatusInput,
  type ControlPlaneRequestOptions,
} from './community-control-plane-client.js'

// The occupancy wire contract stays canonical in control-plane-client; the
// re-exports below let a page import the whole occupancy surface from this
// one module instead of reaching past the facade.
export type {
  ControlPlaneOccupancyClaimInput,
  ControlPlaneOccupancyForceReleaseOutcome,
  ControlPlaneOccupancyHolderView,
  ControlPlaneOccupancyReleaseMode,
  ControlPlaneOccupancyReleaseOutcome,
  ControlPlaneOccupancyStatus,
  ControlPlaneOccupancyStatusInput,
} from './community-control-plane-client.js'
export { ControlPlaneClientError } from './community-control-plane-client.js'

/**
 * The finite, stable presentation categories an occupancy rejection maps onto.
 * The union is the canonical occupancy taxonomy of
 * `controlPlaneOccupancyFailure`; `unavailable` stays the catch-all for
 * outages, expired browser sessions, protocol drift, and unknown codes.
 */
export type ClientOccupancyErrorCategory = ControlPlaneOccupancyFailure

/**
 * Translate one occupancy rejection into the stable category union. The wire
 * code table lives only in control-plane-client, so pages branch on this
 * union and never read wire codes themselves.
 */
export function clientOccupancyErrorCategory(
  error: unknown,
): ClientOccupancyErrorCategory {
  return controlPlaneOccupancyFailure(error)
}

/**
 * One holder release request through the facade. The mode defaults to the
 * immediate release; `drain` lets running tasks finish before the device
 * frees, and `cancel_and_release` belongs to `cancelAndRelease`, which owns
 * the explicit confirmation the Server demands for it.
 */
export interface ClientOccupancyReleaseRequest {
  readonly clientId: string
  readonly mode?: ControlPlaneOccupancyReleaseMode
}

/**
 * The one typed occupancy entry the browser uses (winwincode-cms.1): pages
 * reach claim / status / release / force-release only through this facade.
 * Every method resolves with the frozen Server projection — pages consume the
 * backend-returned occupancy state and never derive an authoritative state of
 * their own — and rejects with the one `ControlPlaneClientError` identity in
 * its stable shape: the code, kind, correlation id, and retryability survive,
 * the message becomes the stable category copy, and server-supplied free text
 * and details are dropped, so no raw server error and no foreign user's
 * identity can leak through to a page.
 */
export interface ClientOccupancyFacade {
  /**
   * Claim the free device for the signed-in identity. Grouping separators in
   * the digit identity are stripped here, an in-flight claim for the same
   * device is joined instead of repeated, and the returned holder view
   * describes the caller's own lease.
   */
  claim(
    input: ControlPlaneOccupancyClaimInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyHolderView>
  /** Read the Server's occupancy projection for the signed-in user. */
  getStatus(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyStatus>
  /** Release the caller's own occupancy; the mode defaults to `release`. */
  release(
    input: ClientOccupancyReleaseRequest,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyReleaseOutcome>
  /** Stop the running tasks and release the device immediately. */
  cancelAndRelease(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyReleaseOutcome>
  /** Owner-only safe cleanup of a recovery-pending lease. */
  forceRelease(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyForceReleaseOutcome>
}

/**
 * The one copy per stable category. Server-derived rejections are rebuilt
 * with this text, so the message a page reads is facade-owned and constant
 * instead of wire text that could carry another user's identity.
 */
const OCCUPANCY_CATEGORY_TEXT: Readonly<
  Record<ClientOccupancyErrorCategory, string>
> = Object.freeze({
  'invalid-request': 'The occupancy request was not valid.',
  'confirmation-required': 'The Server needs the explicit confirmation for this release.',
  'client-not-found': 'The Client device no longer exists.',
  'client-offline': 'The Client device is offline right now.',
  'client-locked': 'The Client device is locked.',
  'new-connections-forbidden': 'The Client device no longer accepts new connections.',
  'access-denied': 'The signed-in account may not occupy this Client device.',
  'occupied-by-other': 'Another user claimed the Client device first.',
  'capacity-exhausted': 'The Client device has no free capacity left.',
  'occupancy-rejected': 'The Client device rejected the occupancy request.',
  'occupancy-ack-timeout': 'The Client device did not acknowledge the occupancy in time.',
  'recovery-pending': 'The Client device is waiting to recover. Try again after it recovers.',
  'permission-denied': 'Only the device Owner can force-release this Client device.',
  'no-active-occupancy': 'There is no active occupancy to release.',
  'wrong-state': 'The occupancy changed before the request landed.',
  'rate-limited': 'Too many attempts. Wait a moment, then try again.',
  'unavailable': 'The request did not go through. Check the connection and try again.',
})

/**
 * Error identities the client mints before or around the wire. Their messages
 * are already facade-owned stable copy, so they pass through untouched;
 * every other rejection is server-derived and gets rebuilt below.
 */
const CLIENT_MINTED_ERROR_CODES: ReadonlySet<string> = new Set([
  'CLIENT_OCCUPANCY_FAILED',
  'CLIENT_OCCUPANCY_ID_INVALID',
  'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  'NETWORK_ERROR',
  'REQUEST_CANCELLED',
  'SCHEMA_VERSION_MISMATCH',
  'TRANSPORT_UNAVAILABLE',
])

function unavailableOccupancyError(): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'CLIENT_OCCUPANCY_FAILED',
    message: OCCUPANCY_CATEGORY_TEXT['unavailable'],
    requestId: null,
    retryable: true,
  })
}

/**
 * Rebuild one occupancy rejection into the facade's stable error shape: the
 * `ControlPlaneClientError` identity, code, kind, correlation id, and
 * retryability survive, while the message becomes the stable category copy
 * and server-supplied details are dropped. Rejections outside the one error
 * identity collapse into the honest unavailable failure.
 */
function stableOccupancyRejection(error: unknown): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) {
    if (CLIENT_MINTED_ERROR_CODES.has(error.code)) return error
    return new ControlPlaneClientError({
      kind: error.kind,
      code: error.code,
      message: OCCUPANCY_CATEGORY_TEXT[controlPlaneOccupancyFailure(error)],
      requestId: error.requestId,
      retryable: error.retryable,
    })
  }
  return unavailableOccupancyError()
}

function rethrowStable(error: unknown): never {
  throw stableOccupancyRejection(error)
}

/**
 * Create the browser occupancy facade over the one wire seam of
 * control-plane-client. The wire facade keeps owning the routes, the payload
 * validation, the privacy projections, and the claim idempotency; this module
 * composes it into the single typed entry pages consume and guarantees the
 * stable rejection shape on top.
 */
export function createClientOccupancyFacade(options: {
  readonly client: ControlPlaneClient
  /** Same deterministic transport seam the base facade was created with. */
  readonly transport?: ControlPlaneClientTransport
}): ClientOccupancyFacade {
  const wire = createControlPlaneClientOccupancy(options)
  const facade: ClientOccupancyFacade = {
    claim(input, requestOptions) {
      return wire.claimOccupancy(input, requestOptions).catch(rethrowStable)
    },
    getStatus(input, requestOptions) {
      return wire.occupancyStatus(input, requestOptions).catch(rethrowStable)
    },
    release(input, requestOptions) {
      return wire.releaseOccupancy({
        clientId: input.clientId,
        mode: input.mode ?? 'release',
      }, requestOptions).catch(rethrowStable)
    },
    cancelAndRelease(input, requestOptions) {
      return wire.releaseOccupancy({
        clientId: input.clientId,
        mode: 'cancel_and_release',
        confirm: true,
      }, requestOptions).catch(rethrowStable)
    },
    forceRelease(input, requestOptions) {
      return wire.forceReleaseOccupancy(input, requestOptions).catch(rethrowStable)
    },
  }
  return Object.freeze(facade)
}
