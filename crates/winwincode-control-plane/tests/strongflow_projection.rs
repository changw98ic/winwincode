// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use sha2::{Digest, Sha256};
#[path = "../../../tests/support/git_candidate.rs"]
mod git_candidate;
use git_candidate::candidate_bundle;
use winwincode_api::generated::{
    Actor, CandidateFileContentGetParameters, CandidateFileContentGetQuery,
    CandidateFileContentGetQueryQuery, ControlPlaneWebSocketSubscribeStartAt,
    DeliveryGetParameters, DeliveryGetQuery, DeliveryGetQueryQuery, EventReadCursor,
    EvidenceGetQuery, EvidenceGetQueryQuery, EvidenceReadBinding, PageRequest,
    ProductSessionRuntimeProjectionGetParameters, ProductSessionRuntimeProjectionGetParametersKind,
    QueryResultResponse, RuntimeProjectionEventCursor, RuntimeProjectionGetParameters,
    RuntimeProjectionGetQuery, RuntimeProjectionGetQueryQuery, StrongFlowReadCursor,
    WorkRunRuntimeProjectionGetParameters, WorkRunRuntimeProjectionGetParametersKind,
};
use winwincode_control_plane::{
    AggregateJournalKey, AggregateJournalRecord, CollaborationCandidateIdentity,
    CollaborationInboxAudience, CollaborationInboxAuthorityError, CollaborationInboxAuthorityPort,
    CollaborationInboxAuthoritySnapshot, CollaborationInboxClock, CollaborationInboxClockError,
    CollaborationInboxCommandContext, CollaborationInboxItemId, CollaborationInboxItemKind,
    CollaborationInboxItemState, CollaborationInboxService, CollaborationInboxSourceError,
    CollaborationInboxSourceItem, CollaborationInboxSourcePort, CollaborationInboxSourceSnapshot,
    CollaborationResponsibilityEntitlement, CommitReceipt, ControlPlane, EventPublishError,
    EventPublisher, FormalCollaborationCommandRoute, LoadedAggregateJournal, NewOutboxEvent,
    OutboxEvent, PageAnnotationAction, PageAnnotationCandidateIdentity, PageAnnotationCommand,
    PageAnnotationId, PageAnnotationRegion, PageAnnotationTarget, PageAnnotationViewport,
    ProductStateStorage, ProjectionEventCursor, ProjectionEventStream, ProjectionEventStreamKey,
    ResponsibilityAssignment, ResponsibilityAssignmentId, ResponsibilityAssignmentState,
    ResponsibilityRole, ResponsibilityTarget, StorageError, StoredState,
    strongflow_projection::{
        DeliveryRuntimeReadRequest, ProductSessionRuntimeReadRequest,
        SqliteTrustedRuntimeProjectionAdapter, StrongFlowProjectionError,
        StrongFlowProjectionQueryPort, StrongFlowProjectionSources, TrustedProjectionReadError,
        TrustedPublicationProjectionAdapter, TrustedPublicationProjectionRead,
        TrustedRuntimeProjectionAdapter, TrustedRuntimeProjectionRead,
        TrustedRuntimeProjectionReadCut, TrustedRuntimeProjectionReadCutReader,
    },
};
use winwincode_delivery::{
    application::attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
    application::verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
    application::workrun_execution::{
        TerminalArtifactReference, TerminalOutcomeStatus,
        test_support::{
            active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
            terminal_outcome_metadata, terminal_worker_outcome,
        },
    },
    domain::{
        AcceptanceCriterionId, AttentionItem, AttentionItemStatus, AttentionItemType,
        CandidatePathFact, CandidatePathState, CriterionVerdict, DELIVERY_SCHEMA_VERSION, Delivery,
        DeliveryStatus, EvidenceId, EvidenceRef, EvidenceRefType, FrozenDeliveryCandidate,
        RepositoryKind, RepositoryRef, SessionBindingId,
        candidate::{
            CandidateHunkFact, freeze_delivery_candidate_from_source,
            test_support::{CandidateFixtureInput, freeze_candidate_fixture},
        },
        rework::{CurrentReworkScope, ReworkDecision, decide_precise_rework},
    },
    projection::runtime::{
        RuntimeProjection,
        test_support::{
            RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
        },
    },
    store::{
        AppendDelivery, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalCodec,
        DeliveryJournalPort, DeliveryMutationOperation, DeliveryReworkHistoryPort, DeliveryStore,
        InMemoryDeliveryJournal,
    },
};
use winwincode_domain::{
    AgentIdentityId, ArtifactId, AttentionItemId, CodexThreadId, ControlPlaneEventId, DeliveryId,
    ExecutionAckSequence, ExecutionEventId, ExecutionJobId, ExecutionMessageId, ExecutionSequence,
    FencingToken, Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, Sha256Digest, UserId, WorkItemState, WorkRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::generated::{ExecutionEventCategory, ExecutionEventRecord};
use winwincode_storage::{
    AggregateJournalPublication, ArtifactAccess, ArtifactChunk, ArtifactMeteringAttribution,
    ArtifactOpen, ArtifactProvenance, ArtifactRetention, ArtifactStore, FakeArtifactObjectStore,
    GitCandidateArtifactManifest, LocalArtifactObjectStore, LocalGitSourceResolver,
    ProjectionReadCut, ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, StateMutation,
    StateRevisionGuard, WorkRunDeviceBindingFacts,
};

static NEXT_POSITIVE_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PositiveTerminalAuthority {
    schema_version: u8,
    delivery_id: DeliveryId,
    work_run_id: WorkRunId,
    job_id: ExecutionJobId,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    issued_at: Instant,
    expires_at: Instant,
    artifacts: Vec<TerminalArtifactReference>,
    codex_thread_id: Option<CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    status: &'static str,
    disposition: PositiveTerminalDisposition,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PositiveTerminalDisposition {
    Settled { delivery_revision: u64 },
}

// PublicationAuthorizationSnapshot is deliberately not constructible from HTTP input.
// Missing sources return TRUSTED_FACTS_UNAVAILABLE.
// WebSocket `runtime-projection.invalidated.v1` is only an invalidation; these reads
// expose complete committed snapshots.

#[derive(Clone)]
struct JournalStorage {
    journal: Arc<Mutex<LoadedAggregateJournal>>,
    states: Arc<Mutex<std::collections::BTreeMap<String, StoredState>>>,
    runtime_read_count: Arc<Mutex<usize>>,
    advance_event_after_runtime_read: bool,
    device_binding: Arc<Mutex<Option<(ExecutionJobId, WorkRunDeviceBindingFacts)>>>,
}
impl ProductStateStorage for JournalStorage {
    fn commit_adapter(&mut self, _commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        Err(StorageError::adapter("read-only test storage"))
    }
    fn load_receipt(
        &self,
        _identity: &ReceiptIdentity,
        _digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        Ok(None)
    }
    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        Ok(self.states.lock().expect("states").get(stream_id).cloned())
    }
    fn load_work_run_device_binding_facts(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<WorkRunDeviceBindingFacts>, StorageError> {
        Ok(self
            .device_binding
            .lock()
            .expect("device binding")
            .as_ref()
            .filter(|(expected, _)| expected == job_id)
            .map(|(_, facts)| facts.clone()))
    }
    fn load_projection_read_cut(
        &self,
        _state_stream_ids: &[String],
        _key: &ProjectionEventStreamKey,
        _expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionReadCut, StorageError> {
        Err(StorageError::adapter(
            "read-only test storage has no SQLite read cut",
        ))
    }
    fn load_journal(
        &self,
        _key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        Ok(Some(self.journal.lock().expect("journal").clone()))
    }
    fn load_projection_event_cursor(
        &self,
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionEventCursor, StorageError> {
        let sequence = if self.advance_event_after_runtime_read
            && *self.runtime_read_count.lock().expect("runtime read count") > 0
        {
            2
        } else {
            1
        };
        let event_id = match key.stream() {
            ProjectionEventStream::Scope | ProjectionEventStream::Lease { .. } => {
                return Err(StorageError::invalid_input(
                    "StrongFlow fixture received a non-StrongFlow event stream",
                ));
            }
            ProjectionEventStream::Delivery(_) => {
                format!("evt_delivery_fixture_{sequence:04}")
            }
            ProjectionEventStream::ProductSession(_) => {
                format!("evt_product_session_fixture_{sequence:04}")
            }
        };
        let current = ProjectionEventCursor::try_new(
            key.clone(),
            sequence,
            Some(ControlPlaneEventId(event_id)),
        )?;
        if let Some(expected) = expected {
            let expected_id = match key.stream() {
                ProjectionEventStream::Scope | ProjectionEventStream::Lease { .. } => {
                    return Err(StorageError::invalid_input(
                        "StrongFlow fixture received a non-StrongFlow event stream",
                    ));
                }
                ProjectionEventStream::Delivery(_) => {
                    format!("evt_delivery_fixture_{:04}", expected.sequence())
                }
                ProjectionEventStream::ProductSession(_) => {
                    format!("evt_product_session_fixture_{:04}", expected.sequence())
                }
            };
            if expected.key() != key
                || expected.sequence() == 0
                || expected.sequence() > current.sequence()
                || expected.event_id().map(|event_id| event_id.0.as_str())
                    != Some(expected_id.as_str())
            {
                return Err(StorageError::invalid_input(
                    "projection event cursor mismatch",
                ));
            }
            return Ok(expected.clone());
        }
        Ok(current)
    }
    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        Ok(Vec::new())
    }
    fn mark_published(&mut self, _event_id: &str) -> Result<(), StorageError> {
        Ok(())
    }
    fn close(self: Box<Self>) -> Result<(), StorageError> {
        Ok(())
    }
}

#[derive(Clone)]
struct E2eInboxSource(CollaborationInboxSourceSnapshot);

impl CollaborationInboxSourcePort for E2eInboxSource {
    fn snapshot(
        &mut self,
        _scope: &RepositoryScope,
    ) -> Result<CollaborationInboxSourceSnapshot, CollaborationInboxSourceError> {
        Ok(self.0.clone())
    }
}

#[derive(Clone)]
struct E2eInboxAuthority(CollaborationInboxAuthoritySnapshot);

impl CollaborationInboxAuthorityPort for E2eInboxAuthority {
    fn authorize(
        &mut self,
        _actor: &Actor,
        _authenticated_scopes: &[winwincode_api::generated::Scope],
        _scope: &RepositoryScope,
        _audience: &CollaborationInboxAudience,
    ) -> Result<CollaborationInboxAuthoritySnapshot, CollaborationInboxAuthorityError> {
        Ok(self.0.clone())
    }
}

struct E2eInboxClock;

impl CollaborationInboxClock for E2eInboxClock {
    fn now_millis(&mut self) -> Result<u64, CollaborationInboxClockError> {
        Ok(1_800_000_000_100)
    }
}
struct NoopPublisher;
impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct UnavailablePublicationAdapter;
impl TrustedPublicationProjectionAdapter for UnavailablePublicationAdapter {
    fn read_current(
        &self,
        _scope: &RepositoryScope,
        _delivery_id: &DeliveryId,
        _delivery_revision: u64,
        _expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError> {
        Err(TrustedProjectionReadError::Unavailable)
    }
}

#[derive(Clone)]
struct RuntimeAdapter {
    read: Arc<Mutex<TrustedRuntimeProjectionRead>>,
    race: Arc<Mutex<bool>>,
    read_count: Arc<Mutex<usize>>,
    expire_after_reads: Option<usize>,
    unavailable: bool,
    atomic_read_cut: bool,
    delivery_event_cursor: Option<ProjectionEventCursor>,
    product_session_event_cursor: Option<ProjectionEventCursor>,
}
impl TrustedRuntimeProjectionAdapter for RuntimeAdapter {
    fn provides_atomic_read_cut(&self) -> bool {
        self.atomic_read_cut
    }

    fn read_delivery_with_storage(
        &self,
        _storage: &dyn ProductStateStorage,
        request: &DeliveryRuntimeReadRequest,
        _delivery: &Delivery,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        self.read_delivery(request)
    }

    fn read_product_session_with_storage(
        &self,
        _storage: &dyn ProductStateStorage,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        self.read_product_session(request)
    }

    fn read_delivery(
        &self,
        request: &DeliveryRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        let mut read_count = self.read_count.lock().expect("read count");
        if self
            .expire_after_reads
            .is_some_and(|threshold| *read_count >= threshold)
        {
            return Err(TrustedProjectionReadError::ExactCutNotRetained);
        }
        *read_count += 1;
        drop(read_count);
        if self.unavailable {
            return Err(TrustedProjectionReadError::Unavailable);
        }
        let read = self.read.lock().expect("runtime read");
        if read.snapshot().delivery_id.as_ref() != Some(request.delivery_id())
            || request.delivery_revision() != read.delivery_revision()
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        if request.expected().is_some_and(|expected| {
            expected.ledger_revision() != read.ledger_revision()
                || expected.accepted_sequence() != read.accepted_sequence()
        }) {
            return Err(TrustedProjectionReadError::Stale);
        }
        let mut raced = self.race.lock().expect("race");
        if *raced {
            return Err(TrustedProjectionReadError::Stale);
        }
        if request.expected().is_none()
            && request.scope().repository_id.0 == "rep_00000000000000000000000002"
        {
            *raced = true;
        }
        let mut read = read.clone();
        if let Some(cursor) = &self.delivery_event_cursor {
            read = read.with_event_cursor(cursor.clone());
        }
        Ok(read)
    }
    fn read_product_session(
        &self,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        *self.read_count.lock().expect("read count") += 1;
        let product_session_id = request.product_session_id();
        if self.unavailable
            || !self
                .read
                .lock()
                .expect("runtime read")
                .snapshot()
                .sessions
                .iter()
                .any(|s| &s.product_session_id == product_session_id)
        {
            Err(TrustedProjectionReadError::Unavailable)
        } else {
            let read = self.read.lock().expect("runtime read");
            if request.scope() != read.scope()
                || request.expected().is_some_and(|expected| {
                    expected.ledger_revision() != read.ledger_revision()
                        || expected.accepted_sequence() != read.accepted_sequence()
                })
            {
                return Err(TrustedProjectionReadError::Stale);
            }
            let mut read = read.clone();
            if let Some(cursor) = &self.product_session_event_cursor {
                read = read.with_event_cursor(cursor.clone());
            }
            Ok(read)
        }
    }
}

struct RuntimeCutReader {
    adapter: RuntimeAdapter,
}

impl TrustedRuntimeProjectionReadCutReader for RuntimeCutReader {
    fn read_delivery_cut(
        &self,
        _storage: &dyn ProductStateStorage,
        request: &DeliveryRuntimeReadRequest,
        _delivery: &Delivery,
    ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError> {
        let runtime = self.adapter.read_delivery(request)?;
        let cursor = self
            .adapter
            .delivery_event_cursor
            .clone()
            .ok_or(TrustedProjectionReadError::Invalid)?;
        Ok(TrustedRuntimeProjectionReadCut::new(runtime, cursor))
    }

    fn read_product_session_cut(
        &self,
        _storage: &dyn ProductStateStorage,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionReadCut, TrustedProjectionReadError> {
        let runtime = self.adapter.read_product_session(request)?;
        let cursor = self
            .adapter
            .product_session_event_cursor
            .clone()
            .ok_or(TrustedProjectionReadError::Invalid)?;
        Ok(TrustedRuntimeProjectionReadCut::new(runtime, cursor))
    }
}

#[derive(Clone)]
struct PublicationAdapter {
    read: TrustedPublicationProjectionRead,
    unavailable: bool,
}
impl TrustedPublicationProjectionAdapter for PublicationAdapter {
    fn read_current(
        &self,
        _scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        delivery_revision: u64,
        expected: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError> {
        if self.unavailable {
            return Err(TrustedProjectionReadError::Unavailable);
        }
        if delivery_id.0 != "dlv_01J00000000000000000000000"
            || delivery_revision != self.read.delivery_revision()
            || expected.is_some_and(|value| value != self.read.publication_revision())
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        Ok(self.read.clone())
    }
}

struct Fixture {
    control_plane: ControlPlane,
    delivery: Delivery,
    scope: RepositoryScope,
    journal: Arc<Mutex<LoadedAggregateJournal>>,
    states: Arc<Mutex<std::collections::BTreeMap<String, StoredState>>>,
    domain_journal: Arc<InMemoryDeliveryJournal>,
    runtime: Arc<Mutex<TrustedRuntimeProjectionRead>>,
    device_binding: Arc<Mutex<Option<(ExecutionJobId, WorkRunDeviceBindingFacts)>>>,
}

#[derive(Clone, Copy)]
enum EventCursorBehavior {
    Stable,
    AdvanceAfterRuntimeRead,
}

fn fixture(runtime_unavailable: bool, publication_unavailable: bool, race: bool) -> Fixture {
    fixture_with_delivery(
        delivery_fixture(false),
        runtime_unavailable,
        publication_unavailable,
        race,
        None,
    )
}

fn runtime_projection_for(delivery: &Delivery) -> RuntimeProjection {
    let Some(session) = delivery
        .snapshot()
        .session_bindings
        .first()
        .filter(|binding| binding.worker_session_id.is_some() && binding.codex_thread_id.is_some())
    else {
        return RuntimeProjection::new(delivery, Vec::new()).expect("empty runtime");
    };
    let binding = accepted_binding(
        delivery,
        &session.id,
        RuntimeAuthorityFixture {
            lease_id: session.lease_id.clone().expect("accepted lease"),
            fencing_token: session.fencing_token.clone().expect("accepted fence"),
            worker_id: session.worker_id.clone().expect("accepted Worker"),
            worker_instance_id: session
                .worker_instance_id
                .clone()
                .expect("accepted Worker instance"),
        },
        delivery
            .snapshot()
            .work_run_aggregate
            .runs
            .iter()
            .find(|run| run.id == session.work_run_id)
            .filter(|run| {
                matches!(
                    run.state,
                    winwincode_domain::WorkRunState::Settled
                        | winwincode_domain::WorkRunState::Failed
                        | winwincode_domain::WorkRunState::Cancelled
                )
            })
            .map(|_| 1),
    )
    .expect("binding");
    let event = accepted_event(
        &binding,
        1,
        "event-runtime-1",
        RuntimeFactFixture::LiveDiff {
            changed_file_count: 2,
            additions: 7,
            deletions: 3,
            source_ref: "runtime:event:1".into(),
        },
    )
    .expect("event");
    RuntimeProjection::replay(delivery, vec![binding], &[event]).expect("runtime replay")
}

#[allow(clippy::too_many_lines)]
fn fixture_with_delivery(
    delivery: Delivery,
    runtime_unavailable: bool,
    publication_unavailable: bool,
    race: bool,
    expire_after_reads: Option<usize>,
) -> Fixture {
    fixture_with_delivery_and_event_behavior(
        delivery,
        None,
        runtime_unavailable,
        publication_unavailable,
        race,
        expire_after_reads,
        EventCursorBehavior::Stable,
    )
}

fn fixture_with_delivery_and_candidate(
    delivery: Delivery,
    candidate: FrozenDeliveryCandidate,
) -> Fixture {
    fixture_with_delivery_and_event_behavior(
        delivery,
        Some(candidate),
        false,
        false,
        false,
        None,
        EventCursorBehavior::Stable,
    )
}

fn fixture_with_delivery_and_candidate_artifacts(
    delivery: Delivery,
    candidate: FrozenDeliveryCandidate,
    artifacts: ArtifactStore,
) -> Fixture {
    fixture_with_delivery_and_event_behavior_and_artifacts(
        delivery,
        Some(candidate),
        false,
        false,
        false,
        None,
        EventCursorBehavior::Stable,
        Some(artifacts),
    )
}

fn fixture_projection_cursor(
    scope: &RepositoryScope,
    stream: ProjectionEventStream,
    event_id: &str,
) -> ProjectionEventCursor {
    let mut encoded_scope = Vec::new();
    for value in [
        "winwincode.command-receipt.scope.v1",
        "repository",
        scope.organization_id.0.as_str(),
        scope.workspace_id.0.as_str(),
        scope.project_id.0.as_str(),
        scope.repository_id.0.as_str(),
    ] {
        encoded_scope.extend_from_slice(&(value.len() as u64).to_be_bytes());
        encoded_scope.extend_from_slice(value.as_bytes());
    }
    let key = ProjectionEventStreamKey::new(
        ReceiptScopeKey::from_encoded(encoded_scope).expect("fixture scope key"),
        stream,
    )
    .expect("fixture event stream key");
    ProjectionEventCursor::try_new(key, 1, Some(ControlPlaneEventId(event_id.to_owned())))
        .expect("fixture event cursor")
}

#[allow(clippy::too_many_lines)]
fn fixture_with_delivery_and_event_behavior(
    delivery: Delivery,
    candidate: Option<FrozenDeliveryCandidate>,
    runtime_unavailable: bool,
    publication_unavailable: bool,
    race: bool,
    expire_after_reads: Option<usize>,
    event_cursor_behavior: EventCursorBehavior,
) -> Fixture {
    fixture_with_delivery_and_event_behavior_and_artifacts(
        delivery,
        candidate,
        runtime_unavailable,
        publication_unavailable,
        race,
        expire_after_reads,
        event_cursor_behavior,
        None,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn fixture_with_delivery_and_event_behavior_and_artifacts(
    delivery: Delivery,
    candidate: Option<FrozenDeliveryCandidate>,
    runtime_unavailable: bool,
    publication_unavailable: bool,
    race: bool,
    expire_after_reads: Option<usize>,
    event_cursor_behavior: EventCursorBehavior,
    artifacts: Option<ArtifactStore>,
) -> Fixture {
    let scope = RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: ProjectId("prj_00000000000000000000000001".into()),
        repository_id: RepositoryId(
            if race {
                "rep_00000000000000000000000002"
            } else {
                "rep_00000000000000000000000001"
            }
            .into(),
        ),
    };
    let memory = Arc::new(InMemoryDeliveryJournal::new());
    DeliveryStore::borrowed(memory.as_ref())
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("1".repeat(64)),
            request_digest: "1".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed journal");
    let loaded = memory.load(delivery.id()).expect("load").expect("journal");
    let aggregate = LoadedAggregateJournal {
        manifest: loaded.manifest,
        records: loaded
            .records
            .into_iter()
            .map(|record| AggregateJournalRecord::new(record.sequence, record.digest, record.bytes))
            .collect(),
    };
    let accepted_sequence = u64::from(delivery.snapshot().session_bindings.first().is_some_and(
        |binding| binding.worker_session_id.is_some() && binding.codex_thread_id.is_some(),
    ));
    let projection = runtime_projection_for(&delivery);
    let runtime = Arc::new(Mutex::new(
        TrustedRuntimeProjectionRead::try_new(
            scope.clone(),
            delivery.revision(),
            Revision(4),
            accepted_sequence,
            Instant("2026-08-25T00:00:00Z".into()),
            &projection,
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .expect("trusted runtime"),
    ));
    let publication = TrustedPublicationProjectionRead::try_new(
        scope.clone(),
        delivery.id().clone(),
        delivery.revision(),
        Revision(0),
        candidate,
        None,
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
    )
    .expect("trusted publication");
    let journal = Arc::new(Mutex::new(aggregate));
    let states = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let runtime_read_count = Arc::new(Mutex::new(0));
    let device_binding = Arc::new(Mutex::new(
        delivery
            .snapshot()
            .work_run_aggregate
            .runs
            .first()
            .map(|run| {
                (
                    run.execution_job_id.clone(),
                    WorkRunDeviceBindingFacts {
                        public_client_id: "123456789012".into(),
                        repository_binding_id: "rbd_00000000000000000000000001".into(),
                        worker_session_id: run.worker_session_id.0.clone(),
                        worker_id: run.worker_id.0.clone(),
                        worker_instance_id: run.worker_instance_id.0.clone(),
                        work_run_id: Some(run.id.0.clone()),
                    },
                )
            }),
    ));
    let delivery_event_cursor = fixture_projection_cursor(
        &scope,
        ProjectionEventStream::Delivery(delivery.id().clone()),
        "evt_delivery_fixture_0001",
    );
    let product_session_event_cursor =
        delivery.snapshot().session_bindings.first().map(|binding| {
            fixture_projection_cursor(
                &scope,
                ProjectionEventStream::ProductSession(binding.product_session_id.clone()),
                "evt_product_session_fixture_0001",
            )
        });
    let storage = Box::new(JournalStorage {
        journal: Arc::clone(&journal),
        states: Arc::clone(&states),
        runtime_read_count: Arc::clone(&runtime_read_count),
        advance_event_after_runtime_read: matches!(
            event_cursor_behavior,
            EventCursorBehavior::AdvanceAfterRuntimeRead
        ),
        device_binding: Arc::clone(&device_binding),
    });
    let mut control_plane = match artifacts {
        Some(artifacts) => {
            ControlPlane::start_with_artifacts(storage, artifacts, Box::new(NoopPublisher))
        }
        None => ControlPlane::start(storage, Box::new(NoopPublisher)),
    }
    .expect("control plane");
    control_plane
        .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
            Box::new(SqliteTrustedRuntimeProjectionAdapter::new(Box::new(
                RuntimeCutReader {
                    adapter: RuntimeAdapter {
                        read: Arc::clone(&runtime),
                        race: Arc::new(Mutex::new(false)),
                        read_count: runtime_read_count,
                        expire_after_reads,
                        unavailable: runtime_unavailable,
                        atomic_read_cut: true,
                        delivery_event_cursor: Some(delivery_event_cursor),
                        product_session_event_cursor,
                    },
                },
            ))),
            Box::new(PublicationAdapter {
                read: publication,
                unavailable: publication_unavailable,
            }),
        ))
        .expect("sources");
    Fixture {
        control_plane,
        delivery,
        scope,
        journal,
        states,
        domain_journal: memory,
        runtime,
        device_binding,
    }
}

fn delivery_fixture(draft: bool) -> Delivery {
    let parsed = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("fixture");
    let mut snapshot = parsed.into_snapshot();
    snapshot.revision = 1;
    snapshot.status = if draft {
        DeliveryStatus::Draft
    } else {
        DeliveryStatus::Ready
    };
    snapshot.evidence.clear();
    snapshot.verdict = None;
    if draft {
        snapshot.work_run_aggregate.runs.clear();
        snapshot.work_run_aggregate.items.clear();
        snapshot.session_bindings.clear();
        snapshot.attention_items.clear();
    }
    Delivery::try_from_snapshot(snapshot).expect("projection fixture")
}

fn approved_solution_review_fixture(status: DeliveryStatus) -> Delivery {
    let approved = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-approved-solution-review.json"
    ))
    .expect("approved solution-review fixture");
    let mut snapshot = approved.into_snapshot();
    snapshot.status = status;
    let review_attention = snapshot
        .attention_items
        .first_mut()
        .expect("approved review Attention");
    review_attention.assigned_to = Some("usr_reviewer".into());
    review_attention.resolved_by = Some("usr_reviewer".into());
    snapshot.updated_at_millis += 1;
    Delivery::try_from_snapshot(snapshot).expect("approved solution-review lifecycle fixture")
}

#[allow(clippy::too_many_lines)]
fn approved_verified_candidate_fixture() -> (Delivery, FrozenDeliveryCandidate) {
    let approved = approved_solution_review_fixture(DeliveryStatus::Ready).into_snapshot();
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("passing Delivery fixture")
    .into_snapshot();

    snapshot.session_bindings = approved.session_bindings;
    snapshot.attention_items = approved.attention_items;
    snapshot.work_run_aggregate = approved.work_run_aggregate;
    snapshot.status = DeliveryStatus::Ready;
    snapshot.work_run_aggregate.runs[0].state = winwincode_domain::WorkRunState::Settled;

    let executor_binding_id = SessionBindingId("binding:executor".into());
    for (role, suffix, bound_at) in [
        ("executor", "02", 1_800_000_000_041),
        ("verifier", "03", 1_800_000_000_061),
    ] {
        let mut run = snapshot.work_run_aggregate.runs[0].clone();
        let mut binding = snapshot.session_bindings[0].clone();
        run.id = WorkRunId(format!("wrn_01J000000000000000000000{suffix}"));
        run.work_item_revision = snapshot.work_run_aggregate.items[0].revision.clone();
        run.execution_job_id = ExecutionJobId(format!("job_01J000000000000000000000{suffix}"));
        run.worker_session_id = WorkerSessionId(format!("wsn_01J000000000000000000000{suffix}"));
        run.product_session_id = Some(ProductSessionId(format!(
            "psn_01J000000000000000000000{suffix}"
        )));
        run.codex_thread_id = Some(CodexThreadId(format!(
            "cdx_01J000000000000000000000{suffix}"
        )));
        run.lease_id = LeaseId(format!("lse_01J000000000000000000000{suffix}"));
        run.state = if role == "executor" {
            winwincode_domain::WorkRunState::CandidateReady
        } else {
            winwincode_domain::WorkRunState::Settled
        };
        binding.id = SessionBindingId(format!("binding:{role}"));
        binding.work_run_id = run.id.clone();
        binding.execution_job_id = run.execution_job_id.clone();
        binding.work_item_revision = run.work_item_revision.clone();
        binding.worker_session_id = Some(run.worker_session_id.clone());
        binding.product_session_id = run.product_session_id.clone().unwrap();
        binding.codex_thread_id.clone_from(&run.codex_thread_id);
        binding.lease_id = Some(run.lease_id.clone());
        binding.execution_profile = Some(role.into());
        let runtime_context = binding
            .runtime_context
            .as_mut()
            .expect("complete fixture runtime context");
        runtime_context.agent_identity.id =
            AgentIdentityId(format!("agt_01J000000000000000000000{suffix}"));
        runtime_context.agent_identity.name = role.into();
        runtime_context.agent_identity.role = role.into();
        binding.bound_at_millis = bound_at;
        snapshot.work_run_aggregate.runs.push(run);
        snapshot.session_bindings.push(binding);
    }
    snapshot.work_run_aggregate.items[0].state = WorkItemState::CandidateReady;
    snapshot.evidence[0].work_run_id = snapshot.work_run_aggregate.runs[2].id.clone();
    snapshot.evidence[0].session_binding_id = snapshot.session_bindings[2].id.clone();
    snapshot.evidence[0].created_at_millis = 1_800_000_000_069;
    let verdict = snapshot.verdict.as_mut().expect("passing verdict");
    for result in &mut verdict.criteria {
        result.evaluated_at_millis = 1_800_000_000_071;
    }
    verdict.produced_at_millis = 1_800_000_000_072;
    snapshot.revision = 1;
    snapshot.updated_at_millis = 1_800_000_000_073;

    let pre_candidate = Delivery::try_from_snapshot(snapshot).expect("candidate lifecycle facts");
    let candidate = freeze_candidate_fixture(
        &pre_candidate,
        &pre_candidate
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| binding.id == executor_binding_id)
            .unwrap()
            .work_run_id,
        &executor_binding_id,
        CandidateFixtureInput {
            finished_at_millis: 1_800_000_000_050,
            base_commit_id: "0123456789012345678901234567890123456789".into(),
            base_tree_id: "1".repeat(40),
            candidate_commit_id: "2".repeat(40),
            candidate_tree_id: "3".repeat(40),
            diff_sha256: "a".repeat(64),
            changed_paths: vec![CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4".repeat(40)),
            }],
            changed_hunks: vec![CandidateHunkFact {
                file_path: "src/invitation.rs".into(),
                hunk_sha256: "b".repeat(64),
                source_hunk_sha256: None,
            }],
            artifact_ref: "artifact:executor".into(),
            artifact_digest: Sha256Digest(format!("sha256:{}", "9".repeat(64))),
            terminal_event_sequence: 12,
        },
    );
    let mut snapshot = pre_candidate.into_snapshot();
    for evidence in &mut snapshot.evidence {
        evidence.candidate_ref = candidate.candidate_ref().into();
    }
    let verdict = snapshot.verdict.as_mut().expect("passing verdict");
    verdict.candidate_ref = candidate.candidate_ref().into();
    for result in &mut verdict.criteria {
        result.candidate_ref = candidate.candidate_ref().into();
    }
    let ready =
        Delivery::try_from_snapshot(snapshot).expect("verified candidate lifecycle fixture");
    (ready, candidate)
}

fn normalize_projection_read_revision(delivery: &Delivery) -> Delivery {
    // The read harness accepts one seeded journal record at revision one. The
    // production transitions have already established the lifecycle facts;
    // only the isolated query coordinate is normalized here.
    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.revision = 1;
    Delivery::try_from_snapshot(snapshot).expect("projection read fixture")
}
fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId("usr_fixture".into()),
        kind: UserActorKind::User,
    })
}
fn delivery_query(
    f: &Fixture,
    cursor: Option<StrongFlowReadCursor>,
    limit: i64,
) -> DeliveryGetQuery {
    DeliveryGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit,
        },
        parameters: DeliveryGetParameters {
            at_cursor: cursor,
            delivery_id: f.delivery.id().clone(),
        },
        query: DeliveryGetQueryQuery::DeliveryGet,
        request_id: RequestId("req_delivery".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: f.scope.clone(),
    }
}
fn detail_and_cursor(
    f: &Fixture,
) -> (
    winwincode_api::generated::DeliveryDetailProjection,
    StrongFlowReadCursor,
) {
    let response: QueryResultResponse =
        StrongFlowProjectionQueryPort::delivery_get(&f.control_plane, &delivery_query(f, None, 20))
            .expect("delivery detail");
    let QueryResultResponse::DeliveryGetResultResponse(response) = response else {
        panic!("detail")
    };
    let detail = response.result;
    let cursor = detail.read_cursor.clone();
    (detail, cursor)
}

