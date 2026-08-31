// SPDX-License-Identifier: Apache-2.0

//! Bounded HTTPS client for the canonical remote Execution Port exchange.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use winwincode_codex::WorkerExecutionPort;
use winwincode_domain::{ExecutionMessageId, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    FrameDirection, RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
    TypedFrame,
};

const MAX_HTTP_RESPONSE_BYTES: usize = 34 * 1024 * 1024;

/// Secret-free separated Worker transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteWorkerPortError;

impl fmt::Display for RemoteWorkerPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote Worker transport failed")
    }
}

impl std::error::Error for RemoteWorkerPortError {}

#[derive(Clone)]
struct Endpoint {
    host: String,
    port: u16,
}

struct SharedRemoteState {
    inbox: VecDeque<(ExecutionMessageId, ExecutionPortMessage)>,
    processing: Vec<ExecutionMessageId>,
    acknowledgements: Vec<ExecutionMessageId>,
}

/// Cloneable Worker-side delivery handle. A delivery is confirmed only after
/// [`WorkerMain`](crate::WorkerMain) accepts the exact generated message.
#[derive(Clone)]
pub struct RemoteWorkerTransportHandle {
    state: Arc<Mutex<SharedRemoteState>>,
}

impl RemoteWorkerTransportHandle {
    /// Takes the next validated Control Plane delivery.
    ///
    /// # Errors
    ///
    /// Returns a stable transport error if the process queue is unavailable.
    pub fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, RemoteWorkerPortError> {
        let mut state = self.state.lock().map_err(|_| RemoteWorkerPortError)?;
        let delivery = state.inbox.pop_front();
        if let Some((id, _)) = &delivery
            && !state.processing.contains(id)
        {
            state.processing.push(id.clone());
        }
        Ok(delivery)
    }

    /// Marks one delivery ready for confirmation on the next authenticated
    /// exchange. Lost HTTP responses therefore replay the same delivery.
    ///
    /// # Errors
    ///
    /// Returns a stable transport error if the process queue is unavailable.
    pub fn confirm(&self, id: ExecutionMessageId) -> Result<(), RemoteWorkerPortError> {
        let mut state = self.state.lock().map_err(|_| RemoteWorkerPortError)?;
        state.processing.retain(|processing| processing != &id);
        if !state.acknowledgements.contains(&id) {
            state.acknowledgements.push(id);
        }
        Ok(())
    }

    /// Releases an unaccepted delivery so the Server's next replay can put it
    /// back in the process inbox.
    ///
    /// # Errors
    ///
    /// Returns a stable transport error if the process queue is unavailable.
    pub fn retry(&self, id: &ExecutionMessageId) -> Result<(), RemoteWorkerPortError> {
        let mut state = self.state.lock().map_err(|_| RemoteWorkerPortError)?;
        state.processing.retain(|processing| processing != id);
        Ok(())
    }
}

/// Worker outbound port using a fresh authenticated TLS connection for each
/// exchange so ordinary disconnects and Server restarts reconnect naturally.
pub struct RemoteWorkerPort {
    endpoint: Endpoint,
    credential_path: PathBuf,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    tls: Arc<rustls::ClientConfig>,
    timeout: Duration,
    state: Arc<Mutex<SharedRemoteState>>,
}

