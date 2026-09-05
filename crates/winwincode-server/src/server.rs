// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, COOKIE, ORIGIN,
    SET_COOKIE, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use futures::StreamExt;
use serde_json::{Value, json};
use winwincode_api::generated::{
    Actor, AuthSessionRequest, ControlPlaneWebSocketProtocolErrorFrame,
    ControlPlaneWebSocketProtocolErrorFrameTypeValue, ControlPlaneWebSocketServerFrame, Error,
    ErrorDetailValue, ErrorDetails, ErrorEnvelope, RetryableError, RetryableErrorCode,
    TerminalError, TerminalErrorCode, UserActor, UserActorKind,
};
use winwincode_control_plane::{CredentialLeakGate, CredentialOutputBoundary};
use winwincode_domain::{
    RequestId, Revision, SchemaVersion, UserAccount, UserAccountRole, UserAccountState, UserId,
};

use crate::application::{StandaloneApplicationClock, SystemStandaloneApplicationClock};
use crate::auth_session::{
    AuthSessionError, SqliteAuthSessionManager, cleared_session_cookie_header,
};
use crate::client_connections::ClientConnectionsApplication;
use crate::client_connections::ClientConnectionsConfig;
use crate::client_connections::ClientConnectionsError;
use crate::client_connections::ClientConnectionsErrorKind;
use crate::client_exchange::ClientExchangePort;
use crate::client_occupancy::ClientOccupancyApplication;
use crate::client_occupancy::ClientOccupancyConfig;
use crate::client_occupancy::ClientOccupancyError;
use crate::client_occupancy::ClientOccupancyErrorKind;
use crate::client_repositories::ClientRepositoriesApplication;
use crate::client_repositories::ClientRepositoriesErrorKind;
use crate::config::{ServerConfig, ServerTls};
use crate::enterprise_identity_protocol::{
    EnterpriseIdentityProtocolApplication, router as enterprise_identity_router,
};
use crate::remote_worker_transport::RemoteWorkerExchangePort;
use crate::transport::{
    ApiError, AuthenticatedPrincipal, ControlPlaneApiPort, RequestAuthenticator,
    TransportCredentials,
};
use crate::user_accounts::{UserAccountServiceErrorKind, generate_temporary_password};

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const FIRST_SUBSCRIPTION_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_REVALIDATION_INTERVAL: Duration = Duration::from_millis(250);
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";
static NEXT_DIAGNOSTIC_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
type ResponseResult<T> = Result<T, BoundaryError>;

struct BoundaryError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    origin: Option<HeaderValue>,
    request_id: Option<RequestId>,
}

impl BoundaryError {
    const fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        origin: Option<HeaderValue>,
    ) -> Self {
        Self {
            status,
            code,
            message,
            origin,
            request_id: None,
        }
    }

    fn with_request_id(mut self, request_id: Option<RequestId>) -> Self {
        self.request_id = request_id;
        self
    }

    fn with_origin(mut self, origin: HeaderValue) -> Self {
        self.origin = Some(origin);
        self
    }

    fn into_response(self) -> Response {
        error_response(
            self.status,
            self.code,
            self.message,
            self.request_id,
            self.origin.as_ref(),
        )
    }
}

#[derive(Clone)]
struct ServerState {
    config: Arc<ServerConfig>,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<dyn ControlPlaneApiPort>,
    remote_worker: Option<Arc<dyn RemoteWorkerExchangePort>>,
    client_exchange: Option<Arc<dyn ClientExchangePort>>,
    client_connections: Option<Arc<ClientConnectionsApplication>>,
    client_occupancy: Option<Arc<ClientOccupancyApplication>>,
    client_repositories: Option<Arc<ClientRepositoriesApplication>>,
}

/// Running standalone listener with deterministic graceful shutdown.
pub struct RunningServer {
    local_address: SocketAddr,
    public_url: String,
    shutdown_grace: Duration,
    handle: Handle<SocketAddr>,
    task: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    background_tasks: Vec<tokio::task::JoinHandle<()>>,
    api: Arc<dyn ControlPlaneApiPort>,
}

impl RunningServer {
    #[must_use]
    pub const fn local_address(&self) -> SocketAddr {
        self.local_address
    }

    #[must_use]
    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// Stops accepting new HTTP work and waits for the listener task to drain.
    ///
    /// The embedded application remains open so a runtime supervisor can drain
    /// Worker commands, interaction acknowledgements, and terminal outbox work
    /// before the application state is closed.
    ///
    /// # Errors
    ///
    /// Reports a listener task or join failure after the listener no longer
    /// accepts work.
    pub async fn shutdown_listener(&mut self) -> Result<(), ServerError> {
        for background in self.background_tasks.drain(..) {
            background.abort();
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        self.handle.graceful_shutdown(Some(self.shutdown_grace));
        task.await
            .map_err(|_| ServerError::new("server task did not shut down cleanly"))?
    }

    /// Closes the embedded application after the runtime has drained.
    ///
    /// # Errors
    ///
    /// Reports an application shutdown failure.
    pub fn shutdown_application(self) -> Result<(), ServerError> {
        self.api
            .shutdown()
            .map_err(|_| ServerError::new("Control Plane shutdown failed"))
    }

    /// Drains connections, stops the listener, and then closes the embedded
    /// Control Plane lifecycle.
    ///
    /// # Errors
    ///
    /// Reports a server task, join, or application shutdown failure after the
    /// listener no longer accepts work.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        let listener_result = self.shutdown_listener().await;
        let application_result = self.shutdown_application();
        listener_result.and(application_result)
    }
}

/// Start the one public HTTP/HTTPS origin.
///
/// # Errors
///
/// Fails if TLS cannot load, the listener cannot bind, or it does not report a
/// listening address within ten seconds.
pub async fn start_server(
    config: ServerConfig,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<dyn ControlPlaneApiPort>,
    enterprise_identity: Option<Arc<EnterpriseIdentityProtocolApplication>>,
) -> Result<RunningServer, ServerError> {
    start_server_with_remote_worker(
        config,
        auth_sessions,
        authenticator,
        api,
        enterprise_identity,
        None,
        None,
    )
    .await
}

/// Starts the public origin with the private authenticated remote Worker
/// exchange attached to the same TLS listener.
///
/// # Errors
///
/// Fails closed when remote Worker mode is configured without TLS, or when
/// the listener cannot start.
pub async fn start_server_with_remote_worker(
    config: ServerConfig,
    auth_sessions: Arc<SqliteAuthSessionManager>,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<dyn ControlPlaneApiPort>,
    enterprise_identity: Option<Arc<EnterpriseIdentityProtocolApplication>>,
    remote_worker: Option<Arc<dyn RemoteWorkerExchangePort>>,
    client_exchange: Option<Arc<dyn ClientExchangePort>>,
) -> Result<RunningServer, ServerError> {
    if remote_worker.is_some() && matches!(config.tls(), ServerTls::Disabled) {
        return Err(ServerError::new(
            "remote Worker exchange requires the Server TLS listener",
        ));
    }
    let config = Arc::new(config);
    let client_connections = match &client_exchange {
        Some(_) => {
            let application = ClientConnectionsApplication::open(
                config.data_directory(),
                &ClientConnectionsConfig::default(),
            )
            .map_err(|error| ServerError::new(error.to_string()))?;
            Some(Arc::new(application))
        }
        // Servers without a Client surface keep the connect routes absent.
        None => None,
    };
    let client_occupancy = match &client_exchange {
        Some(_) => {
            let application = ClientOccupancyApplication::open(
                config.data_directory(),
                &ClientOccupancyConfig::default(),
            )
            .map_err(|error| ServerError::new(error.to_string()))?;
            Some(Arc::new(application))
        }
        // Servers without a Client surface keep the occupancy routes absent.
        None => None,
    };
    // Servers without a Client surface keep the repository directory absent.
    let client_repositories = client_exchange
        .as_ref()
        .map(|_| Arc::new(ClientRepositoriesApplication::open(config.data_directory())));
    // The periodic offline sweep projects heartbeat-stale devices to
    // `offline` and their active occupancy leases to `recovery_pending`
    // (plan 12.5). Each iteration opens and closes its own storage
    // connection; the task holds no lock across awaits.
    let background_tasks = match &client_occupancy {
        Some(application) => {
            let sweep_application = Arc::clone(application);
            let sweep_interval = ClientOccupancyConfig::default().sweep_interval;
            vec![tokio::spawn(async move {
                loop {
                    tokio::time::sleep(sweep_interval).await;
                    let _ = sweep_application.run_server_sweep();
                }
            })]
        }
        None => Vec::new(),
    };
    let state = ServerState {
        config: Arc::clone(&config),
        auth_sessions,
        authenticator,
        api: Arc::clone(&api),
        remote_worker,
        client_exchange,
        client_connections,
        client_occupancy,
        client_repositories,
    };
    let router = router(state, enterprise_identity);
    let handle = Handle::new();
    let task = spawn_listener(&config, router, handle.clone()).await?;
    let local_address =
        match tokio::time::timeout(Duration::from_secs(10), handle.listening()).await {
            Ok(Some(address)) => address,
            Ok(None) => {
                let result = task
                    .await
                    .map_err(|_| ServerError::new("server task failed before binding"))?;
                return Err(result
                    .err()
                    .unwrap_or_else(|| ServerError::new("server stopped before binding")));
            }
            Err(_) => {
                handle.shutdown();
                let _ = task.await;
                return Err(ServerError::new("server listener timed out while binding"));
            }
        };
    Ok(RunningServer {
        local_address,
        public_url: config.public_url().to_owned(),
        shutdown_grace: config.shutdown_grace(),
        handle,
        task: Some(task),
        background_tasks,
        api,
    })
}

