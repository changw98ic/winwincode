// SPDX-License-Identifier: Apache-2.0

//! Device-initiated tunnel for exact, locally authorized preview services.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{SinkExt as _, StreamExt as _};
use http::header::{AUTHORIZATION, HeaderName, HeaderValue};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use winwincode_client_port::preview::{
    DevicePreviewFrame, MAX_PREVIEW_BODY_BYTES, MAX_PREVIEW_HEADERS, PREVIEW_TUNNEL_SCHEMA_VERSION,
    PreviewHeader, PreviewSourceDescriptor, PreviewSourceMode, ServerPreviewFrame,
    is_canonical_git_commit,
};

const MAX_HTTP_RESPONSE_BYTES: usize = MAX_PREVIEW_BODY_BYTES + 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_millis(250);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One service approved by the local run supervisor.
///
/// The socket stays device-local. Only loopback sockets are accepted, and a
/// request from the Backend selects this service by opaque `sourceId`; it can
/// never provide a host or port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedPreviewSource {
    descriptor: PreviewSourceDescriptor,
    socket: SocketAddr,
}

impl AuthorizedPreviewSource {
    /// Binds one preview identity to the exact loopback socket owned by its
    /// managed run.
    ///
    /// # Errors
    ///
    /// Rejects non-loopback/zero-port targets, malformed identities, and a
    /// live/frozen identity mismatch.
    pub fn new(
        descriptor: PreviewSourceDescriptor,
        socket: SocketAddr,
    ) -> Result<Self, PreviewTunnelError> {
        if !socket.ip().is_loopback() || socket.port() == 0 {
            return Err(PreviewTunnelError::new(
                "preview source must be one exact nonzero loopback socket",
            ));
        }
        for (label, value) in [
            ("sourceId", descriptor.source_id.as_str()),
            ("workRunId", descriptor.work_run_id.as_str()),
            (
                "repositoryBindingId",
                descriptor.repository_binding_id.as_str(),
            ),
        ] {
            if !portable_identifier(value) || (label == "workRunId" && !value.starts_with("wrn_")) {
                return Err(PreviewTunnelError::new(format!(
                    "preview {label} is invalid"
                )));
            }
        }
        match (descriptor.mode, descriptor.candidate_commit.as_deref()) {
            (PreviewSourceMode::Live, None) => {}
            (PreviewSourceMode::FrozenCandidate, Some(commit))
                if is_canonical_git_commit(commit) => {}
            _ => {
                return Err(PreviewTunnelError::new(
                    "preview mode and candidate commit do not match",
                ));
            }
        }
        Ok(Self { descriptor, socket })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &PreviewSourceDescriptor {
        &self.descriptor
    }
}

/// Device-local sources currently authorized by a managed preview run.
#[derive(Clone, Default, Debug)]
pub struct AuthorizedPreviewSourceRegistry {
    sources: Arc<RwLock<BTreeMap<String, AuthorizedPreviewSource>>>,
    revision: Arc<AtomicU64>,
}

impl AuthorizedPreviewSourceRegistry {
    /// Adds or replaces a source whose identity and loopback socket were
    /// already checked by [`AuthorizedPreviewSource::new`].
    pub fn insert(&self, source: AuthorizedPreviewSource) {
        let mut sources = self
            .sources
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sources.insert(source.descriptor.source_id.clone(), source);
        self.revision.fetch_add(1, Ordering::Release);
    }

    /// Removes one source and returns whether it was present.
    pub fn remove(&self, source_id: &str) -> bool {
        let mut sources = self
            .sources
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = sources.remove(source_id).is_some();
        if removed {
            self.revision.fetch_add(1, Ordering::Release);
        }
        removed
    }

