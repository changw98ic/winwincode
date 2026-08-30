use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, RepositoryScope, RepositoryScopeKind, Scope, UserActor,
    UserActorKind, WorkspaceScope, WorkspaceScopeKind,
};
use winwincode_control_plane::{
    CommitError, ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
    StateChange, StorageErrorKind,
};
use winwincode_delivery::{
    application::task_breakdown::DeliveryTaskBreakdownApprovedEvent,
    domain::Delivery,
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

const REVIEW_SET_SHA256: &str = "67ba86fac36a5275bef299120fc9d701cb9c1637f2019351e5b9b8ad7c1c4134";
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-task-breakdown-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn approved_delivery() -> Delivery {
    Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-approved-solution-review.json"
    ))
    .expect("approved solution-review fixture")
}

fn delivery_with_task_proposals(task_proposals: &str) -> (Delivery, String) {
    let mut snapshot = approved_delivery().into_snapshot();
    let raw = snapshot.attention_items[0].context.clone();
    let proposals_marker = "\"taskProposals\":";
    let proposals_start =
        raw.find(proposals_marker).expect("task proposal marker") + proposals_marker.len();
    let proposals_end = raw[proposals_start..]
        .find(",\"preparedAt\":")
        .map(|offset| proposals_start + offset)
        .expect("preparedAt marker");
    let mut changed = format!(
        "{}{}{}",
        &raw[..proposals_start],
        task_proposals,
        &raw[proposals_end..]
    );
    let digest_marker = ",\"reviewSetSha256\":\"";
    let digest_marker_start = changed.find(digest_marker).expect("review digest marker");
    let digest_input = format!("{}}}", &changed[..digest_marker_start]);
    let digest = format!("{:x}", Sha256::digest(digest_input.as_bytes()));
    let digest_start = digest_marker_start + digest_marker.len();
    let digest_end = changed[digest_start..]
        .find('"')
        .map(|offset| digest_start + offset)
        .expect("review digest end");
    let old_digest = changed[digest_start..digest_end].to_owned();
    changed.replace_range(digest_start..digest_end, &digest);
    snapshot.attention_items[0].context = changed;
    snapshot.attention_items[0].resolution = snapshot.attention_items[0]
        .resolution
        .take()
        .map(|resolution| resolution.replace(&old_digest, &digest));
    (
        Delivery::try_from_snapshot(snapshot).expect("task proposal Delivery"),
        digest,
    )
}

fn task_breakdown_command(seed: u64) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: UserActorKind::User,
        }),
        command: CommandName::DeliveryApproveTaskBreakdown,
        expected_revision: Revision(1),
        payload: serde_json::json!({
            "deliveryId": "dlv_01J00000000000000000000000",
            "reviewSetSha256": format!("sha256:{REVIEW_SET_SHA256}"),
        }),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(canonical_id("org", seed)),
            workspace_id: WorkspaceId(canonical_id("wsp", seed)),
            project_id: ProjectId(canonical_id("prj", seed)),
            repository_id: RepositoryId(canonical_id("rep", seed)),
        }),
    }
}

fn command_digest(command: &CommandEnvelope) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(command).expect("canonical command JSON"))
    ))
}

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

fn seed_delivery(root: &PathBuf) -> Delivery {
    let delivery = approved_delivery();
    seed_specific_delivery(root, delivery)
}

fn seed_specific_delivery(root: &PathBuf, delivery: Delivery) -> Delivery {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-task-breakdown-journal".into()),
            request_digest: "a".repeat(64),
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
                    RequestId("seed-task-breakdown-state".into()),
                )
                .expect("seed identity"),
                Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("seed Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    "seed-task-breakdown-event",
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
    delivery
}

struct CapturingPublisher {
    events: Arc<Mutex<Vec<OutboxEvent>>>,
}

impl EventPublisher for CapturingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.events.lock().expect("event lock").push(event.clone());
        Ok(())
    }
}

struct FailingPublisher;

impl EventPublisher for FailingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Err(EventPublishError::new("injected publication failure"))
    }
}

