import {
  createQueryCache,
  createQueryCacheLifecycle,
} from '@winwincode/browser-core/query-cache'
import type {
  EventReadCursor as PublicEventReadCursor,
} from '@winwincode/contracts/browser-control'
import type { ControlPlaneClient } from '../../apps/client/src/community-control-plane-client.js'
import type {
  Actor,
  EventReadCursor,
  ControlPlaneWebSocketSubscription,
  QueryRequest,
  Scope,
} from '../../apps/client/src/generated/contracts.js'

declare const productClient: ControlPlaneClient

const cache = createQueryCache({ client: productClient })
const preservedClient: ControlPlaneClient = cache.client
void preservedClient

// @ts-expect-error An arbitrary object is not a browser Control Plane client port.
createQueryCache({ client: {} })

declare const actor: Actor
declare const scope: Scope
declare const validQuery: QueryRequest
const lifecycle = createQueryCacheLifecycle({ client: productClient, actor, scope })
// @ts-expect-error An arbitrary object has no product query type for a cache lifecycle.
createQueryCacheLifecycle({ client: {}, actor, scope })
lifecycle.revalidate({
  ...validQuery,
  // @ts-expect-error A lifecycle preserves the product-generated query discriminator union.
  query: 'not-a-control-plane-query',
})

const invalidEventTypes: ControlPlaneWebSocketSubscription['eventTypes'] = [
  // @ts-expect-error Product-generated event types reject values outside the generated union.
  'not-a-control-plane-event',
]
void invalidEventTypes

declare const validProductCursor: EventReadCursor
productClient.subscribe({
  subscriptionId: '' as never,
  subscription: {} as ControlPlaneWebSocketSubscription,
  onEvent() {},
  onResetRequired() {
    return validProductCursor
  },
})

declare const publicCursor: PublicEventReadCursor
productClient.subscribe({
  subscriptionId: '' as never,
  subscription: {} as ControlPlaneWebSocketSubscription,
  onEvent() {},
  // @ts-expect-error A public cursor does not satisfy the generated cursor invariants.
  onResetRequired() {
    return publicCursor
  },
})
