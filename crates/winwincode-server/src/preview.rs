// SPDX-License-Identifier: Apache-2.0

//! Isolated, short-lived preview access over an outbound Device Client tunnel.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{SinkExt as _, StreamExt as _};
use getrandom::fill;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{mpsc, oneshot};
use winwincode_client_port::preview::{
    DevicePreviewFrame, MAX_PREVIEW_BODY_BYTES, MAX_PREVIEW_HEADERS, PREVIEW_TUNNEL_SCHEMA_VERSION,
    PreviewHeader, PreviewSourceDescriptor, PreviewSourceMode, ServerPreviewFrame,
};
use winwincode_control_plane::{
    ClientOccupancyService, ClientRegistryService, OccupancyLeaseState, ProductStateStorage,
};
use winwincode_storage::SqliteStorage;

const ACCESS_TTL: Duration = Duration::from_mins(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SOURCES_PER_CLIENT: usize = 16;
const MAX_PENDING_REQUESTS: usize = 64;

#[derive(Clone)]
pub(crate) struct PreviewApplication {
    data_directory: PathBuf,
    public_origin: String,
    authority: String,
    broker: Arc<Mutex<BrokerState>>,
}

impl fmt::Debug for PreviewApplication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewApplication")
            .field("public_origin", &self.public_origin)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct BrokerState {
    generation: u64,
    connections: HashMap<String, PreviewConnection>,
    accesses: HashMap<String, PreviewAccess>,
    pending: HashMap<String, PendingRequest>,
}

struct PreviewConnection {
    generation: u64,
    sources: BTreeMap<String, PreviewSourceDescriptor>,
    outbound: mpsc::Sender<ServerPreviewFrame>,
}

struct PreviewAccess {
    owner_user_id: String,
    client_node_id: String,
    source: PreviewSourceDescriptor,
    token_digest: [u8; 32],
    expires_at: std::time::Instant,
}

struct PendingRequest {
    client_node_id: String,
    generation: u64,
    completion: oneshot::Sender<Result<PreviewHttpResponse, PreviewError>>,
}

#[derive(Debug)]
pub(crate) struct PreviewHttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewErrorKind {
    InvalidRequest,
    NotFound,
    Forbidden,
    Conflict,
    Timeout,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewError {
    kind: PreviewErrorKind,
    message: &'static str,
}

impl PreviewError {
    const fn new(kind: PreviewErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub(crate) const fn kind(&self) -> PreviewErrorKind {
        self.kind
    }

    pub(crate) const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for PreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PreviewError {}

impl PreviewApplication {
    pub(crate) fn open(data_directory: PathBuf, public_origin: String) -> Option<Self> {
        let authority = public_origin.split_once("://")?.1.to_ascii_lowercase();
        Some(Self {
            data_directory,
            public_origin,
            authority,
            broker: Arc::new(Mutex::new(BrokerState::default())),
        })
    }

    pub(crate) fn matches_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.authority))
    }

    pub(crate) async fn serve_tunnel(&self, client_node_id: String, mut socket: WebSocket) {
        let registration = tokio::time::timeout(REGISTER_TIMEOUT, socket.next()).await;
        let Ok(Some(Ok(Message::Text(text)))) = registration else {
            let _ = socket.close().await;
            return;
        };
        let Ok(DevicePreviewFrame::Register {
            schema_version,
            sources,
        }) = serde_json::from_str::<DevicePreviewFrame>(&text)
        else {
            let _ = socket.close().await;
            return;
        };
        if schema_version != PREVIEW_TUNNEL_SCHEMA_VERSION {
            let _ = socket.close().await;
            return;
        }
        let Ok(sources) = validate_sources(sources) else {
            let _ = socket.close().await;
            return;
        };
        let (outbound, mut requests) = mpsc::channel(MAX_PENDING_REQUESTS);
        let generation = self.register_connection(&client_node_id, sources, outbound);
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    let Ok(text) = serde_json::to_string(&request) else { break };
                    if socket.send(Message::Text(text.into())).await.is_err() { break }
                }
                message = socket.next() => {
                    let Some(Ok(message)) = message else { break };
                    match message {
                        Message::Text(text) => {
                            let Ok(frame) = serde_json::from_str::<DevicePreviewFrame>(&text) else { break };
                            if !self.apply_device_frame(&client_node_id, generation, frame) { break }
                        }
                        Message::Ping(bytes) => {
                            if socket.send(Message::Pong(bytes)).await.is_err() { break }
                        }
                        Message::Close(_) => break,
                        Message::Binary(_) | Message::Pong(_) => {}
                    }
                }
            }
        }
        self.unregister_connection(&client_node_id, generation);
    }

    pub(crate) fn grant(&self, user_id: &str, request: &Value) -> Result<Value, PreviewError> {
        let fields = request.as_object().ok_or_else(invalid_request)?;
        if fields.len() != 3
            || fields.get("schemaVersion").and_then(Value::as_str) != Some("winwincode/v1")
        {
            return Err(invalid_request());
        }
        let client_id = fields
            .get("clientId")
            .and_then(Value::as_str)
            .filter(|value| portable_identifier(value))
            .ok_or_else(invalid_request)?;
        let source_id = fields
            .get("sourceId")
            .and_then(Value::as_str)
            .filter(|value| portable_identifier(value))
            .ok_or_else(invalid_request)?;
        let client_node_id = self.authorize_holder(user_id, client_id)?;
        let source = {
            let state = self.lock_broker()?;
            state
                .connections
                .get(&client_node_id)
                .and_then(|connection| connection.sources.get(source_id))
                .cloned()
                .ok_or_else(|| {
                    PreviewError::new(
                        PreviewErrorKind::Conflict,
                        "preview source is not connected",
                    )
                })?
        };
        let access_id = random_hex("pva_", 16)?;
        let token = random_hex("", 32)?;
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let expires_at = std::time::Instant::now() + ACCESS_TTL;
        self.lock_broker()?.accesses.insert(
            access_id.clone(),
            PreviewAccess {
                owner_user_id: user_id.to_owned(),
                client_node_id,
                source: source.clone(),
                token_digest: digest,
                expires_at,
            },
        );
        let expires_at_text = (OffsetDateTime::now_utc()
            + time::Duration::seconds(i64::try_from(ACCESS_TTL.as_secs()).unwrap_or(300)))
        .format(&Rfc3339)
        .map_err(|_| unavailable())?;
        Ok(json!({
            "schemaVersion": "winwincode/v1",
            "previewAccessId": access_id,
            "previewUrl": format!("{}/p/{}/{}/", self.public_origin, access_id, token),
            "expiresAt": expires_at_text,
            "source": source,
        }))
    }

    pub(crate) fn revoke(&self, user_id: &str, access_id: &str) -> Result<(), PreviewError> {
        if !portable_identifier(access_id) {
            return Err(invalid_request());
        }
        let mut state = self.lock_broker()?;
        let access = state.accesses.get(access_id).ok_or_else(not_found)?;
        if access.owner_user_id != user_id {
            return Err(PreviewError::new(
                PreviewErrorKind::Forbidden,
                "preview access belongs to another user",
            ));
        }
        state.accesses.remove(access_id);
        Ok(())
    }

    pub(crate) async fn forward(
        &self,
        access_id: &str,
        token: &str,
        method: &Method,
        target: &str,
        request_headers: &HeaderMap,
        body: &[u8],
    ) -> Result<PreviewHttpResponse, PreviewError> {
        if body.len() > MAX_PREVIEW_BODY_BYTES || !allowed_method(method) || !safe_target(target) {
            return Err(invalid_request());
        }
        let token_digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let (client_node_id, generation, source_id, outbound) = {
            let mut state = self.lock_broker()?;
            state
                .accesses
                .retain(|_, access| access.expires_at > std::time::Instant::now());
            let access = state.accesses.get(access_id).ok_or_else(not_found)?;
            if !constant_time_eq(&access.token_digest, &token_digest) {
                return Err(not_found());
            }
            let connection = state
                .connections
                .get(&access.client_node_id)
                .ok_or_else(source_unavailable)?;
            if connection.sources.get(&access.source.source_id) != Some(&access.source) {
                return Err(PreviewError::new(
                    PreviewErrorKind::Conflict,
                    "preview source identity changed",
                ));
            }
            (
                access.client_node_id.clone(),
                connection.generation,
                access.source.source_id.clone(),
                connection.outbound.clone(),
            )
        };
        let request_id = random_hex("pvr_", 16)?;
        let headers = filtered_headers(request_headers)?;
        let (completion, receiver) = oneshot::channel();
        {
            let mut state = self.lock_broker()?;
            if state
                .connections
                .get(&client_node_id)
                .is_none_or(|connection| connection.generation != generation)
                || state.pending.len() >= MAX_PENDING_REQUESTS
            {
                return Err(source_unavailable());
            }
            state.pending.insert(
                request_id.clone(),
                PendingRequest {
                    client_node_id,
                    generation,
                    completion,
                },
            );
        }
        let frame = ServerPreviewFrame::HttpRequest {
            request_id: request_id.clone(),
            source_id,
            method: method.as_str().to_owned(),
            target: target.to_owned(),
            headers,
            body_base64: BASE64.encode(body),
        };
        if outbound.send(frame).await.is_err() {
            self.remove_pending(&request_id);
            return Err(source_unavailable());
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(source_unavailable()),
            Err(_) => {
                self.remove_pending(&request_id);
                Err(PreviewError::new(
                    PreviewErrorKind::Timeout,
                    "preview source response timed out",
                ))
            }
        }
    }

    fn register_connection(
        &self,
        client_node_id: &str,
        sources: BTreeMap<String, PreviewSourceDescriptor>,
        outbound: mpsc::Sender<ServerPreviewFrame>,
    ) -> u64 {
        let mut state = self
            .broker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        let stale_completions = state
            .pending
            .extract_if(|_, request| request.client_node_id == client_node_id)
            .map(|(_, request)| request.completion)
            .collect::<Vec<_>>();
        state.connections.insert(
            client_node_id.to_owned(),
            PreviewConnection {
                generation,
                sources,
                outbound,
            },
        );
        drop(state);
        for completion in stale_completions {
            let _ = completion.send(Err(source_unavailable()));
        }
        generation
    }

    fn unregister_connection(&self, client_node_id: &str, generation: u64) {
        let mut state = self
            .broker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .connections
            .get(client_node_id)
            .is_some_and(|connection| connection.generation == generation)
        {
            state.connections.remove(client_node_id);
            let pending = state
                .pending
                .extract_if(|_, request| request.client_node_id == client_node_id)
                .map(|(_, request)| request.completion)
                .collect::<Vec<_>>();
            drop(state);
            for completion in pending {
                let _ = completion.send(Err(source_unavailable()));
            }
        }
    }

    fn apply_device_frame(
        &self,
        client_node_id: &str,
        generation: u64,
        frame: DevicePreviewFrame,
    ) -> bool {
        let DevicePreviewFrame::HttpResponse {
            request_id,
            status,
            headers,
            body_base64,
        } = frame
        else {
            return false;
        };
        let completion = {
            let Ok(mut state) = self.lock_broker() else {
                return false;
            };
            let belongs = state.pending.get(&request_id).is_some_and(|request| {
                request.client_node_id == client_node_id && request.generation == generation
            });
            if !belongs {
                return true;
            }
            state
                .pending
                .remove(&request_id)
                .map(|request| request.completion)
        };
        let Some(completion) = completion else {
            return true;
        };
        let response = decode_response(status, headers, &body_base64);
        let _ = completion.send(response);
        true
    }

    fn remove_pending(&self, request_id: &str) {
        if let Ok(mut state) = self.lock_broker() {
            state.pending.remove(request_id);
        }
    }

    fn authorize_holder(
        &self,
        user_id: &str,
        public_client_id: &str,
    ) -> Result<String, PreviewError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|_| unavailable())?;
        let node = ClientRegistryService::new(&mut storage)
            .snapshot_by_public_client_id(public_client_id)
            .map_err(|_| unavailable())?
            .ok_or_else(not_found)?;
        let lease = ClientOccupancyService::new(&mut storage)
            .active_lease_for_node(&node.client_node_id)
            .map_err(|_| unavailable())?
            .ok_or_else(|| {
                PreviewError::new(
                    PreviewErrorKind::Conflict,
                    "client occupancy is required for preview access",
                )
            })?;
        let result = if lease.holder_user_id != user_id {
            Err(PreviewError::new(
                PreviewErrorKind::Forbidden,
                "only the occupancy holder may open preview access",
            ))
        } else if !matches!(
            lease.state,
            OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
        ) {
            Err(PreviewError::new(
                PreviewErrorKind::Conflict,
                "client occupancy is not confirmed",
            ))
        } else {
            Ok(node.client_node_id)
        };
        Box::new(storage).close().map_err(|_| unavailable())?;
        result
    }

    fn lock_broker(&self) -> Result<std::sync::MutexGuard<'_, BrokerState>, PreviewError> {
        self.broker.lock().map_err(|_| unavailable())
    }
}