    /// Returns the exact current sources used for the next registration.
    #[must_use]
    pub fn snapshot(&self) -> Vec<AuthorizedPreviewSource> {
        self.sources
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn get(&self, source_id: &str) -> Option<AuthorizedPreviewSource> {
        self.sources
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(source_id)
            .cloned()
    }

    fn is_empty(&self) -> bool {
        self.sources
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

/// One outbound preview tunnel. The caller owns reconnect policy; every call
/// to [`Self::run_once`] creates the connection from the Device Client to the
/// Backend, registers the current exact sources, and serves until disconnect.
#[derive(Clone, Debug)]
pub struct PreviewTunnelClient {
    endpoint: String,
    client_node_id: String,
    device_credential: String,
    registry: AuthorizedPreviewSourceRegistry,
    tls_connector: Option<Arc<rustls::ClientConfig>>,
}

impl PreviewTunnelClient {
    /// Builds one tunnel without opening a socket.
    ///
    /// # Errors
    ///
    /// Rejects a non-WebSocket endpoint, a malformed node id, an empty
    /// credential, duplicate sources, or an empty source set.
    pub fn new(
        endpoint: impl Into<String>,
        client_node_id: impl Into<String>,
        device_credential: impl Into<String>,
        sources: impl IntoIterator<Item = AuthorizedPreviewSource>,
    ) -> Result<Self, PreviewTunnelError> {
        let endpoint = endpoint.into();
        if !(endpoint.starts_with("ws://") || endpoint.starts_with("wss://")) {
            return Err(PreviewTunnelError::new(
                "preview tunnel endpoint must use ws:// or wss://",
            ));
        }
        let client_node_id = client_node_id.into();
        let device_credential = device_credential.into();
        if !portable_identifier(&client_node_id) || device_credential.is_empty() {
            return Err(PreviewTunnelError::new(
                "preview tunnel identity or credential is invalid",
            ));
        }
        let mut indexed = BTreeMap::new();
        for source in sources {
            if indexed
                .insert(source.descriptor.source_id.clone(), source)
                .is_some()
            {
                return Err(PreviewTunnelError::new("preview source ids must be unique"));
            }
        }
        if indexed.is_empty() {
            return Err(PreviewTunnelError::new(
                "preview tunnel requires at least one authorized source",
            ));
        }
        Ok(Self {
            endpoint,
            client_node_id,
            device_credential,
            registry: {
                let registry = AuthorizedPreviewSourceRegistry::default();
                for source in indexed.into_values() {
                    registry.insert(source);
                }
                registry
            },
            tls_connector: None,
        })
    }

    /// Builds a tunnel backed by a source registry that can change while the
    /// connection is reconnecting.
    ///
    /// # Errors
    ///
    /// Rejects a non-WebSocket endpoint, malformed node id, or empty
    /// credential.
    pub fn with_registry(
        endpoint: impl Into<String>,
        client_node_id: impl Into<String>,
        device_credential: impl Into<String>,
        registry: AuthorizedPreviewSourceRegistry,
    ) -> Result<Self, PreviewTunnelError> {
        Self::with_registry_and_tls_root_der(
            endpoint,
            client_node_id,
            device_credential,
            registry,
            None,
        )
    }

    /// Builds a registry-backed tunnel with an optional DER-encoded Server
    /// root, matching the Device exchange transport's TLS trust configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint, identity, credential, or TLS root
    /// is invalid.
    pub fn with_registry_and_tls_root_der(
        endpoint: impl Into<String>,
        client_node_id: impl Into<String>,
        device_credential: impl Into<String>,
        registry: AuthorizedPreviewSourceRegistry,
        tls_root_der: Option<Vec<u8>>,
    ) -> Result<Self, PreviewTunnelError> {
        let endpoint = endpoint.into();
        if !(endpoint.starts_with("ws://") || endpoint.starts_with("wss://")) {
            return Err(PreviewTunnelError::new(
                "preview tunnel endpoint must use ws:// or wss://",
            ));
        }
        let client_node_id = client_node_id.into();
        let device_credential = device_credential.into();
        if !portable_identifier(&client_node_id) || device_credential.is_empty() {
            return Err(PreviewTunnelError::new(
                "preview tunnel identity or credential is invalid",
            ));
        }
        let tls_connector = tls_root_der.map(build_tls_connector).transpose()?;
        Ok(Self {
            endpoint,
            client_node_id,
            device_credential,
            registry,
            tls_connector,
        })
    }

    /// Connects outward and serves preview requests until the Backend closes
    /// the tunnel.
    ///
    /// # Errors
    ///
    /// Returns a secret-free transport or protocol reason. Per-request local
    /// service failures are returned to the Backend as `502` responses and do
    /// not tear down the tunnel.
    pub async fn run_once(&self) -> Result<(), PreviewTunnelError> {
        self.run_once_with_stop(None).await
    }

    async fn run_once_with_stop(
        &self,
        stop: Option<&AtomicBool>,
    ) -> Result<(), PreviewTunnelError> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        if self.registry.is_empty() {
            return Err(PreviewTunnelError::new(
                "preview tunnel has no authorized sources",
            ));
        }
        let mut request = self
            .endpoint
            .as_str()
            .into_client_request()
            .map_err(|_| PreviewTunnelError::new("preview tunnel request is invalid"))?;
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.device_credential))
                .map_err(|_| PreviewTunnelError::new("preview tunnel credential is invalid"))?,
        );
        request.headers_mut().insert(
            "x-winwincode-client-node-id",
            HeaderValue::from_str(&self.client_node_id)
                .map_err(|_| PreviewTunnelError::new("preview tunnel node id is invalid"))?,
        );
        let connector = self.tls_connector.clone().map(Connector::Rustls);
        let connect = tokio::time::timeout(
            IO_TIMEOUT,
            connect_async_tls_with_config(request, None, false, connector),
        );
        let connect = tokio::select! {
            result = connect => result,
            () = wait_for_stop(stop) => return Ok(()),
        };
        let (socket, _) = connect
            .map_err(|_| PreviewTunnelError::new("preview tunnel connection timed out"))?
            .map_err(|_| PreviewTunnelError::new("preview tunnel connection failed"))?;
        let (mut writer, mut reader) = socket.split();
        let sources = self.registry.snapshot();
        let registration_revision = self.registry.revision();
        let register = DevicePreviewFrame::Register {
            schema_version: PREVIEW_TUNNEL_SCHEMA_VERSION.to_owned(),
            sources: sources
                .into_iter()
                .map(|source| source.descriptor.clone())
                .collect(),
        };
        writer
            .send(Message::Text(
                serde_json::to_string(&register)
                    .map_err(|_| PreviewTunnelError::new("preview registration failed"))?
                    .into(),
            ))
            .await
            .map_err(|_| PreviewTunnelError::new("preview tunnel send failed"))?;