#[test]
fn candidate_file_content_query_rejects_zero_length_before_reading_candidate_facts() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    let query = CandidateFileContentGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: CandidateFileContentGetParameters {
            at_cursor: cursor,
            candidate_ref: "candidate:fixture".into(),
            candidate_tree_id: "3".repeat(40),
            delivery_id: f.delivery.id().clone(),
            diff_sha256: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            length: 0,
            offset: 0,
            path: "src/app.txt".into(),
            read_page_limit: 20,
        },
        query: CandidateFileContentGetQueryQuery::CandidateFileContentGet,
        request_id: RequestId("req_candidate_file_content".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: f.scope,
    };

    let error = StrongFlowProjectionQueryPort::candidate_file_content_get(&f.control_plane, &query)
        .expect_err("zero-length reads must fail before candidate source access");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::InvalidRequest
    );
}

fn positive_query_repository(root: &Path) -> (String, String) {
    fs::create_dir_all(root.join("src")).expect("repository root");
    let init = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git init");
    assert!(init.success());
    fs::write(root.join("src/app.txt"), b"base\n").expect("base source");
    positive_query_git(root, &["add", "--", "src/app.txt"]);
    positive_query_commit(root, "base", "2026-08-25T00:00:00Z");
    let base = positive_query_text(positive_query_git(root, &["rev-parse", "HEAD"]));
    fs::write(root.join("src/app.txt"), b"base\ncandidate\n").expect("candidate source");
    positive_query_git(root, &["add", "--", "src/app.txt"]);
    positive_query_commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = positive_query_text(positive_query_git(root, &["rev-parse", "HEAD"]));
    (base, candidate)
}