fn validate_sources(
    sources: Vec<PreviewSourceDescriptor>,
) -> Result<BTreeMap<String, PreviewSourceDescriptor>, PreviewError> {
    if sources.is_empty() || sources.len() > MAX_SOURCES_PER_CLIENT {
        return Err(invalid_request());
    }
    let mut result = BTreeMap::new();
    for source in sources {
        if !portable_identifier(&source.source_id)
            || !portable_identifier(&source.worker_session_id)
            || !portable_identifier(&source.repository_binding_id)
        {
            return Err(invalid_request());
        }
        match (source.mode, source.candidate_commit.as_deref()) {
            (PreviewSourceMode::Live, None) => {}
            (PreviewSourceMode::FrozenCandidate, Some(commit)) if is_commit(commit) => {}
            _ => return Err(invalid_request()),
        }
        if result.insert(source.source_id.clone(), source).is_some() {
            return Err(invalid_request());
        }
    }
    Ok(result)
}

fn filtered_headers(headers: &HeaderMap) -> Result<Vec<PreviewHeader>, PreviewError> {
    let mut result = Vec::new();
    for (name, value) in headers {
        if safe_header_name(name) {
            let value = value.to_str().map_err(|_| invalid_request())?;
            result.push(PreviewHeader {
                name: name.as_str().to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if result.len() > MAX_PREVIEW_HEADERS {
        return Err(invalid_request());
    }
    Ok(result)
}

fn decode_response(
    status: u16,
    headers: Vec<PreviewHeader>,
    body_base64: &str,
) -> Result<PreviewHttpResponse, PreviewError> {
    let status = StatusCode::from_u16(status).map_err(|_| source_unavailable())?;
    if headers.len() > MAX_PREVIEW_HEADERS {
        return Err(source_unavailable());
    }
    let mut safe_headers = HeaderMap::new();
    for header in headers {
        let name =
            HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| source_unavailable())?;
        let value = HeaderValue::from_str(&header.value).map_err(|_| source_unavailable())?;
        if name == http::header::LOCATION && !safe_redirect(&header.value) {
            return Err(source_unavailable());
        }
        if safe_header_name(&name) {
            safe_headers.append(name, value);
        }
    }
    let body = BASE64
        .decode(body_base64)
        .map_err(|_| source_unavailable())?;
    if body.len() > MAX_PREVIEW_BODY_BYTES {
        return Err(source_unavailable());
    }
    Ok(PreviewHttpResponse {
        status,
        headers: safe_headers,
        body,
    })
}

fn safe_header_name(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "authorization"
            | "cookie"
            | "connection"
            | "content-length"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn allowed_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET
            | Method::HEAD
            | Method::POST
            | Method::PUT
            | Method::PATCH
            | Method::DELETE
            | Method::OPTIONS
    )
}

fn safe_target(target: &str) -> bool {
    target.starts_with('/')
        && !target.starts_with("//")
        && target.len() <= 8_192
        && !target.bytes().any(|byte| byte.is_ascii_control())
}

fn safe_redirect(location: &str) -> bool {
    let first_segment = location.split(['/', '?', '#']).next().unwrap_or_default();
    !location.is_empty()
        && !location.starts_with("//")
        && !location.contains(['\\', '\r', '\n'])
        && !first_segment.contains(':')
}

fn portable_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn random_hex(prefix: &str, bytes: usize) -> Result<String, PreviewError> {
    let mut random = vec![0_u8; bytes];
    fill(&mut random).map_err(|_| unavailable())?;
    let mut value = String::with_capacity(prefix.len() + bytes * 2);
    value.push_str(prefix);
    for byte in random {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").map_err(|_| unavailable())?;
    }
    Ok(value)
}

const fn invalid_request() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::InvalidRequest,
        "preview request is invalid",
    )
}

