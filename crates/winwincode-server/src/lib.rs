// SPDX-License-Identifier: Apache-2.0

//! Standalone public server for the embedded `WinWinCode` Control Plane.

mod application;
mod auth_session;
mod client_candidates;
mod client_connections;
mod client_exchange;
mod client_occupancy;
mod client_repositories;
mod client_sessions;
mod config;
mod dispatcher;
mod durable_event_hub;
mod login_rate_limiter;
mod model_authority;
mod password_hash;
mod preview;
mod provider_onboarding;
mod remote_worker_transport;
mod runtime;
mod server;
mod transport;
mod user_accounts;
mod worker_session_credentials;

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
    worker_stop_message,
};
pub use winwincode_api::generated::{AuthSessionRequest, AuthSessionResponse};

pub use config::{ServerConfig, ServerConfigError, ServerTls};
pub use dispatcher::{
    CommandDispatchResponse, CommandFamily, GeneratedContractDispatcher, QueryFamily,
    TypedControlPlaneApiPort,
};
pub use durable_event_hub::{
    CommittedEventContext, DurableEventHub, DurableEventHubClock, DurableEventHubConfig,
    DurableEventHubError, DurableEventHubErrorCode, DurableEventPublisher,
};
pub use model_authority::{
    LocalModelRoute, configure_local_model_authority, credential_create_command,
};
pub use provider_onboarding::{
    ConnectionProbe, ConnectionTestReport, CreateCredentialRequest, CredentialReferenceOnboarded,
    CredentialRotated, EstablishRouteRequest, HttpsConnectionProbe, ModelRouteEstablished,
    OnboardProviderRequest, OnboardingSecretStore, ProbeOutcome, ProviderOnboarded,
    ProviderOnboardingError, ProviderOnboardingErrorKind, ProviderOnboardingService,
    RotateCredentialRequest, TestConnectionRequest,
};
pub use remote_worker_transport::{
    FileRemoteWorkerAuthenticator, ProductionRemoteWorkerExchange, RemoteWorkerExchangePort,
    RemoteWorkerTransportError,
};
pub use runtime::{
    HealthyRuntimeHealth, LocalRuntimeScheduler, LocalRuntimeSupervisor,
    RepositoryRuntimeScheduler, RuntimeControlOutbound, RuntimeHealthHandle, RuntimeHealthPort,
    RuntimeSupervisorError, RuntimeSupervisorErrorKind, ServerExecutionPortCore,
    ServerExecutionPortError, ServerExecutionPortErrorKind,
};
pub use server::{RunningServer, ServerError, start_server, start_server_with_remote_worker};
pub use transport::{
    ApiError, AuthError, AuthenticatedPrincipal, ControlPlaneApiPort, EventSubscription,
    RequestAuthenticator, TransportCredentials,
};
pub use user_accounts::{
    CredentialRejection, UserAccountService, UserAccountServiceError, UserAccountServiceErrorKind,
};
pub use worker_session_credentials::{
    CredentialMaterial, CredentialRotationReceipt, DEFAULT_CREDENTIAL_TTL,
    WorkerSessionCredentialError, WorkerSessionCredentialErrorKind, WorkerSessionCredentialPolicy,
    WorkerSessionCredentialService, issue_credential_material,
};
