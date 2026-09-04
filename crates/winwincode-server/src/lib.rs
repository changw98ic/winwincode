// SPDX-License-Identifier: Apache-2.0

//! Standalone public server for the embedded `WinWinCode` Control Plane.

mod application;
mod auth_session;
mod client_exchange;
mod config;
mod dispatcher;
mod durable_event_hub;
mod enterprise_identity_protocol;
mod enterprise_management;
mod identity_authenticator;
mod remote_worker_transport;
mod runtime;
mod server;
mod transport;

pub use application::{
    StandaloneApplicationClock, StandaloneControlPlaneApplication, SystemStandaloneApplicationClock,
};
pub use auth_session::{
    AuthSessionBootstrap, AuthSessionConfig, AuthSessionError, ExternalIdentitySessionIssuer,
    ExternalIdentitySessionResult, SqliteAuthSessionManager,
};
pub use client_exchange::{
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangeError, ClientExchangeErrorKind,
    ClientExchangePort,
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
pub use enterprise_identity_protocol::EnterpriseIdentityProtocolApplication;
pub use enterprise_management::{
    EnterpriseIdentityManagementApplication, EnterpriseManagementApplicationPort,
    EnterpriseRbacManagementApplication, UnavailableEnterpriseManagementApplication,
};
pub use identity_authenticator::EnterpriseRequestAuthenticator;
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