fn positive_query_git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn positive_query_text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .expect("git output")
        .trim()
        .to_owned()
}

fn positive_query_commit(root: &Path, message: &str, timestamp: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-q", "-m", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .status()
        .expect("git commit");
    assert!(status.success());
}

#[allow(clippy::too_many_lines)]
fn positive_candidate_fixture() -> (Fixture, PathBuf, FrozenDeliveryCandidate, String, String) {
    let root = std::env::temp_dir().join(format!(
        "winwincode-control-plane-candidate-query-{}-{}",
        std::process::id(),
        NEXT_POSITIVE_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = positive_query_repository(&repository);
    let delivery_id = DeliveryId("dlv_01J00000000000000000000000".into());
    let fixture = verdict_fixture(&delivery_id, VerdictFixtureOutcome::Pass);
    let mut snapshot = fixture.delivery.into_snapshot();
    snapshot.spec.repository = RepositoryRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        kind: RepositoryKind::LocalGit,
        locator: "project-one".into(),
    };
    snapshot.spec.base_revision.clone_from(&base_commit);
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.execution_profile.as_deref() == Some("executor"))
        .expect("executor binding");
    let producer_id = binding.work_run_id.clone();
    let job_id = ExecutionJobId("job_01J00000000000000000000001".into());
    let session_id = WorkerSessionId("wsn_01J00000000000000000000001".into());
    let thread_id = CodexThreadId("cdx_01J00000000000000000000001".into());
    let lease_id = LeaseId("lse_01J00000000000000000000001".into());
    let fencing_token = FencingToken("42".into());
    let worker_id = WorkerId("wrk_01J00000000000000000000001".into());
    let worker_instance_id = WorkerInstanceId("wki_01J00000000000000000000001".into());
    let product_session_id = ProductSessionId("psn_01J00000000000000000000001".into());
    binding.product_session_id = product_session_id.clone();
    binding.execution_job_id = job_id.clone();
    binding.worker_session_id = Some(session_id.clone());
    binding.codex_thread_id = Some(thread_id.clone());
    binding.lease_id = Some(lease_id.clone());
    binding.fencing_token = Some(fencing_token.clone());
    binding.worker_id = Some(worker_id.clone());
    binding.worker_instance_id = Some(worker_instance_id.clone());
    let context = binding
        .runtime_context
        .as_mut()
        .expect("executor runtime context");
    context.agent_identity.worker_id = worker_id.clone();
    let producer = snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.id == producer_id)
        .expect("executor WorkRun");
    producer.product_session_id = Some(product_session_id);
    producer.execution_job_id = job_id.clone();
    producer.worker_session_id = session_id.clone();
    producer.codex_thread_id = Some(thread_id.clone());
    producer.lease_id = lease_id.clone();
    producer.fencing_token.clone_from(&fencing_token.0);
    producer.worker_id = worker_id.clone();
    producer.worker_instance_id = worker_instance_id.clone();
    let delivery = Delivery::try_from_snapshot(snapshot).expect("candidate Delivery");

    let artifact_id = ArtifactId("art_01J00000000000000000000001".into());
    let manifest = GitCandidateArtifactManifest::new(
        candidate_commit.clone(),
        candidate_bundle(&repository, &base_commit, &candidate_commit),
    )
    .expect("candidate manifest")
    .encode()
    .expect("manifest encoding");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let mut encoded_scope = Vec::new();
    for value in [
        "winwincode.command-receipt.scope.v1",
        "repository",
        "org_00000000000000000000000001",
        "wsp_00000000000000000000000001",
        "prj_00000000000000000000000001",
        "rep_00000000000000000000000001",
    ] {
        encoded_scope.extend_from_slice(&(value.len() as u64).to_be_bytes());
        encoded_scope.extend_from_slice(value.as_bytes());
    }
    let scope = ReceiptScopeKey::from_encoded(encoded_scope).expect("scope");
    let provenance = ArtifactProvenance::execution_job(
        job_id.clone(),
        1,
        lease_id.clone(),
        fencing_token.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        session_id.clone(),
    )
    .expect("Artifact provenance");
    let mut artifacts = ArtifactStore::open(
        root.join("catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact store");
    let attribution = ArtifactMeteringAttribution {
        organization_id: OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: ProjectId("prj_00000000000000000000000001".into()),
        repository_id: RepositoryId("rep_00000000000000000000000001".into()),
        delivery_id: Some(delivery_id.clone()),
        product_session_id: Some(ProductSessionId("psn_00000000000000000000000001".into())),
        user_id: UserId("usr_00000000000000000000000001".into()),
    };
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId("xmsg_01J00000000000000000000001".into()),
            RequestId("req_01J00000000000000000000001".into()),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            attribution,
            ArtifactRetention::Indefinite,
            1_800_000_000_000,
        ))
        .expect("Artifact open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId("xmsg_01J00000000000000000000002".into()),
            artifact_id.clone(),
            provenance.clone(),
            1_800_000_000_020,
            1,
            "application/octet-stream",
            digest.clone(),
            manifest,
            true,
        ))
        .expect("Artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope,
            artifact_id.clone(),
            digest.clone(),
            provenance.clone(),
        ))
        .expect("Artifact read");
    let resolver = LocalGitSourceResolver::open(&repositories).expect("source resolver");
    let source = resolver
        .resolve_candidate(&object, "project-one", &base_commit)
        .expect("candidate source");
    let authority = session_binding_authority(
        active_lease_identity(
            job_id.clone(),
            1,
            lease_id.clone(),
            fencing_token.clone(),
            worker_id.clone(),
            worker_instance_id.clone(),
            session_id.clone(),
        ),
        Instant("2026-08-25T00:00:00.000Z".into()),
        Instant("2026-08-25T01:00:00.000Z".into()),
    );
    let terminal = delivery_terminal_outcome_facts(
        authority,
        terminal_worker_outcome(
            producer_id,
            job_id,
            1,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance_id,
            session_id,
            TerminalOutcomeStatus::Succeeded,
            terminal_outcome_metadata(
                Some(thread_id),
                1_800_000_000_020,
                ExecutionAckSequence(4),
                vec![TerminalArtifactReference {
                    artifact_id,
                    digest,
                }],
            ),
        ),
    );
    let candidate = freeze_delivery_candidate_from_source(
        &delivery,
        &winwincode_storage::delivery_candidate_source(&source),
        &terminal,
    )
    .expect("freeze candidate");
    let mut f =
        fixture_with_delivery_and_candidate_artifacts(delivery, candidate.clone(), artifacts);
    f.control_plane
        .install_git_source_resolver(Box::new(resolver))
        .expect("install source resolver");
    let delivery_state = StoredState {
        stream_id: format!("delivery:{}", f.delivery.id().0),
        revision: f.delivery.revision(),
        payload: f.delivery.encode_json().expect("delivery JSON"),
    };
    f.states
        .lock()
        .expect("fixture states")
        .insert(delivery_state.stream_id.clone(), delivery_state);
    let active = terminal.authority().active_lease();
    let authority = PositiveTerminalAuthority {
        schema_version: 1,
        delivery_id: f.delivery.id().clone(),
        work_run_id: terminal.work_run_id().clone(),
        job_id: active.execution_job_id().clone(),
        attempt: active.attempt(),
        lease_id: active.lease_id().clone(),
        fencing_token: active.fencing_token().clone(),
        worker_id: active.worker_id().clone(),
        worker_instance_id: active.worker_instance_id().clone(),
        worker_session_id: active.worker_session_id().clone(),
        issued_at: terminal.authority().issued_at().clone(),
        expires_at: terminal.authority().expires_at().clone(),
        artifacts: terminal.metadata().artifacts().to_vec(),
        codex_thread_id: terminal.metadata().codex_thread_id().cloned(),
        finished_at_millis: terminal.metadata().finished_at_millis(),
        last_event_sequence: terminal.metadata().last_event_sequence().clone(),
        status: "succeeded",
        disposition: PositiveTerminalDisposition::Settled {
            delivery_revision: f.delivery.revision(),
        },
    };
    let binding = active.execution_job_id();
    f.states.lock().expect("fixture states").insert(
        format!("delivery-terminal-authority:{}", binding.0),
        StoredState {
            stream_id: format!("delivery-terminal-authority:{}", binding.0),
            revision: 1,
            payload: serde_json::to_vec(&authority).expect("terminal authority JSON"),
        },
    );
    (f, root, candidate, base_commit, candidate_commit)
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_file_content_query_reads_retained_candidate_and_rejects_forged_bindings() {
    let (f, root, candidate, _base_commit, _candidate_commit) = positive_candidate_fixture();
    let (_, cursor) = detail_and_cursor(&f);
    let query = CandidateFileContentGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: CandidateFileContentGetParameters {
            at_cursor: cursor,
            candidate_ref: candidate.candidate_ref().into(),
            candidate_tree_id: candidate.candidate_tree_id().into(),
            delivery_id: f.delivery.id().clone(),
            diff_sha256: Sha256Digest(format!("sha256:{}", candidate.diff_sha256())),
            length: 5,
            offset: 0,
            path: "src/app.txt".into(),
            read_page_limit: 20,
        },
        query: CandidateFileContentGetQueryQuery::CandidateFileContentGet,
        request_id: RequestId("req_candidate_file_content_positive".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: f.scope.clone(),
    };
    let response =
        StrongFlowProjectionQueryPort::candidate_file_content_get(&f.control_plane, &query)
            .expect("candidate content");
    let QueryResultResponse::CandidateFileContentGetResultResponse(response) = response else {
        panic!("candidate content response");
    };
    assert_eq!(
        response.result.candidate.candidate_ref,
        candidate.candidate_ref()
    );
    assert_eq!(response.result.offset, 0);
    assert_eq!(response.result.returned_bytes, 5);
    assert_eq!(response.result.total_bytes, 15);
    assert_eq!(response.result.next_offset, Some(5));
    assert_eq!(response.result.read_cursor, query.parameters.at_cursor);
    assert_eq!(response.result.data_base64, STANDARD.encode(b"base\n"));
    assert_eq!(response.result.media_type, "text/plain");

    let mut tail = query.clone();
    tail.parameters.offset = 5;
    tail.parameters.length = 20;
    let QueryResultResponse::CandidateFileContentGetResultResponse(response) =
        StrongFlowProjectionQueryPort::candidate_file_content_get(&f.control_plane, &tail)
            .expect("candidate tail")
    else {
        panic!("candidate tail response");
    };
    assert_eq!(response.result.next_offset, None);
    assert_eq!(response.result.data_base64, STANDARD.encode(b"candidate\n"));

    #[allow(clippy::type_complexity)]
    let forged_cases: [(&str, fn(&mut CandidateFileContentGetQuery)); 5] = [
        ("path", |query: &mut CandidateFileContentGetQuery| {
            query.parameters.path = "src/missing.txt".into();
        }),
        ("candidate", |query: &mut CandidateFileContentGetQuery| {
            query.parameters.candidate_ref = "candidate:forged".into();
        }),
        ("tree", |query: &mut CandidateFileContentGetQuery| {
            query.parameters.candidate_tree_id = "4".repeat(40);
        }),
        ("diff", |query: &mut CandidateFileContentGetQuery| {
            query.parameters.diff_sha256 = Sha256Digest(format!("sha256:{}", "f".repeat(64)));
        }),
        ("cursor", |query: &mut CandidateFileContentGetQuery| {
            query.parameters.at_cursor.token = "forged".into();
        }),
    ];
    for (label, mutate) in forged_cases {
        let mut forged = query.clone();
        mutate(&mut forged);
        assert!(
            StrongFlowProjectionQueryPort::candidate_file_content_get(&f.control_plane, &forged)
                .is_err(),
            "forged {label} binding must fail"
        );
    }
    f.control_plane.shutdown().expect("control plane shutdown");
    fs::remove_dir_all(root).expect("fixture release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn page_annotation_submit_publishes_attention_and_evidence_for_detail_read() {
    let (delivery, candidate) = approved_verified_candidate_fixture();
    let mut delivery_snapshot = delivery.into_snapshot();
    let producer_work_run_id = candidate.producer_work_run_id().clone();
    delivery_snapshot.status = DeliveryStatus::NeedsAttention;
    let attention = delivery_snapshot
        .attention_items
        .first_mut()
        .expect("candidate attention");
    attention.id = AttentionItemId("att_01J00000000000000000000090".into());
    attention.work_run_id = Some(producer_work_run_id);
    attention.item_type = AttentionItemType::VerificationBlocked;
    attention.blocking = true;
    attention.status = AttentionItemStatus::Open;
    attention.resolution = None;
    attention.resolved_by = None;
    attention.resolved_at_millis = None;
    let verdict = delivery_snapshot
        .verdict
        .as_mut()
        .expect("candidate verdict");
    verdict.status = CriterionVerdict::Fail;
    verdict
        .criteria
        .iter_mut()
        .find(|result| result.criterion_id.0.ends_with('0'))
        .expect("required criterion")
        .verdict = CriterionVerdict::Fail;
    let delivery =
        Delivery::try_from_snapshot(delivery_snapshot).expect("failed candidate verdict");
    let mut snapshot = delivery.into_snapshot();
    let run = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| matches!(run.state, winwincode_domain::WorkRunState::CandidateReady))
        .expect("candidate-ready WorkRun")
        .clone();
    let binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.work_run_id == run.id)
        .expect("candidate SessionBinding")
        .clone();
    let candidate_ref = candidate.candidate_ref().to_owned();
    let candidate_digest = {
        let digest = candidate_ref
            .strip_prefix("git-candidate:")
            .expect("candidate digest");
        Sha256Digest(if digest.starts_with("sha256:") {
            digest.to_owned()
        } else {
            format!("sha256:{digest}")
        })
    };
    snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|candidate_run| candidate_run.id == run.id)
        .expect("candidate-ready WorkRun")
        .candidate_digest = Some(winwincode_domain::CandidateDigest(
        candidate_digest.0.clone(),
    ));
    let diff_sha256 = Sha256Digest(if candidate.diff_sha256().starts_with("sha256:") {
        candidate.diff_sha256().to_owned()
    } else {
        format!("sha256:{}", candidate.diff_sha256())
    });
    let delivery_id = snapshot.spec.delivery_id.clone();
    let spec_id = snapshot.spec.id.clone();
    let candidate_identity = CollaborationCandidateIdentity {
        candidate_ref: candidate_ref.clone(),
        candidate_digest: candidate_digest.clone(),
        candidate_revision: snapshot.revision,
    };
    let mut candidate_evidence = snapshot.evidence[0].clone();
    candidate_evidence.delivery_id = delivery_id.clone();
    candidate_evidence.delivery_spec_id = spec_id.clone();
    candidate_evidence.delivery_spec_revision = snapshot.spec.revision;
    candidate_evidence.work_run_id = run.id.clone();
    candidate_evidence.session_binding_id = binding.id.clone();
    candidate_evidence.candidate_ref = candidate_ref.clone();
    candidate_evidence.evidence_type = EvidenceRefType::Commit;
    candidate_evidence.source_ref = format!("git_commit:{}", candidate.candidate_commit_id());
    snapshot.evidence[0] = candidate_evidence;
    snapshot.evidence.push(EvidenceRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: EvidenceId("evd_01J00000000000000000000011".into()),
        delivery_id: delivery_id.clone(),
        delivery_spec_id: spec_id.clone(),
        delivery_spec_revision: snapshot.spec.revision,
        work_run_id: run.id.clone(),
        session_binding_id: binding.id.clone(),
        candidate_ref: candidate_ref.clone(),
        evidence_type: EvidenceRefType::Diff,
        source_ref: format!("git_diff:{}", diff_sha256.0),
        created_at_millis: 1_800_000_000_074,
    });
    snapshot.evidence.push(EvidenceRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: EvidenceId("evd_01J00000000000000000000012".into()),
        delivery_id: delivery_id.clone(),
        delivery_spec_id: spec_id.clone(),
        delivery_spec_revision: snapshot.spec.revision,
        work_run_id: run.id.clone(),
        session_binding_id: binding.id.clone(),
        candidate_ref: candidate_ref.clone(),
        evidence_type: EvidenceRefType::File,
        source_ref: format!("git_file:{}:", candidate.candidate_tree_id()),
        created_at_millis: 1_800_000_000_075,
    });
    let delivery = Delivery::try_from_snapshot(snapshot).expect("candidate evidence facts");

    let scope = RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: ProjectId("prj_00000000000000000000000001".into()),
        repository_id: RepositoryId("rep_00000000000000000000000001".into()),
    };
    let viewer = UserId("usr_01J00000000000000000000000".into());
    let item_id = CollaborationInboxItemId::DeliveryAttention(AttentionItemId(
        "att_01J00000000000000000000090".into(),
    ));
    let source_item = CollaborationInboxSourceItem {
        id: item_id.clone(),
        kind: CollaborationInboxItemKind::DeliveryAttention,
        target: ResponsibilityTarget::Delivery {
            delivery_id: delivery_id.clone(),
        },
        responsibility_role: ResponsibilityRole::Assignee,
        source_revision: delivery.revision(),
        source_sha256: Sha256Digest(format!("sha256:{}", "1".repeat(64))),
        title_sha256: Sha256Digest(format!("sha256:{}", "2".repeat(64))),
        opened_at_millis: 1_800_000_000_080,
        expires_at_millis: None,
        state: CollaborationInboxItemState::Pending,
        candidate: Some(candidate_identity.clone()),
        command_route: FormalCollaborationCommandRoute::DeliveryResolveAttention {
            attention_item_id: AttentionItemId("att_01J00000000000000000000090".into()),
            delivery_id: delivery_id.clone(),
        },
    };
    let source = CollaborationInboxSourceSnapshot {
        scope: scope.clone(),
        revision: 1,
        snapshot_sha256: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        item_state_guards: std::collections::BTreeMap::new(),
        items: vec![source_item],
    };
    let mut source = source;
    source.snapshot_sha256 = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&source.items).expect("source JSON"))
    ));
    source.item_state_guards.insert(
        source.items[0].id.clone(),
        vec![StateRevisionGuard::new("e2e-page-source", 0).expect("source guard")],
    );
    let assignment = ResponsibilityAssignment {
        id: ResponsibilityAssignmentId("assignment-page-annotation".into()),
        scope: scope.clone(),
        target: ResponsibilityTarget::Delivery {
            delivery_id: delivery_id.clone(),
        },
        role: ResponsibilityRole::Assignee,
        principal_user_id: viewer.clone(),
        state: ResponsibilityAssignmentState::Active,
        revision: 1,
        assigned_by: actor(),
        assigned_at_millis: 1_800_000_000_081,
        accepted_at_millis: Some(1_800_000_000_081),
        expires_at_millis: None,
        ended_at_millis: None,
        target_revision: delivery.revision(),
        target_sha256: Sha256Digest(format!("sha256:{}", "4".repeat(64))),
        rbac_revision: 1,
        rbac_sha256: Sha256Digest(format!("sha256:{}", "5".repeat(64))),
    };
    let authority = CollaborationInboxAuthoritySnapshot {
        scope: scope.clone(),
        viewer_user_id: viewer.clone(),
        assignments: vec![CollaborationResponsibilityEntitlement { assignment }],
        authority_revision: 1,
        authority_sha256: Sha256Digest(format!("sha256:{}", "6".repeat(64))),
        state_guards: vec![
            StateRevisionGuard::new("e2e-page-authority", 0).expect("authority guard"),
        ],
    };

    let root = std::env::temp_dir().join(format!(
        "winwincode-page-annotation-e2e-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("e2e root");
    let screenshot_bytes = STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .expect("valid PNG fixture");
    let screenshot_id = ArtifactId("art_01J00000000000000000000021".into());
    let screenshot_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&screenshot_bytes)));
    let provenance = ArtifactProvenance::execution_job(
        run.execution_job_id.clone(),
        u64::try_from(run.attempt).expect("attempt"),
        run.lease_id.clone(),
        FencingToken(run.fencing_token.clone()),
        run.worker_id.clone(),
        run.worker_instance_id.clone(),
        run.worker_session_id.clone(),
    )
    .expect("screenshot provenance");
    let screenshot_scope = product_scope_key(&scope);
    let screenshot_attribution = ArtifactMeteringAttribution {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
        delivery_id: Some(delivery_id.clone()),
        product_session_id: Some(binding.product_session_id.clone()),
        user_id: viewer.clone(),
    };
    let mut screenshot_store = ArtifactStore::open(
        root.join("artifacts"),
        Box::new(LocalArtifactObjectStore::open(root.join("objects")).expect("object store")),
    )
    .expect("screenshot artifact store");
    screenshot_store
        .open_artifact(ArtifactOpen::new(
            screenshot_scope.clone(),
            ExecutionMessageId("xmsg_01J00000000000000000000021".into()),
            RequestId("req_01J00000000000000000000021".into()),
            screenshot_id.clone(),
            "screenshot",
            "image/png",
            screenshot_digest.clone(),
            screenshot_bytes.len() as u64,
            Some("screen.png".into()),
            provenance.clone(),
            screenshot_attribution,
            ArtifactRetention::Indefinite,
            1_800_000_000_100,
        ))
        .expect("open screenshot artifact");
    screenshot_store
        .append_chunk(&ArtifactChunk::new(
            screenshot_scope,
            ExecutionMessageId("xmsg_01J00000000000000000000022".into()),
            screenshot_id.clone(),
            provenance.clone(),
            1_800_000_000_101,
            1,
            "image/png",
            screenshot_digest.clone(),
            screenshot_bytes,
            true,
        ))
        .expect("complete screenshot artifact");
    let terminal_authority = PositiveTerminalAuthority {
        schema_version: 1,
        delivery_id: delivery_id.clone(),
        work_run_id: run.id.clone(),
        job_id: run.execution_job_id.clone(),
        attempt: u64::try_from(run.attempt).expect("attempt"),
        lease_id: run.lease_id.clone(),
        fencing_token: FencingToken(run.fencing_token.clone()),
        worker_id: run.worker_id.clone(),
        worker_instance_id: run.worker_instance_id.clone(),
        worker_session_id: run.worker_session_id.clone(),
        issued_at: Instant("2026-08-25T00:00:00.000Z".into()),
        expires_at: Instant("2026-08-25T01:00:00.000Z".into()),
        artifacts: vec![TerminalArtifactReference {
            artifact_id: screenshot_id.clone(),
            digest: screenshot_digest.clone(),
        }],
        codex_thread_id: binding.codex_thread_id.clone(),
        finished_at_millis: delivery.snapshot().updated_at_millis,
        last_event_sequence: ExecutionAckSequence(4),
        status: "succeeded",
        disposition: PositiveTerminalDisposition::Settled {
            delivery_revision: delivery.revision(),
        },
    };
    let memory = InMemoryDeliveryJournal::new();
    DeliveryStore::borrowed(&memory)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-page-annotation".into()),
            request_digest: "1".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal");
    let loaded = memory
        .load(delivery.id())
        .expect("load Delivery journal")
        .expect("seeded Delivery journal");
    let first = loaded.records.first().expect("first journal record");
    let mut storage = SqliteStorage::open(&root).expect("e2e storage");
    let seed_identity = ReceiptIdentity::new(
        winwincode_storage::receipt_actor_key(&winwincode_storage::PublicEventActor::System {
            id: winwincode_domain::SystemActorId("sys_01J00000000000000000000000".into()),
        })
        .expect("seed actor key"),
        product_scope_key(&scope),
        RequestId("seed-journal-page-annotation".into()),
    )
    .expect("seed receipt identity");
    storage
        .commit(
            &StateCommit::new(
                seed_identity,
                Sha256Digest(format!("sha256:{}", "7".repeat(64))),
                "e2e-seed-page-annotation",
                0,
                b"{}".to_vec(),
                vec![NewOutboxEvent::internal(
                    "evt_seed_page_annotation",
                    "test.page-annotation.seed",
                    b"{}".to_vec(),
                )],
            )
            .with_state_mutation(
                StateMutation::new(
                    format!("delivery-terminal-authority:{}", run.execution_job_id.0),
                    0,
                    serde_json::to_vec(&terminal_authority).expect("terminal authority JSON"),
                )
                .expect("terminal authority mutation"),
            )
            .with_journal_publication(AggregateJournalPublication::Create {
                key: AggregateJournalKey::new("delivery", delivery.id().0.clone())
                    .expect("journal key"),
                manifest: loaded.manifest,
                first_record: AggregateJournalRecord::new(
                    first.sequence,
                    first.digest.clone(),
                    first.bytes.clone(),
                ),
            }),
        )
        .expect("persist Delivery journal");

    let annotation_id = PageAnnotationId("page_annotation_e2e_01".into());
    let annotation_target = PageAnnotationTarget {
        page_path: "/settings/profile".into(),
        viewport: PageAnnotationViewport {
            width: 1280,
            height: 720,
            device_pixel_ratio: 2.0,
        },
        element: None,
        region: PageAnnotationRegion {
            x: 40,
            y: 80,
            width: 320,
            height: 96,
        },
    };
    let mut inbox = CollaborationInboxService::with_clock_and_artifact_store(
        Box::new(storage),
        Box::new(E2eInboxSource(source.clone())),
        Box::new(E2eInboxAuthority(authority)),
        Box::new(E2eInboxClock),
        screenshot_store,
    );
    let receipt = inbox
        .apply_page_annotation(&PageAnnotationCommand {
            context: CollaborationInboxCommandContext {
                actor: Actor::UserActor(UserActor {
                    id: viewer.clone(),
                    kind: UserActorKind::User,
                }),
                authenticated_scopes: vec![winwincode_api::generated::Scope::RepositoryScope(
                    scope.clone(),
                )],
                scope: scope.clone(),
                audience: CollaborationInboxAudience::Personal(viewer.clone()),
                request_id: RequestId("req_01J00000000000000000000023".into()),
                expected_revision: 0,
            },
            item_id,
            annotation_id: annotation_id.clone(),
            action: PageAnnotationAction::Upsert {
                candidate: PageAnnotationCandidateIdentity {
                    delivery_id: delivery_id.clone(),
                    delivery_spec_id: spec_id.0.clone(),
                    delivery_spec_revision: snapshot_revision(&delivery),
                    candidate_ref: candidate_ref.clone(),
                    candidate_digest,
                    candidate_tree_id: candidate.candidate_tree_id().into(),
                    diff_sha256,
                    work_run_id: run.id.clone(),
                    attempt: u64::try_from(run.attempt).expect("attempt"),
                    session_binding_id: binding.id.0.clone(),
                },
                target: annotation_target.clone(),
                body: "头像按钮在窄屏下被遮挡".into(),
                screenshot_artifact: Some(winwincode_control_plane::PageAnnotationArtifactRef {
                    artifact_id: screenshot_id.0.clone(),
                    digest: screenshot_digest.clone(),
                }),
            },
        })
        .expect("page annotation submit");
    let annotation = match receipt {
        winwincode_control_plane::CollaborationInboxReceipt::PageAnnotation {
            annotation, ..
        } => *annotation,
        other => panic!("unexpected receipt: {other:?}"),
    };
    assert_eq!(annotation.id, annotation_id);
    drop(inbox);

    let stored = SqliteStorage::open(&root).expect("reopen Delivery journal");
    let journal = stored
        .load_journal(
            &AggregateJournalKey::new("delivery", delivery_id.0.clone())
                .expect("Delivery journal key"),
        )
        .expect("load Delivery journal")
        .expect("persisted Delivery journal");
    let published = DeliveryJournalCodec::decode_record(
        &journal
            .records
            .last()
            .expect("Delivery journal tail")
            .payload,
    )
    .expect("published page-annotation Delivery record")
    .snapshot;
    assert_eq!(published.revision(), delivery.revision() + 1);

    let rework_journal = InMemoryDeliveryJournal::new();
    DeliveryStore::borrowed(&rework_journal)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-page-annotation-rework".into()),
            request_digest: "c".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed rework Delivery journal");
    DeliveryStore::borrowed(&rework_journal)
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: published.id().clone(),
            request_id: RequestId("req-page-annotation-rework".into()),
            request_digest: "d".repeat(64),
            operation: DeliveryMutationOperation::PageAnnotationRecorded,
            expected_revision: delivery.revision(),
            snapshot: published.clone(),
        }))
        .expect("append page annotation to rework journal");
    let rework_history = DeliveryStore::borrowed(&rework_journal)
        .validated_rework_history(&published)
        .expect("rebuild rework history from current Delivery tail");
    let rework_scope = CurrentReworkScope::from_candidate(&published, &candidate)
        .expect("page annotation enters current precise rework scope");
    let rework_annotation = rework_scope
        .annotations()
        .into_iter()
        .next()
        .expect("rework target");
    assert!(
        rework_annotation
            .evidence_ref_ids
            .contains(&annotation.evidence_id)
    );
    let ReworkDecision::Start(rework_authorization) = decide_precise_rework(
        &published,
        &candidate,
        &rework_scope,
        &[rework_annotation],
        &rework_history,
    )
    .expect("current page annotation authorizes precise rework") else {
        panic!("expected precise remediator authorization");
    };
    assert!(
        rework_authorization.targets()[0]
            .evidence_ref_ids()
            .contains(&annotation.evidence_id)
    );
    rework_authorization
        .validate_for_dispatch(&published)
        .expect("page annotation remains valid at remediator dispatch");
    drop(stored);

    let runtime_projection = runtime_projection_for(&published);
    let accepted_sequence = u64::from(published.snapshot().session_bindings.first().is_some_and(
        |binding| binding.worker_session_id.is_some() && binding.codex_thread_id.is_some(),
    ));
    let runtime = Arc::new(Mutex::new(
        TrustedRuntimeProjectionRead::try_new(
            scope.clone(),
            published.revision(),
            Revision(4),
            accepted_sequence,
            Instant("2026-08-25T00:00:00Z".into()),
            &runtime_projection,
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .expect("trusted page-annotation runtime"),
    ));
    let delivery_event_cursor = fixture_projection_cursor(
        &scope,
        ProjectionEventStream::Delivery(delivery_id.clone()),
        "evt_page_annotation_delivery_0001",
    );
    let publication = TrustedPublicationProjectionRead::try_new(
        scope.clone(),
        delivery_id.clone(),
        published.revision(),
        Revision(0),
        Some(candidate.clone()),
        None,
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
    )
    .expect("trusted page-annotation publication");

    let mut control_plane = ControlPlane::start_with_artifacts(
        Box::new(SqliteStorage::open(&root).expect("reopen e2e storage")),
        ArtifactStore::open(
            root.join("artifacts"),
            Box::new(LocalArtifactObjectStore::open(root.join("objects")).expect("object store")),
        )
        .expect("reopen screenshot artifact store"),
        Box::new(NoopPublisher),
    )
    .expect("e2e Control Plane");
    control_plane
        .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
            Box::new(RuntimeAdapter {
                read: runtime,
                race: Arc::new(Mutex::new(false)),
                read_count: Arc::new(Mutex::new(0)),
                expire_after_reads: None,
                unavailable: false,
                atomic_read_cut: true,
                delivery_event_cursor: Some(delivery_event_cursor),
                product_session_event_cursor: None,
            }),
            Box::new(PublicationAdapter {
                read: publication,
                unavailable: false,
            }),
        ))
        .expect("page-annotation projection sources");
    let (detail, cursor) = detail_and_cursor_for(&control_plane, &scope, &delivery_id);
    let detail_json = serde_json::to_value(&detail).expect("Delivery detail JSON");
    assert!(detail_json.to_string().contains("页面批注"));
    let evidence = detail
        .evidence
        .iter()
        .find(|evidence| evidence.source_ref == format!("page-annotation:{}", annotation_id.0))
        .expect("ReviewFinding Evidence in same journal");
    let detail_query = EvidenceGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: EvidenceReadBinding {
            at_cursor: cursor,
            candidate_ref: evidence.candidate_ref.clone(),
            delivery_id: delivery_id.clone(),
            evidence_id: evidence.id.clone(),
            read_page_limit: 20,
            session_binding_id: evidence.session_binding_id.clone(),
            source_ref: evidence.source_ref.clone(),
            type_value: "review_finding".into(),
            work_run_id: evidence.work_run_id.clone(),
        },
        query: EvidenceGetQueryQuery::EvidenceGet,
        request_id: RequestId("read-page-annotation-e2e".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    let response = StrongFlowProjectionQueryPort::evidence_get(&control_plane, &detail_query)
        .expect("Evidence detail");
    let QueryResultResponse::EvidenceGetResultResponse(response) = response else {
        panic!("Evidence detail response")
    };
    let page = response
        .result
        .page_annotation
        .expect("page annotation detail");
    assert_eq!(page.body, "头像按钮在窄屏下被遮挡");
    assert_eq!(page.target.page_path, "/settings/profile");
    let target = serde_json::to_value(&page.target).expect("annotation target JSON");
    assert_eq!(target["viewport"]["devicePixelRatio"], 2.0);
    assert_eq!(target["region"]["width"], 320);
    assert!(page.screenshot_artifact.is_some());
    let mut foreign = detail_query;
    foreign.parameters.source_ref = "page-annotation:foreign".into();
    assert!(StrongFlowProjectionQueryPort::evidence_get(&control_plane, &foreign).is_err());
    control_plane.shutdown().expect("Control Plane shutdown");
    fs::remove_dir_all(root).expect("e2e cleanup");
}

