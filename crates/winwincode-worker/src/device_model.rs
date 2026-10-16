// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
#[path = "device_model_concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(all(test, feature = "test-support"))]
#[path = "device_model_history_regression_tests.rs"]
mod history_regression_tests;

use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};
use winwincode_execution_port::generated::{ExecutionPortMessage, ModelChunkMessage};
use winwincode_provider::{
    DeviceModelAdmission, DeviceModelStepOutcome, DeviceProviderError, ProviderExecutionSource,
    ProviderRuntimeStore, model_failure,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceModelSendOutcome {
    Handled,
    Unhandled,
    NotStarted,
}

pub(crate) struct DeviceModels {
    directory: PathBuf,
    recovery_store: Option<ProviderRuntimeStore>,
    execution_source: Arc<dyn ProviderExecutionSource>,
    recovery_version: Option<i64>,
    pending: HashMap<String, PendingModelOpen>,
    sender: mpsc::Sender<(String, DeviceModelStepOutcome)>,
    receiver: mpsc::Receiver<(String, DeviceModelStepOutcome)>,
    chunks: VecDeque<ModelChunkMessage>,
    delivered_high_water: HashMap<String, i64>,
    owned_exchanges: HashSet<String>,
    session_ids: HashSet<String>,
    instance_ids: HashSet<String>,
    intake_log: Option<PathBuf>,
    #[cfg(any(test, feature = "test-support"))]
    fail_next_start: bool,
}

struct PendingModelOpen {
    digest: String,
    open: winwincode_execution_port::generated::ModelOpenMessage,
    start_deadline: Option<std::time::Instant>,
    start_guard: Option<winwincode_codex::LocalModelStartGuard>,
    thread: Option<std::thread::JoinHandle<()>>,
    retry_wake_millis: Option<u64>,
}

impl PendingModelOpen {
    fn can_start(&self) -> bool {
        self.start_guard.as_ref().map_or_else(
            || {
                self.start_deadline
                    .is_some_and(|deadline| std::time::Instant::now() < deadline)
            },
            |check| check(),
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct IntakeLogLine<'a> {
    pub stage: &'a str,
    pub code: &'a str,
    pub exchange_id: &'a str,
    pub sequence: i64,
    pub worker_session_id: &'a str,
    pub thread_id: &'a str,
    pub detail: &'a str,
}

impl DeviceModels {
    pub(crate) fn new(
        directory: &Path,
        execution_source: Arc<dyn ProviderExecutionSource>,
    ) -> Self {
        let intake_log = std::env::var_os("WWC_MODEL_INTAKE_LOG").map(PathBuf::from);
        let (sender, receiver) = mpsc::channel();
        Self {
            directory: directory.to_owned(),
            execution_source,
            recovery_store: None,
            recovery_version: None,
            pending: HashMap::new(),
            sender,
            receiver,
            chunks: VecDeque::new(),
            delivered_high_water: HashMap::new(),
            owned_exchanges: HashSet::new(),
            session_ids: HashSet::new(),
            instance_ids: HashSet::new(),
            intake_log,
            #[cfg(any(test, feature = "test-support"))]
            fail_next_start: false,
        }
    }

    #[cfg(test)]
    fn for_test(configuration_directory: &Path) -> Self {
        let store =
            winwincode_provider::DeviceProviderStore::open(configuration_directory).unwrap();
        Self::new(
            store.runtime_directory(),
            Arc::new(winwincode_provider::LocalDeviceProviderSource::new(
                configuration_directory.to_path_buf(),
            )),
        )
    }

    pub(crate) fn set_intake_log(&mut self, path: Option<PathBuf>) {
        if path.is_some() {
            self.intake_log = path;
        }
    }

    pub(crate) fn note_worker_instance_id(&mut self, worker_instance_id: &str) {
        if !worker_instance_id.is_empty() && self.instance_ids.insert(worker_instance_id.to_owned())
        {
            self.recovery_version = None;
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn refuse_next_start(&mut self) {
        self.fail_next_start = true;
    }

    pub(crate) fn owns_exchange(&self, exchange_id: &str) -> bool {
        self.owned_exchanges.contains(exchange_id)
    }

    fn note_open_identity(
        &mut self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
    ) {
        if self.recovery_store.is_none() {
            self.recovery_store = ProviderRuntimeStore::open_existing(&self.directory).ok();
        }
        self.owned_exchanges
            .insert(open.model_exchange_id.0.clone());
        self.recovery_version = None;
        let session = &open.session_identity;
        if !session.worker_session_id.0.is_empty() {
            self.session_ids.insert(session.worker_session_id.0.clone());
        }
        if !open.worker_session_id.0.is_empty() {
            self.session_ids.insert(open.worker_session_id.0.clone());
        }
        if !open.lease.worker_instance_id.0.is_empty() {
            self.instance_ids
                .insert(open.lease.worker_instance_id.0.clone());
        }
    }

    pub(crate) fn send(
        &mut self,
        message: &ExecutionPortMessage,
        start_deadline: Option<std::time::Instant>,
        start_guard: Option<winwincode_codex::LocalModelStartGuard>,
    ) -> Result<DeviceModelSendOutcome, DeviceProviderError> {
        match message {
            ExecutionPortMessage::ModelOpenMessage(open) => {
                let id = open.model_exchange_id.0.clone();
                let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(open)?));
                self.note_open_identity(open);
                if let Some(previous) = self.pending.get(&id) {
                    return if previous.digest == digest {
                        Ok(DeviceModelSendOutcome::Handled)
                    } else {
                        Err(DeviceProviderError::StateInvalid)
                    };
                }
                let store = self
                    .recovery_store
                    .as_ref()
                    .ok_or(DeviceProviderError::StateInvalid);
                let admission = store
                    .as_ref()
                    .map_err(|error| *error)
                    .and_then(|store| store.try_model_permit(open));
                match admission {
                    // This is a non-blocking queue probe. Every physical request
                    // acquires its own slot after inline context preparation.
                    Ok(DeviceModelAdmission::InvokeAttempt(permit)) => drop(permit),
                    Ok(
                        DeviceModelAdmission::ReplayStoredResult
                        | DeviceModelAdmission::RecoverExchange
                        | DeviceModelAdmission::RejectRequest,
                    ) => {}
                    Ok(DeviceModelAdmission::Deferred) => {
                        return Ok(DeviceModelSendOutcome::NotStarted);
                    }
                    Err(_) => {
                        let chunks = store
                            .as_ref()
                            .ok()
                            .and_then(|store| store.reject_model_start(open).ok())
                            .unwrap_or_else(|| {
                                vec![model_failure(open, "DEVICE_PROVIDER_UNAVAILABLE")]
                            });
                        self.chunks.extend(chunks);
                        return Ok(DeviceModelSendOutcome::Handled);
                    }
                }
                // Each physical attempt runs outside the Worker poll loop.
                let Ok(thread) =
                    self.start_provider(open, start_deadline, start_guard.clone(), false)
                else {
                    return Ok(DeviceModelSendOutcome::NotStarted);
                };
                self.pending.insert(
                    id,
                    PendingModelOpen {
                        digest,
                        open: open.clone(),
                        start_deadline,
                        start_guard,
                        thread: Some(thread),
                        retry_wake_millis: None,
                    },
                );
                Ok(DeviceModelSendOutcome::Handled)
            }
            ExecutionPortMessage::ModelAckMessage(ack) => {
                let store = ProviderRuntimeStore::open_existing(&self.directory)?;
                if ack.error.is_some() {
                    store.cancel_model(&ack.model_exchange_id.0)?;
                    self.chunks
                        .retain(|chunk| chunk.model_exchange_id != ack.model_exchange_id);
                } else if let Some(from) = &ack.replay_from_sequence {
                    self.chunks
                        .extend(store.replay_model(&ack.model_exchange_id.0, from.0)?);
                }
                Ok(DeviceModelSendOutcome::Handled)
            }
            _ => Ok(DeviceModelSendOutcome::Unhandled),
        }
    }

    fn start_provider(
        &mut self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
        start_deadline: Option<std::time::Instant>,
        start_guard: Option<winwincode_codex::LocalModelStartGuard>,
        retry_entry: bool,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        #[cfg(any(test, feature = "test-support"))]
        if std::mem::take(&mut self.fail_next_start) {
            return Err(std::io::Error::other("injected thread start refusal"));
        }
        let directory = self.directory.clone();
        let execution_source = Arc::clone(&self.execution_source);
        let sender = self.sender.clone();
        let open = open.clone();
        let intake_log = self.intake_log.clone();
        std::thread::Builder::new()
            .name("device-provider".to_owned())
            .spawn(move || {
                let outcome = ProviderRuntimeStore::open_existing(&directory)
                    .map(|store| store.with_execution_source(execution_source))
                    .and_then(|store| {
                        let detail = format!(
                            "request_id={} provider_thread={:?} retry_entry={retry_entry}",
                            open.request_id.0,
                            std::thread::current().id(),
                        );
                        write_intake_log(
                            intake_log.as_deref(),
                            IntakeLogLine {
                                stage: "provider_store_opened",
                                code: "DEVICE_PROVIDER_STORE_OPENED",
                                exchange_id: &open.model_exchange_id.0,
                                sequence: 0,
                                worker_session_id: &open.worker_session_id.0,
                                thread_id: &open.session_identity.codex_thread_id.0,
                                detail: &detail,
                            },
                        );
                        let can_start = || {
                            start_guard.as_ref().map_or_else(
                                || {
                                    start_deadline.is_some_and(|deadline| {
                                        std::time::Instant::now() < deadline
                                    })
                                },
                                |check| check(),
                            )
                        };
                        let before_attempt = || store.wait_model_attempt_permit(&open, &can_start);
                        let after_attempt = || {};
                        store.execute_model_recovering_step_authorized(
                            &open,
                            can_start,
                            before_attempt,
                            after_attempt,
                        )
                    })
                    .unwrap_or_else(|error| {
                        DeviceModelStepOutcome::Complete(vec![model_failure(&open, error.code())])
                    });
                // The store, exchange owner and registration have all closed.
                let _ = sender.send((open.model_exchange_id.0, outcome));
            })
    }

    pub(crate) fn start_recorded(
        &self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
    ) -> Result<bool, DeviceProviderError> {
        ProviderRuntimeStore::open_existing(&self.directory)?.model_start_recorded(open)
    }

    pub(crate) fn retry_chunk(&mut self, chunk: ModelChunkMessage) {
        self.chunks.push_front(chunk);
    }

    pub(crate) fn drop_chunk(&mut self, chunk: &ModelChunkMessage) {
        self.chunks.retain(|queued| {
            queued.model_exchange_id != chunk.model_exchange_id || queued.sequence != chunk.sequence
        });
    }

    pub(crate) fn next_chunk(&mut self) -> Result<Option<ModelChunkMessage>, DeviceProviderError> {
        while let Ok((id, outcome)) = self.receiver.try_recv() {
            let Some(mut pending) = self.pending.remove(&id) else {
                return Err(DeviceProviderError::StateInvalid);
            };
            if let Some(thread) = pending.thread.take() {
                thread
                    .join()
                    .map_err(|_| DeviceProviderError::StateInvalid)?;
            }
            let chunks = match outcome {
                DeviceModelStepOutcome::Complete(chunks) => chunks,
                DeviceModelStepOutcome::DeferredRetry { wake_millis } => {
                    pending.retry_wake_millis = Some(wake_millis);
                    self.log_intake(IntakeLogLine {
                        stage: "provider_retry_deferred", code: "DEVICE_PROVIDER_RETRY_DEFERRED",
                        exchange_id: &id, sequence: 0,
                        worker_session_id: &pending.open.worker_session_id.0,
                        thread_id: &pending.open.session_identity.codex_thread_id.0,
                        detail: &format!("request_id={} wake_millis={wake_millis} old_thread_joined=true old_store_closed=true", pending.open.request_id.0),
                    });
                    self.pending.insert(id, pending);
                    continue;
                }
            };
            if !ProviderRuntimeStore::open_existing(&self.directory)?.model_cancelled(&id)? {
                self.owned_exchanges.insert(id.clone());
                if let Some(first) = chunks.first() {
                    let session = first.session_identity.worker_session_id.0.clone();
                    if !session.is_empty() {
                        self.session_ids.insert(session);
                    }
                    let instance = first.lease.worker_instance_id.0.clone();
                    if !instance.is_empty() {
                        self.instance_ids.insert(instance);
                    }
                }
                self.chunks.extend(chunks);
            }
        }
        self.resume_deferred_models()?;
        if self.chunks.is_empty() {
            self.recover_from_durable_store()?;
        }
        let Some(chunk) = self.chunks.pop_front() else {
            return Ok(None);
        };
        let high_water = self
            .delivered_high_water
            .entry(chunk.model_exchange_id.0.clone())
            .or_insert(0);
        if chunk.sequence.0 > *high_water {
            *high_water = chunk.sequence.0;
        }
        Ok(Some(chunk))
    }

    fn resume_deferred_models(&mut self) -> Result<(), DeviceProviderError> {
        let now = winwincode_network::journal::now_millis();
        let mut ready = Vec::new();
        for (id, pending) in &self.pending {
            if pending.thread.is_some() {
                continue;
            }
            if pending.retry_wake_millis.is_none_or(|wake| now >= wake)
                || !pending.can_start()
                || match self.recovery_store.as_ref() {
                    Some(store) => store.model_cancelled(id)?,
                    None => false,
                }
            {
                ready.push((
                    id.clone(),
                    pending.open.clone(),
                    pending.start_deadline,
                    pending.start_guard.clone(),
                ));
            }
        }
        for (id, open, deadline, guard) in ready {
            if let Ok(thread) = self.start_provider(&open, deadline, guard, true) {
                let pending = self
                    .pending
                    .get_mut(&id)
                    .ok_or(DeviceProviderError::StateInvalid)?;
                pending.thread = Some(thread);
            }
        }
        Ok(())
    }

    fn recover_from_durable_store(&mut self) -> Result<(), DeviceProviderError> {
        if self.owned_exchanges.is_empty() && self.session_ids.is_empty() {
            return Ok(());
        }
        let Some(store) = self.recovery_store.as_ref() else {
            return Ok(());
        };
        let version = store.model_data_version()?;
        if self.recovery_version == Some(version) {
            return Ok(());
        }
        let mut exchanges = self.owned_exchanges.clone();
        for session in &self.session_ids {
            exchanges.extend(store.model_exchanges_for_owner(Some(session), None)?);
        }
        for instance in &self.instance_ids {
            exchanges.extend(store.model_exchanges_for_owner(None, Some(instance))?);
        }
        for exchange_id in exchanges {
            if self.pending.contains_key(&exchange_id) {
                continue;
            }
            let high_water = self
                .delivered_high_water
                .get(&exchange_id)
                .copied()
                .unwrap_or(0);
            let frames = store.replay_model(&exchange_id, high_water.saturating_add(1))?;
            if frames.is_empty() {
                continue;
            }
            self.owned_exchanges.insert(exchange_id);
            self.chunks.extend(frames);
        }
        self.recovery_version = Some(version);
        Ok(())
    }

    pub(crate) fn log_intake(&self, line: IntakeLogLine<'_>) {
        write_intake_log(self.intake_log.as_deref(), line);
    }
}

fn write_intake_log(path: Option<&Path>, line: IntakeLogLine<'_>) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "ts={ts} component=device_models stage={} code={} exchange={} seq={} wsn={} thr={} detail={}",
        line.stage,
        line.code,
        line.exchange_id,
        line.sequence,
        line.worker_session_id,
        line.thread_id,
        line.detail
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
        Instant, LeaseId, ModelExchangeId, ProductSessionId, SchemaVersion, SessionIdentity,
        Sha256Digest, WorkerId, WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::{
        EncodedPayload, ExecutionLeaseStamp, ModelChunkMessageKind,
    };
    use winwincode_provider::DeviceProviderStore;

    fn authority_session(thread: &str, worker_session_id: &str) -> SessionIdentity {
        let worker_session_id = WorkerSessionId(worker_session_id.to_owned());
        SessionIdentity {
            codex_thread_id: CodexThreadId(thread.to_owned()),
            product_session_id: ProductSessionId(format!("psn_{}", "C".repeat(26))),
            work_run_id: None,
            worker_session_id,
        }
    }

    fn chunk(
        exchange: &str,
        sequence: i64,
        is_final: bool,
        worker_session_id: &str,
        worker_instance_id: &str,
    ) -> ModelChunkMessage {
        let session = authority_session("cdx_post_tool_second_call", worker_session_id);
        let payload_bytes: &[u8] = if sequence == 1 {
            br#"{"type":"created"}"#
        } else if is_final {
            br#"{"type":"completed","endTurn":true}"#
        } else {
            br#"{"type":"server_model","model":"loopback"}"#
        };
        ModelChunkMessage {
            error: None,
            is_final,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2030-01-01T01:00:00.000Z".to_owned()),
                fencing_token: FencingToken("1".to_owned()),
                issued_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
                job_id: ExecutionJobId(format!("job_{}", "D".repeat(26))),
                lease_id: LeaseId(format!("lse_{}", "E".repeat(26))),
                worker_id: WorkerId(format!("wrk_{}", "F".repeat(26))),
                worker_instance_id: WorkerInstanceId(worker_instance_id.to_owned()),
            },
            message_id: ExecutionMessageId(format!("xmsg_0{exchange}{sequence}")),
            model_exchange_id: ModelExchangeId(exchange.to_owned()),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    payload_bytes,
                ),
                payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload_bytes))),
            }),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
            sequence: ExecutionSequence(sequence),
            session_identity: session.clone(),
            worker_session_id: session.worker_session_id,
        }
    }

    fn retained_open(
        exchange: &str,
        session: &str,
        instance: &str,
    ) -> winwincode_execution_port::generated::ModelOpenMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut open: winwincode_execution_port::generated::ModelOpenMessage =
            serde_json::from_value(
                fixture["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == "model.open")
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let frame = chunk(exchange, 1, false, session, instance);
        open.model_exchange_id = frame.model_exchange_id;
        open.lease = frame.lease;
        open.session_identity = frame.session_identity;
        open.worker_session_id = frame.worker_session_id;
        open
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "winwincode-device-models-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn full_provider_slots_defer_a_never_started_request_without_losing_its_identity() {
        let root = temp_root("deferred-start");
        let mut models = DeviceModels::for_test(&root);
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut open: ExecutionPortMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        if let ExecutionPortMessage::ModelOpenMessage(request) = &mut open {
            let payload = br#"{"provider":"fixture-provider","request":{"model":"fixture-model"}}"#;
            request.request.data_base64 =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload);
            request.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(payload));
        }
        let store = DeviceProviderStore::open(&root).unwrap();
        let ExecutionPortMessage::ModelOpenMessage(request) = &open else {
            unreachable!()
        };
        let permits = (0..3)
            .map(|_| match store.try_model_permit(request).unwrap() {
                DeviceModelAdmission::InvokeAttempt(permit) => permit,
                DeviceModelAdmission::Deferred => panic!("expected a free Provider slot"),
                _ => panic!("expected physical invocation admission"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            models
                .send(
                    &open,
                    Some(std::time::Instant::now() + std::time::Duration::from_mins(1)),
                    None,
                )
                .unwrap(),
            DeviceModelSendOutcome::NotStarted
        );
        assert!(models.pending.is_empty());
        assert!(!store.model_start_recorded(request).unwrap());
        drop(permits);
        models.fail_next_start = true;
        assert_eq!(
            models
                .send(
                    &open,
                    Some(std::time::Instant::now() + std::time::Duration::from_mins(1)),
                    None,
                )
                .unwrap(),
            DeviceModelSendOutcome::NotStarted
        );
        assert!(
            models.pending.is_empty(),
            "thread refusal leaves no started call"
        );
        models
            .send(
                &open,
                Some(std::time::Instant::now() + std::time::Duration::from_mins(1)),
                None,
            )
            .unwrap();
        let ExecutionPortMessage::ModelOpenMessage(open) = open else {
            unreachable!()
        };
        assert!(models.pending.contains_key(&open.model_exchange_id.0));
        models
            .send(
                &ExecutionPortMessage::ModelOpenMessage(open),
                Some(std::time::Instant::now() + std::time::Duration::from_mins(1)),
                None,
            )
            .unwrap();
        assert_eq!(
            models.pending.len(),
            1,
            "same identity never launches two calls"
        );
        let (_, chunks) = models
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let DeviceModelStepOutcome::Complete(chunks) = chunks else {
            panic!("unconfigured provider cannot defer a physical retry");
        };
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_final);
        drop(models);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_provider_frames_are_replayed_above_the_delivered_high_water() {
        let root = temp_root("replay");
        let exchange = format!("mdl_{}", "H".repeat(26));
        let wsn = format!("wsn_{}", "B".repeat(26));
        let wki = format!("wki_{}", "G".repeat(26));
        {
            let store = DeviceProviderStore::open(&root).expect("open device provider store");
            let chunks = vec![
                chunk(&exchange, 1, false, &wsn, &wki),
                chunk(&exchange, 2, false, &wsn, &wki),
                chunk(&exchange, 3, true, &wsn, &wki),
            ];
            store
                .retain_stored_model_exchange_chunks(&retained_open(&exchange, &wsn, &wki), &chunks)
                .expect("seed durable provider exchange");
        }
        let mut models = DeviceModels::for_test(&root);
        models.recovery_store =
            Some(ProviderRuntimeStore::open_existing(&root.join("runtime")).unwrap());
        models.owned_exchanges.insert(exchange.clone());
        models.delivered_high_water.insert(exchange.clone(), 1);
        let recovered: Vec<_> = std::iter::from_fn(|| models.next_chunk().unwrap())
            .take(2)
            .collect();
        assert_eq!(
            recovered
                .iter()
                .map(|chunk| chunk.sequence.0)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(recovered.last().expect("final recovered frame").is_final);
        assert!(models.next_chunk().unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn verification_exchange_recover_skips_foreign_device_store_frames() {
        let root = temp_root("foreign");
        let foreign_exchange = format!("mdl_{}", "A".repeat(26));
        let owned_exchange = format!("mdl_{}", "Z".repeat(26));
        let foreign_wsn = format!("wsn_{}", "F".repeat(26));
        let owned_wsn = format!("wsn_{}", "O".repeat(26));
        let foreign_wki = format!("wki_{}", "1".repeat(26));
        let owned_wki = format!("wki_{}", "2".repeat(26));
        {
            let store = DeviceProviderStore::open(&root).expect("open device provider store");
            store
                .retain_stored_model_exchange_chunks(
                    &retained_open(&foreign_exchange, &foreign_wsn, &foreign_wki),
                    &[
                        chunk(&foreign_exchange, 1, false, &foreign_wsn, &foreign_wki),
                        chunk(&foreign_exchange, 2, true, &foreign_wsn, &foreign_wki),
                    ],
                )
                .expect("seed foreign exchange");
            store
                .retain_stored_model_exchange_chunks(
                    &retained_open(&owned_exchange, &owned_wsn, &owned_wki),
                    &[
                        chunk(&owned_exchange, 1, false, &owned_wsn, &owned_wki),
                        chunk(&owned_exchange, 2, false, &owned_wsn, &owned_wki),
                        chunk(&owned_exchange, 3, true, &owned_wsn, &owned_wki),
                    ],
                )
                .expect("seed owned verification exchange");
        }
        let mut models = DeviceModels::for_test(&root);
        models.recovery_store =
            Some(ProviderRuntimeStore::open_existing(&root.join("runtime")).unwrap());
        models.note_worker_instance_id(&owned_wki);
        models.session_ids.insert(owned_wsn.clone());
        models.owned_exchanges.insert(owned_exchange.clone());

        let recovered: Vec<_> = std::iter::from_fn(|| models.next_chunk().unwrap()).collect();
        assert!(
            recovered
                .iter()
                .all(|chunk| chunk.model_exchange_id.0 == owned_exchange),
            "recover must not re-queue foreign Device-store exchanges, got sequences {:?}",
            recovered
                .iter()
                .map(|chunk| (chunk.model_exchange_id.0.clone(), chunk.sequence.0))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            recovered
                .iter()
                .map(|chunk| chunk.sequence.0)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(models.next_chunk().unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn verification_exchange_recover_matches_session_identity_after_restart() {
        let root = temp_root("restart");
        let exchange = format!("mdl_{}", "V".repeat(26));
        let wsn = format!("wsn_{}", "R".repeat(26));
        let wki = format!("wki_{}", "K".repeat(26));
        {
            let store = DeviceProviderStore::open(&root).expect("open device provider store");
            store
                .retain_stored_model_exchange_chunks(
                    &retained_open(&exchange, &wsn, &wki),
                    &[
                        chunk(&exchange, 1, false, &wsn, &wki),
                        chunk(&exchange, 2, true, &wsn, &wki),
                    ],
                )
                .expect("seed durable verification exchange");
        }
        let mut models = DeviceModels::for_test(&root);
        models.recovery_store =
            Some(ProviderRuntimeStore::open_existing(&root.join("runtime")).unwrap());
        models.session_ids.insert(wsn.clone());
        models.note_worker_instance_id(&wki);
        let recovered: Vec<_> = std::iter::from_fn(|| models.next_chunk().unwrap()).collect();
        assert_eq!(
            recovered
                .iter()
                .map(|chunk| chunk.sequence.0)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod lazy_store_tests {
    use super::DeviceModels;

    #[test]
    fn idle_worker_does_not_open_provider_store() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("providers");
        std::fs::write(&directory, "unavailable provider store").unwrap();
        let mut models = DeviceModels::new(
            &directory,
            std::sync::Arc::new(winwincode_provider::LocalDeviceProviderSource::new(
                directory.clone(),
            )),
        );
        models.note_worker_instance_id("worker-instance");
        assert!(models.next_chunk().unwrap().is_none());
        assert!(models.recovery_store.is_none());
        assert!(directory.is_file());
    }

    #[test]
    fn unavailable_provider_store_returns_one_model_failure_and_keeps_the_worker_idle() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("providers");
        std::fs::write(&directory, "unavailable provider store").unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let open = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let mut models = DeviceModels::new(
            &directory,
            std::sync::Arc::new(winwincode_provider::LocalDeviceProviderSource::new(
                directory.clone(),
            )),
        );
        models.send(&open, None, None).unwrap();
        assert!(models.next_chunk().unwrap().unwrap().error.is_some());
        assert!(models.next_chunk().unwrap().is_none());
        assert!(models.recovery_store.is_none());
    }
}
