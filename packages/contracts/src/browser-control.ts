/** Product-neutral browser identities and Control Plane envelopes. */

type Identifier<Name extends string> = string & { readonly __brand: Name }

export type OrganizationId = Identifier<'OrganizationId'>
export type WorkspaceId = Identifier<'WorkspaceId'>
export type ProjectId = Identifier<'ProjectId'>
export type RepositoryId = Identifier<'RepositoryId'>
export type RequestId = Identifier<'RequestId'>
export type ServiceAccountId = Identifier<'ServiceAccountId'>
export type ControlPlaneEventId = Identifier<'ControlPlaneEventId'>
export type DeliveryId = Identifier<'DeliveryId'>
export type ProductSessionId = Identifier<'ProductSessionId'>
export type LeaseId = Identifier<'LeaseId'>
export type WorkerId = Identifier<'WorkerId'>
export type UserId = Identifier<'UserId'>
export type SystemActorId = Identifier<'SystemActorId'>

export interface UserActor {
  readonly kind: 'user'
  readonly id: UserId
}

export interface SystemActor {
  readonly kind: 'system'
  readonly id: SystemActorId
}

export interface ServiceAccountActor {
  readonly kind: 'service_account'
  readonly id: ServiceAccountId
}

export type Actor = UserActor | ServiceAccountActor | SystemActor

export interface OrganizationScope {
  readonly kind: 'organization'
  readonly organizationId: OrganizationId
}

export interface WorkspaceScope {
  readonly kind: 'workspace'
  readonly organizationId: OrganizationId
  readonly workspaceId: WorkspaceId
}

export interface ProjectScope {
  readonly kind: 'project'
  readonly organizationId: OrganizationId
  readonly workspaceId: WorkspaceId
  readonly projectId: ProjectId
}

export interface RepositoryScope {
  readonly kind: 'repository'
  readonly organizationId: OrganizationId
  readonly workspaceId: WorkspaceId
  readonly projectId: ProjectId
  readonly repositoryId: RepositoryId
}

export type Scope = OrganizationScope | WorkspaceScope | ProjectScope | RepositoryScope
export type ErrorDetailValue =
  | boolean
  | null
  | number
  | string
  | readonly ErrorDetailValue[]
  | { readonly [key: string]: ErrorDetailValue }

export interface ErrorDetails extends Readonly<Record<string, ErrorDetailValue>> {}

export interface ControlPlaneCommandRequest {
  readonly schemaVersion: string
  readonly requestId: RequestId
  readonly actor: Actor
  readonly scope: Scope
  readonly command: string
}

export interface ControlPlaneQueryRequest {
  readonly schemaVersion: string
  readonly requestId: RequestId
  readonly actor: Actor
  readonly scope: Scope
  readonly query: string
  readonly parameters: unknown
  readonly page: unknown
}

export interface ControlPlaneCommandResponse {
  readonly schemaVersion: string
  readonly requestId: RequestId
  readonly command: string
  readonly outcome: string
}

export interface ControlPlaneQueryResponse {
  readonly schemaVersion: string
  readonly requestId: RequestId
  readonly query: string
  readonly result: unknown
  readonly page: unknown
}

export interface ControlPlaneAuthSession {
  readonly schemaVersion: string
  readonly expiresAt: string
  readonly actor: Actor
  readonly authorizedScopes: readonly Scope[]
}

export interface BrowserControlRequestOptions {
  readonly signal?: AbortSignal
}

export interface BrowserControlPasswordCredentials {
  readonly username: string
  readonly password: string
}

export interface BrowserControlOwnerInitialization extends BrowserControlPasswordCredentials {
  readonly bootstrapProof: string
}

export interface BrowserControlInitializationStatus {
  readonly initialized: boolean
}

export type BrowserControlClientErrorKind =
  | 'authentication'
  | 'authorization'
  | 'cancelled'
  | 'configuration'
  | 'network'
  | 'protocol'
  | 'server'
  | 'version'

export interface BrowserControlClientError {
  readonly kind: BrowserControlClientErrorKind
  readonly code: string
  readonly message: string
  readonly requestId: RequestId | null
  readonly retryable: boolean
  readonly details: ErrorDetails
}

export type ControlPlaneWebSocketSubscriptionId = Identifier<'ControlPlaneWebSocketSubscriptionId'>

export type EventReadStream =
  | { readonly kind: 'scope' }
  | { readonly kind: 'delivery'; readonly deliveryId: DeliveryId }
  | { readonly kind: 'product-session'; readonly productSessionId: ProductSessionId }
  | { readonly kind: 'lease'; readonly leaseId: LeaseId; readonly workerId: WorkerId }

export interface EventReadCursor {
  readonly eventId: ControlPlaneEventId | null
  readonly scope: Scope
  readonly sequence: number
  readonly stream: EventReadStream
}

export type ControlPlaneWebSocketSubscribeStartAt = string | EventReadCursor

export interface ControlPlaneWebSocketSubscription {
  readonly eventTypes: readonly string[]
  readonly scope: Scope
  readonly stream: EventReadStream
}

export interface ControlPlaneWebSocketEventFrame {
  readonly authorizationEpoch: number
  readonly event: { readonly type: string }
  readonly eventId: ControlPlaneEventId
  readonly occurredAt: string
  readonly scope: Scope
  readonly sequence: number
  readonly stream: EventReadStream
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly source: object
  readonly type: 'event.v1'
}