fn snapshot_revision(delivery: &Delivery) -> u64 {
    delivery.revision()
}

fn detail_and_cursor_for(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> (
    winwincode_api::generated::DeliveryDetailProjection,
    StrongFlowReadCursor,
) {
    let query = DeliveryGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: DeliveryGetParameters {
            at_cursor: None,
            delivery_id: delivery_id.clone(),
        },
        query: DeliveryGetQueryQuery::DeliveryGet,
        request_id: RequestId("read-page-annotation-delivery".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    let QueryResultResponse::DeliveryGetResultResponse(response) =
        StrongFlowProjectionQueryPort::delivery_get(control_plane, &query)
            .expect("Delivery detail")
    else {
        panic!("Delivery detail response")
    };
    let cursor = response.result.read_cursor.clone();
    (response.result, cursor)
}

fn runtime_query(
    f: &Fixture,
    cursor: StrongFlowReadCursor,
    limit: i64,
) -> RuntimeProjectionGetQuery {
    let binding = &f.delivery.snapshot().session_bindings[0];
    RuntimeProjectionGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit,
        },
        parameters: RuntimeProjectionGetParameters::WorkRunRuntimeProjectionGetParameters(
            WorkRunRuntimeProjectionGetParameters {
                at_cursor: cursor,
                delivery_id: f.delivery.id().clone(),
                kind: WorkRunRuntimeProjectionGetParametersKind::WorkRun,
                product_session_id: binding.product_session_id.clone(),
                work_run_id: binding.work_run_id.clone(),
            },
        ),
        query: RuntimeProjectionGetQueryQuery::RuntimeProjectionGet,
        request_id: RequestId("req_runtime".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: f.scope.clone(),
    }
}

fn product_session_runtime_query(f: &Fixture, scope: RepositoryScope) -> RuntimeProjectionGetQuery {
    RuntimeProjectionGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: RuntimeProjectionGetParameters::ProductSessionRuntimeProjectionGetParameters(
            ProductSessionRuntimeProjectionGetParameters {
                kind: ProductSessionRuntimeProjectionGetParametersKind::ProductSession,
                product_session_id: f.delivery.snapshot().session_bindings[0]
                    .product_session_id
                    .clone(),
            },
        ),
        query: RuntimeProjectionGetQueryQuery::RuntimeProjectionGet,
        request_id: RequestId("req_product_session_runtime".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn canonical_product_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId("org_01J00000000000000000000000".into()),
        workspace_id: WorkspaceId("wsp_01J00000000000000000000000".into()),
        project_id: ProjectId("prj_01J00000000000000000000000".into()),
        repository_id: RepositoryId("rep_01J00000000000000000000000".into()),
    }
}

fn product_scope_key(scope: &RepositoryScope) -> ReceiptScopeKey {
    let mut encoded = Vec::new();
    for value in [
        "winwincode.command-receipt.scope.v1",
        "repository",
        scope.organization_id.0.as_str(),
        scope.workspace_id.0.as_str(),
        scope.project_id.0.as_str(),
        scope.repository_id.0.as_str(),
    ] {
        encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    ReceiptScopeKey::from_encoded(encoded).expect("canonical product scope key")
}

fn product_runtime_state(
    product_session_id: &ProductSessionId,
    label: &str,
    event_count: u64,
) -> Vec<u8> {
    let events = (1..=event_count)
        .map(|sequence| {
            let event = ExecutionEventRecord {
                category: ExecutionEventCategory::Lifecycle,
                event_id: ExecutionEventId(format!("xevt_product_{label}_{sequence}")),
                occurred_at: Instant(format!("2026-08-25T00:00:{sequence:02}Z")),
                payload: None,
                sequence: ExecutionSequence(
                    i64::try_from(sequence).expect("fixture sequence fits in i64"),
                ),
                summary: format!("accepted product runtime event {sequence}"),
            };
            let event_digest = Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&event).expect("runtime event JSON"))
            ));
            serde_json::json!({
                "event": event,
                "eventDigest": event_digest,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "deliveryId": null,
        "workItemId": null,
        "workRunId": null,
        "productSessionId": product_session_id,
        "executionJobId": format!("job_product_{label}"),
        "workerSessionId": format!("wsn_product_{label}"),
        "codexThreadId": format!("cdx_product_{label}"),
        "leaseId": format!("lse_product_{label}"),
        "attempt": 1,
        "fencingToken": "1",
        "workerId": format!("wrk_product_{label}"),
        "workerInstanceId": format!("wki_product_{label}"),
        "highestSequence": event_count,
        "events": events,
    }))
    .expect("runtime ledger JSON")
}

