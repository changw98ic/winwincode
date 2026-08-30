// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use winwincode_codex::WorkerExecutionPort;
use winwincode_codex::model_port_client::{
    ModelAuthorityRejection, ModelCancellationFingerprint, ModelCancellationPhase,
    ModelCancellationReceipt, ModelChunkDelivery, ModelChunkDisposition, ModelChunkFingerprint,
    ModelChunkSink, ModelCursorSnapshot, ModelCursorStore, ModelDisconnectOutcome,
    ModelLeaseAuthority, ModelLeaseAuthoritySource, ModelMessageMetadata, ModelOpenOutcome,
    ModelPortClientErrorCode, ModelSinkDeliveryStatus, ModelTerminationReason,
    OpenModelExchangeCommand, WorkerModelPortClient,
};
use winwincode_domain::{
    CodexThreadId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    ModelExchangeId, ProductSessionId, RequestId, SchemaVersion, SessionIdentity, Sha256Digest,
    StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionLeaseStamp, ExecutionPortError, ExecutionPortErrorCode,
    ExecutionPortMessage, LeaseWriteStatus, ModelChunkMessage, ModelChunkMessageKind,
    ModelGatewayRoute,
};
use winwincode_execution_port::replay::ReplayStreamKey;

#[derive(Clone, Default)]
struct RecordingPort(Arc<Mutex<Vec<ExecutionPortMessage>>>);

impl RecordingPort {
    fn messages(&self) -> Vec<ExecutionPortMessage> {
        self.0.lock().expect("recording port").clone()
    }
}

impl WorkerExecutionPort for RecordingPort {
    type Error = ();

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.0.lock().expect("recording port").push(message);
        std::future::ready(Ok(()))
    }
}

#[derive(Clone, Default)]
struct MemoryCursorStore {
    rows: Arc<Mutex<HashMap<ReplayStreamKey, ModelCursorSnapshot>>>,
    fail_next_completion: Arc<AtomicBool>,
}

impl MemoryCursorStore {
    fn only_snapshot(&self) -> ModelCursorSnapshot {
        self.rows
            .lock()
            .expect("cursor store")
            .values()
            .next()
            .cloned()
            .expect("cursor snapshot")
    }

    fn fail_next_completion(&self) {
        self.fail_next_completion.store(true, Ordering::Relaxed);
    }

    fn is_empty(&self) -> bool {
        self.rows.lock().expect("cursor store").is_empty()
    }
}

impl ModelCursorStore for MemoryCursorStore {
    type Error = &'static str;

    fn load(
        &mut self,
        stream: &ReplayStreamKey,
    ) -> Result<Option<ModelCursorSnapshot>, Self::Error> {
        Ok(self
            .rows
            .lock()
            .map_err(|_| "cursor lock")?
            .get(stream)
            .cloned())
    }

    fn record_delivery(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelChunkFingerprint,
        termination: Option<ModelTerminationReason>,
    ) -> Result<(), Self::Error> {
        let mut rows = self.rows.lock().map_err(|_| "cursor lock")?;
        let snapshot = rows.entry(stream.clone()).or_default();
        if snapshot.confirmed_sequence != expected_sequence
            || fingerprint.sequence != expected_sequence + 1
            || snapshot.termination.is_some()
            || snapshot.cancellation.is_some()
        {
            return Err("cursor conflict");
        }
        snapshot.frames.push(fingerprint.clone());
        snapshot.confirmed_sequence = fingerprint.sequence;
        snapshot.termination = termination;
        Ok(())
    }

    fn terminate(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        reason: ModelTerminationReason,
    ) -> Result<(), Self::Error> {
        let mut rows = self.rows.lock().map_err(|_| "cursor lock")?;
        let snapshot = rows.entry(stream.clone()).or_default();
        if snapshot.confirmed_sequence != expected_sequence
            || snapshot.termination.is_some()
            || snapshot.cancellation.is_some()
        {
            return Err("terminal cursor conflict");
        }
        snapshot.termination = Some(reason);
        Ok(())
    }

