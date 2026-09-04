// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, VecDeque};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, ExecutionAckSequence, ExecutionEventId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId,
    RequestId, SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::action_gateway::{
    ExecutionEnvelope, ExecutionEnvelopeToken, GateDecision, GateInput, PreActionDecisionRecorder,
};
use winwincode_execution_port::action_normalizer::{
    ActionIntent, ActionObject, ActionOperation, ActionPurpose, ActionRisk, ActionScope,
    ActionSource, ObservedAction, ObservedFact,
};
use winwincode_execution_port::generated::{
    ArtifactKind, ArtifactReference, ExecutionEventCategory, ExecutionLeaseStamp,
    RuntimeReplayRequestMessage, RuntimeReplayRequestMessageKind,
};
use winwincode_execution_port::replay::{
    ReplayAcknowledgementStore, ReplayAuthority, ReplayFrame, ReplaySequence, ReplaySnapshot,
    ReplayStore, ReplayStreamKey,
};
use winwincode_execution_port::runtime_replay::RuntimeReplayIdentity;
use winwincode_execution_port::runtime_trace_outbox::{
    ExecutionMode, FencedArtifactWrite, ObserverMode, PerformanceBaselineReport,
    RuntimeTraceActionJournal, RuntimeTraceDraft, RuntimeTraceFact, RuntimeTraceIdentity,
    RuntimeTraceIdentitySource, RuntimeTraceInputError, RuntimeTraceOutboxError,
    RuntimeTracePayload, RuntimeTraceRetention, SecretSafeTraceSummary, StageTraceState,
    ToolTraceOutcome, TraceGateOutcome, WorkerArtifactCache, WorkerArtifactDraft,
    WorkerArtifactReferenceError, WorkerRuntimeTraceOutbox, WorkerRuntimeTraceState,
    persist_artifact_reference, traced_envelope_token,
};

const NOW: &str = "2027-01-15T08:00:02.000Z";

fn id(prefix: &str, suffix: char) -> String {
    format!("{prefix}_{}", suffix.to_string().repeat(26))
}

fn lease(fence: &str) -> ExecutionLeaseStamp {
    ExecutionLeaseStamp {
        attempt: 1,
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken(fence.to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
        job_id: ExecutionJobId(id("job", 'A')),
        lease_id: LeaseId(id("lse", 'A')),
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
    }
}

fn session_identity() -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: CodexThreadId(id("cdx", 'A')),
        product_session_id: ProductSessionId(id("psn", 'A')),
        stage_run_id: Some(StageRunId(id("run", 'A'))),
        worker_session_id: WorkerSessionId(id("wsn", 'A')),
    }
}

fn replay_identity(fence: &str) -> RuntimeReplayIdentity {
    RuntimeReplayIdentity {
        lease: lease(fence),
        worker_session_id: WorkerSessionId(id("wsn", 'A')),
        session_identity: session_identity(),
        codex_thread_id: CodexThreadId(id("cdx", 'A')),
    }
}