fn commit_product_runtime_state(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    product_session_id: &ProductSessionId,
    label: &str,
    event_count: u64,
) {
    let stream_id = format!("product-session:{}", product_session_id.0);
    let public_actor = winwincode_storage::PublicEventActor::System {
        id: winwincode_domain::SystemActorId("sys_01J00000000000000000000000".into()),
    };
    let actor_key =
        winwincode_storage::receipt_actor_key(&public_actor).expect("receipt actor key");
    let receipt_identity = ReceiptIdentity::new(
        actor_key,
        product_scope_key(scope),
        RequestId(format!("req_product_runtime_{label}_{event_count}")),
    )
    .expect("receipt identity");
    let command_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(format!("product-runtime-{label}-{event_count}").as_bytes())
    ));
    storage
        .commit(&StateCommit::new(
            receipt_identity,
            command_digest,
            stream_id,
            event_count.saturating_sub(1),
            product_runtime_state(product_session_id, label, event_count),
            vec![
                NewOutboxEvent::public_projection(
                    ControlPlaneEventId(format!("evt_product_runtime_{label}_{event_count}")),
                    "runtime-projection.invalidated.v1",
                    b"{}".to_vec(),
                    ProjectionEventStream::ProductSession(product_session_id.clone()),
                    winwincode_storage::PublicEventScope::Repository {
                        organization_id: scope.organization_id.clone(),
                        workspace_id: scope.workspace_id.clone(),
                        project_id: scope.project_id.clone(),
                        repository_id: scope.repository_id.clone(),
                    },
                    Instant("2026-08-27T00:00:00.000Z".into()),
                    winwincode_storage::PublicEventSource::ControlPlane {
                        actor: public_actor,
                        component: "strongflow-projection-test".into(),
                    },
                )
                .expect("public runtime projection event"),
            ],
        ))
        .expect("commit product runtime state");
}