    fn record_cancellation_intent(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error> {
        let mut rows = self.rows.lock().map_err(|_| "cursor lock")?;
        let snapshot = rows.entry(stream.clone()).or_default();
        if snapshot.confirmed_sequence != expected_sequence
            || snapshot.termination.is_some()
            || snapshot.cancellation.is_some()
            || fingerprint.confirmed_sequence != expected_sequence
            || fingerprint.phase != ModelCancellationPhase::Intent
        {
            return Err("cancellation cursor conflict");
        }
        snapshot.cancellation = Some(fingerprint.clone());
        Ok(())
    }

    fn complete_cancellation(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error> {
        if self.fail_next_completion.swap(false, Ordering::Relaxed) {
            return Err("injected cancellation completion failure");
        }
        let mut rows = self.rows.lock().map_err(|_| "cursor lock")?;
        let snapshot = rows.get_mut(stream).ok_or("missing cancellation intent")?;
        let existing = snapshot
            .cancellation
            .as_ref()
            .ok_or("missing cancellation intent")?;
        if snapshot.confirmed_sequence != expected_sequence
            || snapshot.termination.is_some()
            || existing.message_id != fingerprint.message_id
            || existing.confirmed_sequence != fingerprint.confirmed_sequence
            || existing.digest != fingerprint.digest
            || existing.phase != ModelCancellationPhase::Intent
        {
            return Err("cancellation completion conflict");
        }
        let mut applied = fingerprint.clone();
        applied.phase = ModelCancellationPhase::Applied;
        snapshot.cancellation = Some(applied);
        snapshot.termination = Some(ModelTerminationReason::Cancelled);
        Ok(())
    }
}

#[derive(Clone)]
struct CurrentAuthority(Arc<Mutex<Option<ModelLeaseAuthority>>>);

impl CurrentAuthority {
    fn new(authority: ModelLeaseAuthority) -> Self {
        Self(Arc::new(Mutex::new(Some(authority))))
    }

    fn replace(&self, authority: ModelLeaseAuthority) {
        *self.0.lock().expect("current authority") = Some(authority);
    }
}

impl ModelLeaseAuthoritySource for CurrentAuthority {
    fn validate_current(
        &self,
        authority: &ModelLeaseAuthority,
        _now: &Instant,
    ) -> Result<(), ModelAuthorityRejection> {
        match self
            .0
            .lock()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?
            .as_ref()
        {
            Some(current) if current == authority => Ok(()),
            Some(_) => Err(ModelAuthorityRejection::StaleLease),
            None => Err(ModelAuthorityRejection::Unavailable),
        }
    }
}

#[derive(Clone, Default)]
struct RecordingSink {
    delivered: Arc<Mutex<Vec<(String, u64)>>>,
    delivered_payloads: Arc<Mutex<Vec<EncodedPayload>>>,
    seen: Arc<Mutex<HashSet<(String, u64)>>>,
    terminated: Arc<Mutex<Vec<(String, ModelTerminationReason)>>>,
    released: Arc<Mutex<HashSet<String>>>,
    fail_next_terminate: Arc<AtomicBool>,
}

impl RecordingSink {
    fn sequences(&self) -> Vec<u64> {
        self.delivered
            .lock()
            .expect("recording sink")
            .iter()
            .map(|(_, sequence)| *sequence)
            .collect()
    }

    fn payloads(&self) -> Vec<EncodedPayload> {
        self.delivered_payloads
            .lock()
            .expect("recording sink payloads")
            .clone()
    }

    fn terminations(&self) -> Vec<(String, ModelTerminationReason)> {
        self.terminated.lock().expect("sink terminations").clone()
    }

    fn released_count(&self) -> usize {
        self.released.lock().expect("sink releases").len()
    }

    fn fail_next_terminate(&self) {
        self.fail_next_terminate.store(true, Ordering::Relaxed);
    }
}

impl ModelChunkSink for RecordingSink {
    type Error = ();

    fn deliver(
        &mut self,
        delivery: ModelChunkDelivery<'_>,
    ) -> impl Future<Output = Result<ModelSinkDeliveryStatus, Self::Error>> {
        let key = (delivery.model_exchange_id.0.clone(), delivery.sequence);
        let inserted = self.seen.lock().expect("sink seen").insert(key.clone());
        if inserted {
            self.delivered.lock().expect("sink deliveries").push(key);
            if let Some(payload) = delivery.payload {
                self.delivered_payloads
                    .lock()
                    .expect("sink payloads")
                    .push(payload.clone());
            }
            std::future::ready(Ok(ModelSinkDeliveryStatus::Applied))
        } else {
            std::future::ready(Ok(ModelSinkDeliveryStatus::Duplicate))
        }
    }

    fn terminate(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        reason: ModelTerminationReason,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        if self.fail_next_terminate.swap(false, Ordering::Relaxed) {
            return std::future::ready(Err(()));
        }
        let key = (model_exchange_id.0.clone(), reason);
        let mut terminated = self.terminated.lock().expect("sink terminations");
        if !terminated.contains(&key) {
            terminated.push(key);
        }
        std::future::ready(Ok(()))
    }

    fn release(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.released
            .lock()
            .expect("sink releases")
            .insert(model_exchange_id.0.clone());
        std::future::ready(Ok(()))
    }
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-03-15T08:00:{second:02}.000Z"))
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn authority(stage: Option<u64>) -> ModelLeaseAuthority {
    let worker_session_id = WorkerSessionId(id("wsn", 1));
    ModelLeaseAuthority {
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: at(50),
            fencing_token: FencingToken("1".into()),
            issued_at: at(1),
            job_id: winwincode_domain::ExecutionJobId(id("job", 1)),
            lease_id: LeaseId(id("lse", 1)),
            worker_id: WorkerId(id("wrk", 1)),
            worker_instance_id: WorkerInstanceId(id("wki", 1)),
        },
        worker_session_id: worker_session_id.clone(),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", 1)),
            product_session_id: ProductSessionId(id("psn", 1)),
            stage_run_id: stage.map(|value| StageRunId(id("run", value))),
            worker_session_id,
        },
    }
}

fn metadata(value: u64, second: u64) -> ModelMessageMetadata {
    ModelMessageMetadata {
        message_id: ExecutionMessageId(id("xmsg", value)),
        sent_at: at(second),
    }
}

fn payload(byte: char) -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".into(),
        data_base64: byte.to_string().repeat(8),
        payload_digest: digest(byte),
    }
}

