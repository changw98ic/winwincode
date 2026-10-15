// SPDX-License-Identifier: Apache-2.0

use sha2::Digest;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use winwincode_client_port::worker_relay::{WorkerRelayRequest, WorkerRelayResponse};
use winwincode_codex::WorkerExecutionPort;
use winwincode_domain::{ExecutionMessageId, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    FrameDirection, RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
    TypedFrame,
};
use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerPortError {
    Transport,
    Network(NetworkFailure),
    NetworkExhausted(NetworkFailure),
    Backpressure,
    Rejected,
    Protocol,
    TooLarge,
    ResponseTooLarge,
}

impl fmt::Display for RemoteWorkerPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::Network(failure) | Self::NetworkExhausted(failure) = self {
            return write!(formatter, "{failure}");
        }
        formatter.write_str(match self {
            Self::Transport => "WORKER_DEVICE_RELAY_UNAVAILABLE",
            Self::Network(_) | Self::NetworkExhausted(_) => unreachable!(),
            Self::Backpressure => "WORKER_DEVICE_RELAY_BACKPRESSURE",
            Self::Rejected => "WORKER_DEVICE_RELAY_REJECTED",
            Self::Protocol => "WORKER_DEVICE_RELAY_PROTOCOL_INVALID",
            Self::TooLarge => "WORKER_MESSAGE_TOO_LARGE",
            Self::ResponseTooLarge => "WORKER_RESPONSE_TOO_LARGE",
        })
    }
}

impl std::error::Error for RemoteWorkerPortError {}

struct RelayIo {
    input: Box<dyn BufRead + Send>,
    output: Box<dyn Write + Send>,
}

#[derive(Clone)]
struct DeviceRelay {
    io: Arc<Mutex<RelayIo>>,
}

impl DeviceRelay {
    async fn request(&self, request: WorkerRelayRequest) -> Result<String, RemoteWorkerPortError> {
        let io = Arc::clone(&self.io);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("worker-device-relay".into())
            .spawn(move || {
                let result = (|| {
                    let mut io = io.lock().map_err(|_| RemoteWorkerPortError::Transport)?;
                    serde_json::to_writer(&mut io.output, &request)
                        .map_err(|_| RemoteWorkerPortError::Transport)?;
                    io.output
                        .write_all(b"\n")
                        .map_err(|_| RemoteWorkerPortError::Transport)?;
                    io.output
                        .flush()
                        .map_err(|_| RemoteWorkerPortError::Transport)?;
                    let mut line = String::new();
                    if io
                        .input
                        .read_line(&mut line)
                        .map_err(|_| RemoteWorkerPortError::Transport)?
                        == 0
                    {
                        return Err(RemoteWorkerPortError::Transport);
                    }
                    let response: WorkerRelayResponse =
                        serde_json::from_str(&line).map_err(|_| RemoteWorkerPortError::Protocol)?;
                    if response.status != 200 {
                        let failure = NetworkFailure::http(response.status, None);
                        return Err(if failure.retryable() {
                            RemoteWorkerPortError::Network(failure)
                        } else {
                            RemoteWorkerPortError::NetworkExhausted(failure)
                        });
                    }
                    Ok(response.body)
                })();
                let _ = sender.send(result);
            })
            .map_err(|_| RemoteWorkerPortError::Transport)?;
        receiver
            .await
            .map_err(|_| RemoteWorkerPortError::Transport)?
    }
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
                .map_err(|_| RemoteWorkerPortError::Protocol)?;
            if let ExecutionPortMessage::LeaseRenewMessage(renewal) = frame.message() {
                eprintln!(
                    "component=worker stage=lease_renewal_received message_sha256={:x} received_at_unix_ms={} prior_expires_at={} new_expires_at={} sent_at={}",
                    sha2::Sha256::digest(renewal.message_id.0.as_bytes()),
                    time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000,
                    renewal.prior_expires_at.0,
                    renewal.lease.expires_at.0,
                    renewal.sent_at.0
                );
            }
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

#[derive(Clone)]
pub struct RemoteWorkerTransportHandle {
    state: Arc<Mutex<SharedRemoteState>>,
}

impl RemoteWorkerTransportHandle {
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.terminal_error = Some(RemoteWorkerPortError::Transport);
        }
    }

    #[must_use]
    pub fn has_pending_controls(&self) -> bool {
        self.state.lock().is_ok_and(|state| !state.inbox.is_empty())
    }
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn empty_for_test() -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedRemoteState {
                inbox: VecDeque::new(),
                processing: Vec::new(),
                acknowledgements: Vec::new(),
                terminal_error: None,
            })),
        }
    }

    #[must_use]
    pub fn authority_rejected(&self) -> bool {
        matches!(self.terminal_error(), Some(RemoteWorkerPortError::Rejected))
            || matches!(self.terminal_error(), Some(RemoteWorkerPortError::NetworkExhausted(failure))
                if matches!(failure.kind, ErrorKind::Authentication | ErrorKind::Authorization))
    }

    #[must_use]
    pub fn terminal_error(&self) -> Option<RemoteWorkerPortError> {
        self.state
            .lock()
            .map_or(Some(RemoteWorkerPortError::Transport), |state| {
                state.terminal_error
            })
    }

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

    pub fn retry(&self, id: &ExecutionMessageId) -> Result<(), RemoteWorkerPortError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?;
        state.processing.retain(|processing| processing != id);
        Ok(())
    }
}