        loop {
            if self.registry.revision() != registration_revision {
                let _ = writer.close().await;
                return Ok(());
            }
            let message = tokio::select! {
                message = tokio::time::timeout(STOP_POLL_INTERVAL, reader.next()) => {
                    match message {
                        Ok(message) => message,
                        Err(_) => continue,
                    }
                }
                () = wait_for_stop(stop) => {
                    let _ = writer.close().await;
                    return Ok(())
                }
            };
            let Some(message) = message else { break };
            let message =
                message.map_err(|_| PreviewTunnelError::new("preview tunnel receive failed"))?;
            let Message::Text(text) = message else {
                if message.is_close() {
                    return Ok(());
                }
                continue;
            };
            let request: ServerPreviewFrame = serde_json::from_str(&text)
                .map_err(|_| PreviewTunnelError::new("preview tunnel frame is invalid"))?;
            let response = self.forward(request).await;
            writer
                .send(Message::Text(
                    serde_json::to_string(&response)
                        .map_err(|_| PreviewTunnelError::new("preview response failed"))?
                        .into(),
                ))
                .await
                .map_err(|_| PreviewTunnelError::new("preview tunnel send failed"))?;
        }
        Ok(())
    }

    async fn forward(&self, frame: ServerPreviewFrame) -> DevicePreviewFrame {
        let ServerPreviewFrame::HttpRequest {
            request_id,
            source_id,
            method,
            target,
            headers,
            body_base64,
        } = frame;
        let result = match self.registry.get(&source_id) {
            Some(source) => {
                forward_http(source.socket, &method, &target, &headers, &body_base64).await
            }
            None => Err(PreviewTunnelError::new(
                "preview source is not authorized on this device",
            )),
        };
        match result {
            Ok((status, headers, body_base64)) => DevicePreviewFrame::HttpResponse {
                request_id,
                status,
                headers,
                body_base64,
            },
            Err(_) => DevicePreviewFrame::HttpResponse {
                request_id,
                status: 502,
                headers: vec![PreviewHeader {
                    name: "content-type".to_owned(),
                    value: "text/plain; charset=utf-8".to_owned(),
                }],
                body_base64: BASE64.encode("preview source unavailable"),
            },
        }
    }
}