async fn spawn_listener(
    config: &ServerConfig,
    router: Router,
    handle: Handle<SocketAddr>,
) -> Result<tokio::task::JoinHandle<Result<(), ServerError>>, ServerError> {
    let address = config.bind_address();
    let make_service = router.into_make_service_with_connect_info::<SocketAddr>();
    let task = match config.tls() {
        ServerTls::Disabled => tokio::spawn(async move {
            axum_server::bind(address)
                .handle(handle)
                .serve(make_service)
                .await
                .map_err(|_| ServerError::new("public HTTP listener failed"))
        }),
        ServerTls::Pem {
            certificate_path,
            private_key_path,
        } => {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let tls = RustlsConfig::from_pem_file(certificate_path, private_key_path)
                .await
                .map_err(|_| ServerError::new("TLS certificate or private key could not load"))?;
            tokio::spawn(async move {
                axum_server::bind_rustls(address, tls)
                    .handle(handle)
                    .serve(make_service)
                    .await
                    .map_err(|_| ServerError::new("public HTTPS listener failed"))
            })
        }
    };
    Ok(task)
}

fn router(
    state: ServerState,
    enterprise_identity: Option<Arc<EnterpriseIdentityProtocolApplication>>,
) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route(
            "/api/v1/auth/session",
            get(get_auth_session)
                .post(create_auth_session)
                .delete(close_auth_session)
                .options(preflight),
        )
        .route(
            "/api/v1/server/initialization",
            get(server_initialization).options(preflight),
        )
        .route("/api/v1/commands", post(command).options(preflight))
        .route("/api/v1/queries", post(query).options(preflight))
        .route("/api/v1/events", get(events).options(preflight))
        .route("/api/v1/users", post(create_user).options(preflight))
        .route(
            "/api/v1/users/state",
            post(set_user_state).options(preflight),
        )
        .route(
            "/api/v1/users/password",
            post(reset_password).options(preflight),
        )
        .route(
            "/internal/v1/execution-port/exchange",
            post(remote_worker_exchange),
        )
        .route(
            "/internal/v1/client/exchange",
            post(client_control_exchange),
        )
        .route("/api/v1/clients", get(list_clients).options(preflight))
        .route(
            "/api/v1/clients/connections",
            post(create_client_connection).options(preflight),
        )
        .route(
            "/api/v1/clients/grants/revoke",
            post(revoke_client_grant).options(preflight),
        )
        .route(
            "/api/v1/clients/occupancy",
            post(create_client_occupancy)
                .delete(release_client_occupancy)
                .options(preflight),
        )
        .route(
            "/api/v1/clients/occupancy/force-release",
            post(force_release_client_occupancy).options(preflight),
        )
        .route(
            "/api/v1/clients/{client_id}/occupancy",
            get(client_occupancy_status).options(preflight),
        )
        .route(
            "/api/v1/repositories",
            get(list_repositories).options(preflight),
        )
        .fallback(not_found)
        .with_state(state);
    let router = enterprise_identity.map_or(router.clone(), |application| {
        router.merge(enterprise_identity_router(application))
    });
    router.layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
}

async fn remote_worker_exchange(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(exchange) = &state.remote_worker else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(credential) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // Registry ordering compares the canonical millisecond `Instant` text.
    // Reuse the application clock so a registration and its placement cannot
    // acquire different fractional-second widths at the HTTP boundary.
    let now = SystemStandaloneApplicationClock.now_instant();
    match exchange.exchange(credential.as_bytes().to_vec(), &body, now) {
        Ok(response) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            response,
        )
            .into_response(),
        Err(error) => {
            if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                eprintln!("remote Worker exchange error: {error}");
            }
            StatusCode::UNAUTHORIZED.into_response()
        }
    }
}

async fn client_control_exchange(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(exchange) = &state.client_exchange else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if headers.get_all(AUTHORIZATION).iter().count() > 1 {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let credential = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .map(str::as_bytes);
    // Registry ordering compares the canonical millisecond `Instant` text.
    // Reuse the application clock so a registration and its placement cannot
    // acquire different fractional-second widths at the HTTP boundary.
    let now = SystemStandaloneApplicationClock.now_instant();
    match exchange.exchange(credential.map(<[u8]>::to_vec), &body, now) {
        Ok(response) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            response,
        )
            .into_response(),
        Err(error) => {
            if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                eprintln!("client exchange error: {error}");
            }
            if error.is_authentication() {
                // One uniform rejection: wrong, missing, or unknown-node
                // credentials are indistinguishable.
                StatusCode::UNAUTHORIZED.into_response()
            } else if error.is_invalid_request() {
                StatusCode::BAD_REQUEST.into_response()
            } else {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        }
    }
}

/// The signed-in user's Client directory: one `DeviceSummary` card per
/// granted Client (§16.4).
async fn list_clients(State(state): State<ServerState>, headers: HeaderMap, uri: Uri) -> Response {
    let Some(application) = &state.client_connections else {
        return not_found().await;
    };
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    match application.list_clients(&user_id.0) {
        Ok(body) => json_response(StatusCode::OK, body, origin.as_ref()),
        Err(error) => connect_flow_error(&error, origin.as_ref()),
    }
}

/// The signed-in user's repository directory for one Client (REPO-100.4):
/// the dual-authorized repository list (plan 13.4), projected field-by-field
/// onto the repo-ui facade shape (REPO-100.3).
///
/// Unlike every other browser route the Client identity arrives in the URL
/// query (`?clientId=…`), so the shared `authorize` query rejection does not
/// apply here; every other authentication step is identical.
async fn list_repositories(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(application) = &state.client_repositories else {
        return not_found().await;
    };
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    let credentials = match extract_credentials(&headers) {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };
    let Ok(principal) = state.authenticator.authenticate(&credentials) else {
        return BoundaryError::new(
            StatusCode::UNAUTHORIZED,
            "AUTHENTICATION_REQUIRED",
            "authentication failed",
            origin,
        )
        .into_response();
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    match application.list(&user_id.0, uri.query()) {
        Ok(body) => json_response(StatusCode::OK, body, origin.as_ref()),
        Err(error) => {
            let (status, code, message) = match error.kind() {
                ClientRepositoriesErrorKind::InvalidRequest => (
                    StatusCode::BAD_REQUEST,
                    "INVALID_REQUEST",
                    "repository list request is invalid",
                ),
                ClientRepositoriesErrorKind::ClientNotFound => (
                    StatusCode::NOT_FOUND,
                    "CLIENT_NOT_FOUND",
                    "no client matches the requested id",
                ),
                ClientRepositoriesErrorKind::Unavailable => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "SERVICE_UNAVAILABLE",
                    "repository directory service is unavailable",
                ),
            };
            connect_error_response(status, code, message, origin.as_ref())
        }
    }
}

