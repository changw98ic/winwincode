// SPDX-License-Identifier: Apache-2.0

//! Bounded HTTPS client for the canonical remote Execution Port exchange.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_rustls::rustls;
use winwincode_codex::WorkerExecutionPort;
use winwincode_domain::{ExecutionMessageId, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    FrameDirection, RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
    TypedFrame,
};

const MAX_HTTP_RESPONSE_BYTES: usize =
    winwincode_execution_port::transport::MAX_REMOTE_RESPONSE_BYTES;

/// Secret-free separated Worker transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerPortError {
    /// Connection, timeout, or temporary Server authority failure.
    Transport,
    /// ACKs and controls were exchanged; the upstream frame must be retried.
    Backpressure,
    /// The Server permanently rejected authentication or protocol authority.
    Rejected,
    /// A permanent protocol failure; execution authority remains distinct.
    Protocol,
    /// One message or response exceeds its deterministic wire budget.
    TooLarge,
}

impl fmt::Display for RemoteWorkerPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Transport => "REMOTE_WORKER_TRANSPORT_UNAVAILABLE",
            Self::Backpressure => "REMOTE_WORKER_BACKPRESSURE",
            Self::Rejected => "REMOTE_WORKER_AUTHENTICATION_REJECTED",
            Self::Protocol => "REMOTE_WORKER_PROTOCOL_INVALID",
            Self::TooLarge => "REMOTE_WORKER_MESSAGE_TOO_LARGE",
        })
    }
}

impl std::error::Error for RemoteWorkerPortError {}

#[derive(Clone)]
pub(crate) struct Endpoint {
    host: String,
    port: u16,
}

struct SharedRemoteState {
    inbox: VecDeque<(ExecutionMessageId, ExecutionPortMessage)>,
    processing: Vec<ExecutionMessageId>,
    acknowledgements: Vec<ExecutionMessageId>,
    terminal_error: Option<RemoteWorkerPortError>,
}

impl SharedRemoteState {
    fn receive_response(
        &mut self,
        response: &RemoteExchangeResponse,
        acknowledgements: &[ExecutionMessageId],
    ) -> Result<(), RemoteWorkerPortError> {
        if !response.has_acceptance_receipt() {
            return Err(RemoteWorkerPortError::Protocol);
        }

        let state = self;
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
                .map_err(remote_frame_error)?;
            state
                .inbox
                .push_back((delivery.delivery_id.clone(), frame.message().clone()));
        }
        if response.frame_accepted() {
            Ok(())
        } else {
            Err(RemoteWorkerPortError::Backpressure)
        }
    }

    fn acknowledgements_for_exchange(&self) -> Vec<ExecutionMessageId> {
        self.acknowledgements
            .iter()
            .take(winwincode_execution_port::transport::MAX_REMOTE_ACKNOWLEDGEMENTS)
            .cloned()
            .collect()
    }
}

/// Cloneable Worker-side delivery handle. A delivery is confirmed only after
/// [`WorkerMain`](crate::WorkerMain) accepts the exact generated message.
#[derive(Clone)]
pub struct RemoteWorkerTransportHandle {
    state: Arc<Mutex<SharedRemoteState>>,
}

impl RemoteWorkerTransportHandle {
    /// A permanent Server rejection ends this process instead of replaying dead authority.
    #[must_use]
    pub fn authority_rejected(&self) -> bool {
        self.terminal_error() == Some(RemoteWorkerPortError::Rejected)
    }

    /// Permanent encoding/protocol faults stop replay without revoking authority.
    #[must_use]
    pub fn terminal_error(&self) -> Option<RemoteWorkerPortError> {
        self.state
            .lock()
            .map_or(Some(RemoteWorkerPortError::Transport), |state| {
                state.terminal_error
            })
    }

    /// Takes the next validated Control Plane delivery.
    ///
    /// # Errors
    ///
    /// Returns a stable transport error if the process queue is unavailable.
    pub fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, RemoteWorkerPortError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?;
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?;
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?;
        state.processing.retain(|processing| processing != id);
        Ok(())
    }
}

/// Worker outbound port using a fresh authenticated TLS connection for each
/// exchange so ordinary disconnects and Server restarts reconnect naturally.
pub struct RemoteWorkerPort {
    accounting_enabled: std::sync::atomic::AtomicBool,
    accounting_wake: Arc<tokio::sync::Notify>,
    accounting_epoch: Arc<std::sync::atomic::AtomicU64>,
    accounting_progress: tokio::sync::watch::Sender<u64>,
    endpoint: Endpoint,
    credential_path: PathBuf,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    client: reqwest::Client,
    timeout: Duration,
    state: Arc<Mutex<SharedRemoteState>>,
}