export interface ControlPlaneWebSocketResetRequiredFrame {
  readonly closeCode: 4409
  readonly earliestAvailable: EventReadCursor
  readonly reason: 'cursor-expired' | 'stream-rebuilt' | 'authorization-boundary'
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly type: 'transport.reset-required.v1'
}

export interface ControlPlaneWebSocketAuthorizationRevokedFrame {
  readonly authorizationEpoch: number
  readonly closeCode: 4403
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly type: 'transport.authorization-revoked.v1'
}

export interface ControlPlaneWebSocketAcknowledgedCursor {
  readonly eventId: ControlPlaneEventId
  readonly scope: Scope
  readonly sequence: number
  readonly stream: EventReadStream
}

export interface BrowserControlSubscribeOptions<
  ProductEventFrame extends ControlPlaneWebSocketEventFrame = ControlPlaneWebSocketEventFrame,
  ProductResetFrame extends ControlPlaneWebSocketResetRequiredFrame =
    ControlPlaneWebSocketResetRequiredFrame,
  ProductAuthorizationRevokedFrame extends ControlPlaneWebSocketAuthorizationRevokedFrame =
    ControlPlaneWebSocketAuthorizationRevokedFrame,
  ProductSubscription extends ControlPlaneWebSocketSubscription =
    ControlPlaneWebSocketSubscription,
  ProductSubscriptionId extends ControlPlaneWebSocketSubscriptionId =
    ControlPlaneWebSocketSubscriptionId,
  ProductStartAt extends ControlPlaneWebSocketSubscribeStartAt =
    ControlPlaneWebSocketSubscribeStartAt,
  ProductEventReadCursor extends EventReadCursor = EventReadCursor,
> {
  readonly subscriptionId: ProductSubscriptionId
  readonly subscription: ProductSubscription
  readonly startAt?: ProductStartAt
  readonly signal?: AbortSignal
  readonly onEventQueued?: (event: ProductEventFrame) => void
  readonly onEvent: (event: ProductEventFrame) => Promise<void> | void
  readonly onResetRequired?: (
    frame: ProductResetFrame | null,
  ) => Promise<ProductEventReadCursor> | ProductEventReadCursor
  readonly onAuthorizationRevoked?: (
    frame: ProductAuthorizationRevokedFrame | null,
  ) => Promise<void> | void
  readonly onError?: (error: BrowserControlClientError) => void
}

export interface BrowserControlSubscription {
  readonly cursor: ControlPlaneWebSocketAcknowledgedCursor | null
  resume(): void
  reconnect(): void
  close(): void
}

/** The product-neutral client surface wrapped by the shared browser query cache. */
export interface QueryCacheClientPort<
  ProductSession extends ControlPlaneAuthSession = ControlPlaneAuthSession,
  ProductCommandRequest extends ControlPlaneCommandRequest = ControlPlaneCommandRequest,
  ProductCommandResponse extends ControlPlaneCommandResponse = ControlPlaneCommandResponse,
  ProductQueryRequest extends ControlPlaneQueryRequest = ControlPlaneQueryRequest,
  ProductQueryResponse extends ControlPlaneQueryResponse = ControlPlaneQueryResponse,
  ProductEventFrame extends ControlPlaneWebSocketEventFrame = ControlPlaneWebSocketEventFrame,
  ProductResetFrame extends ControlPlaneWebSocketResetRequiredFrame =
    ControlPlaneWebSocketResetRequiredFrame,
  ProductAuthorizationRevokedFrame extends ControlPlaneWebSocketAuthorizationRevokedFrame =
    ControlPlaneWebSocketAuthorizationRevokedFrame,
  ProductSubscription extends ControlPlaneWebSocketSubscription =
    ControlPlaneWebSocketSubscription,
  ProductSubscriptionId extends ControlPlaneWebSocketSubscriptionId =
    ControlPlaneWebSocketSubscriptionId,
  ProductStartAt extends ControlPlaneWebSocketSubscribeStartAt =
    ControlPlaneWebSocketSubscribeStartAt,
  ProductEventReadCursor extends EventReadCursor = EventReadCursor,
> {
  readonly serverUrl: string
  restore(options?: BrowserControlRequestOptions): Promise<ProductSession>
  initializeOwner(
    initialization: BrowserControlOwnerInitialization,
    options?: BrowserControlRequestOptions,
  ): Promise<ProductSession>
  login(
    credentials: BrowserControlPasswordCredentials,
    options?: BrowserControlRequestOptions,
  ): Promise<ProductSession>
  initializationStatus(
    options?: BrowserControlRequestOptions,
  ): Promise<BrowserControlInitializationStatus>
  logout(options?: BrowserControlRequestOptions): Promise<void>
  command(
    command: ProductCommandRequest,
    options?: BrowserControlRequestOptions,
  ): Promise<ProductCommandResponse>
  query(
    query: ProductQueryRequest,
    options?: BrowserControlRequestOptions,
  ): Promise<ProductQueryResponse>
  subscribe(options: BrowserControlSubscribeOptions<
    ProductEventFrame,
    ProductResetFrame,
    ProductAuthorizationRevokedFrame,
    ProductSubscription,
    ProductSubscriptionId,
    ProductStartAt,
    ProductEventReadCursor
  >): BrowserControlSubscription
  close(): void
}
