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

export type ClientOccupancyErrorCategory = ControlPlaneOccupancyFailure

export function clientOccupancyErrorCategory(
  error: unknown,
): ClientOccupancyErrorCategory {
  return controlPlaneOccupancyFailure(error)
}

export interface ClientOccupancyReleaseRequest {
  readonly clientId: string
  readonly mode?: ControlPlaneOccupancyReleaseMode
}

export interface ClientOccupancyFacade {
  claim(
    input: ControlPlaneOccupancyClaimInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyHolderView>
  getStatus(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyStatus>
  release(
    input: ClientOccupancyReleaseRequest,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyReleaseOutcome>
  cancelAndRelease(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyReleaseOutcome>
  forceRelease(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyForceReleaseOutcome>
}

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

export function createClientOccupancyFacade(options: {
  readonly client: ControlPlaneClient
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