/// One add-Client attempt (plan 11.4): bounded wait for the Device Client
/// challenge acknowledgement, then the atomic consume-and-grant.
#[allow(clippy::too_many_lines)]
async fn create_client_connection(
    State(state): State<ServerState>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    request: Request<Body>,
) -> Response {
    let Some(application) = &state.client_connections else {
        return not_found().await;
    };
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            origin,
        )
        .into_response();
    }
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    let client_ip = client.ip().to_string();
    match application.connect(&user_id.0, &client_ip, &body).await {
        Ok(body) => json_response(StatusCode::CREATED, body, origin.as_ref()),
        Err(error) => connect_flow_error(&error, origin.as_ref()),
    }
}

/// Immediate grant revocation (contract 3): the holder or an Owner.
async fn revoke_client_grant(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
    request: Request<Body>,
) -> Response {
    let Some(application) = &state.client_connections else {
        return not_found().await;
    };
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            origin,
        )
        .into_response();
    }
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    let acting_is_owner = state
        .auth_sessions
        .accounts()
        .find(&user_id)
        .ok()
        .flatten()
        .is_some_and(|account| {
            account.role == UserAccountRole::Owner && account.state == UserAccountState::Active
        });
    match application.revoke(&user_id.0, acting_is_owner, &body) {
        Ok(body) => json_response(StatusCode::OK, body, origin.as_ref()),
        Err(error) => connect_flow_error(&error, origin.as_ref()),
    }
}

/// Maps one connect flow failure onto its §16.3 wire error code.
fn connect_flow_error(error: &ClientConnectionsError, origin: Option<&HeaderValue>) -> Response {
    let (status, code, message) = match error.kind() {
        ClientConnectionsErrorKind::InvalidRequest => (
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "connect request is invalid",
        ),
        ClientConnectionsErrorKind::ClientNotFound => (
            StatusCode::NOT_FOUND,
            "CLIENT_NOT_FOUND",
            "no client matches the requested id",
        ),
        ClientConnectionsErrorKind::ClientOffline => (
            StatusCode::CONFLICT,
            "CLIENT_OFFLINE",
            "the client is not online",
        ),
        ClientConnectionsErrorKind::ConnectCodeInvalid => (
            StatusCode::CONFLICT,
            "CONNECT_CODE_INVALID",
            "the connection code is not valid for this client",
        ),
        ClientConnectionsErrorKind::ConnectCodeExpired => (
            StatusCode::CONFLICT,
            "CONNECT_CODE_EXPIRED",
            "the connection code is no longer usable",
        ),
        ClientConnectionsErrorKind::ClientConnectionsForbidden => (
            StatusCode::CONFLICT,
            "CLIENT_CONNECTIONS_FORBIDDEN",
            "the client no longer accepts new connections",
        ),
        ClientConnectionsErrorKind::ClientLocked => (
            StatusCode::CONFLICT,
            "CLIENT_LOCKED",
            "the client is locked",
        ),
        ClientConnectionsErrorKind::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "connect attempts are rate limited",
        ),
        ClientConnectionsErrorKind::PermissionDenied => (
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "only the grant holder or an Owner may revoke a grant",
        ),
        ClientConnectionsErrorKind::ResourceNotFound => (
            StatusCode::NOT_FOUND,
            "RESOURCE_NOT_FOUND",
            "no active grant matches the request",
        ),
        ClientConnectionsErrorKind::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "client connect service is unavailable",
        ),
    };
    connect_error_response(status, code, message, origin)
}

/// One occupancy claim (plan 12.2): bounded wait for the Device Client
/// occupancy acknowledgement, then the `occupied` holder view.
async fn create_client_occupancy(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
    request: Request<Body>,
) -> Response {
    let Some(application) = &state.client_occupancy else {
        return not_found().await;
    };
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            origin,
        )
        .into_response();
    }
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    match application.claim(&user_id.0, &body).await {
        Ok(body) => json_response(StatusCode::CREATED, body, origin.as_ref()),
        Err(error) => occupancy_flow_error(&error, origin.as_ref()),
    }
}

/// One occupancy release (plan 12.4): the holder releases, drains, or
/// cancels-and-releases; a release while still `reserving` withdraws the
/// claim.
async fn release_client_occupancy(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
    request: Request<Body>,
) -> Response {
    let Some(application) = &state.client_occupancy else {
        return not_found().await;
    };
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            origin,
        )
        .into_response();
    }
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    match application.release(&user_id.0, &body) {
        Ok(body) => json_response(StatusCode::OK, body, origin.as_ref()),
        Err(error) => occupancy_flow_error(&error, origin.as_ref()),
    }
}

/// The Owner-only safe cleanup of a recovery-pending lease whose window has
/// passed (plan 12.5), with the higher force-fence token going downlink.
async fn force_release_client_occupancy(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
    request: Request<Body>,
) -> Response {
    let Some(application) = &state.client_occupancy else {
        return not_found().await;
    };
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            origin,
        )
        .into_response();
    }
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_owner(&state, &headers, &uri) {
        return error.into_response();
    }
    match application.force_release(&body) {
        Ok(body) => json_response(StatusCode::OK, body, origin.as_ref()),
        Err(error) => occupancy_flow_error(&error, origin.as_ref()),
    }
}

/// The occupancy projection of one Client: full view for the holder, the
/// `occupied-by-other` privacy projection for everyone else (plan §16.4).
async fn client_occupancy_status(
    State(state): State<ServerState>,
    axum::extract::Path(client_id): axum::extract::Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(application) = &state.client_occupancy else {
        return not_found().await;
    };
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let Some(user_id) = principal.actor_user_id() else {
        return connect_error_response(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "a signed-in user is required",
            origin.as_ref(),
        );
    };
    match application.status(&user_id.0, &client_id) {
        Ok(body) => json_response(StatusCode::OK, body, origin.as_ref()),
        Err(error) => occupancy_flow_error(&error, origin.as_ref()),
    }
}

