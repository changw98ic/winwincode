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
use axum::extract::{DefaultBodyLimit, State};
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
    AuthSessionRequest, ControlPlaneWebSocketProtocolErrorFrame,
    ControlPlaneWebSocketProtocolErrorFrameTypeValue, ControlPlaneWebSocketServerFrame, Error,
    ErrorDetailValue, ErrorDetails, ErrorEnvelope, RetryableError, RetryableErrorCode,
    TerminalError, TerminalErrorCode,
};
use winwincode_control_plane::{CredentialLeakGate, CredentialOutputBoundary};
use winwincode_domain::{RequestId, SchemaVersion};

use crate::application::{StandaloneApplicationClock, SystemStandaloneApplicationClock};
use crate::auth_session::{
    AuthSessionError, SqliteAuthSessionManager, cleared_session_cookie_header,
};
use crate::config::{ServerConfig, ServerTls};
use crate::enterprise_identity_protocol::{
    EnterpriseIdentityProtocolApplication, router as enterprise_identity_router,
};
use crate::performance_evaluation::ProductionPerformanceEvaluation;
use crate::remote_worker_transport::RemoteWorkerExchangePort;
use crate::transport::{
    ApiError, AuthenticatedPrincipal, ControlPlaneApiPort, RequestAuthenticator,
    TransportCredentials,
};

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
}

/// Running standalone listener with deterministic graceful shutdown.
pub struct RunningServer {
    local_address: SocketAddr,
    public_url: String,
    shutdown_grace: Duration,
    handle: Handle<SocketAddr>,
    task: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    api: Arc<dyn ControlPlaneApiPort>,
    performance_evaluation: ProductionPerformanceEvaluation,
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

    /// Returns the local operator entry for one predeclared performance pair.
    ///
    /// The entry is composed from the same trusted Server data directory and
    /// does not accept Worker database paths or caller-authored assignments.
    #[must_use]
    pub const fn performance_evaluation(&self) -> &ProductionPerformanceEvaluation {
        &self.performance_evaluation
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
) -> Result<RunningServer, ServerError> {
    if remote_worker.is_some() && matches!(config.tls(), ServerTls::Disabled) {
        return Err(ServerError::new(
            "remote Worker exchange requires the Server TLS listener",
        ));
    }
    let performance_evaluation = ProductionPerformanceEvaluation::from_server_config(&config);
    let config = Arc::new(config);
    let state = ServerState {
        config: Arc::clone(&config),
        auth_sessions,
        authenticator,
        api: Arc::clone(&api),
        remote_worker,
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
        api,
        performance_evaluation,
    })
}

async fn spawn_listener(
    config: &ServerConfig,
    router: Router,
    handle: Handle<SocketAddr>,
) -> Result<tokio::task::JoinHandle<Result<(), ServerError>>, ServerError> {
    let address = config.bind_address();
    let task = match config.tls() {
        ServerTls::Disabled => tokio::spawn(async move {
            axum_server::bind(address)
                .handle(handle)
                .serve(router.into_make_service())
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
                    .serve(router.into_make_service())
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
        .route("/api/v1/commands", post(command).options(preflight))
        .route("/api/v1/queries", post(query).options(preflight))
        .route("/api/v1/events", get(events).options(preflight))
        .route(
            "/internal/v1/execution-port/exchange",
            post(remote_worker_exchange),
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

async fn create_auth_session(State(state): State<ServerState>, request: Request<Body>) -> Response {
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
    let issued = match state.auth_sessions.bootstrap(&credentials) {
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

fn auth_session_error(error: AuthSessionError, origin: Option<HeaderValue>) -> BoundaryError {
    if error.is_authentication() {
        BoundaryError::new(
            StatusCode::UNAUTHORIZED,
            "AUTHENTICATION_REQUIRED",
            "authentication failed",
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