#[test]
fn task_breakdown_command_commits_state_journal_receipt_and_outbox_together() {
    let root = temporary_directory("atomic-commit");
    let source = seed_delivery(&root);
    let command = task_breakdown_command(1);
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");

    let receipt = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect("atomic task-breakdown commit");
    let state = control_plane
        .load_state("delivery:dlv_01J00000000000000000000000")
        .expect("load_state")
        .expect("committed state");
    let committed = Delivery::decode_json(&state.payload).expect("committed Delivery");
    let event: DeliveryTaskBreakdownApprovedEvent =
        serde_json::from_slice(&receipt.events[0].payload).expect("task-breakdown event");

    assert_eq!(state.revision, source.revision() + 1);
    assert_eq!(committed.snapshot().tasks, event.tasks);
    assert_eq!(event.review_set_sha256, REVIEW_SET_SHA256);
    assert_eq!(published.lock().expect("published events").len(), 2);
    control_plane.shutdown().expect("shutdown");

    let storage = SqliteStorage::open(&root).expect("inspection storage");
    let journal = storage
        .load_journal(
            &AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
                .expect("journal key"),
        )
        .expect("load_journal")
        .expect("Delivery journal");
    let replay = storage
        .load_receipt(&receipt.receipt_identity, &command_digest(&command))
        .expect("load_receipt")
        .expect("durable receipt");
    let pending = storage.pending_events().expect("pending_events");

    assert_eq!(journal.records.len(), 2);
    assert_eq!(replay.revision, receipt.revision);
    assert_eq!(replay.events, receipt.events);
    assert_eq!(pending, Vec::<OutboxEvent>::new());
    Box::new(storage).close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_receipt_first_replay_returns_original_graph_revision_and_event() {
    let root = temporary_directory("receipt-first");
    seed_delivery(&root);
    let command = task_breakdown_command(2);
    let replayed_publications = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&replayed_publications),
        }),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect("initial task-breakdown commit");
    let first_event: DeliveryTaskBreakdownApprovedEvent =
        serde_json::from_slice(&first.events[0].payload).expect("first event");
    let tasks = first_event.tasks.clone();
    control_plane.shutdown().expect("initial shutdown");
    assert_eq!(
        replayed_publications
            .lock()
            .expect("initial publications")
            .len(),
        2
    );
    replayed_publications
        .lock()
        .expect("clear initial publications")
        .clear();

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption database");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = 'delivery:dlv_01J00000000000000000000000'",
            [b"corrupt-current-state".as_slice()],
        )
        .expect("corrupt current state");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = ?1 \
             WHERE aggregate_type = 'delivery' AND aggregate_id = 'dlv_01J00000000000000000000000'",
            [b"corrupt-current-journal".as_slice()],
        )
        .expect("corrupt current Delivery journal");
    connection.close().expect("corruption close");
    let inspection = SqliteStorage::open(&root).expect("corrupt journal inspection");
    let corrupt_journal = inspection
        .load_journal(
            &AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
                .expect("journal key"),
        )
        .expect("load_journal")
        .expect("current journal");
    assert_eq!(corrupt_journal.records.len(), 2);
    Box::new(inspection).close().expect("inspection close");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&replayed_publications),
        }),
    )
    .expect("replay Control Plane start");
    let replay = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect("receipt-first replay ignores damaged current facts");
    let replay_event: DeliveryTaskBreakdownApprovedEvent =
        serde_json::from_slice(&replay.events[0].payload).expect("replayed event");

    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert_eq!(replay_event.tasks, tasks);
    assert!(
        replayed_publications
            .lock()
            .expect("replayed publications")
            .is_empty()
    );
    control_plane.shutdown().expect("replay shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_receipt_first_replay_does_not_parse_receipt_bound_fields() {
    let root = temporary_directory("receipt-first-command");
    seed_delivery(&root);
    let command = task_breakdown_command(20);
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect("initial task-breakdown commit");
    control_plane.shutdown().expect("initial shutdown");
    published.lock().expect("published events").clear();

    // Bind the stored receipt to command fields that are deliberately unusable
    // on the mutation path. A receipt hit must prove its result from the stored
    // events instead of decoding the payload or expected revision again.
    let mut receipt_bound_command = command.clone();
    receipt_bound_command.expected_revision = Revision(-1);
    receipt_bound_command.payload = serde_json::json!({"receiptBoundField": true});
    let receipt_bound_digest = command_digest(&receipt_bound_command);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("receipt-bound database");
    connection
        .execute(
            "UPDATE command_receipts SET command_digest = ?1 WHERE request_id = ?2",
            [
                receipt_bound_digest.0.as_str(),
                receipt_bound_command.request_id.0.as_str(),
            ],
        )
        .expect("install receipt-bound command digest");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = 'delivery:dlv_01J00000000000000000000000'",
            [b"corrupt-current-state".as_slice()],
        )
        .expect("corrupt current state");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = ?1 \
             WHERE aggregate_type = 'delivery' AND aggregate_id = 'dlv_01J00000000000000000000000'",
            [b"corrupt-current-journal".as_slice()],
        )
        .expect("corrupt current journal");
    connection.close().expect("receipt-bound database close");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("replay Control Plane start");
    let replay = control_plane
        .commit_delivery_task_breakdown(&receipt_bound_command)
        .expect("receipt replay does not decode receipt-bound command fields");

    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert!(published.lock().expect("published events").is_empty());
    control_plane.shutdown().expect("replay shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_same_scoped_request_with_changed_digest_is_a_conflict() {
    let root = temporary_directory("request-conflict");
    seed_delivery(&root);
    let command = task_breakdown_command(3);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_task_breakdown(&command)
        .expect("initial task-breakdown commit");

    let mut changed_digest = command.clone();
    changed_digest.payload = serde_json::json!({
        "deliveryId": "dlv_01J00000000000000000000000",
        "reviewSetSha256": format!("sha256:{}", "f".repeat(64)),
    });
    let rejected = control_plane
        .commit_delivery_task_breakdown(&changed_digest)
        .expect_err("same scoped request with another digest is rejected");

    assert!(matches!(
        rejected,
        CommitError::Storage(ref error) if error.kind() == StorageErrorKind::RequestConflict
    ));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_rejects_a_non_repository_scope_before_writing_facts() {
    let root = temporary_directory("wrong-scope");
    seed_delivery(&root);
    let mut command = task_breakdown_command(30);
    command.scope = Scope::WorkspaceScope(WorkspaceScope {
        kind: WorkspaceScopeKind::Workspace,
        organization_id: OrganizationId(canonical_id("org", 30)),
        workspace_id: WorkspaceId(canonical_id("wsp", 30)),
    });
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");

    let error = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect_err("task promotion requires the canonical repository scope");
    assert!(matches!(error, CommitError::Storage(ref error)
        if error.kind() == StorageErrorKind::InvalidInput));
    assert!(published.lock().expect("published events").is_empty());
    let state = control_plane
        .load_state("delivery:dlv_01J00000000000000000000000")
        .expect("state read")
        .expect("seed state");
    assert_eq!(state.revision, 1);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_receipt_cannot_be_replayed_by_another_actor_scope_or_delivery() {
    for boundary in ["actor", "scope", "delivery"] {
        let root = temporary_directory(boundary);
        seed_delivery(&root);
        let command = task_breakdown_command(40);
        let published = Arc::new(Mutex::new(Vec::new()));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::clone(&published),
            }),
        )
        .expect("Control Plane start");
        let first = control_plane
            .commit_delivery_task_breakdown(&command)
            .expect("initial task promotion");

        let mut crossed = command.clone();
        match boundary {
            "actor" => {
                crossed.actor = Actor::UserActor(UserActor {
                    id: UserId(canonical_id("usr", 41)),
                    kind: UserActorKind::User,
                });
            }
            "scope" => {
                crossed.scope = Scope::RepositoryScope(RepositoryScope {
                    kind: RepositoryScopeKind::Repository,
                    organization_id: OrganizationId(canonical_id("org", 41)),
                    workspace_id: WorkspaceId(canonical_id("wsp", 41)),
                    project_id: ProjectId(canonical_id("prj", 41)),
                    repository_id: RepositoryId(canonical_id("rep", 41)),
                });
            }
            "delivery" => {
                crossed.payload["deliveryId"] = serde_json::json!("delivery-foreign");
            }
            _ => unreachable!(),
        }

        let error = control_plane
            .commit_delivery_task_breakdown(&crossed)
            .expect_err("a scoped receipt cannot cross its authority boundary");
        let CommitError::Storage(error) = error else {
            panic!("crossed receipt cannot publish")
        };
        assert_eq!(
            error.kind(),
            StorageErrorKind::RequestConflict,
            "{boundary}"
        );
        let state = control_plane
            .load_state("delivery:dlv_01J00000000000000000000000")
            .expect("state read")
            .expect("committed state");
        assert_eq!(state.revision, first.revision, "{boundary}");
        assert_eq!(
            published.lock().expect("published events").len(),
            2,
            "{boundary}"
        );
        control_plane.shutdown().expect("shutdown");

        let storage = SqliteStorage::open(&root).expect("inspection storage");
        let journal = storage
            .load_journal(
                &AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
                    .expect("journal key"),
            )
            .expect("journal read")
            .expect("Delivery journal");
        assert_eq!(journal.records.len(), 2, "{boundary}");
        Box::new(storage).close().expect("inspection close");
        fs::remove_dir_all(root).expect("database cleanup");
    }
}

