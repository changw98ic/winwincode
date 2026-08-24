// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use winwincode_api::generated::{
    Actor, DeliveryGetParameters, DeliveryGetQuery, DeliveryGetQueryQuery,
    DeliveryStageRuntimeProjectionGetParameters, DeliveryStageRuntimeProjectionGetParametersKind,
    PageRequest, QueryResult, QueryResultResponse, RepositoryScope, RepositoryScopeKind,
    RuntimeProjectionGetParameters, RuntimeProjectionGetQuery, RuntimeProjectionGetQueryQuery,
    SchemaVersion, Scope, StrongFlowReadCursor, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    AggregateJournalKey, AggregateJournalRecord, CommitReceipt, ControlPlane, EventPublishError,
    EventPublisher, LoadedAggregateJournal, OutboxEvent, ProductStateStorage, StorageError,
    StoredState,
    strongflow_projection::{
        DeliveryRuntimeReadRequest, StrongFlowProjectionError, StrongFlowProjectionQueryPort,
        StrongFlowProjectionSources, TrustedProjectionReadError,
        TrustedPublicationProjectionAdapter, TrustedPublicationProjectionRead,
        TrustedRuntimeProjectionAdapter, TrustedRuntimeProjectionRead,
    },
};
use winwincode_delivery::{
    domain::{Delivery, DeliveryStatus},
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
    DeliveryId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Revision, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{ReceiptIdentity, StateCommit};

// PublicationAuthorizationSnapshot is deliberately not constructible from HTTP input.
// Missing sources return TRUSTED_FACTS_UNAVAILABLE.
// WebSocket `runtime-projection.invalidated.v1` is only an invalidation; these reads
// expose complete committed snapshots.

#[derive(Clone)]
struct JournalStorage {
    journal: Arc<Mutex<LoadedAggregateJournal>>,
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
    read: TrustedRuntimeProjectionRead,
    race: Arc<Mutex<bool>>,
    unavailable: bool,
}
impl TrustedRuntimeProjectionAdapter for RuntimeAdapter {
    fn read_delivery(
        &self,
        request: &DeliveryRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        if self.unavailable {
            return Err(TrustedProjectionReadError::Unavailable);
        }
        if request.delivery_id() != &self.read.snapshot().delivery_id
            || request.delivery_revision() != self.read.delivery_revision()
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        if request.expected().is_some_and(|expected| {
            expected.ledger_revision() != self.read.ledger_revision()
                || expected.accepted_sequence() != self.read.accepted_sequence()
        }) {
            return Err(TrustedProjectionReadError::Stale);
        }
        let mut raced = self.race.lock().expect("race");
        if *raced {
            return Err(TrustedProjectionReadError::Stale);
        }
        if request.expected().is_none() && request.scope().repository_id.0 == "rep_race" {
            *raced = true;
        }
        Ok(self.read.clone())
    }
    fn read_product_session(
        &self,
        _scope: &RepositoryScope,
        product_session_id: &ProductSessionId,
        _limit: usize,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        if self.unavailable
            || !self
                .read
                .snapshot()
                .sessions
                .iter()
                .any(|s| &s.product_session_id == product_session_id)
        {
            Err(TrustedProjectionReadError::Unavailable)
        } else {
            Ok(self.read.clone())
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
        if delivery_id.0 != "delivery-main"
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
}

fn fixture(runtime_unavailable: bool, publication_unavailable: bool, race: bool) -> Fixture {
    fixture_with_delivery(
        delivery_fixture(false),
        runtime_unavailable,
        publication_unavailable,
        race,
    )
}

#[allow(clippy::too_many_lines)]
fn fixture_with_delivery(
    delivery: Delivery,
    runtime_unavailable: bool,
    publication_unavailable: bool,
    race: bool,
) -> Fixture {
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
    let (projection, accepted_sequence) = if let Some(session) =
        delivery.snapshot().session_bindings.first()
    {
        let binding = accepted_binding(
            &delivery,
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
        (
            RuntimeProjection::replay(&delivery, vec![binding], &[event]).expect("runtime replay"),
            1,
        )
    } else {
        (
            RuntimeProjection::new(&delivery, Vec::new()).expect("empty runtime"),
            0,
        )
    };
    let runtime = TrustedRuntimeProjectionRead::try_new(
        delivery.revision(),
        Revision(4),
        accepted_sequence,
        Instant("2026-08-25T00:00:00Z".into()),
        projection,
        Sha256Digest(format!("sha256:{}", "a".repeat(64))),
    )
    .expect("trusted runtime");
    let publication = TrustedPublicationProjectionRead::try_new(
        delivery.revision(),
        Revision(0),
        None,
        None,
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
    )
    .expect("trusted publication");
    let scope = RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId("org_fixture".into()),
        workspace_id: WorkspaceId("wsp_fixture".into()),
        project_id: ProjectId("prj_fixture".into()),
        repository_id: RepositoryId(if race { "rep_race" } else { "rep_fixture" }.into()),
    };
    let journal = Arc::new(Mutex::new(aggregate));
    let mut control_plane = ControlPlane::start(
        Box::new(JournalStorage {
            journal: Arc::clone(&journal),
        }),
        Box::new(NoopPublisher),
    )
    .expect("control plane");
    control_plane
        .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
            Box::new(RuntimeAdapter {
                read: runtime,
                race: Arc::new(Mutex::new(false)),
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
        scope: Scope::RepositoryScope(f.scope.clone()),
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
    let QueryResult::DeliveryDetailProjection(detail) = response.result else {
        panic!("detail")
    };
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
        scope: Scope::RepositoryScope(f.scope.clone()),
    }
}

#[test]
fn bounded_projection_replay_is_deterministic() {
    let f = fixture(false, false, false);
    let (first, cursor) = detail_and_cursor(&f);
    let second = StrongFlowProjectionQueryPort::delivery_get(
        &f.control_plane,
        &delivery_query(&f, Some(cursor), 100),
    )
    .expect("replay");
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second.result).unwrap()
    );

    let historical = fixture_with_delivery(delivery_fixture(true), false, false, false);
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
        &delivery_query(&historical, Some(old_cursor), 3),
    )
    .expect("retained historical cut replays after current revision advances");
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
        &runtime_query(&f, cursor.clone(), 1),
    )
    .expect("runtime");
    let QueryResult::RuntimeProjectionSnapshot(snapshot) = response.result else {
        panic!("runtime")
    };
    assert_eq!(snapshot.read_cursor, Some(cursor));
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
}