fn product_runtime_snapshot(
    control_plane: &ControlPlane,
    scope: RepositoryScope,
    product_session_id: ProductSessionId,
) -> winwincode_api::generated::RuntimeProjectionSnapshot {
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        control_plane,
        &RuntimeProjectionGetQuery {
            actor: actor(),
            page: PageRequest {
                cursor: None,
                limit: 20,
            },
            parameters:
                RuntimeProjectionGetParameters::ProductSessionRuntimeProjectionGetParameters(
                    ProductSessionRuntimeProjectionGetParameters {
                        kind: ProductSessionRuntimeProjectionGetParametersKind::ProductSession,
                        product_session_id,
                    },
                ),
            query: RuntimeProjectionGetQueryQuery::RuntimeProjectionGet,
            request_id: RequestId("req_product_runtime_public".into()),
            schema_version: SchemaVersion::WinwincodeV1,
            scope,
        },
    )
    .expect("product-session runtime projection");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime projection response")
    };
    response.result
}

fn assert_product_runtime_snapshot(
    snapshot: &winwincode_api::generated::RuntimeProjectionSnapshot,
    product_session_id: &ProductSessionId,
    label: &str,
    sequence: i64,
) {
    assert_eq!(&snapshot.product_session_id, product_session_id);
    assert_eq!(snapshot.revision, Revision(sequence));
    assert_eq!(snapshot.last_projection_sequence, sequence);
    assert!(snapshot.delivery_id.is_none());
    assert!(snapshot.work_run_id.is_none());
    assert!(snapshot.read_cursor.is_none());
    let [session] = snapshot.sessions.as_slice() else {
        panic!("one {label} ProductSession runtime")
    };
    assert_eq!(&session.product_session_id, product_session_id);
    assert_eq!(session.execution_job_id.0, format!("job_product_{label}"));
    assert_eq!(session.worker_session_id.0, format!("wsn_product_{label}"));
    assert_eq!(session.codex_thread_id.0, format!("cdx_product_{label}"));
    assert_eq!(session.lease_id.0, format!("lse_product_{label}"));
    assert_eq!(
        session.session_binding_id,
        format!("product-session-runtime:job_product_{label}")
    );
    assert_eq!(session.as_of_sequence, sequence);
    assert_eq!(session.attempt, 1);
    assert!(session.work_run_id.is_none());
}

#[test]
fn bounded_projection_replay_is_deterministic() {
    let f = fixture(false, false, false);
    let (first, cursor) = detail_and_cursor(&f);
    let second = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect("replay");
    let QueryResultResponse::DeliveryGetResultResponse(second) = second else {
        panic!("detail replay")
    };
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second.result).unwrap()
    );

    let historical = fixture_with_delivery(delivery_fixture(true), false, false, false, None);
    let (_, old_cursor) = detail_and_cursor(&historical);
    let mut next = historical.delivery.clone().into_snapshot();
    next.revision = 2;
    next.status = DeliveryStatus::Ready;
    next.spec.revision += 1;
    next.spec.id = winwincode_delivery::domain::DeliverySpecId("delivery-spec-v2".into());
    next.updated_at_millis += 1;
    next.spec.created_at_millis = next.updated_at_millis;
    let contract = &mut next.work_run_aggregate.contract;
    next.spec.goal = contract.objective.clone();
    next.spec.scope = contract.scope.clone();
    next.spec.out_of_scope = contract.protected_scope.clone();
    next.spec.constraints = contract.constraints.clone();
    next.spec
        .acceptance_criteria
        .truncate(contract.criteria.len());
    for (spec, criterion) in next
        .spec
        .acceptance_criteria
        .iter_mut()
        .zip(&contract.criteria)
    {
        spec.id = AcceptanceCriterionId(criterion.id.0.clone());
    }
    contract.revision = Revision(2);
    let next = Delivery::try_from_snapshot(next).expect("next revision");
    DeliveryStore::borrowed(historical.domain_journal.as_ref())
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: historical.delivery.id().clone(),
            request_id: RequestId("2".repeat(64)),
            request_digest: "2".repeat(64),
            operation: DeliveryMutationOperation::DeliverySpecUpdated,
            expected_revision: 1,
            snapshot: next,
        }))
        .expect("append current revision");
    let loaded = historical
        .domain_journal
        .load(historical.delivery.id())
        .unwrap()
        .unwrap();
    *historical.journal.lock().unwrap() = LoadedAggregateJournal {
        manifest: loaded.manifest,
        records: loaded
            .records
            .into_iter()
            .map(|record| AggregateJournalRecord::new(record.sequence, record.digest, record.bytes))
            .collect(),
    };
    StrongFlowProjectionQueryPort::delivery_get(
        &historical.control_plane,
        &delivery_query(&historical, Some(old_cursor), 20),
    )
    .expect("retained historical cut replays after current revision advances");
}

#[test]
fn delivery_get_rejects_a_retired_delivery_identity_at_the_public_seam() {
    let f = fixture(false, false, false);
    let mut query = delivery_query(&f, None, 20);
    query.parameters.delivery_id = DeliveryId("delivery-main".into());

    let error = StrongFlowProjectionQueryPort::delivery_get(&f.control_plane, &query)
        .expect_err("a retired Delivery identity must not reach storage or cursor creation");

    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::InvalidRequest
    );
}

#[test]
fn bounded_projection_cursor_rejects_a_changed_page_limit() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor.clone()), 200),
    )
    .expect_err("a cursor cannot be replayed with a different page limit");
    assert!(matches!(
        error,
        StrongFlowProjectionError::RevisionConflict(_)
    ));

    let error = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &runtime_query(&f, cursor, 1),
    )
    .expect_err("the paired runtime read must preserve the Delivery page limit");
    assert!(matches!(
        error,
        StrongFlowProjectionError::RevisionConflict(_)
    ));
}
#[test]
fn current_publication_requires_delivery_candidate_verdict_approval_and_target() {
    let f = fixture(false, false, false);
    let (detail, _) = detail_and_cursor(&f);
    assert!(
        detail.publication.is_none(),
        "no candidate/pass verdict/sealed approval means no publication authorization"
    );
}
#[test]
fn delivery_and_runtime_get_share_one_bounded_snapshot_cursor() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &runtime_query(&f, cursor.clone(), 20),
    )
    .expect("runtime");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime")
    };
    let snapshot = response.result;
    assert_eq!(snapshot.read_cursor, Some(cursor.clone()));
    assert_eq!(
        snapshot.event_cursor,
        RuntimeProjectionEventCursor::DeliveryEventReadCursor(cursor.event_cursor)
    );
}

#[test]
fn accepted_workrun_before_thread_attachment_exposes_an_empty_runtime_snapshot() {
    let mut snapshot = delivery_fixture(false).into_snapshot();
    let binding = snapshot
        .session_bindings
        .first_mut()
        .expect("accepted SessionBinding");
    binding.codex_thread_id = None;
    binding.runtime_context = None;
    binding.execution_profile = Some("executor".into());
    binding.source_provenance = serde_json::from_value(
        serde_json::json!({"kind": "work-run-dispatch", "reference": "workrun.appended"}),
    )
    .expect("accepted dispatch provenance");
    let run = snapshot
        .work_run_aggregate
        .runs
        .first_mut()
        .expect("accepted WorkRun");
    run.codex_thread_id = None;
    run.state = winwincode_domain::WorkRunState::Leased;
    let pending = Delivery::try_from_snapshot(snapshot).expect("pending Worker attachment");
    let f = fixture_with_delivery(pending, false, false, false, None);
    let empty = RuntimeProjection::new(&f.delivery, Vec::new()).expect("empty runtime projection");
    *f.runtime.lock().expect("runtime") = TrustedRuntimeProjectionRead::try_new(
        f.scope.clone(),
        f.delivery.revision(),
        Revision(0),
        0,
        Instant("1970-01-01T00:00:00.000Z".into()),
        &empty,
        Sha256Digest(format!("sha256:{}", "a".repeat(64))),
    )
    .expect("trusted empty runtime");

    let (detail, cursor) = detail_and_cursor(&f);
    assert!(
        serde_json::to_value(&detail)
            .expect("serialize detail")
            .get("stages")
            .is_none(),
        "the public projection has no historical stages"
    );
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &runtime_query(&f, cursor.clone(), 20),
    )
    .expect("pending runtime snapshot");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime")
    };
    assert!(response.result.sessions.is_empty());
    assert_eq!(
        response.result.work_run_id,
        Some(f.delivery.snapshot().work_run_aggregate.runs[0].id.clone())
    );
    assert_eq!(response.result.read_cursor, Some(cursor));
    assert_eq!(response.result.rebuilt_at.0, "1970-01-01T00:00:00.000Z");
}