/// Maps one occupancy flow failure onto its central occupancy wire error
/// code.
fn occupancy_flow_error(error: &ClientOccupancyError, origin: Option<&HeaderValue>) -> Response {
    let (status, code, message) = match error.kind() {
        ClientOccupancyErrorKind::InvalidRequest => (
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "occupancy request is invalid",
        ),
        ClientOccupancyErrorKind::ConfirmationRequired => (
            StatusCode::BAD_REQUEST,
            "CONFIRMATION_REQUIRED",
            "cancel_and_release requires the explicit confirm flag",
        ),
        ClientOccupancyErrorKind::ClientNotFound => (
            StatusCode::NOT_FOUND,
            "CLIENT_NOT_FOUND",
            "no client matches the requested id",
        ),
        ClientOccupancyErrorKind::ClientOffline => (
            StatusCode::CONFLICT,
            "CLIENT_OFFLINE",
            "the client is not online",
        ),
        ClientOccupancyErrorKind::ClientLocked => (
            StatusCode::CONFLICT,
            "CLIENT_LOCKED",
            "the client is locked",
        ),
        ClientOccupancyErrorKind::ClientConnectionsForbidden => (
            StatusCode::CONFLICT,
            "CLIENT_CONNECTIONS_FORBIDDEN",
            "the client no longer accepts new occupancy",
        ),
        ClientOccupancyErrorKind::AccessDenied => (
            StatusCode::FORBIDDEN,
            "ACCESS_DENIED",
            "an active use grant on the client is required",
        ),
        ClientOccupancyErrorKind::OccupiedByOther => (
            StatusCode::CONFLICT,
            "OCCUPIED_BY_OTHER",
            "the client is occupied by another user",
        ),
        ClientOccupancyErrorKind::CapacityExhausted => (
            StatusCode::CONFLICT,
            "CAPACITY_EXHAUSTED",
            "the client has no free worker-session slot",
        ),
        ClientOccupancyErrorKind::OccupancyRejected => (
            StatusCode::CONFLICT,
            "OCCUPANCY_REJECTED",
            "the device rejected the occupancy offer",
        ),
        ClientOccupancyErrorKind::OccupancyAckTimeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "OCCUPANCY_ACK_TIMEOUT",
            "the device did not confirm the occupancy offer in time",
        ),
        ClientOccupancyErrorKind::OccupancyRecoveryPending => (
            StatusCode::CONFLICT,
            "OCCUPANCY_RECOVERY_PENDING",
            "the recovery window is still open",
        ),
        ClientOccupancyErrorKind::PermissionDenied => (
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "the acting user may not perform this occupancy change",
        ),
        ClientOccupancyErrorKind::ResourceNotFound => (
            StatusCode::NOT_FOUND,
            "RESOURCE_NOT_FOUND",
            "no active occupancy matches the request",
        ),
        ClientOccupancyErrorKind::WrongState => (
            StatusCode::CONFLICT,
            "WRONG_STATE",
            "the occupancy lease state refuses the requested change",
        ),
        ClientOccupancyErrorKind::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "occupancy claims are rate limited",
        ),
        ClientOccupancyErrorKind::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "client occupancy service is unavailable",
        ),
    };
    connect_error_response(status, code, message, origin)
}

/// The wire error shape of the connect surface: `error.code` carries the
/// §16.3 taxonomy the browser facade translates.
fn connect_error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    origin: Option<&HeaderValue>,
) -> Response {
    json_response(
        status,
        json!({
            "error": {
                "code": code,
                "message": message,
                "retryable": status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
            },
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        }),
        origin,
    )
}

async fn health(State(state): State<ServerState>) -> Response {
    if state.api.health().is_err() {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "status": "unavailable",
                "publicUrl": state.config.public_url(),
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "serverVersion": env!("CARGO_PKG_VERSION"),
            }),
            None,
        );
    }
    json_response(
        StatusCode::OK,
        json!({
            "status": "ready",
            "publicUrl": state.config.public_url(),
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "serverVersion": env!("CARGO_PKG_VERSION"),
            "endpoints": [
                "/api/v1/auth/session",
                "/api/v1/commands",
                "/api/v1/queries",
                "/api/v1/events"
            ]
        }),
        None,
    )
}

async fn get_auth_session(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let origin = match required_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            Some(origin),
        )
        .into_response();
    }
    let credentials = match extract_credentials(&headers) {
        Ok(credentials) => credentials,
        Err(error) => return error.with_origin(origin).into_response(),
    };
    let session = match state.auth_sessions.current(&credentials) {
        Ok(session) => session,
        Err(error) => return auth_session_error(error, Some(origin)).into_response(),
    };
    let value = serde_json::to_value(session).expect("auth response is serializable");
    let mut response = json_response(StatusCode::OK, value, Some(&origin));
    prevent_auth_session_caching(&mut response);
    response
}

/// The one unauthenticated initialization probe. It publishes exactly one
/// boolean — whether a first Owner already exists — and nothing else, so the
/// login page can show or hide the first-time bootstrap entry.
async fn server_initialization(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let origin = match required_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            Some(origin),
        )
        .into_response();
    }
    let mut response = json_response(
        StatusCode::OK,
        json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "initialized": state.auth_sessions.initialized_owner().is_some(),
        }),
        Some(&origin),
    );
    prevent_auth_session_caching(&mut response);
    response
}

async fn create_auth_session(
    State(state): State<ServerState>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match required_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            Some(origin),
        )
        .into_response();
    }
    let body = match parse_json_body(request, Some(origin.clone())).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let login = match parse_login_request(&body, Some(origin.clone())) {
        Ok(login) => login,
        Err(error) => return error.into_response(),
    };
    let credentials = match extract_credentials(&headers) {
        Ok(credentials) => credentials,
        Err(error) => return error.with_origin(origin).into_response(),
    };
    // A Bearer credential carries the one-time bootstrap proof for first
    // initialization; otherwise the request is a username + password login.
    let issued = if credentials.bearer().is_some() {
        state
            .auth_sessions
            .initialize(&credentials, &login.username, &login.password)
    } else {
        let client = client.ip().to_string();
        state
            .auth_sessions
            .login(&credentials, &client, &login.username, &login.password)
    };
    let issued = match issued {
        Ok(issued) => issued,
        Err(error) => return auth_session_error(error, Some(origin)).into_response(),
    };
    let Ok(cookie_text) = issued.set_cookie_header() else {
        return auth_session_error(AuthSessionError::response_encoding(), Some(origin))
            .into_response();
    };
    let Ok(cookie) = HeaderValue::from_str(&cookie_text) else {
        return auth_session_error(AuthSessionError::response_encoding(), Some(origin))
            .into_response();
    };
    let Ok(auth_response) = issued.response() else {
        return auth_session_error(AuthSessionError::response_encoding(), Some(origin))
            .into_response();
    };
    let value = serde_json::to_value(auth_response).expect("auth response is serializable");
    let mut response = json_response(StatusCode::CREATED, value, Some(&origin));
    prevent_auth_session_caching(&mut response);
    response.headers_mut().insert(SET_COOKIE, cookie);
    response
}

async fn close_auth_session(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match required_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if uri.query().is_some() {
        return BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            Some(origin),
        )
        .into_response();
    }
    let body = match parse_json_body(request, Some(origin.clone())).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = parse_auth_session_request(body, Some(origin.clone())) {
        return error.into_response();
    }
    let credentials = match extract_credentials(&headers) {
        Ok(credentials) => credentials,
        Err(error) => return error.with_origin(origin).into_response(),
    };
    if let Err(error) = state.auth_sessions.revoke(&credentials) {
        return auth_session_error(error, Some(origin)).into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    apply_cors(response.headers_mut(), &origin);
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_static(cleared_session_cookie_header()),
    );
    prevent_auth_session_caching(&mut response);
    response
}

fn prevent_auth_session_caching(response: &mut Response) {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn parse_auth_session_request(
    value: Value,
    origin: Option<HeaderValue>,
) -> ResponseResult<AuthSessionRequest> {
    serde_json::from_value(value).map_err(|_| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "auth session request does not match the supported contract",
            origin,
        )
    })
}

/// One username + password login or initialization request body.
struct LoginRequest {
    username: String,
    password: String,
}

fn parse_login_request(value: &Value, origin: Option<HeaderValue>) -> ResponseResult<LoginRequest> {
    let invalid = |origin: Option<HeaderValue>| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "login request must carry string username and password fields",
            origin,
        )
    };
    if value.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION) {
        return Err(BoundaryError::new(
            StatusCode::UPGRADE_REQUIRED,
            "CLIENT_UPGRADE_REQUIRED",
            "client protocol is unsupported; upgrade to winwincode/v1",
            origin,
        ));
    }
    let Some(fields) = value.as_object() else {
        return Err(invalid(origin));
    };
    if fields.len() != 3 {
        return Err(invalid(origin));
    }
    let username = fields
        .get("username")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(origin.clone()))?;
    let password = fields
        .get("password")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(origin))?;
    Ok(LoginRequest {
        username: username.to_owned(),
        password: password.to_owned(),
    })
}

fn auth_session_error(error: AuthSessionError, origin: Option<HeaderValue>) -> BoundaryError {
    if error.is_authentication() {
        BoundaryError::new(
            StatusCode::UNAUTHORIZED,
            "AUTHENTICATION_REQUIRED",
            "authentication failed",
            origin,
        )
    } else if error.is_already_initialized() {
        BoundaryError::new(
            StatusCode::CONFLICT,
            "WRONG_STATE",
            "server initialization already completed",
            origin,
        )
    } else if error.is_rate_limited() {
        BoundaryError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "login attempts are rate limited",
            origin,
        )
    } else if error.is_invalid_request() {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "login request is invalid",
            origin,
        )
    } else {
        BoundaryError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "browser session service is unavailable",
            origin,
        )
    }
}