impl RemoteWorkerPort {
    /// Starts the financial reconciler independently of heartbeat and control
    /// delivery. It reads only local receipt sources and never invokes a Provider.
    #[must_use]
    pub fn spawn_accounting_reconciler(
        &self,
        provider_directory: PathBuf,
        key: winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey,
    ) -> tokio::task::JoinHandle<()> {
        self.accounting_enabled
            .store(true, std::sync::atomic::Ordering::Release);
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let timeout = self.timeout;
        let wake = Arc::clone(&self.accounting_wake);
        let epoch = Arc::clone(&self.accounting_epoch);
        let progress = self.accounting_progress.clone();
        tokio::spawn(async move {
            let token = key.accounting_query_token();
            let url = format!(
                "https://{}:{}/internal/v1/execution-port/accounting",
                endpoint.host, endpoint.port
            );
            let mut offset = 0;
            loop {
                let requested = epoch.load(std::sync::atomic::Ordering::Acquire);
                let query = async {
                    let response = client
                        .get(&url)
                        .bearer_auth(&token.0)
                        .header("X-Accounting-Offset", offset.to_string())
                        .send()
                        .await
                        .map_err(|_| RemoteWorkerPortError::Transport)?;
                    let bytes = bounded_http_response(response).await?;
                    serde_json::from_slice::<
                        winwincode_execution_port::accounting::PendingAccountingPage,
                    >(&bytes)
                    .map_err(|_| RemoteWorkerPortError::Protocol)
                };
                if let Ok(Ok(page)) = tokio::time::timeout(timeout, query).await {
                    offset = page.next_offset.unwrap_or(0);
                    for lease in page.leases {
                        let directory = provider_directory.clone();
                        let source_key = key.clone();
                        // SQLite scans run off the async runtime's control threads.
                        let statement = tokio::task::spawn_blocking(move || {
                            winwincode_provider::DeviceProviderStore::open(&directory)
                                .and_then(|store| store.accounting_statement(&lease, &source_key))
                        })
                        .await;
                        let Ok(Ok(Some(statement))) = statement else {
                            continue;
                        };
                        let Ok(bytes) = statement.encode() else {
                            continue;
                        };
                        let send = async {
                            let response = client
                                .post(&url)
                                .bearer_auth(&token.0)
                                .header("Content-Type", "application/json")
                                .body(bytes)
                                .send()
                                .await
                                .map_err(|_| RemoteWorkerPortError::Transport)?;
                            bounded_http_response(response).await
                        };
                        if !matches!(tokio::time::timeout(timeout, send).await, Ok(Ok(_))) {
                            remote_transport_debug("Provider accounting receipt remains pending");
                        }
                    }
                } else {
                    remote_transport_debug("Provider accounting query remains pending");
                }
                progress.send_replace(requested);
                tokio::select! { () = tokio::time::sleep(Duration::from_secs(10)) => {}, () = wake.notified() => {} }
            }
        })
    }
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
            return Err(RemoteWorkerPortError::Transport);
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
            .map_err(|_| RemoteWorkerPortError::Transport)?;
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(|_| RemoteWorkerPortError::Transport)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .http1_only()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| RemoteWorkerPortError::Protocol)?;
        let state = Arc::new(Mutex::new(SharedRemoteState {
            inbox: VecDeque::new(),
            processing: Vec::new(),
            acknowledgements: Vec::new(),
            terminal_error: None,
        }));
        Ok((
            Self {
                accounting_enabled: std::sync::atomic::AtomicBool::new(false),
                accounting_wake: Arc::new(tokio::sync::Notify::new()),
                accounting_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                accounting_progress: tokio::sync::watch::channel(0).0,
                endpoint,
                credential_path,
                worker_id,
                worker_instance_id,
                client,
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
        let terminal = matches!(&message, ExecutionPortMessage::JobOutcomeMessage(_));
        let result = self.exchange_inner(message).await;
        if terminal
            && self
                .accounting_enabled
                .load(std::sync::atomic::Ordering::Acquire)
        {
            let requested = self
                .accounting_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                + 1;
            let mut progress = self.accounting_progress.subscribe();
            self.accounting_wake.notify_one();
            // Give the final financial handoff a bounded drain before a short-lived
            // Worker exits. Missing bills remain durable and are retried on restart.
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while *progress.borrow_and_update() < requested {
                    if progress.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
        if let Err(
            error @ (RemoteWorkerPortError::Rejected
            | RemoteWorkerPortError::Protocol
            | RemoteWorkerPortError::TooLarge),
        ) = result
            && let Ok(mut state) = self.state.lock()
        {
            state.terminal_error = Some(error);
        }
        result
    }

    async fn exchange_inner(
        &mut self,
        message: ExecutionPortMessage,
    ) -> Result<(), RemoteWorkerPortError> {
        let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
            .and_then(|frame| RemoteTransportAdapter::<NoopCore>::encode(&frame))
            .map_err(|error| {
                remote_transport_debug("outbound generated frame is invalid");
                remote_frame_error(error)
            })?;
        let acknowledgements = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?
            .acknowledgements_for_exchange();
        let request = RemoteExchangeRequest::new(
            self.worker_id.clone(),
            self.worker_instance_id.clone(),
            acknowledgements.clone(),
            frame,
        )
        .and_then(|request| request.with_acceptance_receipt().encode())
        .map_err(remote_frame_error)?;
        let credential = read_private_credential(&self.credential_path).inspect_err(|_| {
            remote_transport_debug("credential file failed private-file validation");
        })?;
        let response = Box::pin(tokio::time::timeout(
            self.timeout,
            send_https_request(&self.endpoint, &self.client, &credential, &request),
        ))
        .await
        .map_err(|_| {
            remote_transport_debug("exchange timed out");
            RemoteWorkerPortError::Transport
        })??;
        let response = RemoteExchangeResponse::decode(&response).map_err(remote_frame_error)?;
        self.state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?
            .receive_response(&response, &acknowledgements)
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

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned error callback for Result::map_err"
)]
fn remote_frame_error(
    error: winwincode_execution_port::transport::FrameError,
) -> RemoteWorkerPortError {
    match error {
        winwincode_execution_port::transport::FrameError::TooLarge => {
            RemoteWorkerPortError::TooLarge
        }
        _ => RemoteWorkerPortError::Protocol,
    }
}

async fn send_https_request(
    endpoint: &Endpoint,
    client: &reqwest::Client,
    credential: &[u8],
    body: &[u8],
) -> Result<Vec<u8>, RemoteWorkerPortError> {
    let token = std::str::from_utf8(credential).map_err(|_| RemoteWorkerPortError::Rejected)?;
    if token.bytes().any(|byte| !(0x21..=0x7e).contains(&byte)) {
        return Err(RemoteWorkerPortError::Rejected);
    }
    let url = format!(
        "https://{}:{}/internal/v1/execution-port/exchange",
        endpoint.host, endpoint.port
    );
    let response = client
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .map_err(|_| RemoteWorkerPortError::Transport)?;
    bounded_http_response(response).await
}

async fn bounded_http_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, RemoteWorkerPortError> {
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(RemoteWorkerPortError::Rejected),
        413 => return Err(RemoteWorkerPortError::TooLarge),
        408 | 429 | 500..=599 => return Err(RemoteWorkerPortError::Transport),
        _ => return Err(RemoteWorkerPortError::Protocol),
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64)
    {
        return Err(RemoteWorkerPortError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| RemoteWorkerPortError::Transport)?
    {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_HTTP_RESPONSE_BYTES)
        {
            return Err(RemoteWorkerPortError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn remote_transport_debug(message: &str) {
    if std::env::var_os("WWC_DEBUG_REMOTE_WORKER").is_some() {
        eprintln!("remote Worker transport: {message}");
    }
}

/// The only accepted Worker origin form, `https://HOST:PORT`; shared by the
/// `--remote` entry and the managed session config validator.
pub(crate) fn parse_origin(origin: &str) -> Result<Endpoint, RemoteWorkerPortError> {
    let authority = origin
        .strip_prefix("https://")
        .filter(|value| !value.contains('/') && !value.contains('@'))
        .ok_or(RemoteWorkerPortError::Transport)?;
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or(RemoteWorkerPortError::Transport)?;
    if host.is_empty()
        || host
            .chars()
            .any(|character| character.is_ascii_control() || character.is_whitespace())
    {
        return Err(RemoteWorkerPortError::Transport);
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| RemoteWorkerPortError::Transport)?;
    if port == 0 {
        return Err(RemoteWorkerPortError::Transport);
    }
    Ok(Endpoint {
        host: host.to_owned(),
        port,
    })
}

#[cfg(unix)]
fn read_private_credential(path: &Path) -> Result<Vec<u8>, RemoteWorkerPortError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|_| RemoteWorkerPortError::Transport)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(RemoteWorkerPortError::Transport);
    }
    let bytes = fs::read(path).map_err(|_| RemoteWorkerPortError::Transport)?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err(RemoteWorkerPortError::Transport);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{RemoteWorkerPortError, bounded_http_response, parse_origin};

    #[test]
    fn backpressure_delivers_controls_and_acknowledges_the_sent_prefix_without_accepting_upstream()
    {
        use super::*;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let message: ExecutionPortMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "worker.heartbeat_ack")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let id = winwincode_execution_port::transport::execution_message_id(&message).unwrap();
        let delivery = winwincode_execution_port::transport::RemoteExchangeDelivery {
            delivery_id: id.clone(),
            frame: RemoteTransportAdapter::<NoopCore>::encode(
                &TypedFrame::new(FrameDirection::ControlPlaneToWorker, message).unwrap(),
            )
            .unwrap(),
        };
        let sent = ExecutionMessageId("xmsg_00000000000000000000000011".into());
        let suffix = ExecutionMessageId("xmsg_00000000000000000000000012".into());
        let mut state = SharedRemoteState {
            inbox: VecDeque::new(),
            processing: Vec::new(),
            acknowledgements: vec![sent.clone(), suffix.clone()],
            terminal_error: None,
        };
        let response = RemoteExchangeResponse::with_acceptance(vec![delivery], false).unwrap();
        assert_eq!(
            state.receive_response(&response, std::slice::from_ref(&sent)),
            Err(RemoteWorkerPortError::Backpressure)
        );
        assert_eq!(state.acknowledgements, vec![suffix]);
        assert_eq!(state.inbox.len(), 1);
        assert!(state.terminal_error.is_none());
        assert_eq!(
            state.receive_response(&response, std::slice::from_ref(&sent)),
            Err(RemoteWorkerPortError::Backpressure)
        );
        assert_eq!(
            state.inbox.len(),
            1,
            "lost response replay deduplicates controls"
        );
        assert_eq!(
            state.receive_response(
                &RemoteExchangeResponse::with_acceptance(Vec::new(), true).unwrap(),
                &[]
            ),
            Ok(())
        );
        assert_eq!(
            state.receive_response(&RemoteExchangeResponse::new(Vec::new()).unwrap(), &[]),
            Err(RemoteWorkerPortError::Protocol)
        );
    }

    #[test]
    fn queued_confirmations_fit_one_exchange_and_keep_the_unsent_suffix() {
        use super::SharedRemoteState;
        use winwincode_domain::ExecutionMessageId;
        let mut state = SharedRemoteState {
            inbox: std::collections::VecDeque::new(),
            processing: Vec::new(),
            acknowledgements: (0..129)
                .map(|id| ExecutionMessageId(format!("xmsg_{id:026}")))
                .collect(),
            terminal_error: None,
        };
        let first = state.acknowledgements_for_exchange();
        assert_eq!(first.len(), 128);
        assert_eq!(
            state.acknowledgements.len(),
            129,
            "a failed exchange keeps every confirmation"
        );
        state.acknowledgements.retain(|id| !first.contains(id));
        assert_eq!(
            state.acknowledgements_for_exchange(),
            vec![ExecutionMessageId(format!("xmsg_{:026}", 128))]
        );
    }

    #[tokio::test]
    async fn close_delimited_body_is_bounded_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let _ = stream
                .write_all(&vec![b'x'; super::MAX_HTTP_RESPONSE_BYTES + 1])
                .await;
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            bounded_http_response(response).await,
            Err(RemoteWorkerPortError::TooLarge)
        );
        task.await.unwrap();
    }

    #[test]
    fn origin_parser_accepts_only_a_host_and_nonzero_port() {
        let endpoint = parse_origin("https://127.0.0.1:8443").expect("valid HTTPS origin");
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 8443);
    }

    #[test]
    fn origin_parser_rejects_ambiguous_or_header_shaped_authorities() {
        for origin in [
            "http://127.0.0.1:8443",
            "https://127.0.0.1",
            "https://127.0.0.1:0",
            "https://127.0.0.1:8443/path",
            "https://user@127.0.0.1:8443",
            "https://127.0.0.1 bad:8443",
            "https://127.0.0.1\nX-Injected: yes:8443",
        ] {
            assert!(
                parse_origin(origin).is_err(),
                "origin must be rejected: {origin:?}"
            );
        }
    }

    #[tokio::test]
    async fn http_status_and_body_framing_are_independent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (response, expected) in [
            (
                "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n",
                Err(RemoteWorkerPortError::Rejected),
            ),
            (
                "HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
                Err(RemoteWorkerPortError::Rejected),
            ),
            (
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
                Err(RemoteWorkerPortError::Protocol),
            ),
            (
                "HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n",
                Err(RemoteWorkerPortError::TooLarge),
            ),
            (
                "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n",
                Err(RemoteWorkerPortError::Transport),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                Ok(b"{}".to_vec()),
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;extension=yes\r\n{}\r\n0\r\nX-Test: done\r\n\r\n",
                Ok(b"{}".to_vec()),
            ),
            (
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}",
                Ok(b"{}".to_vec()),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}",
                Err(RemoteWorkerPortError::Transport),
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{}",
                Err(RemoteWorkerPortError::Transport),
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(bounded_http_response(response).await, expected);
            task.await.unwrap();
        }
    }
}