/// Owns one reconnecting outbound tunnel and stops it when dropped.
pub struct PreviewTunnelSupervisor {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    registry: AuthorizedPreviewSourceRegistry,
}

impl fmt::Debug for PreviewTunnelSupervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewTunnelSupervisor")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl PreviewTunnelSupervisor {
    /// Starts a tunnel loop. An empty registry stays idle until a managed run
    /// publishes an authorized source.
    ///
    /// # Errors
    ///
    /// Rejects invalid tunnel identity values or a thread that cannot be
    /// created.
    pub fn start(
        endpoint: impl Into<String>,
        client_node_id: impl Into<String>,
        device_credential: impl Into<String>,
        registry: AuthorizedPreviewSourceRegistry,
    ) -> Result<Self, PreviewTunnelError> {
        Self::start_with_tls_root_der(endpoint, client_node_id, device_credential, registry, None)
    }

    /// Starts a tunnel loop with the optional Device Server TLS root.
    ///
    /// # Errors
    ///
    /// Returns an error when the tunnel configuration is invalid or the
    /// supervisor thread cannot be created.
    pub fn start_with_tls_root_der(
        endpoint: impl Into<String>,
        client_node_id: impl Into<String>,
        device_credential: impl Into<String>,
        registry: AuthorizedPreviewSourceRegistry,
        tls_root_der: Option<Vec<u8>>,
    ) -> Result<Self, PreviewTunnelError> {
        let client = PreviewTunnelClient::with_registry_and_tls_root_der(
            endpoint,
            client_node_id,
            device_credential,
            registry.clone(),
            tls_root_der,
        )?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("winwincode-preview-tunnel".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let mut reconnect_delay = RECONNECT_DELAY;
                while !thread_stop.load(Ordering::Acquire) {
                    if client.registry.is_empty() {
                        sleep_with_stop(&thread_stop, STOP_POLL_INTERVAL);
                        continue;
                    }
                    let result = runtime.block_on(client.run_once_with_stop(Some(&thread_stop)));
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let delay = if result.is_ok() {
                        reconnect_delay = RECONNECT_DELAY;
                        RECONNECT_DELAY
                    } else {
                        let delay = reconnect_delay;
                        reconnect_delay =
                            reconnect_delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
                        delay
                    };
                    sleep_with_stop(&thread_stop, delay);
                }
            })
            .map_err(|_| PreviewTunnelError::new("preview tunnel supervisor thread failed"))?;
        Ok(Self {
            stop,
            thread: Some(thread),
            registry,
        })
    }

    /// Returns the registry used for managed-run source updates.
    #[must_use]
    pub const fn registry(&self) -> &AuthorizedPreviewSourceRegistry {
        &self.registry
    }

    /// Stops the connection and waits for the reconnect loop to exit.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PreviewTunnelSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn wait_for_stop(stop: Option<&AtomicBool>) {
    let Some(stop) = stop else {
        std::future::pending::<()>().await;
        return;
    };
    while !stop.load(Ordering::Acquire) {
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
}

const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

fn sleep_with_stop(stop: &AtomicBool, duration: Duration) {
    let started = std::time::Instant::now();
    while !stop.load(Ordering::Acquire) {
        let elapsed = started.elapsed();
        if elapsed >= duration {
            break;
        }
        thread::sleep(duration.saturating_sub(elapsed).min(STOP_POLL_INTERVAL));
    }
}