impl RemoteWorkerPort {
    /// Opens a TLS client from one DER trust root and a private credential
    /// file. The only accepted origin form is `https://HOST:PORT`.
    ///
    /// # Errors
    ///
    /// Rejects malformed origins, trust roots, timeouts, and non-private
    /// credential files before the Worker registers.
    pub fn open(
        origin: &str,
        tls_root_der: &[u8],
        credential_path: impl Into<PathBuf>,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        timeout: Duration,
    ) -> Result<(Self, RemoteWorkerTransportHandle), RemoteWorkerPortError> {
        if timeout.is_zero() || timeout > Duration::from_mins(1) {
            return Err(RemoteWorkerPortError);
        }
        let endpoint = parse_origin(origin)?;
        let credential_path = credential_path.into();
        read_private_credential(&credential_path)?;
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                tls_root_der.to_vec(),
            ))
            .map_err(|_| RemoteWorkerPortError)?;
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(|_| RemoteWorkerPortError)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let state = Arc::new(Mutex::new(SharedRemoteState {
            inbox: VecDeque::new(),
            processing: Vec::new(),
            acknowledgements: Vec::new(),
        }));
        Ok((
            Self {
                endpoint,
                credential_path,
                worker_id,
                worker_instance_id,
                tls: Arc::new(tls),
                timeout,
                state: Arc::clone(&state),
            },
            RemoteWorkerTransportHandle { state },
        ))
    }

    async fn exchange(
        &mut self,
        message: ExecutionPortMessage,
    ) -> Result<(), RemoteWorkerPortError> {
        let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
            .and_then(|frame| RemoteTransportAdapter::<NoopCore>::encode(&frame))
            .map_err(|_| {
                remote_transport_debug("outbound generated frame is invalid");
                RemoteWorkerPortError
            })?;
        let acknowledgements = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError)?
            .acknowledgements
            .clone();
        let request = RemoteExchangeRequest::new(
            self.worker_id.clone(),
            self.worker_instance_id.clone(),
            acknowledgements.clone(),
            frame,
        )
        .and_then(|request| request.encode())
        .map_err(|_| {
            remote_transport_debug("outbound exchange request is invalid");
            RemoteWorkerPortError
        })?;
        let credential = read_private_credential(&self.credential_path).inspect_err(|_| {
            remote_transport_debug("credential file failed private-file validation");
        })?;
        let response = Box::pin(tokio::time::timeout(
            self.timeout,
            send_https_request(&self.endpoint, Arc::clone(&self.tls), &credential, &request),
        ))
        .await
        .map_err(|_| {
            remote_transport_debug("exchange timed out");
            RemoteWorkerPortError
        })?
        .inspect_err(|_| remote_transport_debug("HTTPS request failed"))?;
        let response = RemoteExchangeResponse::decode(&response).map_err(|_| {
            remote_transport_debug("exchange response did not match the bounded canonical schema");
            RemoteWorkerPortError
        })?;
        let mut state = self.state.lock().map_err(|_| RemoteWorkerPortError)?;
        state
            .acknowledgements
            .retain(|id| !acknowledgements.contains(id));
        for delivery in response.deliveries() {
            if state
                .inbox
                .iter()
                .any(|(existing, _)| existing == &delivery.delivery_id)
                || state.processing.contains(&delivery.delivery_id)
                || state.acknowledgements.contains(&delivery.delivery_id)
            {
                continue;
            }
            let frame = RemoteTransportAdapter::<NoopCore>::decode(&delivery.frame)
                .map_err(|_| RemoteWorkerPortError)?;
            state
                .inbox
                .push_back((delivery.delivery_id.clone(), frame.message().clone()));
        }
        Ok(())
    }
}

impl WorkerExecutionPort for RemoteWorkerPort {
    type Error = RemoteWorkerPortError;

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.exchange(message)
    }
}

struct NoopCore;

impl winwincode_execution_port::transport::ExecutionPortCore for NoopCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