fn namespaced_tool_payload() -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".into(),
        data_base64: "eyJ0eXBlIjoib3V0cHV0X2l0ZW0uZG9uZSIsIml0ZW0iOnsidHlwZSI6ImZ1bmN0aW9uX2NhbGwiLCJpZCI6ImZjXzEiLCJuYW1lIjoic2VhcmNoIiwibmFtZXNwYWNlIjoid29ya3NwYWNlIiwiYXJndW1lbnRzIjoie30iLCJjYWxsX2lkIjoiY2FsbF8xIn19".into(),
        payload_digest: Sha256Digest(
            "sha256:687f3d6a7b12ee9c3f612cb4ef57ddab6b9c4709f94faa81991371a166571f22".into(),
        ),
    }
}

fn open_command(
    authority: &ModelLeaseAuthority,
    exchange: u64,
    message: u64,
    request_payload: char,
) -> OpenModelExchangeCommand {
    OpenModelExchangeCommand {
        metadata: metadata(message, 10),
        authority: authority.clone(),
        model_exchange_id: ModelExchangeId(id("mdl", exchange)),
        request_id: RequestId(id("req", exchange)),
        route: ModelGatewayRoute {
            capability: "responses".into(),
            route: "provider:model".into(),
        },
        request: payload(request_payload),
    }
}

fn chunk(
    authority: &ModelLeaseAuthority,
    exchange: u64,
    sequence: i64,
    body: char,
    is_final: bool,
    error: Option<ExecutionPortError>,
) -> ModelChunkMessage {
    ModelChunkMessage {
        error,
        is_final,
        kind: ModelChunkMessageKind::ModelChunk,
        lease: authority.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", 100 + sequence.cast_unsigned())),
        model_exchange_id: ModelExchangeId(id("mdl", exchange)),
        payload: Some(payload(body)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(10 + sequence.cast_unsigned()),
        sequence: ExecutionSequence(sequence),
        session_identity: authority.session_identity.clone(),
        worker_session_id: authority.worker_session_id.clone(),
    }
}

fn model_acks(port: &RecordingPort) -> Vec<winwincode_execution_port::generated::ModelAckMessage> {
    port.messages()
        .into_iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::ModelAckMessage(ack) => Some(ack),
            _ => None,
        })
        .collect()
}

fn assert_resume_ack_flow(port: &RecordingPort) {
    let acks = model_acks(port);
    assert_eq!(
        acks.iter().map(|ack| &ack.status).collect::<Vec<_>>(),
        vec![
            &LeaseWriteStatus::Accepted,
            &LeaseWriteStatus::Duplicate,
            &LeaseWriteStatus::Gap,
            &LeaseWriteStatus::Accepted,
            &LeaseWriteStatus::Gap,
            &LeaseWriteStatus::Accepted,
            &LeaseWriteStatus::Duplicate,
        ]
    );
    assert_eq!(acks[2].replay_from_sequence, Some(ExecutionSequence(2)));
    assert_eq!(acks[4].replay_from_sequence, Some(ExecutionSequence(3)));
}

