#![cfg(feature = "test-support")]
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use winwincode_api::generated::{CommandEnvelope, WorkItemsCreateCommand};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent, StateChange,
};
use winwincode_delivery::{
    domain::Delivery,
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{DeliveryId, RequestId, Sha256Digest};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, PublicEventActor, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit, receipt_actor_key,
};
#[derive(Default)]
struct CapturingJournal {
    publication: Mutex<Option<AtomicPublication>>,
}

impl DeliveryJournalPort for CapturingJournal {
    fn load(
        &self,
        _delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        Ok(None)
    }

    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        *self.publication.lock().expect("publication lock") = Some(publication);
        Ok(())
    }
}

fn seed_delivery(root: &PathBuf, delivery: &Delivery) {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("c".repeat(64)),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal publication");
    let publication = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("seed publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = publication
    else {
        panic!("seed must create the Delivery journal");
    };
    let publication = AggregateJournalPublication::Create {
        key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
        manifest,
        first_record: AggregateJournalRecord::new(
            first_record.sequence,
            first_record.digest,
            first_record.bytes,
        ),
    };
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    let receipt = storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"seed-actor".to_vec()).expect("seed actor"),
                    ReceiptScopeKey::from_encoded(b"seed-scope".to_vec()).expect("seed scope"),
                    RequestId("c".repeat(64)),
                )
                .expect("seed identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("seed Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    "seed-event",
                    "delivery.seeded",
                    b"seed".to_vec(),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed transaction");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed event acknowledgement");
    Box::new(storage).close().expect("seed storage close");
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogEntry<'a> {
    schema_version: u32,
    repository_scope: &'a winwincode_domain::RepositoryScope,
    delivery_id: &'a DeliveryId,
}

fn seed_catalog(root: &Path, scope: &winwincode_domain::RepositoryScope, delivery: &Delivery) {
    let payload = serde_json::to_vec(&CatalogEntry {
        schema_version: 1,
        repository_scope: scope,
        delivery_id: delivery.id(),
    })
    .unwrap();
    let stream = format!(
        "delivery-catalog:{:x}:{}",
        Sha256::digest(serde_json::to_vec(scope).unwrap()),
        delivery.id().0
    );
    let scope_key = ReceiptScopeKey::from_encoded(serde_json::to_vec(scope).unwrap()).unwrap();
    let actor_key = receipt_actor_key(&PublicEventActor::System {
        id: winwincode_domain::SystemActorId("sys_00000000000000000000000000".into()),
    })
    .unwrap();
    let mut storage = SqliteStorage::open(root).unwrap();
    let receipt = storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(actor_key, scope_key, RequestId("seed-catalog".into())).unwrap(),
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
            stream,
            0,
            payload,
            vec![NewOutboxEvent::internal(
                "seed-catalog-event",
                "delivery.catalog.seeded",
                b"{}".to_vec(),
            )],
        ))
        .unwrap();
    storage.mark_published(&receipt.events[0].event_id).unwrap();
    Box::new(storage).close().unwrap();
}

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the restart test keeps mutation, replay, and durable row-count checks together"
)]
fn workitem_creation_survives_sqlite_restart_and_replays_without_extra_writes() {
    let root = std::env::temp_dir().join(format!("wwc-workitems-atomic-{}", std::process::id()));
    assert!(!root.exists(), "test state must be isolated");
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical fixture")
    .into_snapshot();
    snapshot.revision = 1;
    snapshot.created_at_millis = 1_700_000_000_000;
    snapshot.updated_at_millis = 1_700_000_000_000;
    snapshot.spec.created_at_millis = 1_700_000_000_000;
    snapshot.status = winwincode_delivery::domain::DeliveryStatus::Ready;
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.work_run_aggregate.items.clear();
    snapshot.work_run_aggregate.runs.clear();
    let delivery = Delivery::try_from_snapshot(snapshot).expect("canonical empty work graph");
    seed_delivery(&root, &delivery);
    let scope = winwincode_domain::RepositoryScope {
        kind: winwincode_domain::RepositoryScopeKind::Repository,
        organization_id: winwincode_domain::OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: winwincode_domain::WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: winwincode_domain::ProjectId("prj_00000000000000000000000001".into()),
        repository_id: winwincode_domain::RepositoryId("rep_00000000000000000000000001".into()),
    };
    seed_catalog(&root, &scope, &delivery);
    let contract = &delivery.snapshot().work_run_aggregate.contract;
    let command: WorkItemsCreateCommand = serde_json::from_value(serde_json::json!({
        "schemaVersion":"winwincode/v1", "requestId":"req_00000000000000000000000001",
        "actor":{"kind":"system","id":"sys_00000000000000000000000001"},
        "scope":{"kind":"repository","organizationId":"org_00000000000000000000000001","workspaceId":"wsp_00000000000000000000000001","projectId":"prj_00000000000000000000000001","repositoryId":"rep_00000000000000000000000001"},
        "command":"workitems.create", "expectedRevision":1,
        "payload":{"deliveryId":delivery.id(),"expectedRevision":1,"contractRevision":contract.revision,
          "items":[{"id":"wit_00000000000000000000000009","title":"Implement","goal":"Implement the approved requirement","criterionIds":[contract.criteria[0].id],"dependsOn":[]}]}
    })).expect("create command");
    let mut plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start");
    let created = plane.work_items_create(&command).expect("create WorkItem");
    assert_eq!(created.result.items.len(), 1);
    assert_eq!(
        created.result.items[0].id.0,
        "wit_00000000000000000000000009"
    );
    plane.shutdown().expect("shutdown before restart");
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart");
    let replayed = restarted
        .work_items_create(&command)
        .expect("replay original creation");
    assert_eq!(
        serde_json::to_value(&created).unwrap(),
        serde_json::to_value(&replayed).unwrap()
    );
    let mut changed = command.clone();
    changed.payload.items[0].goal = "Different task under the original request".into();
    assert!(
        restarted.work_items_create(&changed).is_err(),
        "the original request must not accept changed task content"
    );
    let unchanged = restarted
        .work_items_create(&command)
        .expect("conflict must preserve original receipt");
    assert_eq!(
        serde_json::to_value(&created).unwrap(),
        serde_json::to_value(&unchanged).unwrap()
    );
    restarted.shutdown().expect("restarted shutdown");
    let storage = SqliteStorage::open(&root).expect("reopen committed state directly");
    let state = storage
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("load state")
        .expect("persisted Delivery");
    let persisted = Delivery::decode_json(&state.payload).expect("decode persisted Delivery");
    assert_eq!(
        persisted.snapshot().work_run_aggregate.items,
        created.result.items
    );
    assert_eq!(persisted.revision(), 2);
    Box::new(storage).close().expect("close inspected state");
    let database = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspect durable state");
    let counts: (i64, i64, i64) = database.query_row(
        "SELECT (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1), (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.changed.v1')",
        rusqlite::params![delivery.id().0, command.request_id.0],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).expect("durable record counts");
    assert_eq!(
        counts,
        (2, 1, 1),
        "seed plus one mutation, one receipt and one notification"
    );
    drop(database);
    fs::remove_dir_all(root).expect("cleanup isolated database");
}