#[test]
fn delivery_snapshot_does_not_skip_an_event_committed_after_its_source_read() {
    let f = fixture_with_delivery_and_event_behavior(
        delivery_fixture(false),
        None,
        false,
        false,
        false,
        None,
        EventCursorBehavior::AdvanceAfterRuntimeRead,
    );
    let response = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .expect("an event after the baseline remains available to the WebSocket subscriber");
    let QueryResultResponse::DeliveryGetResultResponse(response) = response else {
        panic!("delivery detail")
    };
    assert_eq!(response.result.read_cursor.event_cursor.sequence.0, 1);
    assert_eq!(
        response
            .result
            .read_cursor
            .event_cursor
            .event_id
            .expect("baseline event")
            .0,
        "evt_delivery_fixture_0001"
    );
}

#[test]
fn product_session_snapshot_has_its_own_exact_event_cursor() {
    let f = fixture(false, false, false);
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &product_session_runtime_query(&f, f.scope.clone()),
    )
    .expect("product-session runtime");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime")
    };
    let RuntimeProjectionEventCursor::ProductSessionEventReadCursor(cursor) =
        response.result.event_cursor
    else {
        panic!("product-session event cursor")
    };
    assert_eq!(cursor.scope, f.scope);
    assert_eq!(cursor.sequence.0, 1);
    assert_eq!(
        cursor.event_id.expect("event id").0,
        "evt_product_session_fixture_0001"
    );
}

#[test]
fn product_session_snapshot_does_not_skip_an_event_committed_after_its_source_read() {
    let f = fixture_with_delivery_and_event_behavior(
        delivery_fixture(false),
        None,
        false,
        false,
        false,
        None,
        EventCursorBehavior::AdvanceAfterRuntimeRead,
    );
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &product_session_runtime_query(&f, f.scope.clone()),
    )
    .expect("an event after the baseline remains available to the WebSocket subscriber");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime snapshot")
    };
    let RuntimeProjectionEventCursor::ProductSessionEventReadCursor(cursor) =
        response.result.event_cursor
    else {
        panic!("product-session cursor")
    };
    assert_eq!(cursor.sequence.0, 1);
    assert_eq!(
        cursor.event_id.expect("baseline event").0,
        "evt_product_session_fixture_0001"
    );
}

#[test]
fn public_sqlite_product_session_read_cut_isolated_and_restartable() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-control-plane-product-public-test-{}",
        std::process::id()
    ));
    let scope = canonical_product_scope();
    let first_id = ProductSessionId("psn_01J00000000000000000000000".into());
    let second_id = ProductSessionId("psn_01J00000000000000000000001".into());
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage");
    commit_product_runtime_state(&mut storage, &scope, &first_id, "first", 1);
    commit_product_runtime_state(&mut storage, &scope, &second_id, "second", 1);
    commit_product_runtime_state(&mut storage, &scope, &second_id, "second", 2);

    let mut control_plane =
        ControlPlane::start(Box::new(storage), Box::new(NoopPublisher)).expect("Control Plane");
    control_plane
        .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
            Box::new(SqliteTrustedRuntimeProjectionAdapter::from_sqlite_storage()),
            Box::new(UnavailablePublicationAdapter),
        ))
        .expect("projection sources");

    let first = product_runtime_snapshot(&control_plane, scope.clone(), first_id.clone());
    let second = product_runtime_snapshot(&control_plane, scope.clone(), second_id.clone());
    assert_product_runtime_snapshot(&first, &first_id, "first", 1);
    assert_product_runtime_snapshot(&second, &second_id, "second", 2);
    let _: winwincode_api::generated::RuntimeProjectionSnapshot = serde_json::from_value(
        serde_json::to_value(&first).expect("first ProductSession snapshot JSON"),
    )
    .expect("first ProductSession snapshot matches the generated public union");
    let _: winwincode_api::generated::RuntimeProjectionSnapshot = serde_json::from_value(
        serde_json::to_value(&second).expect("second ProductSession snapshot JSON"),
    )
    .expect("second ProductSession snapshot matches the generated public union");
    let (
        RuntimeProjectionEventCursor::ProductSessionEventReadCursor(first_cursor),
        RuntimeProjectionEventCursor::ProductSessionEventReadCursor(second_cursor),
    ) = (first.event_cursor, second.event_cursor)
    else {
        panic!("ProductSession event cursors");
    };
    assert_eq!(first_cursor.stream.product_session_id, first_id);
    assert_eq!(second_cursor.stream.product_session_id, second_id);
    assert_eq!(first_cursor.sequence.0, 1);
    assert_eq!(second_cursor.sequence.0, 2);
    assert_ne!(
        first_cursor.event_id.expect("first event id"),
        second_cursor.event_id.expect("second event id")
    );

    control_plane.shutdown().expect("Control Plane shutdown");
    let mut restarted = ControlPlane::start(
        Box::new(SqliteStorage::open(&root).expect("reopened SQLite storage")),
        Box::new(NoopPublisher),
    )
    .expect("restarted Control Plane");
    restarted
        .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
            Box::new(SqliteTrustedRuntimeProjectionAdapter::from_sqlite_storage()),
            Box::new(UnavailablePublicationAdapter),
        ))
        .expect("restarted projection sources");
    let first_after_restart = product_runtime_snapshot(&restarted, scope, first_id.clone());
    let second_after_restart =
        product_runtime_snapshot(&restarted, canonical_product_scope(), second_id.clone());
    assert_product_runtime_snapshot(&first_after_restart, &first_id, "first", 1);
    assert_eq!(first_after_restart.sessions, first.sessions);
    assert_product_runtime_snapshot(&second_after_restart, &second_id, "second", 2);
    assert_eq!(second_after_restart.sessions, second.sessions);
    restarted
        .shutdown()
        .expect("restarted Control Plane shutdown");
    fs::remove_dir_all(root).expect("temporary product runtime directory");
}

#[test]
fn delivery_projection_is_owned_by_delivery_and_maps_to_generated_dto() {
    let f = fixture(false, false, false);
    let (detail, _) = detail_and_cursor(&f);
    assert_eq!(detail.delivery_id, *f.delivery.id());
    assert_eq!(detail.ownership.repository_id, f.scope.repository_id);
}

#[test]
fn workrun_aggregate_bootstrap_reads_without_item_hint() {
    use winwincode_api::generated::{WorkRunGetParameters, WorkRunGetQuery, WorkRunGetQueryQuery};
    let snapshot = delivery_fixture(false).into_snapshot();
    let f = fixture_with_delivery(
        Delivery::try_from_snapshot(snapshot).expect("canonical Delivery without history"),
        false,
        false,
        false,
        None,
    );
    let (_, cursor) = detail_and_cursor(&f);
    let mut query = WorkRunGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: WorkRunGetParameters {
            at_cursor: Some(cursor.clone()),
            delivery_id: f.delivery.id().clone(),
            work_item_id: None,
            work_run_id: None,
        },
        query: WorkRunGetQueryQuery::WorkRunGet,
        request_id: RequestId("req_workrun_bootstrap".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: f.scope.clone(),
    };
    let result = StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect("Delivery-wide canonical read");
    let QueryResultResponse::WorkRunGetResultResponse(response) = result else {
        panic!("WorkRun aggregate response");
    };
    assert_eq!(response.result.read_cursor, cursor);
    assert_eq!(
        response.result.runs,
        f.delivery.snapshot().work_run_aggregate.runs
    );
    assert_eq!(response.result.device_bindings.len(), 1);
    assert_eq!(
        response.result.device_bindings[0].work_run_id,
        response.result.runs[0].id
    );
    assert_eq!(
        response.result.device_bindings[0].client_id.0,
        "123456789012"
    );
    assert_eq!(
        response.result.device_bindings[0].repository_binding_id.0,
        "rbd_00000000000000000000000001"
    );
    {
        let mut binding = f.device_binding.lock().expect("device binding");
        binding.as_mut().expect("binding facts").1.worker_session_id =
            "wsn_00000000000000000000000099".into();
    }
    StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect("the launch session and execution session are distinct identities");
    {
        let mut binding = f.device_binding.lock().expect("device binding");
        binding.as_mut().expect("binding facts").1.worker_id =
            "wrk_00000000000000000000000099".into();
    }
    let error = StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect_err("a binding for another Worker is rejected");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
    f.device_binding
        .lock()
        .expect("device binding")
        .as_mut()
        .expect("binding facts")
        .1
        .worker_id
        .clone_from(&response.result.runs[0].worker_id.0);
    StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect("the same committed WorkRun replays after trusted facts recover");
    assert!(!response.result.items.is_empty());
    assert_eq!(
        response.result.graph_items.len(),
        response.result.items.len()
    );
    assert_eq!(
        response.result.graph_items[0].work_item_id,
        response.result.items[0].id
    );
    let run = &response.result.runs[0];
    query.parameters.work_run_id = Some(run.id.clone());
    StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect("an exact WorkRun can be opened directly");
    query.parameters.work_item_id = Some(winwincode_domain::WorkItemId(
        "wit_00000000000000000000000099".into(),
    ));
    StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect_err("a foreign WorkItem remains rejected");
    query.parameters.work_item_id = None;
    query.parameters.work_run_id = Some(WorkRunId("wrn_00000000000000000000000099".into()));
    StrongFlowProjectionQueryPort::workrun_get(&f.control_plane, &query)
        .expect_err("a foreign WorkRun remains rejected");
}

#[test]
fn canonical_runtime_reads_without_history_and_rejects_a_foreign_lease() {
    let snapshot = delivery_fixture(false).into_snapshot();
    let f = fixture_with_delivery(
        Delivery::try_from_snapshot(snapshot).expect("canonical fixture without historical stages"),
        false,
        false,
        false,
        None,
    );
    let (detail, cursor) = detail_and_cursor(&f);
    assert!(
        serde_json::to_value(&detail)
            .expect("serialize detail")
            .get("stages")
            .is_none()
    );
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &runtime_query(&f, cursor, 20),
    )
    .expect("canonical WorkRun runtime");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime response");
    };
    assert_eq!(response.result.sessions.len(), 1);
    let session = &f.delivery.snapshot().session_bindings[0];
    assert_eq!(
        response.result.work_run_id.as_ref(),
        Some(&session.work_run_id)
    );

    // This test-only trusted source seals a different lease; the public read
    // still has to compare it against the persisted accepted assignment.
    let binding = accepted_binding(
        &f.delivery,
        &session.id,
        RuntimeAuthorityFixture::default(),
        Some(1),
    )
    .expect("test-only foreign runtime authority");
    let event = accepted_event(
        &binding,
        1,
        "event:foreign-lease",
        RuntimeFactFixture::Checkpoint,
    )
    .expect("test-only event");
    let projection = RuntimeProjection::replay(&f.delivery, vec![binding], &[event])
        .expect("test-only sealed runtime source");
    *f.runtime.lock().expect("runtime") = TrustedRuntimeProjectionRead::try_new(
        f.scope.clone(),
        f.delivery.revision(),
        Revision(4),
        1,
        Instant("2026-08-25T00:00:00Z".into()),
        &projection,
        Sha256Digest(format!("sha256:{}", "c".repeat(64))),
    )
    .expect("test-only trusted read");
    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .expect_err("foreign lease is rejected before publishing a read cursor");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::RevisionConflict
    );
}

#[test]
fn delivery_get_projects_current_diagram_execution_through_the_generated_contract() {
    let (delivery, candidate) = approved_verified_candidate_fixture();
    let f = fixture_with_delivery_and_candidate(delivery, candidate.clone());
    let (detail, _) = detail_and_cursor(&f);
    let current_candidate = detail.current_candidate.expect("current Candidate summary");
    let execution = detail
        .diagram_execution
        .expect("approved solution review has diagram execution facts");
    let details = execution
        .details
        .expect("finished execution has bounded current Candidate details");

    assert_eq!(execution.delivery_id, detail.delivery_id);
    assert_eq!(execution.delivery_revision, detail.delivery_revision);
    assert_eq!(execution.state, "execution-finished");
    assert_eq!(
        execution.review_set_sha256,
        detail.solution_review.unwrap().review_set_sha256
    );
    assert_eq!(details.candidate, current_candidate);
    assert_eq!(
        details.diff_sha256.0,
        format!("sha256:{}", candidate.diff_sha256())
    );
    assert_eq!(details.files.len(), 1);
    assert_eq!(details.files[0].path, "src/invitation.rs");
    assert_eq!(details.files[0].state, "present");
    assert_eq!(
        details.provenance.work_run_id,
        candidate.producer_work_run_id().clone()
    );
    assert_eq!(
        details.provenance.session_binding_id,
        candidate.producer_session_binding_id().0
    );
    assert_eq!(
        details.provenance.work_item_id,
        f.delivery
            .snapshot()
            .work_run_aggregate
            .runs
            .iter()
            .find(|run| &run.id == candidate.producer_work_run_id())
            .expect("Candidate producer WorkRun")
            .work_item_id
    );
    assert_eq!(execution.affected_file_count.0, 1);
}