fn assert_cancellation_ack_authority(port: &RecordingPort, authority: &ModelLeaseAuthority) {
    let cancellation_acks = model_acks(port)
        .into_iter()
        .filter(|ack| {
            ack.error
                .as_ref()
                .is_some_and(|error| error.code == ExecutionPortErrorCode::Cancelled)
        })
        .collect::<Vec<_>>();
    assert_eq!(cancellation_acks.len(), 2);
    assert!(cancellation_acks.iter().all(|ack| {
        ack.status == LeaseWriteStatus::RejectedConflict
            && ack.ack_sequence.0 == 1
            && ack.replay_from_sequence.is_none()
            && ack.lease == authority.lease
            && ack.worker_session_id == authority.worker_session_id
            && ack.session_identity == authority.session_identity
    }));
}

#[tokio::test]
async fn product_session_stream_delivers_once_and_resumes_from_confirmed_cursor() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let port = RecordingPort::default();
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let mut client = WorkerModelPortClient::new(port.clone(), store.clone(), current, sink.clone());
    assert_eq!(
        client
            .open(open_command(&authority, 1, 1, 'a'))
            .await
            .expect("open ProductSession model stream"),
        ModelOpenOutcome::Opened
    );
    assert_eq!(
        client
            .open(open_command(&authority, 1, 2, 'a'))
            .await
            .expect("exact open replay"),
        ModelOpenOutcome::Duplicate
    );

    let first = chunk(&authority, 1, 1, 'b', false, None);
    assert_eq!(
        client
            .accept_chunk(&first, metadata(10, 12))
            .await
            .expect("first chunk"),
        ModelChunkDisposition::Delivered {
            confirmed_sequence: 1,
            termination: None,
        }
    );
    assert_eq!(
        client
            .accept_chunk(&first, metadata(11, 13))
            .await
            .expect("first chunk replay"),
        ModelChunkDisposition::Duplicate {
            confirmed_sequence: 1
        }
    );

    let third = chunk(&authority, 1, 3, 'd', true, None);
    assert_eq!(
        client
            .accept_chunk(&third, metadata(12, 14))
            .await
            .expect("gap"),
        ModelChunkDisposition::Gap {
            confirmed_sequence: 1,
            replay_from_sequence: 2,
        }
    );
    client
        .accept_chunk(&chunk(&authority, 1, 2, 'c', false, None), metadata(13, 15))
        .await
        .expect("second chunk");
    assert_eq!(
        client
            .handle_disconnect(&ModelExchangeId(id("mdl", 1)), metadata(14, 16), true)
            .await
            .expect("resume request"),
        ModelDisconnectOutcome::ResumeRequested {
            confirmed_sequence: 2,
            replay_from_sequence: 3,
        }
    );
    assert_eq!(sink.sequences(), vec![1, 2]);
    assert_eq!(
        client
            .accept_chunk(&third, metadata(15, 17))
            .await
            .expect("third replay"),
        ModelChunkDisposition::Delivered {
            confirmed_sequence: 3,
            termination: Some(ModelTerminationReason::Completed),
        }
    );
    assert_eq!(sink.sequences(), vec![1, 2, 3]);
    assert_eq!(
        client
            .accept_chunk(&third, metadata(16, 18))
            .await
            .expect("terminal duplicate"),
        ModelChunkDisposition::Duplicate {
            confirmed_sequence: 3
        }
    );
    assert_eq!(sink.sequences(), vec![1, 2, 3]);

    assert_resume_ack_flow(&port);
    let persisted = serde_json::to_string(&store.only_snapshot()).expect("cursor JSON");
    assert!(!persisted.contains("dataBase64"));
    assert!(!persisted.contains("provider:model"));
}

#[tokio::test]
async fn restart_reopens_exchange_then_requests_only_the_unconfirmed_suffix() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    {
        let mut first = WorkerModelPortClient::new(
            RecordingPort::default(),
            store.clone(),
            current.clone(),
            sink.clone(),
        );
        first
            .open(open_command(&authority, 2, 20, 'a'))
            .await
            .expect("initial open");
        first
            .accept_chunk(&chunk(&authority, 2, 1, 'b', false, None), metadata(21, 12))
            .await
            .expect("confirmed first chunk");
    }

    let restarted_port = RecordingPort::default();
    let mut restarted =
        WorkerModelPortClient::new(restarted_port.clone(), store, current, sink.clone());
    restarted
        .open(open_command(&authority, 2, 22, 'a'))
        .await
        .expect("idempotent Provider open after Worker restart");
    assert_eq!(
        restarted
            .handle_disconnect(&ModelExchangeId(id("mdl", 2)), metadata(23, 13), true)
            .await
            .expect("resume after restart"),
        ModelDisconnectOutcome::ResumeRequested {
            confirmed_sequence: 1,
            replay_from_sequence: 2,
        }
    );
    assert_eq!(sink.sequences(), vec![1]);
    let acks = model_acks(&restarted_port);
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].ack_sequence.0, 1);
    assert_eq!(acks[0].replay_from_sequence, Some(ExecutionSequence(2)));
}