fn build_tls_connector(root_der: Vec<u8>) -> Result<Arc<rustls::ClientConfig>, PreviewTunnelError> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(root_der))
        .map_err(|_| PreviewTunnelError::new("preview tunnel TLS root is invalid"))?;
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|_| PreviewTunnelError::new("preview tunnel TLS config is invalid"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

async fn forward_http(
    socket: SocketAddr,
    method: &str,
    target: &str,
    headers: &[PreviewHeader],
    body_base64: &str,
) -> Result<(u16, Vec<PreviewHeader>, String), PreviewTunnelError> {
    if !allowed_method(method) || !safe_target(target) || headers.len() > MAX_PREVIEW_HEADERS {
        return Err(PreviewTunnelError::new("preview HTTP request is invalid"));
    }
    let body = BASE64
        .decode(body_base64)
        .map_err(|_| PreviewTunnelError::new("preview HTTP body is invalid"))?;
    if body.len() > MAX_PREVIEW_BODY_BYTES {
        return Err(PreviewTunnelError::new("preview HTTP body is too large"));
    }
    let mut request = format!(
        "{method} {target} HTTP/1.1\r\nHost: {socket}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for header in headers {
        if safe_forward_header(header) {
            request.push_str(&header.name);
            request.push_str(": ");
            request.push_str(&header.value);
            request.push_str("\r\n");
        }
    }
    request.push_str("\r\n");

    let mut stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(socket))
        .await
        .map_err(|_| PreviewTunnelError::new("preview source connect timed out"))?
        .map_err(|_| PreviewTunnelError::new("preview source connect failed"))?;
    tokio::time::timeout(IO_TIMEOUT, async {
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|_| PreviewTunnelError::new("preview source write timed out"))?
    .map_err(|_| PreviewTunnelError::new("preview source write failed"))?;
    let mut response = Vec::new();
    tokio::time::timeout(
        IO_TIMEOUT,
        stream
            .take(u64::try_from(MAX_HTTP_RESPONSE_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut response),
    )
    .await
    .map_err(|_| PreviewTunnelError::new("preview source read timed out"))?
    .map_err(|_| PreviewTunnelError::new("preview source read failed"))?;
    if response.len() > MAX_HTTP_RESPONSE_BYTES {
        return Err(PreviewTunnelError::new(
            "preview source response is too large",
        ));
    }
    parse_http_response(&response)
}

fn parse_http_response(
    response: &[u8],
) -> Result<(u16, Vec<PreviewHeader>, String), PreviewTunnelError> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| PreviewTunnelError::new("preview source response has no headers"))?;
    let head = std::str::from_utf8(&response[..header_end])
        .map_err(|_| PreviewTunnelError::new("preview source response headers are invalid"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|status| (100..=599).contains(status))
        .ok_or_else(|| PreviewTunnelError::new("preview source status is invalid"))?;
    let mut headers = Vec::new();
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(PreviewTunnelError::new(
                "preview source response header is invalid",
            ));
        };
        let header = PreviewHeader {
            name: name.trim().to_ascii_lowercase(),
            value: value.trim().to_owned(),
        };
        if header.name == "content-length" {
            content_length = header.value.parse::<usize>().ok();
        } else if header.name == "transfer-encoding"
            && header.value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
        if header.name == "location" && !safe_redirect(&header.value) {
            return Err(PreviewTunnelError::new(
                "preview source redirect leaves the authorized service",
            ));
        }
        if safe_forward_header(&header) {
            headers.push(header);
        }
    }
    if headers.len() > MAX_PREVIEW_HEADERS {
        return Err(PreviewTunnelError::new(
            "preview source returned too many headers",
        ));
    }
    let encoded = &response[header_end + 4..];
    let body = if chunked {
        decode_chunked(encoded)?
    } else if let Some(length) = content_length {
        encoded
            .get(..length)
            .ok_or_else(|| PreviewTunnelError::new("preview source body is incomplete"))?
            .to_vec()
    } else {
        encoded.to_vec()
    };
    if body.len() > MAX_PREVIEW_BODY_BYTES {
        return Err(PreviewTunnelError::new(
            "preview source response body is too large",
        ));
    }
    Ok((status, headers, BASE64.encode(body)))
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, PreviewTunnelError> {
    let mut decoded = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| PreviewTunnelError::new("preview chunk size is missing"))?;
        let size = std::str::from_utf8(&body[..line_end])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|value| usize::from_str_radix(value.trim(), 16).ok())
            .ok_or_else(|| PreviewTunnelError::new("preview chunk size is invalid"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        let chunk = body
            .get(..size)
            .ok_or_else(|| PreviewTunnelError::new("preview chunk is incomplete"))?;
        decoded.extend_from_slice(chunk);
        if decoded.len() > MAX_PREVIEW_BODY_BYTES || body.get(size..size + 2) != Some(b"\r\n") {
            return Err(PreviewTunnelError::new("preview chunked body is invalid"));
        }
        body = &body[size + 2..];
    }
}