async fn send_https_request(
    endpoint: &Endpoint,
    tls: Arc<rustls::ClientConfig>,
    credential: &[u8],
    body: &[u8],
) -> Result<Vec<u8>, RemoteWorkerPortError> {
    let token = std::str::from_utf8(credential).map_err(|_| RemoteWorkerPortError)?;
    if token.bytes().any(|byte| !(0x21..=0x7e).contains(&byte)) {
        return Err(RemoteWorkerPortError);
    }
    let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|_| {
            remote_transport_debug("TCP connect failed");
            RemoteWorkerPortError
        })?;
    let server_name = rustls::pki_types::ServerName::try_from(endpoint.host.clone())
        .map_err(|_| RemoteWorkerPortError)?;
    let mut stream = TlsConnector::from(tls)
        .connect(server_name, stream)
        .await
        .map_err(|_| {
            remote_transport_debug("TLS handshake failed");
            RemoteWorkerPortError
        })?;
    let head = format!(
        "POST /internal/v1/execution-port/exchange HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.host,
        endpoint.port,
        token,
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(|_| {
        remote_transport_debug("HTTPS header write failed");
        RemoteWorkerPortError
    })?;
    stream.write_all(body).await.map_err(|_| {
        remote_transport_debug("HTTPS body write failed");
        RemoteWorkerPortError
    })?;
    stream.flush().await.map_err(|_| {
        remote_transport_debug("HTTPS request flush failed");
        RemoteWorkerPortError
    })?;
    let mut response = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    while !http_response_complete(&response)? {
        let read = stream.read(&mut chunk).await.map_err(|_| {
            remote_transport_debug("HTTPS response read failed");
            RemoteWorkerPortError
        })?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(RemoteWorkerPortError);
        }
    }
    parse_http_response(&response)
}

fn http_response_complete(response: &[u8]) -> Result<bool, RemoteWorkerPortError> {
    let Some(split) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Ok(false);
    };
    let head = std::str::from_utf8(&response[..split]).map_err(|_| RemoteWorkerPortError)?;
    let content_length = head
        .lines()
        .skip(1)
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .ok_or(RemoteWorkerPortError)?;
    let expected = split
        .checked_add(4)
        .and_then(|value| value.checked_add(content_length))
        .ok_or(RemoteWorkerPortError)?;
    if response.len() > expected {
        return Err(RemoteWorkerPortError);
    }
    Ok(response.len() == expected)
}

fn parse_http_response(response: &[u8]) -> Result<Vec<u8>, RemoteWorkerPortError> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| {
            remote_transport_debug("HTTPS response has no header boundary");
            RemoteWorkerPortError
        })?;
    let head = std::str::from_utf8(&response[..split]).map_err(|_| RemoteWorkerPortError)?;
    if !head
        .lines()
        .next()
        .is_some_and(|line| line == "HTTP/1.1 200 OK" || line == "HTTP/1.0 200 OK")
    {
        remote_transport_debug("HTTPS response status is not 200");
        return Err(RemoteWorkerPortError);
    }
    let content_length = head
        .lines()
        .skip(1)
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .ok_or_else(|| {
            remote_transport_debug("HTTPS response has no Content-Length");
            RemoteWorkerPortError
        })?;
    let body = &response[split + 4..];
    if body.len() != content_length {
        remote_transport_debug("HTTPS response body length differs from Content-Length");
        return Err(RemoteWorkerPortError);
    }
    Ok(body.to_vec())
}

fn remote_transport_debug(message: &str) {
    if std::env::var_os("WWC_DEBUG_REMOTE_WORKER").is_some() {
        eprintln!("remote Worker transport: {message}");
    }
}

fn parse_origin(origin: &str) -> Result<Endpoint, RemoteWorkerPortError> {
    let authority = origin
        .strip_prefix("https://")
        .filter(|value| !value.contains('/') && !value.contains('@'))
        .ok_or(RemoteWorkerPortError)?;
    let (host, port) = authority.rsplit_once(':').ok_or(RemoteWorkerPortError)?;
    if host.is_empty() || host.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RemoteWorkerPortError);
    }
    let port = port.parse::<u16>().map_err(|_| RemoteWorkerPortError)?;
    if port == 0 {
        return Err(RemoteWorkerPortError);
    }
    Ok(Endpoint {
        host: host.to_owned(),
        port,
    })
}

#[cfg(unix)]
fn read_private_credential(path: &Path) -> Result<Vec<u8>, RemoteWorkerPortError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|_| RemoteWorkerPortError)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(RemoteWorkerPortError);
    }
    let bytes = fs::read(path).map_err(|_| RemoteWorkerPortError)?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err(RemoteWorkerPortError);
    }
    Ok(bytes)
}
