import type {
  ControlPlaneAuthSession,
  ControlPlaneCommandRequest,
  ControlPlaneCommandResponse,
  ControlPlaneQueryRequest,
  ControlPlaneQueryResponse,
  ErrorDetails as PublicErrorDetails,
  QueryCacheClientPort,
  ControlPlaneWebSocketEventFrame as PublicEventFrame,
  Scope as PublicScope,
} from '@winwincode/contracts/browser-control'
import type { ControlPlaneClient } from '../../apps/client/src/community-control-plane-client.js'
import type {
  AuthSessionResponse,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  CommandRequest,
  ControlPlaneWebSocketEventFrame,
  EventReadCursor,
  ErrorDetails,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketSubscribeStartAt,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  QueryRequest,
  QueryResultResponse,
  Scope,
} from '../../apps/client/src/generated/contracts.js'

type Extends<Source, Target> = [Source] extends [Target] ? true : false
type Expect<Value extends true> = Value

type ProductQueryCachePort = QueryCacheClientPort<
  AuthSessionResponse,
  CommandRequest,
  CommandAcceptedResponse | CommandCompletedResponse,
  QueryRequest,
  QueryResultResponse,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  EventReadCursor
>

type GeneratedHttpClient = ReturnType<
  typeof import('../../apps/client/src/generated/control-plane-client.js').createControlPlaneHttpClient
>
type GeneratedWebSocketClient = ReturnType<
  typeof import('../../apps/client/src/generated/control-plane-client.js').createControlPlaneWebSocketClient
>

type GeneratedContractsFitPublicPorts = [
  Expect<Extends<AuthSessionResponse, ControlPlaneAuthSession>>,
  Expect<Extends<CommandRequest, ControlPlaneCommandRequest>>,
  Expect<Extends<CommandAcceptedResponse, ControlPlaneCommandResponse>>,
  Expect<Extends<CommandCompletedResponse, ControlPlaneCommandResponse>>,
  Expect<Extends<QueryRequest, ControlPlaneQueryRequest>>,
  Expect<Extends<QueryResultResponse, ControlPlaneQueryResponse>>,
  Expect<Extends<ControlPlaneWebSocketEventFrame, PublicEventFrame>>,
  Expect<Extends<Scope, PublicScope>>,
  Expect<Extends<ErrorDetails, PublicErrorDetails>>,
  Expect<Extends<PublicErrorDetails, ErrorDetails>>,
  Expect<Extends<ControlPlaneClient, ProductQueryCachePort>>,
  Expect<Extends<Parameters<GeneratedHttpClient['submitCommand']>[0], CommandRequest>>,
  Expect<Extends<CommandRequest, Parameters<GeneratedHttpClient['submitCommand']>[0]>>,
  Expect<Extends<Awaited<ReturnType<GeneratedHttpClient['submitQuery']>>, QueryResultResponse>>,
  Expect<Extends<QueryResultResponse, Awaited<ReturnType<GeneratedHttpClient['submitQuery']>>>>,
  Expect<Extends<Parameters<GeneratedWebSocketClient['subscribe']>[0],
    ControlPlaneWebSocketSubscriptionId>>,
  Expect<Extends<ControlPlaneWebSocketSubscriptionId,
    Parameters<GeneratedWebSocketClient['subscribe']>[0]>>,
  Expect<Extends<Parameters<GeneratedWebSocketClient['subscribe']>[1],
    ControlPlaneWebSocketSubscription>>,
  Expect<Extends<ControlPlaneWebSocketSubscription,
    Parameters<GeneratedWebSocketClient['subscribe']>[1]>>,
  Expect<Extends<
    Awaited<ReturnType<NonNullable<
      Parameters<ControlPlaneClient['subscribe']>[0]['onResetRequired']
    >>>,
    EventReadCursor
  >>,
  Expect<Extends<
    EventReadCursor,
    Awaited<ReturnType<NonNullable<
      Parameters<ControlPlaneClient['subscribe']>[0]['onResetRequired']
    >>>
  >>,
]

export type BrowserControlAssignabilityGate = GeneratedContractsFitPublicPorts