#[tokio::test]
async fn restart_rehydrates_confirmed_terminal_frames_into_the_new_kernel_sink() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let store = MemoryCursorStore::default();
    let first_chunk = chunk(&authority, 12, 1, 'b', false, None);
    let final_chunk = chunk(&authority, 12, 2, 'c', true, None);
    {
        let mut first = WorkerModelPortClient::new(
            RecordingPort::default(),
            store.clone(),
            current.clone(),
            RecordingSink::default(),
        );
        first
            .open(open_command(&authority, 12, 120, 'a'))
            .await
            .expect("open initial terminal stream");
        first
            .accept_chunk(&first_chunk, metadata(121, 12))
            .await
            .expect("persist initial frame");
        first
            .accept_chunk(&final_chunk, metadata(122, 13))
            .await
            .expect("persist terminal frame");
    }

    let restarted_sink = RecordingSink::default();
    let mut restarted = WorkerModelPortClient::new(
        RecordingPort::default(),
        store,
        current,
        restarted_sink.clone(),
    );
    restarted
        .open(open_command(&authority, 12, 120, 'a'))
        .await
        .expect("reopen exact terminal stream");
    assert_eq!(
        restarted
            .accept_chunk(&first_chunk, metadata(123, 14))
            .await
            .expect("rehydrate first confirmed frame"),
        ModelChunkDisposition::Duplicate {
            confirmed_sequence: 2,
        }
    );
    assert_eq!(restarted_sink.sequences(), vec![1]);
    assert_eq!(restarted_sink.released_count(), 0);
    assert_eq!(
        restarted
            .accept_chunk(&final_chunk, metadata(124, 15))
            .await
            .expect("rehydrate terminal confirmed frame"),
        ModelChunkDisposition::Duplicate {
            confirmed_sequence: 2,
        }
    );
    assert_eq!(restarted_sink.sequences(), vec![1, 2]);
    assert_eq!(restarted_sink.released_count(), 1);
}

#[tokio::test]
async fn namespaced_tool_payload_is_exact_across_restart_replay_and_cancellation() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let mut tool_chunk = chunk(&authority, 10, 1, 'b', false, None);
    let expected_payload = namespaced_tool_payload();
    tool_chunk.payload = Some(expected_payload.clone());

    {
        let mut first = WorkerModelPortClient::new(
            RecordingPort::default(),
            store.clone(),
            current.clone(),
            sink.clone(),
        );
        first
            .open(open_command(&authority, 10, 100, 'a'))
            .await
            .expect("open namespaced tool stream");
        assert_eq!(
            first
                .accept_chunk(&tool_chunk, metadata(101, 12))
                .await
                .expect("deliver namespaced tool payload"),
            ModelChunkDisposition::Delivered {
                confirmed_sequence: 1,
                termination: None,
            }
        );
    }

    let mut restarted = WorkerModelPortClient::new(
        RecordingPort::default(),
        store.clone(),
        current,
        sink.clone(),
    );
    restarted
        .open(open_command(&authority, 10, 102, 'a'))
        .await
        .expect("reopen namespaced tool stream");
    assert_eq!(
        restarted
            .accept_chunk(&tool_chunk, metadata(103, 13))
            .await
            .expect("deduplicate exact namespaced tool replay"),
        ModelChunkDisposition::Duplicate {
            confirmed_sequence: 1,
        }
    );
    assert_eq!(sink.payloads(), [expected_payload]);

    assert_eq!(
        restarted
            .cancel_exchange(&ModelExchangeId(id("mdl", 10)), metadata(104, 14))
            .await
            .expect("cancel replayed namespaced tool stream"),
        ModelCancellationReceipt {
            confirmed_sequence: 1,
            replayed: false,
        }
    );
    let snapshot = store.only_snapshot();
    assert_eq!(snapshot.confirmed_sequence, 1);
    assert_eq!(
        snapshot.termination,
        Some(ModelTerminationReason::Cancelled)
    );
    assert_eq!(sink.payloads(), [namespaced_tool_payload()]);
}