fn trace_identity(sequence: i64, message_suffix: char) -> RuntimeTraceIdentity {
    let event_offset = u32::try_from(sequence - 1).expect("positive fixture sequence");
    RuntimeTraceIdentity {
        lease: lease("7"),
        worker_session_id: WorkerSessionId(id("wsn", 'A')),
        session_identity: session_identity(),
        message_id: ExecutionMessageId(id("xmsg", message_suffix)),
        event_id: ExecutionEventId(id(
            "xevt",
            char::from_u32(u32::from('A') + event_offset).expect("suffix"),
        )),
        sequence: ExecutionSequence(sequence),
        occurred_at: Instant(format!("2027-01-15T08:00:{sequence:02}.000Z")),
        sent_at: Instant(format!("2027-01-15T08:01:{sequence:02}.000Z")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorityError {
    Stale,
    Foreign,
}

#[derive(Debug, Clone)]
struct FixtureAuthority {
    expected: RuntimeReplayIdentity,
    stale: bool,
}

impl ReplayAuthority for FixtureAuthority {
    type Context = RuntimeReplayIdentity;
    type Error = AuthorityError;

    fn validate_active_lease(
        &self,
        stream: &ReplayStreamKey,
        identity: &Self::Context,
    ) -> Result<(), Self::Error> {
        if self.stale {
            return Err(AuthorityError::Stale);
        }
        if identity != &self.expected || stream != &self.expected.stream_key() {
            return Err(AuthorityError::Foreign);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct MemoryStore {
    snapshots: BTreeMap<ReplayStreamKey, ReplaySnapshot>,
    loads: usize,
    writes: usize,
}

impl ReplayStore for MemoryStore {
    type Error = &'static str;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.loads += 1;
        Ok(self.snapshots.get(stream).cloned())
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let snapshot = self.snapshots.entry(stream.clone()).or_default();
        if snapshot.highest_sequence != expected_highest_sequence {
            return Err("highest changed");
        }
        snapshot.events.push(frame.clone());
        snapshot.highest_sequence = frame.sequence;
        self.writes += 1;
        Ok(())
    }
}

impl ReplayAcknowledgementStore for MemoryStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        ack_sequence: ReplaySequence,
    ) -> Result<(), Self::Error> {
        let snapshot = self.snapshots.get_mut(stream).ok_or("stream missing")?;
        if snapshot.ack_sequence != expected_ack_sequence {
            return Err("ack changed");
        }
        snapshot.ack_sequence = ack_sequence;
        self.writes += 1;
        Ok(())
    }
}

fn authority() -> FixtureAuthority {
    FixtureAuthority {
        expected: replay_identity("7"),
        stale: false,
    }
}

fn draft(sequence: i64, fact: RuntimeTraceFact) -> RuntimeTraceDraft {
    let message_offset = u32::try_from(sequence - 1).expect("positive fixture sequence");
    let message_suffix = char::from_u32(u32::from('A') + message_offset).expect("suffix");
    RuntimeTraceDraft {
        identity: trace_identity(sequence, message_suffix),
        category: ExecutionEventCategory::Activity,
        summary: SecretSafeTraceSummary::new(format!("typed runtime fact {sequence}"))
            .expect("summary"),
        fact,
        artifacts: Vec::new(),
    }
}

fn decode_base64(value: &str) -> Vec<u8> {
    fn sextet(byte: u8) -> u8 {
        match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    }

    let mut decoded = Vec::new();
    for chunk in value.as_bytes().chunks(4) {
        let a = sextet(chunk[0]);
        let b = sextet(chunk[1]);
        let c = sextet(chunk[2]);
        let d = sextet(chunk[3]);
        decoded.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            decoded.push(((b & 0b0000_1111) << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            decoded.push(((c & 0b0000_0011) << 6) | d);
        }
    }
    decoded
}

#[derive(Debug, Clone)]
struct StoredArtifact {
    identity: RuntimeReplayIdentity,
    artifact_id: ArtifactId,
    digest: Sha256Digest,
    size_bytes: i64,
    content: Vec<u8>,
}

#[derive(Default)]
struct RecordingArtifactCache {
    writes: Vec<StoredArtifact>,
}

impl WorkerArtifactCache for RecordingArtifactCache {
    type Error = &'static str;

    fn store(&mut self, write: FencedArtifactWrite<'_>) -> Result<(), Self::Error> {
        self.writes.push(StoredArtifact {
            identity: write.identity.clone(),
            artifact_id: write.descriptor.artifact_id.clone(),
            digest: write.descriptor.digest.clone(),
            size_bytes: write.descriptor.size_bytes,
            content: write.content.to_vec(),
        });
        Ok(())
    }
}

#[test]
fn large_content_goes_to_the_existing_artifact_cache_and_trace_rows_hold_only_references() {
    let raw = b"large command output with TOKEN-not-written-to-trace-row";
    let mut cache = RecordingArtifactCache::default();
    let identity = replay_identity("7");
    let reference = persist_artifact_reference(
        &mut cache,
        &authority(),
        &identity,
        WorkerArtifactDraft {
            artifact_id: ArtifactId(id("art", 'A')),
            kind: ArtifactKind::CommandOutput,
            media_type: "text/plain".to_owned(),
            file_name: Some("command-output.txt".to_owned()),
            content: raw,
        },
    )
    .expect("fenced Artifact write");

    assert_eq!(cache.writes.len(), 1);
    assert_eq!(cache.writes[0].identity, identity);
    assert_eq!(cache.writes[0].artifact_id, reference.artifact_id);
    assert_eq!(cache.writes[0].digest, reference.digest);
    assert_eq!(
        cache.writes[0].size_bytes,
        i64::try_from(raw.len()).expect("fixture size")
    );
    assert_eq!(cache.writes[0].content, raw);
    assert_eq!(
        reference.digest.0,
        format!("sha256:{:x}", Sha256::digest(raw))
    );

    let mut store = MemoryStore::default();
    let outbox = WorkerRuntimeTraceOutbox::new();
    let mut trace = draft(
        1,
        RuntimeTraceFact::Tool {
            source: ActionSource::Shell,
            outcome: ToolTraceOutcome::Succeeded,
        },
    );
    trace.artifacts.push(reference.clone());
    let retained = outbox
        .retain(&mut store, &authority(), trace)
        .expect("trace retention");
    let RuntimeTraceRetention::Ready { message, .. } = retained else {
        panic!("trace must be ready");
    };
    let row = &store.snapshots.values().next().expect("snapshot").events[0].frame;
    assert!(!row.windows(raw.len()).any(|window| window == raw));
    let encoded = message.event.payload.expect("trace payload");
    let decoded = decode_base64(&encoded.data_base64);
    let payload: RuntimeTracePayload = serde_json::from_slice(&decoded).expect("typed payload");
    assert_eq!(payload.artifacts, [reference]);
    assert!(!decoded.windows(raw.len()).any(|window| window == raw));
    assert_eq!(
        encoded.payload_digest.0,
        format!("sha256:{:x}", Sha256::digest(decoded))
    );
}

fn action_trace_fixture() -> (ExecutionEnvelope<()>, ActionIntent, ObservedAction) {
    (
        ExecutionEnvelope {
            token: ExecutionEnvelopeToken {
                version: 3,
                digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            },
            policy: (),
        },
        ActionIntent {
            object: ActionObject::ProductionCode,
            operation: ActionOperation::Modify,
            intent: ActionPurpose::Implement,
            scope: ActionScope::Local,
            targets: vec!["crates/kernel/src/lib.rs".to_owned()],
            requirement_refs: vec!["REQ-1".to_owned()],
            plan_refs: vec!["PLAN-1".to_owned()],
            expected_effect: "implement current requirement".to_owned(),
            scope_delta: None,
            rollback: Some("restore prior version".to_owned()),
            executor_risk: ActionRisk::Medium,
        },
        ObservedAction {
            source: ActionSource::File,
            objects: vec![ActionObject::ProductionCode],
            operation: ActionOperation::Modify,
            scope: ActionScope::Local,
            targets: vec!["crates/kernel/src/lib.rs".to_owned()],
            facts: vec![ObservedFact::FilePath],
            minimum_risk: ActionRisk::Medium,
        },
    )
}

#[test]
fn stage_action_gate_tool_candidate_and_runtime_facts_remain_typed_and_ordered() {
    let (envelope, intent, observed) = action_trace_fixture();
    let action_draft = RuntimeTraceDraft::gateway_action(
        trace_identity(2, 'B'),
        &GateInput {
            envelope: &envelope,
            intent: &intent,
            observed: &observed,
        },
    )
    .expect("action draft");
    let gate_draft = RuntimeTraceDraft::gateway_decision(
        trace_identity(3, 'C'),
        &GateInput {
            envelope: &envelope,
            intent: &intent,
            observed: &observed,
        },
        &GateDecision::Allow,
    )
    .expect("gate draft");
    assert_eq!(
        traced_envelope_token(&gate_draft.fact),
        Some(envelope.token.clone())
    );

    let facts = vec![
        draft(
            1,
            RuntimeTraceFact::Stage {
                state: StageTraceState::Started,
            },
        ),
        action_draft,
        gate_draft,
        draft(
            4,
            RuntimeTraceFact::Tool {
                source: ActionSource::File,
                outcome: ToolTraceOutcome::Succeeded,
            },
        ),
        draft(
            5,
            RuntimeTraceFact::Candidate {
                digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            },
        ),
        draft(
            6,
            RuntimeTraceFact::Runtime {
                state: WorkerRuntimeTraceState::Checkpointed,
            },
        ),
    ];
    let outbox = WorkerRuntimeTraceOutbox::new();
    let mut store = MemoryStore::default();
    for trace in facts {
        assert!(matches!(
            outbox.retain(&mut store, &authority(), trace),
            Ok(RuntimeTraceRetention::Ready {
                duplicate: false,
                ..
            })
        ));
    }
    let snapshot = store.snapshots.values().next().expect("snapshot");
    assert_eq!(snapshot.highest_sequence, 6);
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn duplicate_retry_and_restart_replay_return_the_original_message() {
    let outbox = WorkerRuntimeTraceOutbox::new();
    let mut store = MemoryStore::default();
    let first = outbox
        .retain(
            &mut store,
            &authority(),
            draft(
                1,
                RuntimeTraceFact::Runtime {
                    state: WorkerRuntimeTraceState::Started,
                },
            ),
        )
        .expect("first retain");
    let RuntimeTraceRetention::Ready {
        message: original, ..
    } = first
    else {
        panic!("first trace must be ready");
    };

    let mut retry = draft(
        1,
        RuntimeTraceFact::Runtime {
            state: WorkerRuntimeTraceState::Started,
        },
    );
    retry.identity.message_id = ExecutionMessageId(id("xmsg", 'B'));
    let duplicate = outbox
        .retain(&mut store, &authority(), retry)
        .expect("duplicate retain");
    let RuntimeTraceRetention::Ready {
        message, duplicate, ..
    } = duplicate
    else {
        panic!("duplicate trace must be ready");
    };
    assert!(duplicate);
    assert_eq!(message, original);
    assert_eq!(store.writes, 1);

    let restarted_outbox = WorkerRuntimeTraceOutbox::new();
    let replay = restarted_outbox
        .resume(
            &mut store,
            &authority(),
            &RuntimeReplayRequestMessage {
                after_sequence: ExecutionAckSequence(0),
                kind: RuntimeReplayRequestMessageKind::RuntimeReplayRequest,
                lease: lease("7"),
                max_events: 100,
                message_id: ExecutionMessageId(id("xmsg", 'C')),
                request_id: RequestId(id("req", 'A')),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: Instant(NOW.to_owned()),
                session_identity: session_identity(),
                worker_session_id: WorkerSessionId(id("wsn", 'A')),
            },
        )
        .expect("restart replay");
    assert_eq!(replay.events, [*original]);
}

#[test]
fn performance_baseline_is_bounded_secret_safe_and_replayable() {
    assert_eq!(ExecutionMode::default(), ExecutionMode::React);
    assert_eq!(ObserverMode::default(), ObserverMode::Off);
    assert_eq!(
        ExecutionMode::from_config("delegated_patch_shadow"),
        Some(ExecutionMode::DelegatedPatchShadow)
    );
    assert_eq!(
        ObserverMode::from_config("ambiguous_only"),
        Some(ObserverMode::AmbiguousOnly)
    );
    assert_eq!(ExecutionMode::from_config("unknown"), None);
    assert_eq!(ObserverMode::from_config("unknown"), None);
    let report = PerformanceBaselineReport {
        execution_mode: ExecutionMode::React,
        observer_mode: ObserverMode::Off,
        primary_model_call_count: 2,
        primary_model_input_tokens: 100,
        primary_model_cached_tokens: 25,
        primary_model_output_tokens: 30,
        primary_model_wait_ms: 900,
        tool_call_count: 3,
        patch_call_count: 1,
        patch_apply_ms: 40,
        files_changed: 2,
        validation_ms: 80,
        observer_call_count: 0,
        observer_wait_ms: 0,
        repair_rounds: 0,
        turn_count: 2,
        total_runtime_ms: 1_500,
    };
    let outbox = WorkerRuntimeTraceOutbox::new();
    let mut store = MemoryStore::default();
    let retained = outbox
        .retain(
            &mut store,
            &authority(),
            draft(
                1,
                RuntimeTraceFact::PerformanceBaseline {
                    report: report.clone(),
                },
            ),
        )
        .expect("retain performance baseline");
    let RuntimeTraceRetention::Ready { message, .. } = retained else {
        panic!("performance baseline must be ready");
    };
    let encoded = message.event.payload.expect("performance payload");
    let decoded = decode_base64(&encoded.data_base64);
    let payload: RuntimeTracePayload = serde_json::from_slice(&decoded).expect("typed payload");
    assert_eq!(
        payload.fact,
        RuntimeTraceFact::PerformanceBaseline {
            report: report.clone()
        }
    );
    let json = String::from_utf8(decoded).expect("JSON payload");
    assert!(!json.contains("Authorization"));
    assert!(!json.contains("sourceCode"));
    assert!(!json.contains("patchContent"));

    let mut invalid = report;
    invalid.primary_model_cached_tokens = -1;
    let error = outbox
        .retain(
            &mut MemoryStore::default(),
            &authority(),
            draft(1, RuntimeTraceFact::PerformanceBaseline { report: invalid }),
        )
        .expect_err("negative metric must be rejected");
    assert!(matches!(
        error,
        RuntimeTraceOutboxError::Input(RuntimeTraceInputError::InvalidIdentity)
    ));
}

#[test]
fn stale_fencing_token_causes_zero_outbox_and_artifact_writes() {
    let stale = FixtureAuthority {
        expected: replay_identity("8"),
        stale: true,
    };
    let mut store = MemoryStore::default();
    let result = WorkerRuntimeTraceOutbox::new().retain(
        &mut store,
        &stale,
        draft(
            1,
            RuntimeTraceFact::Runtime {
                state: WorkerRuntimeTraceState::Started,
            },
        ),
    );
    assert!(result.is_err());
    assert_eq!(store.loads, 0);
    assert_eq!(store.writes, 0);

    let mut cache = RecordingArtifactCache::default();
    let artifact = persist_artifact_reference(
        &mut cache,
        &stale,
        &replay_identity("7"),
        WorkerArtifactDraft {
            artifact_id: ArtifactId(id("art", 'A')),
            kind: ArtifactKind::Log,
            media_type: "text/plain".to_owned(),
            file_name: None,
            content: b"not written",
        },
    );
    assert_eq!(
        artifact,
        Err(WorkerArtifactReferenceError::Authority(
            AuthorityError::Stale
        ))
    );
    assert!(cache.writes.is_empty());
}

#[test]
fn unsafe_summaries_and_changed_artifact_references_fail_before_store_access() {
    assert_eq!(
        SecretSafeTraceSummary::new("Authorization: Bearer TOKEN"),
        Err(RuntimeTraceInputError::UnsafeSummary)
    );
    let mut store = MemoryStore::default();
    let mut trace = draft(
        1,
        RuntimeTraceFact::Stage {
            state: StageTraceState::Started,
        },
    );
    trace.artifacts = vec![
        ArtifactReference {
            artifact_id: ArtifactId(id("art", 'A')),
            digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        },
        ArtifactReference {
            artifact_id: ArtifactId(id("art", 'A')),
            digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        },
    ];
    let error = WorkerRuntimeTraceOutbox::new()
        .retain(&mut store, &authority(), trace)
        .expect_err("conflicting reference must fail");
    assert!(format!("{error}").contains("conflicting Artifact references"));
    assert_eq!(store.loads, 0);
    assert_eq!(store.writes, 0);
}

#[derive(Debug)]
struct FixtureIdentities(VecDeque<RuntimeTraceIdentity>);

impl RuntimeTraceIdentitySource for FixtureIdentities {
    type Error = &'static str;

    fn next_identity(&mut self) -> Result<RuntimeTraceIdentity, Self::Error> {
        self.0.pop_front().ok_or("identity exhausted")
    }
}

#[test]
fn the_concrete_gateway_journal_durably_retains_the_decision() {
    let envelope = ExecutionEnvelope {
        token: ExecutionEnvelopeToken {
            version: 4,
            digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
        },
        policy: (),
    };
    let intent = ActionIntent {
        object: ActionObject::ExternalResource,
        operation: ActionOperation::Execute,
        intent: ActionPurpose::Verify,
        scope: ActionScope::External,
        targets: vec!["mcp://fixture/read".to_owned()],
        requirement_refs: vec!["REQ-1".to_owned()],
        plan_refs: vec!["PLAN-1".to_owned()],
        expected_effect: "read current fixture".to_owned(),
        scope_delta: None,
        rollback: None,
        executor_risk: ActionRisk::Medium,
    };
    let observed = ObservedAction {
        source: ActionSource::Mcp,
        objects: vec![ActionObject::ExternalResource],
        operation: ActionOperation::Execute,
        scope: ActionScope::External,
        targets: vec!["mcp://fixture/read".to_owned()],
        facts: vec![ObservedFact::McpCapability],
        minimum_risk: ActionRisk::Medium,
    };
    let mut journal = RuntimeTraceActionJournal::new(
        MemoryStore::default(),
        authority(),
        FixtureIdentities(VecDeque::from([
            trace_identity(1, 'A'),
            trace_identity(2, 'B'),
        ])),
    );
    journal
        .record(
            GateInput {
                envelope: &envelope,
                intent: &intent,
                observed: &observed,
            },
            &GateDecision::PauseForHuman {
                reason: "external approval".to_owned(),
            },
        )
        .expect("journal decision");
    let (store, _, _) = journal.into_parts();
    assert_eq!(store.writes, 2);
    let events = &store.snapshots.values().next().expect("snapshot").events;
    let action_message: winwincode_execution_port::generated::RuntimeEventMessage =
        serde_json::from_slice(&events[0].frame).expect("action message");
    let action_payload: RuntimeTracePayload = serde_json::from_slice(&decode_base64(
        &action_message.event.payload.expect("payload").data_base64,
    ))
    .expect("action payload");
    assert!(matches!(
        action_payload.fact,
        RuntimeTraceFact::Action { .. }
    ));
    let message: winwincode_execution_port::generated::RuntimeEventMessage =
        serde_json::from_slice(&events[1].frame).expect("gate message");
    let payload: RuntimeTracePayload = serde_json::from_slice(&decode_base64(
        &message.event.payload.expect("payload").data_base64,
    ))
    .expect("payload");
    assert!(matches!(
        payload.fact,
        RuntimeTraceFact::Gate {
            decision: TraceGateOutcome::PauseForHuman,
            ..
        }
    ));
}