async fn create_user(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require_owner(&state, &headers, &uri) {
        return error.into_response();
    }
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let creation = match parse_create_user_request(&body, origin.clone()) {
        Ok(creation) => creation,
        Err(error) => return error.into_response(),
    };
    let Ok(temporary_password) = generate_temporary_password() else {
        return account_error(UserAccountServiceErrorKind::Storage, origin).into_response();
    };
    let occurred_at = SystemStandaloneApplicationClock.now_instant();
    match state.auth_sessions.accounts().create_user(
        &creation.username,
        creation.role,
        &temporary_password,
        &occurred_at,
    ) {
        Ok(account) => json_response(
            StatusCode::CREATED,
            json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "user": account_json(&account),
                "temporaryPassword": temporary_password,
            }),
            origin.as_ref(),
        ),
        Err(error) => account_error(error.kind(), origin).into_response(),
    }
}

async fn set_user_state(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    let (_, acting) = match require_owner(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let update = match parse_user_state_request(&body, origin.clone()) {
        Ok(update) => update,
        Err(error) => return error.into_response(),
    };
    if update.state == UserAccountState::Disabled && update.user_id == acting.user_id {
        return BoundaryError::new(
            StatusCode::CONFLICT,
            "WRONG_STATE",
            "the acting Owner cannot disable the account in use",
            origin,
        )
        .into_response();
    }
    let occurred_at = SystemStandaloneApplicationClock.now_instant();
    let account = match state.auth_sessions.accounts().set_state(
        &update.user_id,
        &Revision(update.expected_revision),
        update.state,
        &occurred_at,
    ) {
        Ok(account) => account,
        Err(error) => return account_error(error.kind(), origin).into_response(),
    };
    if update.state == UserAccountState::Disabled {
        // Disable kill switch: revoke every live session immediately.
        let actor = Actor::UserActor(UserActor {
            kind: UserActorKind::User,
            id: account.user_id.clone(),
        });
        if state.auth_sessions.revoke_actor_sessions(&actor).is_err() {
            return BoundaryError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                "browser session service is unavailable",
                origin,
            )
            .into_response();
        }
    }
    json_response(
        StatusCode::OK,
        json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "user": account_json(&account),
        }),
        origin.as_ref(),
    )
}

async fn reset_password(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    // Password reset is available to the account holder themself and to an
    // Owner on behalf of anyone.
    let (principal, _, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let reset = match parse_password_reset_request(&body, origin.clone()) {
        Ok(reset) => reset,
        Err(error) => return error.into_response(),
    };
    let occurred_at = SystemStandaloneApplicationClock.now_instant();
    let acting_user = principal.actor_user_id();
    let self_reset = acting_user.is_some_and(|user| user == reset.user_id);
    let temporary_password;
    if self_reset {
        // A self reset must prove the current password.
        let (Some(current_password), Some(new_password)) =
            (&reset.current_password, &reset.new_password)
        else {
            return BoundaryError::new(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "self reset requires currentPassword and newPassword",
                origin,
            )
            .into_response();
        };
        let account = match state.auth_sessions.accounts().find(&reset.user_id) {
            Ok(Some(account)) => account,
            Ok(None) => {
                return account_error(UserAccountServiceErrorKind::NotFound, origin)
                    .into_response();
            }
            Err(error) => return account_error(error.kind(), origin).into_response(),
        };
        match state
            .auth_sessions
            .accounts()
            .verify_credentials(&account.username, current_password)
        {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                return BoundaryError::new(
                    StatusCode::UNAUTHORIZED,
                    "AUTHENTICATION_REQUIRED",
                    "authentication failed",
                    origin,
                )
                .into_response();
            }
            Err(error) => return account_error(error.kind(), origin).into_response(),
        }
        temporary_password = new_password.clone();
    } else {
        // Resetting somebody else's password requires the Owner role.
        if let Err(error) = require_owner(&state, &headers, &uri) {
            return error.into_response();
        }
        // An Owner reset issues a fresh temporary password returned once.
        match generate_temporary_password() {
            Ok(password) => temporary_password = password,
            Err(_) => {
                return account_error(UserAccountServiceErrorKind::Storage, origin).into_response();
            }
        }
    }
    let account = match state.auth_sessions.accounts().set_password(
        &reset.user_id,
        &Revision(reset.expected_revision),
        &temporary_password,
        &occurred_at,
    ) {
        Ok(account) => account,
        Err(error) => return account_error(error.kind(), origin).into_response(),
    };
    let mut value = json!({
        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        "user": account_json(&account),
    });
    if !self_reset {
        value["temporaryPassword"] = Value::String(temporary_password);
    }
    json_response(StatusCode::OK, value, origin.as_ref())
}

fn require_owner(
    state: &ServerState,
    headers: &HeaderMap,
    uri: &Uri,
) -> ResponseResult<(AuthenticatedPrincipal, UserAccount)> {
    let (principal, _, _) = authorize(state, headers, uri)?;
    let Some(user_id) = principal.actor_user_id() else {
        return Err(BoundaryError::new(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "owner role is required",
            None,
        ));
    };
    let account = state
        .auth_sessions
        .accounts()
        .find(&user_id)
        .map_err(|_| {
            BoundaryError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                "user account service is unavailable",
                None,
            )
        })?
        .ok_or_else(|| {
            BoundaryError::new(
                StatusCode::FORBIDDEN,
                "PERMISSION_DENIED",
                "owner role is required",
                None,
            )
        })?;
    if account.role != UserAccountRole::Owner || account.state != UserAccountState::Active {
        return Err(BoundaryError::new(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "owner role is required",
            None,
        ));
    }
    Ok((principal, account))
}

fn account_json(account: &UserAccount) -> Value {
    json!({
        "userId": account.user_id.0,
        "username": account.username,
        "normalizedUsername": account.normalized_username,
        "role": account.role.as_str(),
        "state": account.state.as_str(),
        "createdAt": account.created_at.0,
        "updatedAt": account.updated_at.0,
        "revision": account.revision.0,
    })
}

fn account_error(kind: UserAccountServiceErrorKind, origin: Option<HeaderValue>) -> BoundaryError {
    match kind {
        UserAccountServiceErrorKind::InvalidInput => BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "user account request is invalid",
            origin,
        ),
        UserAccountServiceErrorKind::Conflict | UserAccountServiceErrorKind::AlreadyInitialized => {
            BoundaryError::new(
                StatusCode::CONFLICT,
                "WRONG_STATE",
                "user account request conflicts with durable state",
                origin,
            )
        }
        UserAccountServiceErrorKind::NotFound => BoundaryError::new(
            StatusCode::NOT_FOUND,
            "RESOURCE_NOT_FOUND",
            "user account does not exist",
            origin,
        ),
        UserAccountServiceErrorKind::RevisionConflict => BoundaryError::new(
            StatusCode::CONFLICT,
            "REVISION_CONFLICT",
            "user account revision differs from the expected revision",
            origin,
        ),
        UserAccountServiceErrorKind::InvalidCredentials => BoundaryError::new(
            StatusCode::UNAUTHORIZED,
            "AUTHENTICATION_REQUIRED",
            "authentication failed",
            origin,
        ),
        UserAccountServiceErrorKind::AccountDisabled | UserAccountServiceErrorKind::Storage => {
            BoundaryError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                "user account service is unavailable",
                origin,
            )
        }
    }
}

struct CreateUserRequest {
    username: String,
    role: UserAccountRole,
}