fn safe_forward_header(header: &PreviewHeader) -> bool {
    let Ok(name) = HeaderName::from_bytes(header.name.as_bytes()) else {
        return false;
    };
    if HeaderValue::from_str(&header.value).is_err() {
        return false;
    }
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
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
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

fn allowed_method(method: &str) -> bool {
    matches!(
        method,
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
    )
}

fn portable_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

/// Secret-free preview tunnel failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewTunnelError {
    message: String,
}

impl PreviewTunnelError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PreviewTunnelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PreviewTunnelError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> PreviewSourceDescriptor {
        PreviewSourceDescriptor {
            source_id: "pvs_demo".to_owned(),
            work_run_id: "wrn_demo".to_owned(),
            repository_binding_id: "rbd_demo".to_owned(),
            mode: PreviewSourceMode::Live,
            candidate_commit: None,
        }
    }

    #[test]
    fn only_one_exact_loopback_service_can_be_authorized() {
        assert!(
            AuthorizedPreviewSource::new(descriptor(), "127.0.0.1:4173".parse().unwrap()).is_ok()
        );
        assert!(
            AuthorizedPreviewSource::new(descriptor(), "169.254.169.254:80".parse().unwrap())
                .is_err()
        );
        assert!(
            AuthorizedPreviewSource::new(descriptor(), "127.0.0.1:0".parse().unwrap()).is_err()
        );
    }

    #[test]
    fn preview_source_identity_is_the_work_run() {
        assert_eq!(descriptor().work_run_id, "wrn_demo");
        let json = serde_json::to_value(descriptor()).expect("serializes");
        assert_eq!(json["workRunId"], "wrn_demo");
        assert!(json.get("workerSessionId").is_none());
    }

    #[test]
    fn absolute_targets_and_sensitive_headers_never_reach_the_source() {
        assert!(!safe_target("http://127.0.0.1:9000/private"));
        assert!(!safe_target("//169.254.169.254/latest/meta-data"));
        assert!(!allowed_method("CONNECT"));
        assert!(!safe_forward_header(&PreviewHeader {
            name: "cookie".to_owned(),
            value: "session=secret".to_owned(),
        }));
        assert!(!safe_redirect("http://169.254.169.254/latest/meta-data"));
        assert!(!safe_redirect("javascript:alert(1)"));
        assert!(!safe_redirect("//127.0.0.1:9000/private"));
        assert!(safe_redirect("/sign-in"));
        assert!(safe_redirect("next"));
    }

    #[test]
    fn registry_updates_and_unknown_sources_fail_closed() {
        let registry = AuthorizedPreviewSourceRegistry::default();
        let source =
            AuthorizedPreviewSource::new(descriptor(), "127.0.0.1:4173".parse().expect("socket"))
                .expect("source");
        registry.insert(source);
        assert_eq!(registry.snapshot().len(), 1);

        let client = PreviewTunnelClient::with_registry(
            "ws://127.0.0.1:1",
            "cnd_demo",
            "device-secret",
            registry.clone(),
        )
        .expect("client");
        let response = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(client.forward(ServerPreviewFrame::HttpRequest {
                request_id: "pvr_unknown".to_owned(),
                source_id: "pvs_unknown".to_owned(),
                method: "GET".to_owned(),
                target: "/".to_owned(),
                headers: Vec::new(),
                body_base64: String::new(),
            }));
        assert!(matches!(
            response,
            DevicePreviewFrame::HttpResponse { status: 502, .. }
        ));
        assert!(registry.remove("pvs_demo"));
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn supervisor_reconnects_with_current_sources_and_stops_cleanly() {
        use tokio_tungstenite::accept_async;

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let counts = Arc::new(RwLock::new(Vec::new()));
        let server_counts = Arc::clone(&counts);
        let server = thread::spawn(move || {
            let server_runtime = tokio::runtime::Runtime::new().expect("server runtime");
            for _ in 0..3 {
                let (stream, _) = server_runtime.block_on(listener.accept()).expect("accept");
                let mut socket = server_runtime
                    .block_on(accept_async(stream))
                    .expect("websocket");
                let Some(Ok(Message::Text(register))) = server_runtime.block_on(socket.next())
                else {
                    panic!("expected register");
                };
                let DevicePreviewFrame::Register { sources, .. } =
                    serde_json::from_str(&register).expect("register frame")
                else {
                    panic!("expected register frame");
                };
                server_counts
                    .write()
                    .expect("counts lock")
                    .push(sources.len());
                while server_runtime.block_on(socket.next()).is_some() {}
            }
        });

        let registry = AuthorizedPreviewSourceRegistry::default();
        registry.insert(
            AuthorizedPreviewSource::new(descriptor(), "127.0.0.1:4173".parse().expect("socket"))
                .expect("source"),
        );
        let supervisor = PreviewTunnelSupervisor::start(
            format!("ws://{address}"),
            "cnd_demo",
            "device-secret",
            registry.clone(),
        )
        .expect("supervisor");

        let second = PreviewSourceDescriptor {
            source_id: "pvs_second".to_owned(),
            ..descriptor()
        };
        let second =
            AuthorizedPreviewSource::new(second, "127.0.0.1:4174".parse().expect("socket"))
                .expect("second source");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while counts.read().expect("counts lock").is_empty() && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        registry.insert(second);
        while counts.read().expect("counts lock").len() < 2 && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(registry.remove("pvs_demo"));
        while counts.read().expect("counts lock").len() < 3 && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        supervisor.shutdown();
        server.join().expect("server");
        assert_eq!(*counts.read().expect("counts lock"), vec![1, 2, 1]);
    }

    #[test]
    fn frozen_candidate_requires_a_full_lowercase_sha1_or_sha256_object_id() {
        for length in [40, 64] {
            let mut candidate = descriptor();
            candidate.mode = PreviewSourceMode::FrozenCandidate;
            candidate.candidate_commit = Some("a".repeat(length));
            assert!(
                AuthorizedPreviewSource::new(candidate, "127.0.0.1:4173".parse().unwrap()).is_ok()
            );
        }
        for commit in [
            "a".repeat(41),
            "a".repeat(63),
            "A".repeat(40),
            format!("{}g", "a".repeat(39)),
        ] {
            let mut candidate = descriptor();
            candidate.mode = PreviewSourceMode::FrozenCandidate;
            candidate.candidate_commit = Some(commit);
            assert!(
                AuthorizedPreviewSource::new(candidate, "127.0.0.1:4173".parse().unwrap()).is_err()
            );
        }
    }

    #[test]
    fn exact_authorized_service_is_forwarded() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /app?mode=preview HTTP/1.1\r\n"));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 16\r\n\r\n<h1>preview</h1>",
                )
                .unwrap();
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (status, headers, body) = runtime
            .block_on(forward_http(address, "GET", "/app?mode=preview", &[], ""))
            .unwrap();
        server.join().unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers[0].name, "content-type");
        assert_eq!(BASE64.decode(body).unwrap(), b"<h1>preview</h1>");
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn outbound_tunnel_authenticates_registers_and_relays() {
        use std::io::{Read as _, Write as _};

        use tokio_tungstenite::accept_hdr_async;
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        let source_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source_listener.local_addr().unwrap();
        let source_server = thread::spawn(move || {
            let (mut socket, _) = source_listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = socket.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /app HTTP/1.1\r\n"));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\nok",
                )
                .unwrap();
        });

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let tunnel_listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let tunnel_address = tunnel_listener.local_addr().unwrap();
        let source = AuthorizedPreviewSource::new(descriptor(), source_address).unwrap();
        let tunnel = PreviewTunnelClient::new(
            format!("ws://{tunnel_address}/internal/v1/preview/tunnel"),
            "cnd_demo",
            "device-secret",
            [source],
        )
        .unwrap();
        let backend = async move {
            let (stream, _) = tunnel_listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(stream, |request: &Request, response: Response| {
                assert_eq!(request.headers()[AUTHORIZATION], "Bearer device-secret");
                assert_eq!(request.headers()["x-winwincode-client-node-id"], "cnd_demo");
                Ok(response)
            })
            .await
            .unwrap();
            let Message::Text(register) = socket.next().await.unwrap().unwrap() else {
                panic!("expected registration")
            };
            assert!(matches!(
                serde_json::from_str::<DevicePreviewFrame>(&register).unwrap(),
                DevicePreviewFrame::Register { sources, .. } if sources.len() == 1
            ));
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerPreviewFrame::HttpRequest {
                        request_id: "pvr_demo".to_owned(),
                        source_id: "pvs_demo".to_owned(),
                        method: "GET".to_owned(),
                        target: "/app".to_owned(),
                        headers: Vec::new(),
                        body_base64: String::new(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let Message::Text(response) = socket.next().await.unwrap().unwrap() else {
                panic!("expected response")
            };
            assert!(matches!(
                serde_json::from_str::<DevicePreviewFrame>(&response).unwrap(),
                DevicePreviewFrame::HttpResponse { status: 200, body_base64, .. }
                    if BASE64.decode(&body_base64).unwrap() == b"ok"
            ));
            socket.close(None).await.unwrap();
        };
        let (client_result, ()) =
            runtime.block_on(futures::future::join(tunnel.run_once(), backend));
        client_result.unwrap();
        source_server.join().unwrap();
    }

    #[test]
    fn custom_tls_root_connects_to_wss_tunnel() {
        use rcgen::generate_simple_self_signed;
        use tokio_rustls::TlsAcceptor;
        use tokio_tungstenite::accept_async;

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rcgen::CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
                ),
            )
            .expect("server TLS config");

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let source = AuthorizedPreviewSource::new(
            descriptor(),
            "127.0.0.1:4173".parse().expect("source socket"),
        )
        .expect("source");
        let registry = AuthorizedPreviewSourceRegistry::default();
        registry.insert(source);
        let client = PreviewTunnelClient::with_registry_and_tls_root_der(
            format!(
                "wss://localhost:{}/internal/v1/preview/tunnel",
                address.port()
            ),
            "cnd_demo",
            "device-secret",
            registry,
            Some(cert.der().to_vec()),
        )
        .expect("client");

        let backend = async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let stream = TlsAcceptor::from(Arc::new(server_config))
                .accept(stream)
                .await
                .expect("TLS handshake");
            let mut socket = accept_async(stream).await.expect("websocket");
            let Some(Ok(Message::Text(register))) = socket.next().await else {
                panic!("expected registration");
            };
            assert!(matches!(
                serde_json::from_str::<DevicePreviewFrame>(&register).expect("register frame"),
                DevicePreviewFrame::Register { .. }
            ));
            socket.close(None).await.expect("close");
        };

        let (client_result, ()) =
            runtime.block_on(futures::future::join(client.run_once(), backend));
        client_result.expect("custom root WSS connection");
    }
}