fn setup(
    seed: u64,
) -> (
    PathBuf,
    Delivery,
    winwincode_domain::RepositoryScope,
    WorkItemsCreateCommand,
    ControlPlane,
) {
    let root =
        std::env::temp_dir().join(format!("wwc-workitems-extra-{seed}-{}", std::process::id()));
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .unwrap()
    .into_snapshot();
    snapshot.revision = 1;
    snapshot.created_at_millis = 1_700_000_000_000;
    snapshot.updated_at_millis = 1_700_000_000_000;
    snapshot.spec.created_at_millis = 1_700_000_000_000;
    snapshot.status = winwincode_delivery::domain::DeliveryStatus::Ready;
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.work_run_aggregate.items.clear();
    snapshot.work_run_aggregate.runs.clear();
    let delivery = Delivery::try_from_snapshot(snapshot).unwrap();
    seed_delivery(&root, &delivery);
    let scope = winwincode_domain::RepositoryScope {
        kind: winwincode_domain::RepositoryScopeKind::Repository,
        organization_id: winwincode_domain::OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: winwincode_domain::WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: winwincode_domain::ProjectId("prj_00000000000000000000000001".into()),
        repository_id: winwincode_domain::RepositoryId("rep_00000000000000000000000001".into()),
    };
    seed_catalog(&root, &scope, &delivery);
    let contract = &delivery.snapshot().work_run_aggregate.contract;
    let command: WorkItemsCreateCommand = serde_json::from_value(serde_json::json!({"schemaVersion":"winwincode/v1","requestId":format!("req_{seed:026}"),"actor":{"kind":"system","id":"sys_00000000000000000000000001"},"scope":{"kind":"repository","organizationId":scope.organization_id,"workspaceId":scope.workspace_id,"projectId":scope.project_id,"repositoryId":scope.repository_id},"command":"workitems.create","expectedRevision":1,"payload":{"deliveryId":delivery.id(),"expectedRevision":1,"contractRevision":contract.revision,"items":[{"id":format!("wit_{seed:026}"),"title":"Implement","goal":"Implement approved requirement","criterionIds":[contract.criteria[0].id],"dependsOn":[]}]}})).unwrap();
    let plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .unwrap();
    (root, delivery, scope, command, plane)
}