fn parse_create_user_request(
    value: &Value,
    origin: Option<HeaderValue>,
) -> ResponseResult<CreateUserRequest> {
    let invalid = |origin: Option<HeaderValue>| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "user creation must carry string username and owner|member role fields",
            origin,
        )
    };
    if value.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION) {
        return Err(BoundaryError::new(
            StatusCode::UPGRADE_REQUIRED,
            "CLIENT_UPGRADE_REQUIRED",
            "client protocol is unsupported; upgrade to winwincode/v1",
            origin,
        ));
    }
    let Some(fields) = value.as_object() else {
        return Err(invalid(origin));
    };
    if fields.len() != 3 {
        return Err(invalid(origin));
    }
    let username = fields
        .get("username")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(origin.clone()))?;
    let role = match fields.get("role").and_then(Value::as_str) {
        Some("owner") => UserAccountRole::Owner,
        Some("member") => UserAccountRole::Member,
        _ => return Err(invalid(origin)),
    };
    Ok(CreateUserRequest {
        username: username.to_owned(),
        role,
    })
}

struct UserStateRequest {
    user_id: UserId,
    expected_revision: i64,
    state: UserAccountState,
}

fn parse_user_state_request(
    value: &Value,
    origin: Option<HeaderValue>,
) -> ResponseResult<UserStateRequest> {
    let invalid = |origin: Option<HeaderValue>| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "user state update must carry userId, active|disabled state, and expectedRevision",
            origin,
        )
    };
    if value.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION) {
        return Err(BoundaryError::new(
            StatusCode::UPGRADE_REQUIRED,
            "CLIENT_UPGRADE_REQUIRED",
            "client protocol is unsupported; upgrade to winwincode/v1",
            origin,
        ));
    }
    let Some(fields) = value.as_object() else {
        return Err(invalid(origin));
    };
    if fields.len() != 4 {
        return Err(invalid(origin));
    }
    let user_id = fields
        .get("userId")
        .and_then(Value::as_str)
        .map(|value| UserId(value.to_owned()))
        .ok_or_else(|| invalid(origin.clone()))?;
    let expected_revision = fields
        .get("expectedRevision")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid(origin.clone()))?;
    let state = match fields.get("state").and_then(Value::as_str) {
        Some("active") => UserAccountState::Active,
        Some("disabled") => UserAccountState::Disabled,
        _ => return Err(invalid(origin)),
    };
    Ok(UserStateRequest {
        user_id,
        expected_revision,
        state,
    })
}

struct PasswordResetRequest {
    user_id: UserId,
    expected_revision: i64,
    current_password: Option<String>,
    new_password: Option<String>,
}

fn parse_password_reset_request(
    value: &Value,
    origin: Option<HeaderValue>,
) -> ResponseResult<PasswordResetRequest> {
    let invalid = |origin: Option<HeaderValue>| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "password reset must carry userId and expectedRevision",
            origin,
        )
    };
    if value.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION) {
        return Err(BoundaryError::new(
            StatusCode::UPGRADE_REQUIRED,
            "CLIENT_UPGRADE_REQUIRED",
            "client protocol is unsupported; upgrade to winwincode/v1",
            origin,
        ));
    }
    let Some(fields) = value.as_object() else {
        return Err(invalid(origin));
    };
    if fields.len() < 3 || fields.len() > 5 {
        return Err(invalid(origin));
    }
    let user_id = fields
        .get("userId")
        .and_then(Value::as_str)
        .map(|value| UserId(value.to_owned()))
        .ok_or_else(|| invalid(origin.clone()))?;
    let expected_revision = fields
        .get("expectedRevision")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid(origin.clone()))?;
    let optional_string = |name: &str| fields.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(PasswordResetRequest {
        user_id,
        expected_revision,
        current_password: optional_string("currentPassword"),
        new_password: optional_string("newPassword"),
    })
}

async fn command(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let request_id = request_id_from_value(&body);
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.with_request_id(request_id).into_response(),
    };
    api_response(
        state.api.command(&principal, body),
        request_id,
        origin.as_ref(),
    )
}

async fn query(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let origin = match allowed_origin(&state, &headers) {
        Ok(origin) => origin,
        Err(error) => return error.into_response(),
    };
    let body = match parse_json_body(request, origin.clone()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let request_id = request_id_from_value(&body);
    let (principal, origin, _) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.with_request_id(request_id).into_response(),
    };
    api_response(
        state.api.query(&principal, body),
        request_id,
        origin.as_ref(),
    )
}

async fn parse_json_body(
    request: Request<Body>,
    origin: Option<HeaderValue>,
) -> ResponseResult<Value> {
    let bytes = to_bytes(request.into_body(), MAX_REQUEST_BYTES)
        .await
        .map_err(|_| {
            BoundaryError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "REQUEST_TOO_LARGE",
                "request body exceeds the public limit",
                origin.clone(),
            )
        })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_JSON",
            "request body must be valid JSON",
            origin.clone(),
        )
    })?;
    if value.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION) {
        let request_id = request_id_from_value(&value);
        return Err(BoundaryError::new(
            StatusCode::UPGRADE_REQUIRED,
            "CLIENT_UPGRADE_REQUIRED",
            "client protocol is unsupported; upgrade to winwincode/v1",
            origin,
        )
        .with_request_id(request_id));
    }
    Ok(value)
}

async fn preflight(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let origin = match allowed_origin(&state, &headers) {
        Ok(Some(origin)) => origin,
        Ok(None) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "ORIGIN_REQUIRED",
                "browser origin is required",
                None,
                None,
            );
        }
        Err(error) => return error.into_response(),
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    apply_cors(response.headers_mut(), &origin);
    response.headers_mut().insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
    );
    response.headers_mut().insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization, Content-Type"),
    );
    response
}

async fn events(
    State(state): State<ServerState>,
    headers: HeaderMap,
    uri: Uri,
    websocket: WebSocketUpgrade,
) -> Response {
    let (principal, origin, credentials) = match authorize(&state, &headers, &uri) {
        Ok(authorized) => authorized,
        Err(error) => return error.into_response(),
    };
    let api = Arc::clone(&state.api);
    let authenticator = Arc::clone(&state.authenticator);
    let mut response = websocket
        .on_upgrade(move |socket| event_socket(socket, principal, credentials, authenticator, api))
        .into_response();
    if let Some(origin) = origin {
        apply_cors(response.headers_mut(), &origin);
    }
    response
}

async fn event_socket(
    mut socket: WebSocket,
    principal: AuthenticatedPrincipal,
    credentials: TransportCredentials,
    authenticator: Arc<dyn RequestAuthenticator>,
    api: Arc<dyn ControlPlaneApiPort>,
) {
    let first = tokio::time::timeout(FIRST_SUBSCRIPTION_FRAME_TIMEOUT, socket.next()).await;
    let Ok(Some(Ok(Message::Text(text)))) = first else {
        send_socket_error(
            &mut socket,
            &ApiError::new(
                408,
                "SUBSCRIPTION_REQUIRED",
                "subscription frame is required",
            ),
        )
        .await;
        return;
    };
    let Ok(first_frame) = serde_json::from_str(&text) else {
        send_socket_error(
            &mut socket,
            &ApiError::new(400, "INVALID_FRAME", "invalid JSON frame"),
        )
        .await;
        return;
    };
    if !principal_is_current(authenticator.as_ref(), &credentials, &principal) {
        close_revoked_socket(&mut socket).await;
        return;
    }
    let mut subscription = match api.subscribe(&principal, first_frame) {
        Ok(subscription) => subscription,
        Err(error) => {
            send_socket_error(&mut socket, &error).await;
            return;
        }
    };
    for frame in subscription.initial_frames {
        if !principal_is_current(authenticator.as_ref(), &credentials, &principal) {
            close_revoked_socket(&mut socket).await;
            return;
        }
        if send_value(&mut socket, &frame).await.is_err() {
            return;
        }
    }
    let mut session_revalidation = tokio::time::interval(SESSION_REVALIDATION_INTERVAL);

    loop {
        tokio::select! {
            biased;
            event = subscription.events.recv() => match event {
                Some(frame) => if !forward_subscription_event(
                    &mut socket,
                    &frame,
                    authenticator.as_ref(),
                    &credentials,
                    &principal,
                ).await {
                    break;
                },
                None => break,
            },
            _ = session_revalidation.tick() => {
                if !principal_is_current(authenticator.as_ref(), &credentials, &principal) {
                    close_revoked_socket(&mut socket).await;
                    break;
                }
            }
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if !principal_is_current(authenticator.as_ref(), &credentials, &principal) {
                        close_revoked_socket(&mut socket).await;
                        break;
                    }
                    let Ok(frame) = serde_json::from_str(&text) else {
                        send_socket_error(&mut socket, &ApiError::new(400, "INVALID_FRAME", "invalid JSON frame")).await;
                        break;
                    };
                    match api.event_control(&principal, frame) {
                        Ok(frames) => {
                            for frame in frames {
                                if send_value(&mut socket, &frame).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            send_socket_error(&mut socket, &error).await;
                            break;
                        }
                    }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(Message::Binary(_) | Message::Pong(_))) => {}
            }
        }
    }
}

