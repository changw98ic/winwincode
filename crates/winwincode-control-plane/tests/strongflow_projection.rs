// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketSubscribeStartAt, DeliveryGetParameters, DeliveryGetQuery,
    DeliveryGetQueryQuery, DeliveryStageRuntimeProjectionGetParameters,
    DeliveryStageRuntimeProjectionGetParametersKind, EventReadCursor, PageRequest,
    ProductSessionRuntimeProjectionGetParameters, ProductSessionRuntimeProjectionGetParametersKind,
    QueryResultResponse, RepositoryScope, RepositoryScopeKind, RuntimeProjectionEventCursor,
    RuntimeProjectionGetParameters, RuntimeProjectionGetQuery, RuntimeProjectionGetQueryQuery,
    SchemaVersion, StrongFlowReadCursor, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    AggregateJournalKey, AggregateJournalRecord, CommitReceipt, ControlPlane, EventPublishError,
    EventPublisher, LoadedAggregateJournal, OutboxEvent, ProductStateStorage,
    ProjectionEventCursor, ProjectionEventStream, StorageError, StoredState,
    strongflow_projection::{
        DeliveryRuntimeReadRequest, ProductSessionRuntimeReadRequest, StrongFlowProjectionError,
        StrongFlowProjectionQueryPort, StrongFlowProjectionSources, TrustedProjectionReadError,
        TrustedPublicationProjectionAdapter, TrustedPublicationProjectionRead,
        TrustedRuntimeProjectionAdapter, TrustedRuntimeProjectionRead,
    },
};
use winwincode_delivery::{
    domain::{AttentionItem, AttentionItemStatus, AttentionItemType, Delivery, DeliveryStatus},
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
    AttentionItemId, ControlPlaneEventId, DeliveryId, Instant, OrganizationId, ProjectId,
    RepositoryId, RequestId, Revision, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{ReceiptIdentity, StateCommit};

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
    fn commit(&mut self, _commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
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
    fn load_journal(
        &self,
        _key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        Ok(Some(self.journal.lock().expect("journal").clone()))
    }
    fn load_projection_event_cursor(
        &self,
        key: &winwincode_control_plane::ProjectionEventStreamKey,
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

#[derive(Clone)]
struct RuntimeAdapter {
    read: Arc<Mutex<TrustedRuntimeProjectionRead>>,
    race: Arc<Mutex<bool>>,
    read_count: Arc<Mutex<usize>>,
    expire_after_reads: Option<usize>,
    unavailable: bool,
}
impl TrustedRuntimeProjectionAdapter for RuntimeAdapter {
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
        if request.delivery_id() != &read.snapshot().delivery_id
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
        Ok(read.clone())
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
            Ok(read.clone())
        }
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
    let Some(session) = delivery.snapshot().session_bindings.first() else {
        return RuntimeProjection::new(delivery, Vec::new()).expect("empty runtime");
    };
    let binding = accepted_binding(
        delivery,
        &session.id,
        RuntimeAuthorityFixture::default(),
        Some(1),
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
        runtime_unavailable,
        publication_unavailable,
        race,
        expire_after_reads,
        EventCursorBehavior::Stable,
    )
}

#[allow(clippy::too_many_lines)]
fn fixture_with_delivery_and_event_behavior(
    delivery: Delivery,
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
    let accepted_sequence = u64::from(!delivery.snapshot().session_bindings.is_empty());
    let projection = runtime_projection_for(&delivery);
    let runtime = Arc::new(Mutex::new(
        TrustedRuntimeProjectionRead::try_new(
            scope.clone(),
            delivery.revision(),
            Revision(4),
            accepted_sequence,
            Instant("2026-08-25T00:00:00Z".into()),
            projection,
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .expect("trusted runtime"),
    ));
    let publication = TrustedPublicationProjectionRead::try_new(
        scope.clone(),
        delivery.id().clone(),
        delivery.revision(),
        Revision(0),
        None,
        None,
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
    )
    .expect("trusted publication");
    let journal = Arc::new(Mutex::new(aggregate));
    let runtime_read_count = Arc::new(Mutex::new(0));
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
            Box::new(RuntimeAdapter {
                read: Arc::clone(&runtime),
                race: Arc::new(Mutex::new(false)),
                read_count: runtime_read_count,
                expire_after_reads,
                unavailable: runtime_unavailable,
            }),
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
        snapshot.stage_runs.clear();
        snapshot.session_bindings.clear();
        snapshot.attention_items.clear();
    }
    Delivery::try_from_snapshot(snapshot).expect("projection fixture")
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
                stage_run_id: binding.stage_run_id.clone(),
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
fn delivery_snapshot_does_not_skip_an_event_committed_after_its_source_read() {
    let f = fixture_with_delivery_and_event_behavior(
        delivery_fixture(false),
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
fn delivery_projection_is_owned_by_delivery_and_maps_to_generated_dto() {
    let f = fixture(false, false, false);
    let (detail, _) = detail_and_cursor(&f);
    assert_eq!(detail.delivery_id, *f.delivery.id());
    assert_eq!(detail.ownership.repository_id, f.scope.repository_id);
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
    let stage_run_id = snapshot.stage_runs[0].id.clone();
    snapshot.attention_items.push(AttentionItem {
        schema_version: 3,
        id: AttentionItemId("attention-redaction".into()),
        delivery_id: snapshot.id.clone(),
        delivery_spec_id: snapshot.spec.id.clone(),
        stage_run_id: Some(stage_run_id),
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
        runtime_projection_for(&f.delivery),
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