#[test]
fn workitem_creation_rolls_back_each_atomic_member() {
    for (index, (name, trigger)) in [("state", "CREATE TRIGGER f BEFORE UPDATE ON product_state BEGIN SELECT RAISE(ABORT,'x'); END;"), ("journal", "CREATE TRIGGER f BEFORE INSERT ON aggregate_journal_records BEGIN SELECT RAISE(ABORT,'x'); END;"), ("receipt", "CREATE TRIGGER f BEFORE INSERT ON command_receipts BEGIN SELECT RAISE(ABORT,'x'); END;"), ("outbox", "CREATE TRIGGER f BEFORE INSERT ON outbox WHEN NEW.topic='delivery.changed.v1' BEGIN SELECT RAISE(ABORT,'x'); END;")].into_iter().enumerate() {
        let (root, delivery, _, command, mut plane) = setup(index as u64 + 10);
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap().execute_batch(trigger).unwrap();
        assert!(plane.work_items_create(&command).is_err(), "{name}");
        let state = plane.load_state(&format!("delivery:{}", delivery.id().0)).unwrap().unwrap();
        assert_eq!(state.revision, 1, "{name}");
        let db = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
        let counts: (i64, i64, i64) = db.query_row("SELECT (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_id = ?1), (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.changed.v1')", rusqlite::params![delivery.id().0, command.request_id.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert_eq!(counts, (1, 0, 0), "{name}: no partial durable facts");
        db.execute_batch("DROP TRIGGER f").unwrap();
        let committed = plane.work_items_create(&command).expect("same command succeeds after removing failure injection");
        assert_eq!(committed.current_revision.0, 2);
        drop(db);
        plane.shutdown().unwrap(); fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn workitem_creation_rejects_foreign_scope_and_revision_race() {
    let (root, delivery, _scope, command, mut plane) = setup(31);
    let mut foreign = command.clone();
    foreign.scope = serde_json::from_value(serde_json::json!({"kind":"repository","organizationId":"org_00000000000000000000000031","workspaceId":"wsp_00000000000000000000000031","projectId":"prj_00000000000000000000000031","repositoryId":"rep_00000000000000000000000031"})).unwrap();
    assert!(plane.work_items_create(&foreign).is_err());
    let first = plane.work_items_create(&command).unwrap();
    let mut stale = command.clone();
    stale.payload.expected_revision = winwincode_domain::Revision(0);
    assert!(plane.work_items_create(&stale).is_err());
    assert_eq!(
        plane
            .load_state(&format!("delivery:{}", delivery.id().0))
            .unwrap()
            .unwrap()
            .revision,
        u64::try_from(first.current_revision.0).expect("positive created revision")
    );
    plane.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workitem_creation_rejects_cross_actor_and_cross_delivery_reuse() {
    let (root, delivery, _scope, command, mut plane) = setup(41);
    plane.work_items_create(&command).unwrap();
    let mut actor = command.clone();
    actor.request_id = RequestId("req_00000000000000000000000041".into());
    actor.actor = serde_json::from_value(
        serde_json::json!({"kind":"user","id":"usr_00000000000000000000000041"}),
    )
    .unwrap();
    assert!(plane.work_items_create(&actor).is_err());
    let mut delivery_reuse = command.clone();
    delivery_reuse.request_id = RequestId("req_00000000000000000000000042".into());
    delivery_reuse.payload.delivery_id = DeliveryId("dlv_00000000000000000000000042".into());
    assert!(plane.work_items_create(&delivery_reuse).is_err());
    assert_eq!(
        plane
            .load_state(&format!("delivery:{}", delivery.id().0))
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    plane.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn generic_control_plane_commit_cannot_bypass_workitem_authority() {
    let (root, delivery, _scope, command, mut plane) = setup(51);
    let envelope: CommandEnvelope =
        serde_json::from_value(serde_json::to_value(&command).unwrap()).unwrap();
    let error = plane
        .commit(
            &envelope,
            StateChange::new(
                format!("delivery:{}", delivery.id().0),
                b"forged".to_vec(),
                vec![],
            ),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        winwincode_control_plane::CommitError::Storage(_)
    ));
    assert_eq!(
        plane
            .load_state(&format!("delivery:{}", delivery.id().0))
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    plane.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}