fn principal_is_current(
    authenticator: &dyn RequestAuthenticator,
    credentials: &TransportCredentials,
    principal: &AuthenticatedPrincipal,
) -> bool {
    authenticator
        .authenticate(credentials)
        .is_ok_and(|current| current == *principal)
}

fn is_authorization_revoked_frame(frame: &Value) -> bool {
    matches!(
        serde_json::from_value::<ControlPlaneWebSocketServerFrame>(frame.clone()),
        Ok(ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketAuthorizationRevokedFrame(_))
    )
}

async fn forward_subscription_event(
    socket: &mut WebSocket,
    frame: &Value,
    authenticator: &dyn RequestAuthenticator,
    credentials: &TransportCredentials,
    principal: &AuthenticatedPrincipal,
) -> bool {
    if is_authorization_revoked_frame(frame) {
        if send_value(socket, frame).await.is_err() {
            return false;
        }
        close_revoked_socket(socket).await;
        return false;
    }
    if !principal_is_current(authenticator, credentials, principal) {
        close_revoked_socket(socket).await;
        return false;
    }
    send_value(socket, frame).await.is_ok()
}

async fn close_revoked_socket(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 4403,
            reason: "authorization changed".into(),
        })))
        .await;
}

async fn send_value(socket: &mut WebSocket, value: &Value) -> Result<(), ()> {
    if CredentialLeakGate::default()
        .inspect_serializable(CredentialOutputBoundary::WebSocket, value)
        .is_err()
    {
        let redacted = websocket_error_json(
            &ApiError::new(
                500,
                "CREDENTIAL_OUTPUT_REJECTED",
                "public output was rejected by the Credential leak gate",
            ),
            next_diagnostic_request_id(),
        );
        let text = serde_json::to_string(&redacted).map_err(|_| ())?;
        socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| ())?;
        return Err(());
    }
    let text = serde_json::to_string(value).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

async fn send_socket_error(socket: &mut WebSocket, error: &ApiError) {
    let frame = websocket_error_json(error, next_diagnostic_request_id());
    let _ = send_value(socket, &frame).await;
    let _ = socket.send(Message::Close(None)).await;
}

async fn not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "public endpoint not found",
        None,
        None,
    )
}

fn authorize(
    state: &ServerState,
    headers: &HeaderMap,
    uri: &Uri,
) -> ResponseResult<(
    AuthenticatedPrincipal,
    Option<HeaderValue>,
    TransportCredentials,
)> {
    if uri.query().is_some() {
        return Err(BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "QUERY_PARAMETERS_FORBIDDEN",
            "credentials and routing values are not accepted in the URL query",
            None,
        ));
    }
    let origin = allowed_origin(state, headers)?;
    let credentials = extract_credentials(headers)?;
    match state.authenticator.authenticate(&credentials) {
        Ok(principal) => Ok((principal, origin, credentials)),
        Err(_) => Err(BoundaryError::new(
            StatusCode::UNAUTHORIZED,
            "AUTHENTICATION_REQUIRED",
            "authentication failed",
            origin,
        )),
    }
}

fn required_origin(state: &ServerState, headers: &HeaderMap) -> ResponseResult<HeaderValue> {
    allowed_origin(state, headers)?.ok_or_else(|| {
        BoundaryError::new(
            StatusCode::BAD_REQUEST,
            "ORIGIN_REQUIRED",
            "browser origin is required",
            None,
        )
    })
}

fn allowed_origin(state: &ServerState, headers: &HeaderMap) -> ResponseResult<Option<HeaderValue>> {
    let Some(origin) = headers.get(ORIGIN) else {
        return Ok(None);
    };
    let Ok(origin_text) = origin.to_str() else {
        return Err(BoundaryError::new(
            StatusCode::FORBIDDEN,
            "ORIGIN_DENIED",
            "browser origin is not allowed",
            None,
        ));
    };
    if !state.config.allowed_origins().contains(origin_text) {
        return Err(BoundaryError::new(
            StatusCode::FORBIDDEN,
            "ORIGIN_DENIED",
            "browser origin is not allowed",
            None,
        ));
    }
    Ok(Some(origin.clone()))
}

fn extract_credentials(headers: &HeaderMap) -> ResponseResult<TransportCredentials> {
    if headers.get_all(AUTHORIZATION).iter().count() > 1 {
        return Err(ambiguous_credentials());
    }
    let bearer = match headers.get(AUTHORIZATION) {
        Some(value) => {
            let value = value.to_str().map_err(|_| ambiguous_credentials())?;
            let (scheme, credential) = value.split_once(' ').ok_or_else(ambiguous_credentials)?;
            if !scheme.eq_ignore_ascii_case("bearer") || credential.is_empty() {
                return Err(ambiguous_credentials());
            }
            Some(credential.to_owned())
        }
        None => None,
    };
    let session_cookie = session_cookie(headers)?;
    if bearer.is_some() && session_cookie.is_some() {
        return Err(ambiguous_credentials());
    }
    Ok(TransportCredentials::new(bearer, session_cookie))
}

fn session_cookie(headers: &HeaderMap) -> ResponseResult<Option<String>> {
    let mut session = None;
    for header in headers.get_all(COOKIE) {
        let text = header.to_str().map_err(|_| ambiguous_credentials())?;
        for pair in text.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == "wwc_session"
                && (value.is_empty() || session.replace(value.to_owned()).is_some())
            {
                return Err(ambiguous_credentials());
            }
        }
    }
    Ok(session)
}

fn ambiguous_credentials() -> BoundaryError {
    BoundaryError::new(
        StatusCode::BAD_REQUEST,
        "INVALID_AUTHENTICATION",
        "authentication headers are invalid or ambiguous",
        None,
    )
}

fn api_response(
    result: Result<Value, ApiError>,
    request_id: Option<RequestId>,
    origin: Option<&HeaderValue>,
) -> Response {
    match result {
        Ok(value) => json_response(StatusCode::OK, value, origin),
        Err(error) => {
            let public_error_candidate = json!({
                "code": error.code(),
                "message": error.message(),
            });
            if CredentialLeakGate::default()
                .inspect_serializable(CredentialOutputBoundary::Http, &public_error_candidate)
                .is_err()
            {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CREDENTIAL_OUTPUT_REJECTED",
                    "public output was rejected by the Credential leak gate",
                    request_id,
                    origin,
                );
            }
            error_response(
                api_error_status(error.status()),
                error.code(),
                error.message(),
                request_id,
                origin,
            )
        }
    }
}

fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    request_id: Option<RequestId>,
    origin: Option<&HeaderValue>,
) -> Response {
    json_response(
        status,
        error_json(status, code, message, request_id),
        origin,
    )
}

fn error_json(
    status: StatusCode,
    source_code: &str,
    message: &str,
    request_id: Option<RequestId>,
) -> Value {
    let request_id = request_id.unwrap_or_else(next_diagnostic_request_id);
    serde_json::to_value(error_envelope(status, source_code, message, request_id))
        .expect("generated ErrorEnvelope is serializable")
}