const fn not_found() -> PreviewError {
    PreviewError::new(PreviewErrorKind::NotFound, "preview access was not found")
}

const fn source_unavailable() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::Unavailable,
        "preview source is unavailable",
    )
}

const fn unavailable() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::Unavailable,
        "preview service is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, worker: &str) -> PreviewSourceDescriptor {
        PreviewSourceDescriptor {
            source_id: id.to_owned(),
            worker_session_id: worker.to_owned(),
            repository_binding_id: "rbd_demo".to_owned(),
            mode: PreviewSourceMode::Live,
            candidate_commit: None,
        }
    }

    #[test]
    fn reconnect_changes_source_identity_and_invalidates_old_access() {
        let application = PreviewApplication::open(
            PathBuf::from("unused"),
            "https://preview.example".to_owned(),
        )
        .unwrap();
        let (sender, _receiver) = mpsc::channel(1);
        let first = source("pvs_demo", "ws_old");
        application.register_connection(
            "cnd_demo",
            BTreeMap::from([(first.source_id.clone(), first.clone())]),
            sender,
        );
        application.broker.lock().unwrap().accesses.insert(
            "pva_demo".to_owned(),
            PreviewAccess {
                owner_user_id: "usr_demo".to_owned(),
                client_node_id: "cnd_demo".to_owned(),
                source: first,
                token_digest: Sha256::digest(b"token").into(),
                expires_at: std::time::Instant::now() + ACCESS_TTL,
            },
        );
        let (sender, _receiver) = mpsc::channel(1);
        let replacement = source("pvs_demo", "ws_new");
        application.register_connection(
            "cnd_demo",
            BTreeMap::from([(replacement.source_id.clone(), replacement)]),
            sender,
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime
            .block_on(application.forward(
                "pva_demo",
                "token",
                &Method::GET,
                "/",
                &HeaderMap::new(),
                &[],
            ))
            .unwrap_err();
        assert_eq!(error.kind(), PreviewErrorKind::Conflict);
    }

    #[test]
    fn cloud_metadata_and_hop_by_hop_inputs_are_rejected_or_stripped() {
        assert!(!safe_target("//169.254.169.254/latest/meta-data"));
        assert!(!allowed_method(&Method::CONNECT));
        assert!(!safe_redirect("http://169.254.169.254/latest/meta-data"));
        assert!(safe_redirect("/sign-in"));
        let mut headers = HeaderMap::new();
        headers.insert(http::header::COOKIE, HeaderValue::from_static("secret=x"));
        headers.insert("x-preview-test", HeaderValue::from_static("ok"));
        assert_eq!(filtered_headers(&headers).unwrap().len(), 1);
    }

    #[test]
    fn backend_relays_only_the_access_bound_source_and_revocation_is_immediate() {
        let application = PreviewApplication::open(
            PathBuf::from("unused"),
            "https://preview.example".to_owned(),
        )
        .unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let source = source("pvs_demo", "ws_demo");
        let generation = application.register_connection(
            "cnd_demo",
            BTreeMap::from([(source.source_id.clone(), source.clone())]),
            sender,
        );
        application.broker.lock().unwrap().accesses.insert(
            "pva_demo".to_owned(),
            PreviewAccess {
                owner_user_id: "usr_demo".to_owned(),
                client_node_id: "cnd_demo".to_owned(),
                source,
                token_digest: Sha256::digest(b"token").into(),
                expires_at: std::time::Instant::now() + ACCESS_TTL,
            },
        );
        let relay = application.clone();
        let responder = application.clone();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let request = tokio::spawn(async move {
                relay
                    .forward(
                        "pva_demo",
                        "token",
                        &Method::GET,
                        "/index.html",
                        &HeaderMap::new(),
                        &[],
                    )
                    .await
            });
            let frame = receiver.recv().await.unwrap();
            let ServerPreviewFrame::HttpRequest {
                request_id,
                source_id,
                target,
                ..
            } = frame;
            assert_eq!(source_id, "pvs_demo");
            assert_eq!(target, "/index.html");
            assert!(responder.apply_device_frame(
                "cnd_demo",
                generation,
                DevicePreviewFrame::HttpResponse {
                    request_id,
                    status: 200,
                    headers: vec![PreviewHeader {
                        name: "content-type".to_owned(),
                        value: "text/html".to_owned(),
                    }],
                    body_base64: BASE64.encode("preview ok"),
                },
            ));
            let response = request.await.unwrap().unwrap();
            assert_eq!(response.status, StatusCode::OK);
            assert_eq!(response.body, b"preview ok");
        });
        application.revoke("usr_demo", "pva_demo").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime
            .block_on(application.forward(
                "pva_demo",
                "token",
                &Method::GET,
                "/",
                &HeaderMap::new(),
                &[],
            ))
            .unwrap_err();
        assert_eq!(error.kind(), PreviewErrorKind::NotFound);
    }
}