#[test]
fn task_breakdown_same_revision_stale_review_digest_maps_to_revision_conflict() {
    let root = temporary_directory("review-digest-stale");
    seed_delivery(&root);
    let mut command = task_breakdown_command(6);
    command.payload = serde_json::json!({
        "deliveryId": "dlv_01J00000000000000000000000",
        "reviewSetSha256": format!("sha256:{}", "f".repeat(64)),
    });
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");

    let rejected = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect_err("stale reviewSetSha256 is a revision-token conflict");
    let CommitError::Storage(error) = rejected else {
        panic!("stale review digest must fail before publication")
    };
    assert_eq!(error.kind(), StorageErrorKind::RevisionConflict);
    assert_eq!(
        error.to_string(),
        "reviewSetSha256 no longer identifies the current solution review"
    );
    let state = control_plane
        .load_state("delivery:dlv_01J00000000000000000000000")
        .expect("state read")
        .expect("seed state");
    assert_eq!(state.revision, 1);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_invalid_reviewed_graphs_change_no_durable_fact() {
    let invalid_graphs = [
        ("empty", "[]"),
        (
            "duplicate-task",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required","criterion-optional"],"blockedByTaskIds":[]},{"id":"task:a","title":"B","goal":"B goal","acceptanceCriterionIds":["criterion-required"],"blockedByTaskIds":[]}]"#,
        ),
        (
            "empty-criteria",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":[],"blockedByTaskIds":[]}]"#,
        ),
        (
            "duplicate-criterion",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required","criterion-required"],"blockedByTaskIds":[]}]"#,
        ),
        (
            "foreign-criterion",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion:foreign"],"blockedByTaskIds":[]}]"#,
        ),
        (
            "incomplete-coverage",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required"],"blockedByTaskIds":[]}]"#,
        ),
        (
            "self-dependency",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required","criterion-optional"],"blockedByTaskIds":["task:a"]}]"#,
        ),
        (
            "duplicate-dependency",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required"],"blockedByTaskIds":["task:b","task:b"]},{"id":"task:b","title":"B","goal":"B goal","acceptanceCriterionIds":["criterion-optional"],"blockedByTaskIds":[]}]"#,
        ),
        (
            "missing-dependency",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required","criterion-optional"],"blockedByTaskIds":["task:missing"]}]"#,
        ),
        (
            "cycle",
            r#"[{"id":"task:a","title":"A","goal":"A goal","acceptanceCriterionIds":["criterion-required"],"blockedByTaskIds":["task:b"]},{"id":"task:b","title":"B","goal":"B goal","acceptanceCriterionIds":["criterion-optional"],"blockedByTaskIds":["task:a"]}]"#,
        ),
    ];

    for (offset, (name, proposals)) in invalid_graphs.into_iter().enumerate() {
        assert_invalid_reviewed_graph_changes_no_fact(
            name,
            proposals,
            100 + u64::try_from(offset).expect("invalid graph offset"),
        );
    }
}

fn assert_invalid_reviewed_graph_changes_no_fact(name: &str, proposals: &str, seed: u64) {
    let root = temporary_directory(name);
    let (source, digest) = delivery_with_task_proposals(proposals);
    seed_specific_delivery(&root, source.clone());
    let mut command = task_breakdown_command(seed);
    command.payload["reviewSetSha256"] = serde_json::json!(format!("sha256:{digest}"));
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");

    let error = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect_err("an invalid reviewed graph cannot be promoted");
    assert!(
        matches!(error, CommitError::Storage(ref error)
        if error.kind() == StorageErrorKind::InvalidInput),
        "{name}"
    );
    let state = control_plane
        .load_state("delivery:dlv_01J00000000000000000000000")
        .expect("state read")
        .expect("seed state");
    assert_eq!(state.revision, source.revision(), "{name}");
    assert_eq!(
        state.payload,
        source.encode_json().expect("source JSON"),
        "{name}"
    );
    assert!(
        published.lock().expect("published events").is_empty(),
        "{name}"
    );
    control_plane.shutdown().expect("shutdown");

    let storage = SqliteStorage::open(&root).expect("inspection storage");
    let journal = storage
        .load_journal(
            &AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
                .expect("journal key"),
        )
        .expect("journal read")
        .expect("Delivery journal");
    assert_eq!(journal.records.len(), 1, "{name}");
    assert!(
        storage.pending_events().expect("pending events").is_empty(),
        "{name}"
    );
    Box::new(storage).close().expect("inspection close");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let counts: (i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?1), \
               (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.task_breakdown.approved')",
            [command.request_id.0.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("invalid graph fact counts");
    assert_eq!(counts, (0, 0), "{name}");
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_revision_race_commits_no_partial_loser_facts() {
    let root = temporary_directory("revision-race");
    seed_delivery(&root);
    let winner_command = task_breakdown_command(4);
    let mut loser_command = task_breakdown_command(5);
    loser_command.expected_revision = Revision(0);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");

    let winner = control_plane
        .commit_delivery_task_breakdown(&winner_command)
        .expect("first revision wins");
    let rejected = control_plane
        .commit_delivery_task_breakdown(&loser_command)
        .expect_err("stale competing revision is rejected");
    let CommitError::Storage(error) = rejected else {
        panic!("old expectedRevision must fail before publication")
    };
    assert_eq!(error.kind(), StorageErrorKind::RevisionConflict);
    assert_eq!(
        error.to_string(),
        "expected revision 0, but current revision is 2"
    );
    control_plane.shutdown().expect("shutdown");

    let storage = SqliteStorage::open(&root).expect("inspection storage");
    let journal = storage
        .load_journal(
            &AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
                .expect("journal key"),
        )
        .expect("load_journal")
        .expect("Delivery journal");
    let pending = storage.pending_events().expect("pending_events");
    assert_eq!(journal.records.len(), 2);
    assert_eq!(journal.records[1].sequence, winner.revision);
    assert_eq!(pending, Vec::<OutboxEvent>::new());
    Box::new(storage).close().expect("inspection close");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let loser_facts: (i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?1), \
               (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.task_breakdown.approved')",
            [loser_command.request_id.0.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("loser fact counts");
    assert_eq!(loser_facts, (0, 1));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn task_breakdown_failure_at_each_atomic_member_rolls_back_all_four() {
    let failure_points = [
        (
            "product_state",
            "CREATE TRIGGER fail_task_state BEFORE UPDATE ON product_state \
             WHEN NEW.stream_id = 'delivery:dlv_01J00000000000000000000000' \
             BEGIN SELECT RAISE(ABORT, 'injected task state failure'); END;",
        ),
        (
            "aggregate_journal_records",
            "CREATE TRIGGER fail_task_journal BEFORE INSERT ON aggregate_journal_records \
             WHEN NEW.aggregate_type = 'delivery' AND NEW.sequence = 2 \
             BEGIN SELECT RAISE(ABORT, 'injected task journal failure'); END;",
        ),
        (
            "command_receipts",
            "CREATE TRIGGER fail_task_receipt BEFORE INSERT ON command_receipts \
             WHEN NEW.request_id LIKE 'req_%' \
             BEGIN SELECT RAISE(ABORT, 'injected task receipt failure'); END;",
        ),
        (
            "outbox",
            "CREATE TRIGGER fail_task_outbox BEFORE INSERT ON outbox \
             WHEN NEW.topic = 'delivery.task_breakdown.approved' \
             BEGIN SELECT RAISE(ABORT, 'injected task outbox failure'); END;",
        ),
    ];

    for (offset, (member, trigger)) in failure_points.into_iter().enumerate() {
        let root = temporary_directory(member);
        let source = seed_delivery(&root);
        let command =
            task_breakdown_command(10 + u64::try_from(offset).expect("failure point index"));
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("failure injection database");
        connection.execute_batch(trigger).expect("failure trigger");
        connection.close().expect("injector close");
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .expect("Control Plane start");

        let error = control_plane
            .commit_delivery_task_breakdown(&command)
            .expect_err("injected atomic member failure");
        assert!(matches!(error, CommitError::Storage(_)), "{member}");
        let state = control_plane
            .load_state("delivery:dlv_01J00000000000000000000000")
            .expect("state read")
            .expect("seed state");
        assert_eq!(state.revision, source.revision(), "{member}");
        assert_eq!(
            state.payload,
            source.encode_json().expect("seed Delivery JSON"),
            "{member}"
        );
        control_plane.shutdown().expect("shutdown");

        let storage = SqliteStorage::open(&root).expect("inspection storage");
        let journal = storage
            .load_journal(
                &AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
                    .expect("journal key"),
            )
            .expect("journal read")
            .expect("seed Delivery journal");
        let pending = storage.pending_events().expect("outbox read");
        assert_eq!(journal.records.len(), 1, "{member}");
        assert_eq!(pending, Vec::<OutboxEvent>::new(), "{member}");
        Box::new(storage).close().expect("inspection close");

        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("inspection database");
        let counts: (i64, i64) = connection
            .query_row(
                "SELECT \
                   (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?1), \
                   (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.task_breakdown.approved')",
                [command.request_id.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("atomic rollback counts");
        assert_eq!(counts, (0, 0), "{member}");
        connection.close().expect("inspection close");
        fs::remove_dir_all(root).expect("database cleanup");
    }
}

#[test]
fn task_breakdown_publish_failure_keeps_the_committed_event_for_replay() {
    let root = temporary_directory("publish-replay");
    seed_delivery(&root);
    let command = task_breakdown_command(20);
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(FailingPublisher))
            .expect("Control Plane start");

    let error = control_plane
        .commit_delivery_task_breakdown(&command)
        .expect_err("publication fails after the atomic commit");
    let receipt = match error {
        CommitError::PublicationPending { receipt, .. } => *receipt,
        other @ CommitError::Storage(_) => {
            panic!("expected PublicationPending, got {other:?}")
        }
    };
    let event: DeliveryTaskBreakdownApprovedEvent =
        serde_json::from_slice(&receipt.events[0].payload).expect("committed task event");
    assert_eq!(event.tasks.len(), 1);
    control_plane
        .shutdown()
        .expect_err("failing publisher leaves the event pending during shutdown");

    let storage = SqliteStorage::open(&root).expect("inspection storage");
    let pending = storage.pending_events().expect("pending_events");
    assert_eq!(pending, receipt.events);
    let stored_event: DeliveryTaskBreakdownApprovedEvent =
        serde_json::from_slice(&pending[0].payload).expect("pending task event");
    assert_eq!(stored_event, event);
    Box::new(storage).close().expect("inspection close");

    let published = Arc::new(Mutex::new(Vec::new()));
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("outbox replay start");
    let replay = restarted
        .commit_delivery_task_breakdown(&command)
        .expect("same command returns the durable receipt");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, receipt.revision);
    assert_eq!(replay.events, receipt.events);
    assert_eq!(published.lock().expect("published events").len(), 2);
    restarted.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn generic_control_plane_commit_cannot_bypass_task_breakdown_authority() {
    let root = temporary_directory("generic-bypass");
    let command = task_breakdown_command(21);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("ControlPlane start");

    assert_eq!(command.command, CommandName::DeliveryApproveTaskBreakdown);
    let rejected = control_plane
        .commit(
            &command,
            StateChange::new(
                "delivery:dlv_01J00000000000000000000000",
                b"caller-authored-task-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "caller-authored-task-event",
                    "delivery.task_breakdown.approved",
                    b"caller-authored-task-event".to_vec(),
                )],
            ),
        )
        .expect_err("generic Control Plane commit is rejected");
    assert!(matches!(rejected, CommitError::Storage(_)));
    assert!(
        control_plane
            .load_state("delivery:dlv_01J00000000000000000000000")
            .expect("state read")
            .is_none()
    );
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}