fn websocket_error_json(error: &ApiError, request_id: RequestId) -> Value {
    let status = api_error_status(error.status());
    serde_json::to_value(ControlPlaneWebSocketProtocolErrorFrame {
        error: error_envelope(status, error.code(), error.message(), request_id),
        type_value: ControlPlaneWebSocketProtocolErrorFrameTypeValue::TransportErrorV1,
    })
    .expect("generated WebSocket error frame is serializable")
}

fn api_error_status(status: u16) -> StatusCode {
    StatusCode::from_u16(status).map_or(StatusCode::INTERNAL_SERVER_ERROR, |status| {
        if status.is_client_error() || status.is_server_error() {
            status
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })
}

fn error_envelope(
    status: StatusCode,
    source_code: &str,
    message: &str,
    request_id: RequestId,
) -> ErrorEnvelope {
    let reason = public_reason(source_code);
    let message = public_message(reason, message);
    let mut details = ErrorDetails::new();
    details.insert(
        "reason".to_owned(),
        ErrorDetailValue::Variant4(reason.to_owned()),
    );
    if status == StatusCode::UPGRADE_REQUIRED {
        details.insert(
            "supportedSchemaVersion".to_owned(),
            ErrorDetailValue::Variant4(SUPPORTED_SCHEMA_VERSION.to_owned()),
        );
    }
    let error = match retryable_code(status, source_code) {
        Some(code) => Error::RetryableError(RetryableError {
            code,
            details,
            message: message.clone(),
            retryable: true,
        }),
        None => Error::TerminalError(TerminalError {
            code: terminal_code(status, source_code),
            details,
            message,
            retryable: false,
        }),
    };
    ErrorEnvelope {
        error,
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
    }
}

fn public_message(reason: &str, message: &str) -> String {
    if matches!(reason, "APPLICATION_ERROR" | "INTERNAL_ERROR") {
        return "request failed at the public Control Plane boundary".to_owned();
    }
    if message.is_empty() || message.encode_utf16().count() > 4096 {
        "request failed at the public Control Plane boundary".to_owned()
    } else {
        message.to_owned()
    }
}

fn retryable_code(status: StatusCode, source_code: &str) -> Option<RetryableErrorCode> {
    match source_code {
        "RATE_LIMITED" => Some(RetryableErrorCode::RateLimited),
        "READ_CURSOR_EXPIRED" => Some(RetryableErrorCode::ReadCursorExpired),
        "SERVICE_UNAVAILABLE" => Some(RetryableErrorCode::ServiceUnavailable),
        "TRUSTED_FACTS_UNAVAILABLE" => Some(RetryableErrorCode::TrustedFactsUnavailable),
        _ if matches!(
            status,
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
        ) =>
        {
            Some(RetryableErrorCode::ServiceUnavailable)
        }
        _ if status == StatusCode::TOO_MANY_REQUESTS => Some(RetryableErrorCode::RateLimited),
        _ => None,
    }
}

fn terminal_code(status: StatusCode, source_code: &str) -> TerminalErrorCode {
    match source_code {
        "AUTHENTICATION_REQUIRED" => TerminalErrorCode::AuthenticationRequired,
        "PERMISSION_DENIED" => TerminalErrorCode::PermissionDenied,
        "RESOURCE_NOT_FOUND" => TerminalErrorCode::ResourceNotFound,
        "IDEMPOTENCY_CONFLICT" => TerminalErrorCode::IdempotencyConflict,
        "REVISION_CONFLICT" => TerminalErrorCode::RevisionConflict,
        "CANDIDATE_STALE" => TerminalErrorCode::CandidateStale,
        "WRONG_STATE" => TerminalErrorCode::WrongState,
        "INTERNAL_ERROR" => TerminalErrorCode::InternalError,
        "INVALID_REQUEST" => TerminalErrorCode::InvalidRequest,
        _ => match status {
            StatusCode::UNAUTHORIZED => TerminalErrorCode::AuthenticationRequired,
            StatusCode::FORBIDDEN => TerminalErrorCode::PermissionDenied,
            StatusCode::NOT_FOUND => TerminalErrorCode::ResourceNotFound,
            StatusCode::CONFLICT => TerminalErrorCode::WrongState,
            status if status.is_server_error() => TerminalErrorCode::InternalError,
            _ => TerminalErrorCode::InvalidRequest,
        },
    }
}

fn public_reason(source_code: &str) -> &'static str {
    match source_code {
        "REQUEST_TOO_LARGE" => "REQUEST_TOO_LARGE",
        "INVALID_JSON" => "INVALID_JSON",
        "CLIENT_UPGRADE_REQUIRED" => "CLIENT_UPGRADE_REQUIRED",
        "ORIGIN_REQUIRED" => "ORIGIN_REQUIRED",
        "SUBSCRIPTION_REQUIRED" => "SUBSCRIPTION_REQUIRED",
        "INVALID_FRAME" => "INVALID_FRAME",
        "QUERY_PARAMETERS_FORBIDDEN" => "QUERY_PARAMETERS_FORBIDDEN",
        "ORIGIN_DENIED" => "ORIGIN_DENIED",
        "INVALID_AUTHENTICATION" => "INVALID_AUTHENTICATION",
        "NOT_FOUND" => "NOT_FOUND",
        "CREDENTIAL_OUTPUT_REJECTED" => "CREDENTIAL_OUTPUT_REJECTED",
        "APPLICATION_RESPONSE_INVALID" => "APPLICATION_RESPONSE_INVALID",
        "AUTHENTICATION_REQUIRED" => "AUTHENTICATION_REQUIRED",
        "PERMISSION_DENIED" => "PERMISSION_DENIED",
        "RESOURCE_NOT_FOUND" => "RESOURCE_NOT_FOUND",
        "IDEMPOTENCY_CONFLICT" => "IDEMPOTENCY_CONFLICT",
        "REVISION_CONFLICT" => "REVISION_CONFLICT",
        "READ_CURSOR_EXPIRED" => "READ_CURSOR_EXPIRED",
        "CANDIDATE_STALE" => "CANDIDATE_STALE",
        "WRONG_STATE" => "WRONG_STATE",
        "RATE_LIMITED" => "RATE_LIMITED",
        "SERVICE_UNAVAILABLE" => "SERVICE_UNAVAILABLE",
        "TRUSTED_FACTS_UNAVAILABLE" => "TRUSTED_FACTS_UNAVAILABLE",
        "INTERNAL_ERROR" => "INTERNAL_ERROR",
        _ => "APPLICATION_ERROR",
    }
}

fn request_id_from_value(value: &Value) -> Option<RequestId> {
    let candidate = value.get("requestId")?.as_str()?;
    let suffix = candidate.strip_prefix("req_")?;
    if suffix.len() != 26 || !suffix.bytes().all(is_crockford_base32) {
        return None;
    }
    Some(RequestId(candidate.to_owned()))
}

const fn is_crockford_base32(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

fn next_diagnostic_request_id() -> RequestId {
    let serial = NEXT_DIAGNOSTIC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    RequestId(format!("req_{serial:026}"))
}

fn json_response(
    mut status: StatusCode,
    mut value: Value,
    origin: Option<&HeaderValue>,
) -> Response {
    let request_id = request_id_from_value(&value);
    if CredentialLeakGate::default()
        .inspect_serializable(CredentialOutputBoundary::Http, &value)
        .is_err()
    {
        status = StatusCode::INTERNAL_SERVER_ERROR;
        value = error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "CREDENTIAL_OUTPUT_REJECTED",
            "public output was rejected by the Credential leak gate",
            request_id,
        );
    }
    let mut response = (status, Json(value)).into_response();
    if let Some(origin) = origin {
        apply_cors(response.headers_mut(), origin);
    }
    response
}

fn apply_cors(headers: &mut HeaderMap, origin: &HeaderValue) {
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    headers.insert(
        ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    headers.insert(VARY, HeaderValue::from_static("Origin"));
}

/// Listener/lifecycle failure that omits socket internals and credential data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerError {
    message: String,
}

impl ServerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServerError {}
