// SPDX-License-Identifier: Apache-2.0

mod application;
mod auth_session;
mod client_candidates;
mod client_connections;
mod client_exchange;
mod client_occupancy;
mod client_repositories;
mod client_sessions;
mod config;
mod device_providers;
mod dispatcher;
mod durable_event_hub;
mod login_rate_limiter;
mod password_hash;
mod preview;
mod remote_worker_transport;
mod runtime;
mod server;
mod transport;
mod user_accounts;

pub use application::{
    StandaloneApplicationClock, StandaloneControlPlaneApplication, SystemStandaloneApplicationClock,
};
pub use auth_session::{
    AuthSessionBootstrap, AuthSessionConfig, AuthSessionError, OwnerInitializationHook,
    SqliteAuthSessionManager,
};
pub use client_candidates::{
    ClientCandidatesApplication, ClientCandidatesConfig, ClientCandidatesError,
    ClientCandidatesErrorKind,
};
pub use client_connections::{
    ClientConnectionsApplication, ClientConnectionsConfig, ClientConnectionsError,
    ClientConnectionsErrorKind,
};
pub use client_exchange::{
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangeError, ClientExchangeErrorKind,
    ClientExchangePort,
};
pub use client_occupancy::{
    ClientOccupancyApplication, ClientOccupancyConfig, ClientOccupancyError,
    ClientOccupancyErrorKind, OfflineSweepOutcome,
};
pub use client_repositories::{
    ClientRepositoriesApplication, ClientRepositoriesError, ClientRepositoriesErrorKind,
};
pub use client_sessions::{
    ClientSessionsApplication, ClientSessionsConfig, ClientSessionsError, ClientSessionsErrorKind,
    enqueue_managed_app_command, managed_app_command_message, worker_stop_message,
};
pub use winwincode_api::generated::{AuthSessionRequest, AuthSessionResponse};

pub use config::{ServerConfig, ServerConfigError};
pub use dispatcher::{
    CommandDispatchResponse, CommandFamily, GeneratedContractDispatcher, QueryFamily,
    TypedControlPlaneApiPort,
};
pub use durable_event_hub::{
    CommittedEventContext, DurableEventHub, DurableEventHubClock, DurableEventHubConfig,
    DurableEventHubError, DurableEventHubErrorCode, DurableEventPublisher,
};
pub use remote_worker_transport::{
    ProductionRemoteWorkerExchange, RemoteWorkerExchangePort, RemoteWorkerTransportError,
};
pub use runtime::{
    HealthyRuntimeHealth, RepositoryRuntimeScheduler, RuntimeControlOutbound, RuntimeHealthPort,
    RuntimeSupervisorError, RuntimeSupervisorErrorKind, ServerExecutionPortCore,
    ServerExecutionPortError, ServerExecutionPortErrorKind,
};
#[cfg(feature = "local-worker")]
pub use runtime::{LocalRuntimeScheduler, LocalRuntimeSupervisor, RuntimeHealthHandle};
pub use server::{RunningServer, ServerError, start_server, start_server_with_remote_worker};
pub use transport::{
    ApiError, AuthError, AuthenticatedPrincipal, ControlPlaneApiPort, EventSubscription,
    RequestAuthenticator, TransportCredentials,
};
pub use user_accounts::{
    CredentialRejection, UserAccountService, UserAccountServiceError, UserAccountServiceErrorKind,
};