#[tokio::test]
async fn old_lease_and_foreign_stage_streams_are_rejected_before_codex_or_ack() {
    let authority = authority(Some(1));
    let current = CurrentAuthority::new(authority.clone());
    let port = RecordingPort::default();
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let mut client =
        WorkerModelPortClient::new(port.clone(), store.clone(), current.clone(), sink.clone());
    client
        .open(open_command(&authority, 3, 30, 'a'))
        .await
        .expect("open DeliveryStage stream");

    let mut foreign_stage = chunk(&authority, 3, 1, 'b', false, None);
    foreign_stage.session_identity.stage_run_id = Some(StageRunId(id("run", 2)));
    assert_eq!(
        client
            .accept_chunk(&foreign_stage, metadata(31, 12))
            .await
            .expect_err("foreign StageRun")
            .code(),
        ModelPortClientErrorCode::StaleAuthority
    );

    let mut replacement = authority.clone();
    replacement.lease.lease_id = LeaseId(id("lse", 2));
    replacement.lease.fencing_token = FencingToken("2".into());
    current.replace(replacement);
    assert_eq!(
        client
            .accept_chunk(&chunk(&authority, 3, 1, 'b', false, None), metadata(32, 13),)
            .await
            .expect_err("old lease chunk")
            .code(),
        ModelPortClientErrorCode::StaleAuthority
    );
    assert!(sink.sequences().is_empty());
    assert!(store.is_empty());
    assert_eq!(model_acks(&port).len(), 0);
    assert_eq!(
        client
            .handle_disconnect(&ModelExchangeId(id("mdl", 3)), metadata(33, 14), true)
            .await
            .expect("stale disconnect terminates"),
        ModelDisconnectOutcome::Terminated(ModelTerminationReason::StaleAuthority)
    );
    assert_eq!(
        store.only_snapshot().termination,
        Some(ModelTerminationReason::StaleAuthority)
    );
    assert_eq!(
        sink.terminations(),
        [(id("mdl", 3), ModelTerminationReason::StaleAuthority)]
    );
    assert_eq!(sink.released_count(), 1);
    assert_eq!(model_acks(&port).len(), 0);
}

#[tokio::test]
async fn expired_lease_disconnect_terminates_and_releases_resources() {
    let authority = authority(None);
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let mut client = WorkerModelPortClient::new(
        RecordingPort::default(),
        store.clone(),
        CurrentAuthority::new(authority.clone()),
        sink.clone(),
    );
    client
        .open(open_command(&authority, 9, 90, 'a'))
        .await
        .expect("open expiring stream");

    assert_eq!(
        client
            .handle_disconnect(&ModelExchangeId(id("mdl", 9)), metadata(91, 50), true)
            .await
            .expect("expired lease becomes terminal"),
        ModelDisconnectOutcome::Terminated(ModelTerminationReason::StaleAuthority)
    );
    assert_eq!(
        store.only_snapshot().termination,
        Some(ModelTerminationReason::StaleAuthority)
    );
    assert_eq!(
        sink.terminations(),
        [(id("mdl", 9), ModelTerminationReason::StaleAuthority)]
    );
    assert_eq!(sink.released_count(), 1);
}

