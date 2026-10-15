// SPDX-License-Identifier: Apache-2.0

use sha2::Digest;
use std::collections::{BTreeMap, HashMap, VecDeque};
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
    retries: HashMap<RelayRetryQueue, RelayRetry>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum RelayRetryQueue {
    Exchange(Option<std::mem::Discriminant<ExecutionPortMessage>>),
    AccountingRead,
    AccountingWrite,
}

struct RelayRetry {
    retry: winwincode_network::RequestRetryState,
    retry_at: Option<std::time::Instant>,
    retry_error: Option<RemoteWorkerPortError>,
}

impl RelayIo {
    fn new(input: impl BufRead + Send + 'static, output: impl Write + Send + 'static) -> Self {
        Self {
            input: Box::new(input),
            output: Box::new(output),
            retries: HashMap::new(),
        }
    }
}

fn relay_retry_queue(request: &WorkerRelayRequest) -> RelayRetryQueue {
    match request {
        WorkerRelayRequest::Exchange { body } => RelayRetryQueue::Exchange(
            RemoteExchangeRequest::decode(body.as_bytes())
                .ok()
                .and_then(|request| {
                    RemoteTransportAdapter::<NoopCore>::decode(request.frame()).ok()
                })
                .map(|frame| std::mem::discriminant(frame.message())),
        ),
        WorkerRelayRequest::Accounting { body: None, .. } => RelayRetryQueue::AccountingRead,
        WorkerRelayRequest::Accounting { body: Some(_), .. } => RelayRetryQueue::AccountingWrite,
    }
}

fn relay_retry() -> winwincode_network::RequestRetryState {
    winwincode_network::RequestRetry::new(
        winwincode_network::defaults().max_attempts,
        b"worker-device-relay",
    )
    .for_replay(winwincode_network::Replay::ReplayExact)
    .state()
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
                    let queue = relay_retry_queue(&request);
                    if let Some(retry) = io.retries.get(&queue)
                        && retry
                            .retry_at
                            .is_some_and(|at| std::time::Instant::now() < at)
                    {
                        return Err(retry
                            .retry_error
                            .unwrap_or(RemoteWorkerPortError::Transport));
                    }
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
                    if let Some(failure) = response.failure.or_else(|| {
                        (response.status != 200)
                            .then(|| NetworkFailure::http(response.status, None))
                    }) {
                        if failure.kind == ErrorKind::ProtocolInvalid
                            && failure.phase == Phase::Decode
                        {
                            return Err(RemoteWorkerPortError::NetworkExhausted(failure));
                        }
                        let retry = io.retries.entry(queue).or_insert_with(|| RelayRetry {
                            retry: relay_retry(),
                            retry_at: None,
                            retry_error: None,
                        });
                        let error = match retry.retry.decision_after(&failure) {
                            winwincode_network::RetryDecision::RetryAfter(delay)
                            | winwincode_network::RetryDecision::DeferredUntil(delay) => {
                                retry.retry_at = Some(std::time::Instant::now() + delay);
                                RemoteWorkerPortError::Network(failure)
                            }
                            _ => RemoteWorkerPortError::NetworkExhausted(failure),
                        };
                        retry.retry_error = Some(error);
                        return Err(error);
                    }
                    io.retries.remove(&queue);
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
    lease_deadlines: BTreeMap<String, (String, std::time::Instant)>,
    pending_renewals: BTreeMap<String, (String, String, std::time::Instant)>,
}

impl SharedRemoteState {
    fn check_authority(&mut self) -> Result<(), RemoteWorkerPortError> {
        if self.terminal_error.is_none()
            && self
                .lease_deadlines
                .values()
                .any(|(_, deadline)| std::time::Instant::now() >= *deadline)
        {
            self.terminal_error = Some(RemoteWorkerPortError::NetworkExhausted(
                NetworkFailure::new(
                    ErrorKind::AuthorityExpired,
                    Acceptance::NotSent,
                    Phase::Connect,
                ),
            ));
        }
        self.terminal_error.map_or(Ok(()), Err)
    }

