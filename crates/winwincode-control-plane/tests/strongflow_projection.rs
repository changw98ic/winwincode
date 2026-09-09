// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketSubscribeStartAt, DeliveryGetParameters, DeliveryGetQuery,
    DeliveryGetQueryQuery, DeliveryStageRuntimeProjectionGetParameters,
    DeliveryStageRuntimeProjectionGetParametersKind, EventReadCursor, PageRequest,
    ProductSessionRuntimeProjectionGetParameters, ProductSessionRuntimeProjectionGetParametersKind,
    QueryResultResponse, RuntimeProjectionEventCursor, RuntimeProjectionGetParameters,
    RuntimeProjectionGetQuery, RuntimeProjectionGetQueryQuery, StrongFlowReadCursor,
};
use winwincode_control_plane::{
    AggregateJournalKey, AggregateJournalRecord, CommitReceipt, ControlPlane, EventPublishError,
    EventPublisher, LoadedAggregateJournal, NewOutboxEvent, OutboxEvent, ProductStateStorage,
    ProjectionEventCursor, ProjectionEventStream, ProjectionEventStreamKey, StorageError,
    StoredState,
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
    domain::{
        AcceptanceCriterionId, AttentionItem, AttentionItemStatus, AttentionItemType,
        CandidatePathFact, CandidatePathState, Delivery, DeliveryStage, DeliveryStatus,
        FrozenDeliveryCandidate, SessionBindingId, StageRun, StageRunActorType, StageRunStatus,
        candidate::{
            CandidateHunkFact,
            test_support::{CandidateFixtureInput, freeze_candidate_fixture},
        },
    },
    projection::runtime::{
        RuntimeProjection,
        test_support::{
            RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
        },
    },
    store::{
        AppendDelivery, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort,
        DeliveryMutationOperation, DeliveryStore, InMemoryDeliveryJournal,
    },
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, ControlPlaneEventId, DeliveryId, DeliveryTaskId,
    ExecutionEventId, ExecutionJobId, ExecutionSequence, Instant, OrganizationId, ProductSessionId,
    ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest, StageRunId, UserId,
    WorkRunId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::generated::{ExecutionEventCategory, ExecutionEventRecord};
use winwincode_storage::{
    ProjectionReadCut, ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit,
};

// PublicationAuthorizationSnapshot is deliberately not constructible from HTTP input.
// Missing sources return TRUSTED_FACTS_UNAVAILABLE.
// WebSocket `runtime-projection.invalidated.v1` is only an invalidation; these reads
// expose complete committed snapshots.

#[derive(Clone)]
struct JournalStorage {
    journal: Arc<Mutex<LoadedAggregateJournal>>,
    runtime_read_count: Arc<Mutex<usize>>,
    advance_event_after_runtime_read: bool,
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
    fn load_state(&self, _stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        Ok(None)
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
    domain_journal: Arc<InMemoryDeliveryJournal>,
    runtime: Arc<Mutex<TrustedRuntimeProjectionRead>>,
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
    let runtime_read_count = Arc::new(Mutex::new(0));
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
    let mut control_plane = ControlPlane::start(
        Box::new(JournalStorage {
            journal: Arc::clone(&journal),
            runtime_read_count: Arc::clone(&runtime_read_count),
            advance_event_after_runtime_read: matches!(
                event_cursor_behavior,
                EventCursorBehavior::AdvanceAfterRuntimeRead
            ),
        }),
        Box::new(NoopPublisher),
    )
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
        domain_journal: memory,
        runtime,
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
        DeliveryStatus::Verifying
    };
    snapshot.evidence.clear();
    snapshot.verdict = None;
    if draft {
        snapshot.tasks.clear();
        snapshot.work_run_aggregate.runs.clear();
        snapshot.work_run_aggregate.items.clear();
        snapshot.stage_runs.clear();
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
fn approved_ready_to_deliver_fixture() -> (Delivery, FrozenDeliveryCandidate) {
    let approved = approved_solution_review_fixture(DeliveryStatus::Executing).into_snapshot();
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("passing Delivery fixture")
    .into_snapshot();

    let promoted_task = snapshot.tasks.first_mut().expect("promoted solution task");
    promoted_task.id = DeliveryTaskId("task:invitation".into());
    promoted_task.title = "Implement invitation flow".into();
    promoted_task.goal = "Deliver every current acceptance criterion.".into();
    promoted_task.acceptance_criterion_ids = vec![
        AcceptanceCriterionId("criterion-required".into()),
        AcceptanceCriterionId("criterion-optional".into()),
    ];
    promoted_task.blocked_by_task_ids.clear();
    snapshot.stage_runs = approved.stage_runs;
    snapshot.session_bindings = approved.session_bindings;
    snapshot.attention_items = approved.attention_items;
    snapshot.work_run_aggregate = approved.work_run_aggregate;
    snapshot.work_run_aggregate.runs[0].state = winwincode_domain::WorkRunState::Settled;

    let executor_stage_run_id = StageRunId("stage:executor".into());
    snapshot.stage_runs.push(StageRun {
        schema_version: 3,
        id: executor_stage_run_id.clone(),
        delivery_id: snapshot.id.clone(),
        delivery_task_id: Some(snapshot.tasks[0].id.clone()),
        stage: DeliveryStage::Executing,
        actor_type: StageRunActorType::Codex,
        role: "executor".into(),
        status: StageRunStatus::Succeeded,
        attempt: 1,
        started_at_millis: 1_800_000_000_040,
        finished_at_millis: Some(1_800_000_000_050),
    });

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
        run.lease_id = winwincode_domain::LeaseId(format!("lse_01J000000000000000000000{suffix}"));
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
        binding.bound_at_millis = bound_at;
        snapshot.work_run_aggregate.runs.push(run);
        snapshot.session_bindings.push(binding);
    }
    snapshot.work_run_aggregate.items[0].state = winwincode_domain::WorkItemState::CandidateReady;
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
    let ready = Delivery::try_from_snapshot(snapshot).expect("ready-to-deliver lifecycle fixture");
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
        parameters: RuntimeProjectionGetParameters::DeliveryStageRuntimeProjectionGetParameters(
            DeliveryStageRuntimeProjectionGetParameters {
                at_cursor: cursor,
                delivery_id: f.delivery.id().clone(),
                kind: DeliveryStageRuntimeProjectionGetParametersKind::DeliveryStage,
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
        "deliveryTaskId": null,
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
    snapshot.stage_runs.clear();
    let binding = snapshot
        .session_bindings
        .first_mut()
        .expect("accepted SessionBinding");
    binding.codex_thread_id = None;
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
        detail.stages.is_empty(),
        "no historical stage is needed to select a WorkRun"
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
    std::fs::remove_dir_all(root).expect("temporary product runtime directory");
}

#[test]
fn delivery_projection_is_owned_by_delivery_and_maps_to_generated_dto() {
    let f = fixture(false, false, false);
    let (detail, _) = detail_and_cursor(&f);
    assert_eq!(detail.delivery_id, *f.delivery.id());
    assert_eq!(detail.ownership.repository_id, f.scope.repository_id);
}

#[test]
fn workrun_aggregate_bootstrap_reads_without_a_work_item_or_stage_hint() {
    use winwincode_api::generated::{WorkRunGetParameters, WorkRunGetQuery, WorkRunGetQueryQuery};
    let mut snapshot = delivery_fixture(false).into_snapshot();
    snapshot.stage_runs.clear();
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
    let mut snapshot = delivery_fixture(false).into_snapshot();
    snapshot.stage_runs.clear();
    let f = fixture_with_delivery(
        Delivery::try_from_snapshot(snapshot).expect("canonical fixture without historical stages"),
        false,
        false,
        false,
        None,
    );
    let (detail, cursor) = detail_and_cursor(&f);
    assert!(detail.stages.is_empty());
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
    let (delivery, candidate) = approved_ready_to_deliver_fixture();
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
        approved_solution_review_fixture(DeliveryStatus::Executing),
        false,
        false,
        false,
        None,
    );
    let expected = detail_and_cursor(&approved)
        .0
        .solution_review
        .expect("approved solution review");
    for status in [DeliveryStatus::Verifying, DeliveryStatus::Reworking] {
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
        approved_solution_review_fixture(DeliveryStatus::Executing),
        false,
        false,
        false,
        None,
    );
    let expected = detail_and_cursor(&baseline)
        .0
        .solution_review
        .expect("approved solution review");
    let (ready, candidate) = approved_ready_to_deliver_fixture();
    let ready_projection = detail_and_cursor(&fixture_with_delivery_and_candidate(
        ready.clone(),
        candidate.clone(),
    ))
    .0;
    assert_eq!(
        ready_projection.status,
        winwincode_api::generated::DeliveryStatus::ReadyToDeliver
    );
    assert_eq!(ready_projection.solution_review, Some(expected.clone()));

    let mut review_snapshot = ready.clone().into_snapshot();
    let approval =
        winwincode_delivery::application::verdict::test_support::delivery_approval_fixture(
            &ready,
            1_800_000_000_080,
        );
    let approval_id = approval.id.clone();
    assert!(
        approval.work_run_id.is_none(),
        "a human approval is not an execution"
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
    assert_eq!(
        review_projection.status,
        winwincode_api::generated::DeliveryStatus::NeedsAttention
    );
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
            winwincode_api::generated::DeliveryStatus::Reworking,
        ),
        (
            resolve(AttentionDecision::Resolved),
            winwincode_api::generated::DeliveryStatus::Delivered,
        ),
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
    for status in [
        DeliveryStatus::Draft,
        DeliveryStatus::Ready,
        DeliveryStatus::Planning,
        DeliveryStatus::PlanReview,
        DeliveryStatus::Clarifying,
    ] {
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
fn delivery_get_rejects_a_codex_stage_without_one_exact_session_binding() {
    let mut snapshot = delivery_fixture(false).into_snapshot();
    snapshot.session_bindings.clear();
    let delivery = Delivery::try_from_snapshot(snapshot)
        .expect("the canonical aggregate can represent a pre-dispatch Codex stage");
    let f = fixture_with_delivery(delivery, false, false, false, None);

    let error = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, None, 20),
    )
    .expect_err("the public detail cannot emit codex plus a null SessionBinding");

    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
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