#[tokio::test]
async fn changed_duplicate_and_nonresumable_disconnect_terminate_explicitly() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let port = RecordingPort::default();
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let mut client = WorkerModelPortClient::new(port.clone(), store.clone(), current, sink.clone());
    client
        .open(open_command(&authority, 4, 40, 'a'))
        .await
        .expect("open");
    client
        .accept_chunk(&chunk(&authority, 4, 1, 'b', false, None), metadata(41, 12))
        .await
        .expect("first chunk");
    let conflicting = chunk(&authority, 4, 1, 'c', false, None);
    assert_eq!(
        client
            .accept_chunk(&conflicting, metadata(42, 13))
            .await
            .expect_err("changed duplicate")
            .code(),
        ModelPortClientErrorCode::ExchangeConflict
    );
    assert_eq!(sink.sequences(), vec![1]);
    assert_eq!(
        store.only_snapshot().termination,
        Some(ModelTerminationReason::MessageConflict)
    );
    let acks = model_acks(&port);
    let conflict = acks.last().expect("conflict ack");
    assert_eq!(conflict.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(
        conflict.error.as_ref().map(|error| &error.code),
        Some(&ExecutionPortErrorCode::MessageConflict)
    );

    let second_store = MemoryCursorStore::default();
    let second_port = RecordingPort::default();
    let second_sink = RecordingSink::default();
    let mut second = WorkerModelPortClient::new(
        second_port.clone(),
        second_store.clone(),
        CurrentAuthority::new(authority.clone()),
        second_sink.clone(),
    );
    second
        .open(open_command(&authority, 5, 50, 'a'))
        .await
        .expect("open nonresumable stream");
    assert_eq!(
        second
            .handle_disconnect(&ModelExchangeId(id("mdl", 5)), metadata(51, 15), false)
            .await
            .expect("nonresumable disconnect"),
        ModelDisconnectOutcome::Terminated(ModelTerminationReason::InterruptedNotResumable)
    );
    assert_eq!(
        second_store.only_snapshot().termination,
        Some(ModelTerminationReason::InterruptedNotResumable)
    );
    assert_eq!(
        second_sink.terminations(),
        [(
            id("mdl", 5),
            ModelTerminationReason::InterruptedNotResumable,
        )]
    );
    assert_eq!(second_sink.released_count(), 1);
    let terminal_ack = model_acks(&second_port).pop().expect("terminal ack");
    assert_eq!(terminal_ack.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(
        terminal_ack.error.map(|error| error.code),
        Some(ExecutionPortErrorCode::ModelStreamFailed)
    );
}

#[tokio::test]
async fn cancellation_is_authoritative_replayable_and_releases_resources_once() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let port = RecordingPort::default();
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let mut client =
        WorkerModelPortClient::new(port.clone(), store.clone(), current.clone(), sink.clone());
    client
        .open(open_command(&authority, 6, 60, 'a'))
        .await
        .expect("open cancellable stream");
    client
        .accept_chunk(&chunk(&authority, 6, 1, 'b', false, None), metadata(61, 12))
        .await
        .expect("deliver before cancellation");

    let cancellation_metadata = metadata(62, 13);
    assert_eq!(
        client
            .cancel_exchange(
                &ModelExchangeId(id("mdl", 6)),
                cancellation_metadata.clone(),
            )
            .await
            .expect("cancel stream"),
        ModelCancellationReceipt {
            confirmed_sequence: 1,
            replayed: false,
        }
    );
    assert_eq!(
        client
            .cancel_exchange(
                &ModelExchangeId(id("mdl", 6)),
                cancellation_metadata.clone(),
            )
            .await
            .expect("exact cancellation replay"),
        ModelCancellationReceipt {
            confirmed_sequence: 1,
            replayed: true,
        }
    );
    assert_eq!(
        client
            .cancel_exchange(&ModelExchangeId(id("mdl", 6)), metadata(63, 13))
            .await
            .expect_err("changed cancellation identity conflicts")
            .code(),
        ModelPortClientErrorCode::ExchangeConflict
    );
    assert_eq!(
        sink.terminations(),
        [(id("mdl", 6), ModelTerminationReason::Cancelled,)]
    );
    assert_eq!(sink.released_count(), 1);
    let snapshot = store.only_snapshot();
    assert_eq!(snapshot.confirmed_sequence, 1);
    assert_eq!(
        snapshot.termination,
        Some(ModelTerminationReason::Cancelled)
    );
    assert_eq!(
        snapshot
            .cancellation
            .as_ref()
            .map(|fingerprint| &fingerprint.message_id),
        Some(&cancellation_metadata.message_id)
    );
    assert_cancellation_ack_authority(&port, &authority);

    assert_eq!(
        client
            .release_terminal(&ModelExchangeId(id("mdl", 6)))
            .await
            .expect("forget local terminal exchange"),
        ModelTerminationReason::Cancelled
    );
    assert_eq!(sink.released_count(), 1);

    let restarted_port = RecordingPort::default();
    let mut restarted =
        WorkerModelPortClient::new(restarted_port.clone(), store, current, sink.clone());
    restarted
        .open(open_command(&authority, 6, 64, 'a'))
        .await
        .expect("reopen durable cancelled stream after restart");
    assert!(
        restarted
            .cancel_exchange(&ModelExchangeId(id("mdl", 6)), cancellation_metadata)
            .await
            .expect("durable cancellation replay")
            .replayed
    );
    assert_eq!(sink.terminations().len(), 1);
    assert_eq!(sink.released_count(), 1);
    assert_eq!(model_acks(&restarted_port).len(), 1);
}