#[test]
fn approved_solution_review_remains_visible_in_execution_and_verification_successors() {
    let approved = fixture_with_delivery(
        approved_solution_review_fixture(DeliveryStatus::Ready),
        false,
        false,
        false,
        None,
    );
    let expected = detail_and_cursor(&approved)
        .0
        .solution_review
        .expect("approved solution review");
    for status in [DeliveryStatus::Ready, DeliveryStatus::Reworking] {
        let successor = fixture_with_delivery(
            approved_solution_review_fixture(status),
            false,
            false,
            false,
            None,
        );
        let actual = detail_and_cursor(&successor)
            .0
            .solution_review
            .expect("the approved review remains the current solution authority");
        assert_eq!(actual, expected, "{status:?}");
    }
}

#[test]
fn approved_solution_review_survives_ready_delivery_review_and_delivery_settlement() {
    let baseline = fixture_with_delivery(
        approved_solution_review_fixture(DeliveryStatus::Ready),
        false,
        false,
        false,
        None,
    );
    let expected = detail_and_cursor(&baseline)
        .0
        .solution_review
        .expect("approved solution review");
    let (ready, candidate) = approved_verified_candidate_fixture();
    let ready_projection = detail_and_cursor(&fixture_with_delivery_and_candidate(
        ready.clone(),
        candidate.clone(),
    ))
    .0;
    assert_eq!(ready_projection.status, WorkItemState::CandidateReady);
    assert_eq!(ready_projection.solution_review, Some(expected.clone()));

    let mut review_snapshot = ready.clone().into_snapshot();
    let approval =
        winwincode_delivery::application::verdict::test_support::delivery_approval_fixture(
            &ready,
            1_800_000_000_080,
        );
    let approval_id = approval.id.clone();
    assert!(
        approval.work_run_id.is_some(),
        "DeliveryApproval is bound to the exact candidate producer WorkRun"
    );
    review_snapshot.attention_items.push(approval);
    review_snapshot.status = DeliveryStatus::NeedsAttention;
    review_snapshot.revision += 1;
    review_snapshot.updated_at_millis = 1_800_000_000_080;
    let delivery_review =
        Delivery::try_from_snapshot(review_snapshot).expect("canonical approval fixture");
    let review_projection = detail_and_cursor(&fixture_with_delivery_and_candidate(
        normalize_projection_read_revision(&delivery_review),
        candidate.clone(),
    ))
    .0;
    assert_eq!(review_projection.status, WorkItemState::WaitingHuman);
    assert_eq!(review_projection.solution_review, Some(expected.clone()));

    let approval = delivery_review
        .snapshot()
        .attention_items
        .iter()
        .find(|item| item.id == approval_id)
        .expect("DeliveryReview Attention");
    let resolve = |decision| {
        resolve_attention(
            &delivery_review,
            ResolveAttentionInput {
                expected_revision: delivery_review.revision(),
                attention_item_id: approval.id.clone(),
                work_run_id: approval.work_run_id.clone(),
                expected_context: approval.context.clone(),
                actor: "usr_approver".into(),
                decision,
                resolution: "reviewed exact candidate and verdict".into(),
                now_millis: 1_800_000_000_081,
            },
        )
        .expect("DeliveryReview settlement")
        .into_delivery()
    };
    for (settled, expected_status) in [
        (
            resolve(AttentionDecision::Dismissed),
            WorkItemState::CandidateReady,
        ),
        (resolve(AttentionDecision::Resolved), WorkItemState::Done),
    ] {
        let projection = detail_and_cursor(&fixture_with_delivery_and_candidate(
            normalize_projection_read_revision(&settled),
            candidate.clone(),
        ))
        .0;
        assert_eq!(projection.status, expected_status);
        assert_eq!(projection.solution_review, Some(expected.clone()));
    }
}

#[test]
fn approved_solution_review_rejects_states_outside_its_frozen_successor_set() {
    for status in [DeliveryStatus::Draft, DeliveryStatus::Clarifying] {
        let fixture = fixture_with_delivery(
            approved_solution_review_fixture(status),
            false,
            false,
            false,
            None,
        );
        let error = StrongFlowProjectionQueryPort::delivery_get(
            &fixture.control_plane,
            &delivery_query(&fixture, None, 20),
        )
        .expect_err("a settled approval is stale outside its explicit successor states");
        assert_eq!(
            error.code(),
            winwincode_api::generated::ErrorCode::TrustedFactsUnavailable,
            "{status:?}"
        );
    }
}

#[test]
fn missing_trusted_publication_adapter_keeps_production_query_closed() {
    let f = fixture(false, true, false);
    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .expect_err("publication adapter is required");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
}
#[test]
fn missing_trusted_runtime_adapter_keeps_production_query_closed() {
    let f = fixture(true, false, false);
    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .expect_err("runtime adapter is required");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
}
#[test]
fn public_projection_excludes_logs_credentials_payloads_and_live_diff_details() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &runtime_query(&f, cursor, 20),
    )
    .expect("runtime");
    let json = serde_json::to_string(&response).unwrap();
    for forbidden in [
        "stdout",
        "stderr",
        "credential",
        "toolPayload",
        "unifiedDiff",
        "filePath",
        "hunk",
    ] {
        assert!(!json.contains(forbidden), "{forbidden}");
    }
    assert!(json.contains("changedFileCount"));
    assert!(json.contains("\"detailsVisible\":false"));
}

#[test]
fn public_attention_projection_excludes_raw_context_and_resolution() {
    let mut snapshot = delivery_fixture(false).into_snapshot();
    let work_run_id = snapshot.work_run_aggregate.runs[0].id.clone();
    snapshot.attention_items.push(AttentionItem {
        schema_version: 3,
        id: AttentionItemId("attention-redaction".into()),
        delivery_id: snapshot.id.clone(),
        delivery_spec_id: snapshot.spec.id.clone(),
        work_run_id: Some(work_run_id),
        item_type: AttentionItemType::DecisionRequired,
        title: "Decision recorded".into(),
        context: "RAW_CONTEXT_SECRET_SENTINEL".into(),
        options: Vec::new(),
        assigned_to: Some("usr_reviewer".into()),
        blocking: false,
        status: AttentionItemStatus::Resolved,
        resolution: Some("RAW_RESOLUTION_SECRET_SENTINEL".into()),
        resolved_by: Some("usr_reviewer".into()),
        created_at_millis: 1_800_000_000_010,
        resolved_at_millis: Some(1_800_000_000_020),
    });
    let delivery = Delivery::try_from_snapshot(snapshot).expect("redaction fixture");
    let f = fixture_with_delivery(delivery, false, false, false, None);

    let response = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .expect("delivery detail");
    let json = serde_json::to_string(&response).expect("projection JSON");
    assert!(!json.contains("RAW_CONTEXT_SECRET_SENTINEL"));
    assert!(!json.contains("RAW_RESOLUTION_SECRET_SENTINEL"));
    assert!(json.contains("\"resolutionSummary\":\"resolved\""));
}
#[test]
fn raw_http_worker_and_websocket_facts_cannot_construct_projection() {
    let f = fixture(false, false, false);
    let query = runtime_query(&f, detail_and_cursor(&f).1, 20);
    let mut raw = serde_json::to_value(query).unwrap();
    raw.as_object_mut()
        .unwrap()
        .insert("workerFact".into(), serde_json::json!({"stdout":"secret"}));
    assert!(serde_json::from_value::<RuntimeProjectionGetQuery>(raw).is_err());
}
#[test]
fn stale_foreign_or_raced_projection_read_fails_closed() {
    let f = fixture(false, false, true);
    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StrongFlowProjectionError::ReadCursorExpired(_)
            | StrongFlowProjectionError::RevisionConflict(_)
    ));
}
#[test]
fn websocket_projection_events_use_only_committed_cursors() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    assert!(cursor.token.starts_with("sfc1_"));
    assert!(
        cursor
            .token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    );
    assert_eq!(cursor.event_cursor.sequence.0, 1);
    assert_eq!(
        cursor.event_cursor.event_id.as_ref().expect("event id").0,
        "evt_delivery_fixture_0001"
    );
    let start = ControlPlaneWebSocketSubscribeStartAt::EventReadCursor(
        EventReadCursor::DeliveryEventReadCursor(cursor.event_cursor.clone()),
    );
    let start_json = serde_json::to_value(start).expect("subscription cursor JSON");
    assert_eq!(
        start_json,
        serde_json::to_value(cursor.event_cursor).expect("HTTP cursor JSON")
    );
}

#[test]
fn event_cursor_is_part_of_the_authenticated_delivery_cut() {
    let f = fixture(false, false, false);
    let (_, mut cursor) = detail_and_cursor(&f);
    cursor.event_cursor.event_id = Some(ControlPlaneEventId("evt_delivery_fixture_forged".into()));

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("another durable event cannot be substituted under the same token");
    assert!(matches!(
        error,
        StrongFlowProjectionError::RevisionConflict(_)
    ));
}

#[test]
fn foreign_repository_scope_cannot_relabel_a_delivery_projection() {
    let f = fixture(false, false, false);
    let mut query = delivery_query(&f, None, 20);
    query.scope.repository_id = RepositoryId("rep_00000000000000000000000003".into());

    let error = StrongFlowProjectionQueryPort::delivery_get(&f.control_plane, &query)
        .expect_err("trusted facts must prove the exact repository scope");
    assert!(matches!(
        error,
        StrongFlowProjectionError::PermissionDenied(_)
            | StrongFlowProjectionError::RevisionConflict(_)
    ));
}

#[test]
fn foreign_repository_scope_cannot_read_a_product_session_projection() {
    let f = fixture(false, false, false);
    let mut foreign = f.scope.clone();
    foreign.repository_id = RepositoryId("rep_00000000000000000000000003".into());

    let error = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &product_session_runtime_query(&f, foreign),
    )
    .expect_err("product-session runtime facts must prove the exact repository scope");
    assert!(matches!(
        error,
        StrongFlowProjectionError::PermissionDenied(_)
            | StrongFlowProjectionError::RevisionConflict(_)
    ));
}

#[test]
fn forged_future_delivery_revision_is_not_reported_as_retention_loss() {
    let f = fixture(false, false, false);
    let (_, mut cursor) = detail_and_cursor(&f);
    cursor.delivery_revision = Revision(cursor.delivery_revision.0 + 100);

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("a future revision cannot be a previously retained cut");
    assert!(!matches!(
        error,
        StrongFlowProjectionError::ReadCursorExpired(_)
    ));
}

#[test]
fn mismatched_runtime_cursor_is_not_reported_as_retention_loss() {
    let f = fixture(false, false, false);
    let (_, mut cursor) = detail_and_cursor(&f);
    cursor.runtime_accepted_sequence += 1;

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("a mismatched cut must fail as stale or invalid");
    assert!(!matches!(
        error,
        StrongFlowProjectionError::ReadCursorExpired(_)
    ));
}

#[test]
fn malformed_cursor_token_fails_before_it_can_name_a_trusted_cut() {
    let f = fixture(false, false, false);
    let (_, mut cursor) = detail_and_cursor(&f);
    cursor.token = "sfc1_not-a-canonical-seal".into();

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("a malformed token is not an authorized read cursor");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::InvalidRequest
    );
}

#[test]
fn only_an_exact_cut_removed_from_retention_reports_cursor_expired() {
    let f = fixture_with_delivery(delivery_fixture(false), false, false, false, Some(2));
    let (_, cursor) = detail_and_cursor(&f);

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("the adapter explicitly reports a formerly issued cut was removed");
    assert!(matches!(
        error,
        StrongFlowProjectionError::ReadCursorExpired(_)
    ));
}

#[test]
fn cursor_rejects_rewritten_delivery_content_at_the_same_revision() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    let mut changed = f.delivery.clone().into_snapshot();
    changed.spec.title = "Rewritten title at the same revision".into();
    let changed = Delivery::try_from_snapshot(changed).expect("valid rewritten delivery");
    let replacement = InMemoryDeliveryJournal::new();
    DeliveryStore::borrowed(&replacement)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("3".repeat(64)),
            request_digest: "3".repeat(64),
            snapshot: changed,
        }))
        .expect("seed replacement journal");
    let loaded = replacement
        .load(f.delivery.id())
        .expect("load replacement")
        .expect("replacement journal");
    *f.journal.lock().expect("journal") = LoadedAggregateJournal {
        manifest: loaded.manifest,
        records: loaded
            .records
            .into_iter()
            .map(|record| AggregateJournalRecord::new(record.sequence, record.digest, record.bytes))
            .collect(),
    };

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("a cursor must seal the exact canonical Delivery content");
    assert!(matches!(
        error,
        StrongFlowProjectionError::RevisionConflict(_)
    ));
}

#[test]
fn cursor_rejects_changed_runtime_content_behind_reused_source_seal() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    let current = f.runtime.lock().expect("runtime read").clone();
    let replacement = TrustedRuntimeProjectionRead::try_new(
        f.scope.clone(),
        current.delivery_revision(),
        current.ledger_revision().clone(),
        current.accepted_sequence(),
        Instant("2026-08-25T00:00:01Z".into()),
        &runtime_projection_for(&f.delivery),
        Sha256Digest(format!("sha256:{}", "a".repeat(64))),
    )
    .expect("replacement runtime read");
    *f.runtime.lock().expect("runtime read") = replacement;

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 20),
    )
    .expect_err("a cursor must seal runtime content, not trust a reused owner seal");
    assert!(matches!(
        error,
        StrongFlowProjectionError::RevisionConflict(_)
    ));
}

#[test]
fn runtime_projection_selects_the_exact_complete_workrun_binding() {
    let f = fixture(false, false, false);
    let (_, cursor) = detail_and_cursor(&f);
    let response = StrongFlowProjectionQueryPort::runtime_projection_get(
        &f.control_plane,
        &runtime_query(&f, cursor, 20),
    )
    .expect("runtime projection");
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        panic!("runtime projection response")
    };
    let expected = f
        .delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .last()
        .expect("complete WorkRun")
        .id
        .clone();
    assert_eq!(response.result.work_run_id, Some(expected.clone()));
    assert_eq!(response.result.sessions.len(), 1);
    assert_eq!(response.result.sessions[0].work_run_id, Some(expected));
}