    fn receive_response(
        &mut self,
        response: &RemoteExchangeResponse,
        acknowledgements: &[ExecutionMessageId],
        request_started: std::time::Instant,
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
            let authority = match frame.message() {
                ExecutionPortMessage::JobDispatchMessage(dispatch) => Some(&dispatch.lease),
                ExecutionPortMessage::LeaseRenewMessage(renewal) => Some(&renewal.lease),
                _ => None,
            };
            if let Some(lease) = authority {
                let expires = time::OffsetDateTime::parse(
                    &lease.expires_at.0,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| RemoteWorkerPortError::Protocol)?;
                let server_time = time::OffsetDateTime::parse(
                    &response.server_time().0,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| RemoteWorkerPortError::Protocol)?;
                let lifetime = Duration::try_from(expires - server_time).unwrap_or(Duration::ZERO);
                let deadline = request_started
                    .checked_add(lifetime)
                    .ok_or(RemoteWorkerPortError::Protocol)?;
                if deadline <= std::time::Instant::now() {
                    let error = RemoteWorkerPortError::NetworkExhausted(NetworkFailure::new(
                        ErrorKind::AuthorityExpired,
                        Acceptance::ResponseReceived,
                        Phase::Decode,
                    ));
                    state.terminal_error = Some(error);
                    return Err(error);
                }
                if matches!(frame.message(), ExecutionPortMessage::LeaseRenewMessage(_)) {
                    state
                        .pending_renewals
                        .entry(delivery.delivery_id.0.clone())
                        .or_insert_with(|| {
                            (lease.job_id.0.clone(), lease.expires_at.0.clone(), deadline)
                        });
                } else {
                    state
                        .lease_deadlines
                        .entry(lease.job_id.0.clone())
                        .or_insert_with(|| (lease.expires_at.0.clone(), deadline));
                }
            }
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
    pub async fn wait_for_terminal_error(&self) -> RemoteWorkerPortError {
        loop {
            if let Ok(mut state) = self.state.lock() {
                if let Err(error) = state.check_authority() {
                    return error;
                }
            } else {
                return RemoteWorkerPortError::Transport;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
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
                lease_deadlines: BTreeMap::new(),
                pending_renewals: BTreeMap::new(),
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
        state.check_authority()?;
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
        state.check_authority()?;
        state.processing.retain(|processing| processing != &id);
        if let Some((job_id, expires_at, deadline)) = state.pending_renewals.remove(&id.0)
            && state
                .lease_deadlines
                .get(&job_id)
                .is_some_and(|(prior, _)| expires_at > *prior)
        {
            state.lease_deadlines.insert(job_id, (expires_at, deadline));
        }
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
            lease_deadlines: BTreeMap::new(),
            pending_renewals: BTreeMap::new(),
        }));
        (
            Self {
                accounting_enabled: std::sync::atomic::AtomicBool::new(false),
                accounting_wake: Arc::new(tokio::sync::Notify::new()),
                accounting_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                accounting_progress: tokio::sync::watch::channel(0).0,
                relay: DeviceRelay {
                    io: Arc::new(Mutex::new(RelayIo::new(input, output))),
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
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let mut offset = 0;
            loop {
                let requested = epoch.load(std::sync::atomic::Ordering::Acquire);
                let query = relay
                    .request(WorkerRelayRequest::Accounting { body: None, offset })
                    .await;
                if let Err(error @ RemoteWorkerPortError::NetworkExhausted(_)) = &query {
                    if let Ok(mut state) = state.lock() {
                        state.terminal_error = Some(*error);
                    }
                    break;
                }
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
                            if let Err(error) = relay
                                .request(WorkerRelayRequest::Accounting {
                                    body: Some(body),
                                    offset: 0,
                                })
                                .await
                            {
                                if matches!(error, RemoteWorkerPortError::NetworkExhausted(_)) {
                                    if let Ok(mut state) = state.lock() {
                                        state.terminal_error = Some(error);
                                    }
                                    return;
                                }
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
        let completed_job = match &message {
            ExecutionPortMessage::JobOutcomeMessage(outcome) => {
                Some(outcome.lease.job_id.0.clone())
            }
            _ => None,
        };
        let terminal = completed_job.is_some();
        let result = self.exchange_inner(message).await;
        if result.is_ok()
            && let Some(job_id) = completed_job
            && let Ok(mut state) = self.state.lock()
        {
            state.lease_deadlines.remove(&job_id);
            state
                .pending_renewals
                .retain(|_, (pending_job, _, _)| *pending_job != job_id);
        }
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
        let request_started = std::time::Instant::now();
        let body = self
            .relay
            .request(WorkerRelayRequest::Exchange { body })
            .await?;
        let response = decode_exchange_response(body.as_bytes())?;
        self.state
            .lock()
            .map_err(|_| RemoteWorkerPortError::Transport)?
            .receive_response(&response, &acknowledgements, request_started)
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
    #[allow(clippy::too_many_lines)]
    async fn retrying_one_relay_request_does_not_block_heartbeat_accounting_or_cancel() {
        #[derive(Clone, Default)]
        struct Output(Arc<Mutex<Vec<u8>>>);
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let message = |kind: &str| -> ExecutionPortMessage {
            serde_json::from_value(
                fixture["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == kind)
                    .unwrap()
                    .clone(),
            )
            .unwrap()
        };
        let cancellation = message("job.cancel");
        let delivery_id =
            winwincode_execution_port::transport::execution_message_id(&cancellation).unwrap();
        let control = winwincode_execution_port::transport::RemoteExchangeDelivery {
            delivery_id: delivery_id.clone(),
            frame: RemoteTransportAdapter::<NoopCore>::encode(
                &TypedFrame::new(FrameDirection::ControlPlaneToWorker, cancellation.clone())
                    .unwrap(),
            )
            .unwrap(),
        };
        let success = String::from_utf8(
            RemoteExchangeResponse::with_acceptance(Vec::new(), true)
                .unwrap()
                .encode()
                .unwrap(),
        )
        .unwrap();
        let failure = NetworkFailure::http(503, Some(Duration::from_mins(1)));

        for failed_kind in [
            "artifact.chunk",
            "runtime.event",
            "job.outcome",
            "action.enforcement_request",
        ] {
            let responses = [
                WorkerRelayResponse {
                    status: 503,
                    body: String::new(),
                    failure: Some(failure),
                },
                WorkerRelayResponse {
                    status: 200,
                    body: String::from_utf8(
                        RemoteExchangeResponse::with_acceptance(vec![control.clone()], true)
                            .unwrap()
                            .encode()
                            .unwrap(),
                    )
                    .unwrap(),
                    failure: None,
                },
                WorkerRelayResponse {
                    status: 200,
                    body: "accounting".into(),
                    failure: None,
                },
                WorkerRelayResponse {
                    status: 200,
                    body: success.clone(),
                    failure: None,
                },
            ];
            let mut input = Vec::new();
            for response in responses {
                serde_json::to_writer(&mut input, &response).unwrap();
                input.push(b'\n');
            }
            let output = Output::default();
            let (mut port, handle) = RemoteWorkerPort::with_io(
                WorkerId("worker".into()),
                WorkerInstanceId("instance".into()),
                std::io::Cursor::new(input),
                output.clone(),
            );
            let error = RemoteWorkerPortError::Network(failure);
            assert_eq!(port.send(message(failed_kind)).await, Err(error));
            assert_eq!(
                port.send(message("worker.heartbeat")).await,
                Ok(()),
                "{failed_kind} retry must not prevent a heartbeat from fetching cancellation"
            );
            assert_eq!(
                handle.next_control().unwrap(),
                Some((delivery_id.clone(), cancellation.clone()))
            );
            handle.confirm(delivery_id.clone()).unwrap();
            assert_eq!(
                port.send(message(failed_kind)).await,
                Err(error),
                "new delivery acknowledgements must preserve the original request's retry delay"
            );
            assert_eq!(
                port.relay
                    .request(WorkerRelayRequest::Accounting {
                        body: None,
                        offset: 0
                    })
                    .await,
                Ok("accounting".into())
            );
            assert_eq!(port.send(message("job.cancel_ack")).await, Ok(()));
            assert_eq!(
                port.send(message(failed_kind)).await,
                Err(error),
                "other successful requests must preserve the original retry delay"
            );
            let requests = output.0.lock().unwrap();
            let requests = requests
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice::<WorkerRelayRequest>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                requests.len(),
                4,
                "only the original failed attempt is written"
            );
            let WorkerRelayRequest::Exchange { body } = &requests[3] else {
                panic!("cancel acknowledgement is an exchange");
            };
            assert_eq!(
                RemoteExchangeRequest::decode(body.as_bytes())
                    .unwrap()
                    .acknowledgements(),
                std::slice::from_ref(&delivery_id)
            );
        }
    }

    #[tokio::test]
    async fn exchange_and_accounting_share_one_json_lines_channel() {
        use std::os::unix::net::UnixStream;
        let (worker, device) = UnixStream::pair().unwrap();
        let relay = DeviceRelay {
            io: Arc::new(Mutex::new(RelayIo::new(
                BufReader::new(worker.try_clone().unwrap()),
                worker,
            ))),
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
                serde_json::to_writer(
                    &mut output,
                    &WorkerRelayResponse {
                        status: 200,
                        body,
                        failure: None,
                    },
                )
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

    #[tokio::test]
    async fn execution_authority_uses_fresh_server_time_and_replay_does_not_extend_it() {
        use winwincode_execution_port::transport::RemoteExchangeDelivery;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut dispatch = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "job.dispatch")
            .unwrap()
            .clone();
        dispatch["sentAt"] = "2098-01-01T00:00:00.000Z".into();
        dispatch["lease"]["expiresAt"] = "2099-01-01T00:00:00.200Z".into();
        let message: ExecutionPortMessage = serde_json::from_value(dispatch).unwrap();
        let ExecutionPortMessage::JobDispatchMessage(dispatch) = &message else {
            unreachable!()
        };
        let delivery_id = dispatch.message_id.clone();
        let job_id = dispatch.lease.job_id.0.clone();
        let response = RemoteExchangeResponse::with_acceptance_at(
            vec![RemoteExchangeDelivery {
                delivery_id,
                frame: RemoteTransportAdapter::<NoopCore>::encode(
                    &TypedFrame::new(FrameDirection::ControlPlaneToWorker, message).unwrap(),
                )
                .unwrap(),
            }],
            true,
            winwincode_domain::Instant("2099-01-01T00:00:00.000Z".into()),
        )
        .unwrap();
        let (_, handle) = RemoteWorkerPort::with_io(
            WorkerId("worker".into()),
            WorkerInstanceId("instance".into()),
            std::io::Cursor::new(Vec::new()),
            Vec::new(),
        );
        let deadline = {
            let mut state = handle.state.lock().unwrap();
            state
                .receive_response(&response, &[], std::time::Instant::now())
                .unwrap();
            state.lease_deadlines[&job_id].1
        };
        assert!(deadline <= std::time::Instant::now() + Duration::from_millis(200));
        tokio::time::sleep(Duration::from_millis(60)).await;
        {
            let mut state = handle.state.lock().unwrap();
            state
                .receive_response(&response, &[], std::time::Instant::now())
                .unwrap();
            assert_eq!(state.lease_deadlines[&job_id].1, deadline);
        }
        let error =
            tokio::time::timeout(Duration::from_millis(300), handle.wait_for_terminal_error())
                .await
                .unwrap();
        assert!(
            matches!(error, RemoteWorkerPortError::NetworkExhausted(failure) if failure.kind == ErrorKind::AuthorityExpired)
        );
        assert_eq!(handle.next_control(), Err(error));
        assert_eq!(
            handle.confirm(response.deliveries()[0].delivery_id.clone()),
            Err(error)
        );

        let (_, late_handle) = RemoteWorkerPort::with_io(
            WorkerId("worker".into()),
            WorkerInstanceId("instance".into()),
            std::io::Cursor::new(Vec::new()),
            Vec::new(),
        );
        let mut state = late_handle.state.lock().unwrap();
        let request_started = std::time::Instant::now()
            .checked_sub(Duration::from_millis(300))
            .unwrap();
        assert!(matches!(
            state.receive_response(&response, &[], request_started),
            Err(RemoteWorkerPortError::NetworkExhausted(failure))
                if failure.kind == ErrorKind::AuthorityExpired
        ));
        assert!(state.inbox.is_empty());
        assert!(state.lease_deadlines.is_empty());
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
            lease_deadlines: BTreeMap::new(),
            pending_renewals: BTreeMap::new(),
        };
        let response = RemoteExchangeResponse::with_acceptance(vec![delivery], false).unwrap();
        assert_eq!(
            state.receive_response(
                &response,
                std::slice::from_ref(&sent),
                std::time::Instant::now()
            ),
            Err(RemoteWorkerPortError::Backpressure)
        );
        assert_eq!(state.acknowledgements, vec![suffix]);
        assert_eq!(state.inbox.len(), 1);
        assert!(state.terminal_error.is_none());
        assert_eq!(
            state.receive_response(
                &response,
                std::slice::from_ref(&sent),
                std::time::Instant::now()
            ),
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
                &[],
                std::time::Instant::now()
            ),
            Ok(())
        );
        assert_eq!(
            state.receive_response(
                &RemoteExchangeResponse::new(Vec::new()).unwrap(),
                &[],
                std::time::Instant::now()
            ),
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
            lease_deadlines: BTreeMap::new(),
            pending_renewals: BTreeMap::new(),
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