#[tokio::test]
async fn cancellation_intent_recovers_after_sink_failure() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let cancellation_metadata = metadata(72, 13);
    sink.fail_next_terminate();
    {
        let mut client = WorkerModelPortClient::new(
            RecordingPort::default(),
            store.clone(),
            current.clone(),
            sink.clone(),
        );
        client
            .open(open_command(&authority, 7, 70, 'a'))
            .await
            .expect("open cancellation recovery stream");
        assert_eq!(
            client
                .cancel_exchange(
                    &ModelExchangeId(id("mdl", 7)),
                    cancellation_metadata.clone(),
                )
                .await
                .expect_err("inject sink termination failure")
                .code(),
            ModelPortClientErrorCode::CodexSink
        );
        assert_eq!(
            client
                .accept_chunk(&chunk(&authority, 7, 1, 'b', false, None), metadata(74, 14),)
                .await
                .expect_err("cancellation intent blocks later Provider chunks")
                .code(),
            ModelPortClientErrorCode::AlreadyTerminal
        );
        assert_eq!(
            client
                .handle_disconnect(&ModelExchangeId(id("mdl", 7)), metadata(75, 15), true)
                .await
                .expect_err("cancellation intent blocks resume")
                .code(),
            ModelPortClientErrorCode::AlreadyTerminal
        );
    }
    let intent = store.only_snapshot();
    assert!(intent.termination.is_none());
    assert_eq!(
        intent
            .cancellation
            .as_ref()
            .map(|cancellation| cancellation.phase),
        Some(ModelCancellationPhase::Intent)
    );

    let port = RecordingPort::default();
    let mut restarted =
        WorkerModelPortClient::new(port.clone(), store.clone(), current, sink.clone());
    restarted
        .open(open_command(&authority, 7, 73, 'a'))
        .await
        .expect("reopen intent after restart");
    assert!(
        restarted
            .cancel_exchange(&ModelExchangeId(id("mdl", 7)), cancellation_metadata)
            .await
            .expect("resume exact cancellation intent")
            .replayed
    );
    assert_eq!(
        store.only_snapshot().termination,
        Some(ModelTerminationReason::Cancelled)
    );
    assert_eq!(sink.terminations().len(), 1);
    assert_eq!(sink.released_count(), 1);
    assert_eq!(model_acks(&port).len(), 1);
}

#[tokio::test]
async fn cancellation_intent_recovers_after_terminal_cursor_failure() {
    let authority = authority(None);
    let current = CurrentAuthority::new(authority.clone());
    let store = MemoryCursorStore::default();
    let sink = RecordingSink::default();
    let cancellation_metadata = metadata(82, 13);
    store.fail_next_completion();
    {
        let mut client = WorkerModelPortClient::new(
            RecordingPort::default(),
            store.clone(),
            current.clone(),
            sink.clone(),
        );
        client
            .open(open_command(&authority, 8, 80, 'a'))
            .await
            .expect("open cursor recovery stream");
        assert_eq!(
            client
                .cancel_exchange(
                    &ModelExchangeId(id("mdl", 8)),
                    cancellation_metadata.clone(),
                )
                .await
                .expect_err("inject terminal cursor failure")
                .code(),
            ModelPortClientErrorCode::CursorStore
        );
    }
    assert_eq!(sink.terminations().len(), 1);
    assert_eq!(sink.released_count(), 0);
    assert_eq!(
        store
            .only_snapshot()
            .cancellation
            .as_ref()
            .map(|cancellation| cancellation.phase),
        Some(ModelCancellationPhase::Intent)
    );

    let port = RecordingPort::default();
    let mut restarted =
        WorkerModelPortClient::new(port.clone(), store.clone(), current, sink.clone());
    restarted
        .open(open_command(&authority, 8, 83, 'a'))
        .await
        .expect("reopen interrupted completion after restart");
    assert!(
        restarted
            .cancel_exchange(&ModelExchangeId(id("mdl", 8)), cancellation_metadata)
            .await
            .expect("complete persisted cancellation intent")
            .replayed
    );
    let applied = store.only_snapshot();
    assert_eq!(applied.termination, Some(ModelTerminationReason::Cancelled));
    assert_eq!(
        applied
            .cancellation
            .as_ref()
            .map(|cancellation| cancellation.phase),
        Some(ModelCancellationPhase::Applied)
    );
    assert_eq!(sink.terminations().len(), 1);
    assert_eq!(sink.released_count(), 1);
    assert_eq!(model_acks(&port).len(), 1);
}