pub struct RemoteWorkerPort {
    accounting_enabled: std::sync::atomic::AtomicBool,
    accounting_wake: Arc<tokio::sync::Notify>,
    accounting_epoch: Arc<std::sync::atomic::AtomicU64>,
    accounting_progress: tokio::sync::watch::Sender<u64>,
    relay: DeviceRelay,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    state: Arc<Mutex<SharedRemoteState>>,
}

impl RemoteWorkerPort {
    #[must_use]
    pub fn open(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
    ) -> (Self, RemoteWorkerTransportHandle) {
        Self::with_io(
            worker_id,
            worker_instance_id,
            BufReader::new(std::io::stdin()),
            std::io::stdout(),
        )
    }

    pub fn with_io(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        input: impl BufRead + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> (Self, RemoteWorkerTransportHandle) {
        let state = Arc::new(Mutex::new(SharedRemoteState {
            inbox: VecDeque::new(),
            processing: Vec::new(),
            acknowledgements: Vec::new(),
            terminal_error: None,
        }));
        (
            Self {
                accounting_enabled: std::sync::atomic::AtomicBool::new(false),
                accounting_wake: Arc::new(tokio::sync::Notify::new()),
                accounting_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                accounting_progress: tokio::sync::watch::channel(0).0,
                relay: DeviceRelay {
                    io: Arc::new(Mutex::new(RelayIo {
                        input: Box::new(input),
                        output: Box::new(output),
                    })),
                },
                worker_id,
                worker_instance_id,
                state: Arc::clone(&state),
            },
            RemoteWorkerTransportHandle { state },
        )
    }

    #[must_use]
    pub fn spawn_accounting_reconciler(
        &self,
        provider_directory: PathBuf,
    ) -> tokio::task::JoinHandle<()> {
        self.accounting_enabled
            .store(true, std::sync::atomic::Ordering::Release);
        let relay = self.relay.clone();
        let wake = Arc::clone(&self.accounting_wake);
        let epoch = Arc::clone(&self.accounting_epoch);
        let progress = self.accounting_progress.clone();
        tokio::spawn(async move {
            let mut offset = 0;
            loop {
                let requested = epoch.load(std::sync::atomic::Ordering::Acquire);
                let query = relay
                    .request(WorkerRelayRequest::Accounting { body: None, offset })
                    .await;
                if let Ok(body) = query {
                    if let Ok(page) = serde_json::from_str::<
                        winwincode_execution_port::accounting::PendingAccountingPage,
                    >(&body)
                    {
                        offset = page.next_offset.unwrap_or(0);
                        for lease in page.leases {
                            let directory = provider_directory.clone();
                            let statement = tokio::task::spawn_blocking(move || {
                                winwincode_provider::DeviceProviderStore::open(&directory)
                                    .and_then(|store| store.accounting_statement(&lease))
                            })
                            .await;
                            let Ok(Ok(Some(statement))) = statement else {
                                continue;
                            };
                            let Ok(bytes) = statement.encode() else {
                                continue;
                            };
                            let Ok(body) = String::from_utf8(bytes) else {
                                continue;
                            };
                            if relay
                                .request(WorkerRelayRequest::Accounting {
                                    body: Some(body),
                                    offset: 0,
                                })
                                .await
                                .is_err()
                            {
                                remote_transport_debug(
                                    "Provider accounting receipt remains pending",
                                );
                            }
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
            error @ (RemoteWorkerPortError::Transport
            | RemoteWorkerPortError::Rejected
            | RemoteWorkerPortError::Protocol
            | RemoteWorkerPortError::ResponseTooLarge
            | RemoteWorkerPortError::NetworkExhausted(_)),
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
        if let Some(error) = self
            .state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?
            .terminal_error
        {
            return Err(error);
        }
        let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
            .and_then(|frame| RemoteTransportAdapter::<NoopCore>::encode(&frame))
            .map_err(remote_frame_error)?;
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
        let body = String::from_utf8(request).map_err(|_| RemoteWorkerPortError::Protocol)?;
        let body = self
            .relay
            .request(WorkerRelayRequest::Exchange { body })
            .await?;
        let response = decode_exchange_response(body.as_bytes())?;
        self.state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?
            .receive_response(&response, &acknowledgements)
    }
}

fn decode_exchange_response(bytes: &[u8]) -> Result<RemoteExchangeResponse, RemoteWorkerPortError> {
    RemoteExchangeResponse::decode(bytes).map_err(|_| {
        RemoteWorkerPortError::NetworkExhausted(
            NetworkFailure::new(
                ErrorKind::ProtocolInvalid,
                Acceptance::ResponseReceived,
                Phase::Decode,
            )
            .with_diagnostic(winwincode_network::NetworkDiagnostic::new(
                winwincode_network::DiagnosticCode::JsonSchema,
            )),
        )
    })
}

fn remote_transport_debug(message: &str) {
    if std::env::var_os("WWC_DEBUG_REMOTE_WORKER").is_some() {
        eprintln!("Worker device relay: {message}");
    }
}

impl WorkerExecutionPort for RemoteWorkerPort {
    type Error = RemoteWorkerPortError;

    fn has_pending_controls(&self) -> bool {
        self.state.lock().is_ok_and(|state| !state.inbox.is_empty())
    }

    fn failure_kind(error: &Self::Error) -> winwincode_codex::ExecutionPortFailureKind {
        use winwincode_codex::ExecutionPortFailureKind;
        match error {
            RemoteWorkerPortError::Backpressure => ExecutionPortFailureKind::Backpressure,
            RemoteWorkerPortError::Transport | RemoteWorkerPortError::Network(_) => {
                ExecutionPortFailureKind::Unavailable
            }
            RemoteWorkerPortError::TooLarge => ExecutionPortFailureKind::MessageRejected,
            RemoteWorkerPortError::NetworkExhausted(_)
            | RemoteWorkerPortError::Rejected
            | RemoteWorkerPortError::Protocol
            | RemoteWorkerPortError::ResponseTooLarge => ExecutionPortFailureKind::Terminal,
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exchange_and_accounting_share_one_json_lines_channel() {
        use std::os::unix::net::UnixStream;
        let (worker, device) = UnixStream::pair().unwrap();
        let relay = DeviceRelay {
            io: Arc::new(Mutex::new(RelayIo {
                input: Box::new(BufReader::new(worker.try_clone().unwrap())),
                output: Box::new(worker),
            })),
        };
        let server = std::thread::spawn(move || {
            let mut input = BufReader::new(device.try_clone().unwrap());
            let mut output = device;
            let mut requests = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let request: WorkerRelayRequest = serde_json::from_str(&line).unwrap();
                let body = match &request {
                    WorkerRelayRequest::Exchange { body } => body.clone(),
                    WorkerRelayRequest::Accounting { offset, .. } => offset.to_string(),
                };
                requests.push(request);
                serde_json::to_writer(&mut output, &WorkerRelayResponse { status: 200, body })
                    .unwrap();
                output.write_all(b"\n").unwrap();
                output.flush().unwrap();
            }
            requests
        });
        let (exchange, accounting) = tokio::join!(
            relay.request(WorkerRelayRequest::Exchange {
                body: "exchange".into()
            }),
            relay.request(WorkerRelayRequest::Accounting {
                body: None,
                offset: 3
            }),
        );
        assert_eq!(exchange.unwrap(), "exchange");
        assert_eq!(accounting.unwrap(), "3");
        assert_eq!(server.join().unwrap().len(), 2);
    }

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
            inbox: VecDeque::new(),
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
}
