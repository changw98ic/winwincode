use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, DeliveryId, Instant, LeaseId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId, WorkerId,
    WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, CommitReceipt,
    LoadedAggregateJournal, MAX_STATE_MUTATION_BYTES_PER_COMMIT, MAX_STATE_MUTATION_PAYLOAD_BYTES,
    MAX_STATE_MUTATIONS_PER_COMMIT, NewOutboxEvent, OutboxEvent, PendingAuditEvent,
    ProductStateStorage, ProjectionEventCursor, ProjectionEventStream, ProjectionEventStreamKey,
    ProjectionReadCut, PublicEventActor, PublicEventScope, PublicEventSource, ReceiptActorKey,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, StateMutation,
    StateRevisionGuard, StorageError, StorageErrorKind, StoredState, receipt_actor_key,
    receipt_scope_key,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-storage-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn receipt_identity(actor: &str, scope: &str, request_id: &str) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(actor.as_bytes().to_vec()).expect("actor key"),
        ReceiptScopeKey::from_encoded(scope.as_bytes().to_vec()).expect("scope key"),
        RequestId(request_id.to_owned()),
    )
    .expect("receipt identity")
}

fn projection_actor() -> PublicEventActor {
    PublicEventActor::User {
        id: UserId("usr_01J00000000000000000000000".into()),
    }
}

fn projection_scope() -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId("org_01J00000000000000000000000".into()),
        workspace_id: WorkspaceId("wsp_01J00000000000000000000000".into()),
        project_id: ProjectId("prj_01J00000000000000000000000".into()),
        repository_id: RepositoryId("rep_01J00000000000000000000000".into()),
    }
}

fn projection_receipt_identity(request_id: &str) -> ReceiptIdentity {
    projection_receipt_identity_for_scope(request_id, &projection_scope())
}

fn projection_receipt_identity_for_scope(
    request_id: &str,
    scope: &PublicEventScope,
) -> ReceiptIdentity {
    ReceiptIdentity::new(
        receipt_actor_key(&projection_actor()).expect("canonical actor key"),
        receipt_scope_key(scope).expect("canonical scope key"),
        RequestId(request_id.to_owned()),
    )
    .expect("projection receipt identity")
}

fn public_projection_event(
    event_id: impl Into<String>,
    stream: ProjectionEventStream,
) -> NewOutboxEvent {
    public_projection_event_for_scope(event_id, stream, projection_scope())
}

fn public_projection_event_for_scope(
    event_id: impl Into<String>,
    stream: ProjectionEventStream,
    scope: PublicEventScope,
) -> NewOutboxEvent {
    NewOutboxEvent::public_projection(
        ControlPlaneEventId(event_id.into()),
        "projection.invalidated",
        b"{}".to_vec(),
        stream,
        scope,
        Instant("2026-08-27T00:00:00.000Z".into()),
        PublicEventSource::ControlPlane {
            actor: projection_actor(),
            component: "storage-test".into(),
        },
    )
    .expect("canonical public projection")
}

fn state_commit(
    identity: (&str, &str, &str),
    digest: &str,
    stream_id: &str,
    expected_revision: u64,
    state: &[u8],
    event_id: &str,
) -> StateCommit {
    StateCommit::new(
        receipt_identity(identity.0, identity.1, identity.2),
        Sha256Digest(digest.to_owned()),
        stream_id,
        expected_revision,
        state.to_vec(),
        vec![NewOutboxEvent::internal(
            event_id,
            "control-plane.state.changed",
            b"event".to_vec(),
        )],
    )
}

fn projection_commit(
    request_id: &str,
    state_stream: &str,
    event_id: &str,
    stream: ProjectionEventStream,
) -> StateCommit {
    projection_commit_for_scope(
        request_id,
        state_stream,
        event_id,
        stream,
        projection_scope(),
    )
}

fn projection_commit_for_scope(
    request_id: &str,
    state_stream: &str,
    event_id: &str,
    stream: ProjectionEventStream,
    scope: PublicEventScope,
) -> StateCommit {
    StateCommit::new(
        projection_receipt_identity_for_scope(request_id, &scope),
        Sha256Digest(format!("sha256:{}", "d".repeat(64))),
        state_stream,
        0,
        b"projection-state".to_vec(),
        vec![public_projection_event_for_scope(event_id, stream, scope)],
    )
}

fn projection_key(stream: ProjectionEventStream) -> ProjectionEventStreamKey {
    projection_key_for_scope(stream, &projection_scope())
}

fn projection_key_for_scope(
    stream: ProjectionEventStream,
    scope: &PublicEventScope,
) -> ProjectionEventStreamKey {
    ProjectionEventStreamKey::new(receipt_scope_key(scope).expect("scope key"), stream)
        .expect("projection stream key")
}

#[test]
fn sqlite_projection_read_cut_reads_state_and_cursor_from_one_durable_cut() {
    let root = temporary_directory("projection-read-cut");
    let mut storage = SqliteStorage::open(&root).expect("storage should open");
    let stream =
        ProjectionEventStream::Delivery(DeliveryId("dlv_01J00000000000000000000000".into()));
    let key = projection_key(stream);
    let state_stream = "runtime:fixture-read-cut".to_owned();
    storage
        .commit(&StateCommit::new(
            projection_receipt_identity("request:read-cut"),
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            &state_stream,
            0,
            b"runtime-state-v1".to_vec(),
            vec![public_projection_event(
                "evt_read_cut_00000001",
                key.stream().clone(),
            )],
        ))
        .expect("state and event should commit");

    let cut = storage
        .load_projection_read_cut(std::slice::from_ref(&state_stream), &key, None)
        .expect("read cut should load");
    assert_eq!(cut.states().len(), 1);
    assert_eq!(cut.states()[0].stream_id, state_stream);
    assert_eq!(cut.states()[0].payload, b"runtime-state-v1");
    assert_eq!(cut.projection_event_cursor().sequence(), 1);
    assert_eq!(
        cut.projection_event_cursor()
            .event_id()
            .expect("event id")
            .0,
        "evt_read_cut_00000001"
    );

    Box::new(storage).close().expect("storage should close");
    let storage = SqliteStorage::open(&root).expect("storage should restart");
    let cut = storage
        .load_projection_read_cut(&["runtime:fixture-read-cut".to_owned()], &key, None)
        .expect("restarted read cut should load");
    assert_eq!(cut.states()[0].payload, b"runtime-state-v1");
    assert_eq!(cut.projection_event_cursor().sequence(), 1);
    Box::new(storage)
        .close()
        .expect("restarted storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn public_context_is_atomic_exact_and_survives_restart() {
    let root = temporary_directory("public-context-restart");
    let mut storage = SqliteStorage::open(&root).expect("storage should open");
    let stream = ProjectionEventStream::ProductSession(ProductSessionId(
        "psn_01J00000000000000000000000".into(),
    ));
    storage
        .commit(&projection_commit(
            "req_public_context_restart",
            "product-session:fixture",
            "evt_public_context_restart",
            stream.clone(),
        ))
        .expect("public context should commit");
    Box::new(storage).close().expect("storage should close");

    let storage = SqliteStorage::open(&root).expect("storage should restart");
    let event = storage
        .load_outbox_event("evt_public_context_restart")
        .expect("durable event lookup")
        .expect("durable event");
    let context = event
        .event()
        .public_context
        .as_ref()
        .expect("durable public context");
    assert_eq!(context.scope(), &projection_scope());
    assert_eq!(context.stream(), &stream);
    assert_eq!(context.occurred_at().0, "2026-08-27T00:00:00.000Z");
    assert_eq!(
        context.source(),
        &PublicEventSource::ControlPlane {
            actor: projection_actor(),
            component: "storage-test".into(),
        }
    );
    Box::new(storage)
        .close()
        .expect("restarted storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn public_context_scope_and_actor_must_match_the_atomic_receipt() {
    let root = temporary_directory("public-context-authority");
    let mut storage = SqliteStorage::open(&root).expect("storage should open");
    let event = public_projection_event(
        "evt_public_context_authority",
        ProjectionEventStream::Delivery(DeliveryId("dlv_01J00000000000000000000000".into())),
    );
    let other_actor = PublicEventActor::User {
        id: UserId("usr_01J00000000000000000000001".into()),
    };
    let receipt = ReceiptIdentity::new(
        receipt_actor_key(&other_actor).expect("other actor key"),
        receipt_scope_key(&projection_scope()).expect("scope key"),
        RequestId("req_public_context_authority".into()),
    )
    .expect("foreign receipt");
    let error = storage
        .commit(&StateCommit::new(
            receipt,
            Sha256Digest(format!("sha256:{}", "f".repeat(64))),
            "public-context-authority",
            0,
            b"must-not-commit".to_vec(),
            vec![event],
        ))
        .expect_err("public actor and receipt actor must match");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    let other_scope = PublicEventScope::Repository {
        organization_id: OrganizationId("org_01J00000000000000000000000".into()),
        workspace_id: WorkspaceId("wsp_01J00000000000000000000000".into()),
        project_id: ProjectId("prj_01J00000000000000000000000".into()),
        repository_id: RepositoryId("rep_01J00000000000000000000001".into()),
    };
    let scope_error = storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                receipt_actor_key(&projection_actor()).expect("actor key"),
                receipt_scope_key(&other_scope).expect("other scope key"),
                RequestId("req_public_context_other_scope".into()),
            )
            .expect("foreign scope receipt"),
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            "public-context-authority",
            0,
            b"must-not-commit".to_vec(),
            vec![public_projection_event(
                "evt_public_context_other_scope",
                ProjectionEventStream::Delivery(DeliveryId(
                    "dlv_01J00000000000000000000000".into(),
                )),
            )],
        ))
        .expect_err("public scope and receipt scope must match");
    assert_eq!(scope_error.kind(), StorageErrorKind::InvalidInput);
    assert!(
        storage
            .load_state("public-context-authority")
            .expect("state lookup")
            .is_none()
    );
    assert!(storage.pending_events().expect("pending events").is_empty());
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sqlite_projection_read_cut_never_pairs_a_state_with_a_concurrent_cursor() {
    let root = temporary_directory("projection-read-cut-race");
    let stream =
        ProjectionEventStream::Delivery(DeliveryId("dlv_01J00000000000000000000000".into()));
    let key = projection_key(stream.clone());
    let state_stream = "runtime:fixture-read-cut-race".to_owned();
    let mut seed = SqliteStorage::open(&root).expect("storage should open");
    seed.commit(&StateCommit::new(
        projection_receipt_identity("request:seed"),
        Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        &state_stream,
        0,
        b"revision:1".to_vec(),
        vec![public_projection_event(
            "evt_read_cut_race_0000000000000001",
            stream.clone(),
        )],
    ))
    .expect("seed should commit");
    Box::new(seed).close().expect("seed storage should close");

    let reader = SqliteStorage::open(&root).expect("reader storage should open");
    let mut writer = SqliteStorage::open(&root).expect("writer storage should open");
    let writer_state_stream = state_stream.clone();
    let writer_handle = thread::spawn(move || {
        for revision in 2..=101_u64 {
            writer
                .commit(&StateCommit::new(
                    projection_receipt_identity(&format!("request:revision-{revision}")),
                    Sha256Digest(format!("sha256:{revision:064x}")),
                    &writer_state_stream,
                    revision - 1,
                    format!("revision:{revision}").into_bytes(),
                    vec![public_projection_event(
                        format!("evt_read_cut_race_{revision:016}"),
                        stream.clone(),
                    )],
                ))
                .expect("concurrent revision should commit");
        }
        Box::new(writer)
            .close()
            .expect("writer storage should close");
    });

    for _ in 0..2_000 {
        let cut = reader
            .load_projection_read_cut(std::slice::from_ref(&state_stream), &key, None)
            .expect("read cut should load during concurrent commits");
        let state = cut.states().first().expect("state should be present");
        let revision = state.revision;
        assert_eq!(state.payload, format!("revision:{revision}").into_bytes());
        assert_eq!(cut.projection_event_cursor().sequence(), revision);
        assert_eq!(
            cut.projection_event_cursor()
                .event_id()
                .expect("event id")
                .0,
            format!("evt_read_cut_race_{revision:016}")
        );
    }
    writer_handle.join().expect("writer thread should finish");
    Box::new(reader)
        .close()
        .expect("reader storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn delivery_projection_stream(value: &str) -> ProjectionEventStream {
    ProjectionEventStream::Delivery(DeliveryId(value.into()))
}

#[test]
fn delivery_projection_stream_rejects_a_retired_delivery_identity() {
    let error = ProjectionEventStreamKey::new(
        ReceiptScopeKey::from_encoded(b"scope:repository-one".to_vec()).expect("scope key"),
        delivery_projection_stream("delivery-main"),
    )
    .expect_err("a Delivery event stream must use the canonical dlv_ identity");

    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
}

#[test]
fn exact_durable_event_lookup_reads_published_rows_after_restart() {
    let root = temporary_directory("exact-durable-event");
    let digest = format!("sha256:{}", "a".repeat(64));
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let commit = state_commit(
        ("worker-one", "repository-one", "req_event_lookup"),
        &digest,
        "delivery:one",
        0,
        b"state",
        "execution-job:job_one",
    );
    let receipt = storage.commit(&commit).expect("event commit");
    let pending = storage
        .load_outbox_event("execution-job:job_one")
        .expect("pending exact read")
        .expect("pending event");
    assert_eq!(pending.event(), &receipt.events[0]);
    assert_eq!(pending.receipt_identity(), &receipt.receipt_identity);
    assert_eq!(pending.command_digest(), &receipt.command_digest);
    assert_eq!(pending.stream_id(), receipt.stream_id);
    assert_eq!(pending.revision(), receipt.revision);
    storage
        .mark_published("execution-job:job_one")
        .expect("publish acknowledgement");
    Box::new(storage).close().expect("first storage close");

    let restarted = SqliteStorage::open(&root).expect("restart storage");
    let published = restarted
        .load_outbox_event("execution-job:job_one")
        .expect("published exact read")
        .expect("published event");
    assert_eq!(published.event(), &receipt.events[0]);
    assert!(
        restarted
            .load_outbox_event("execution-job:missing")
            .expect("missing exact read")
            .is_none()
    );
    Box::new(restarted).close().expect("restart close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn pending_audit_event_is_atomic_and_replays_only_with_the_original_payload() {
    let root = temporary_directory("audit-fact-atomic");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let fact = PendingAuditEvent::new("aud_original", b"{\"kind\":\"terminal\"}".to_vec())
        .expect("pending audit event");
    let commit = state_commit(
        ("audit-actor", "audit-scope", "audit-request"),
        &format!("sha256:{}", "e".repeat(64)),
        "audit-stream",
        0,
        b"audit-state",
        "audit-event",
    )
    .with_pending_audit_event(fact.clone());
    let receipt = storage.commit(&commit).expect("atomic audit commit");
    assert_eq!(
        storage.load_pending_audit_event(&receipt.receipt_identity),
        Ok(Some(fact.clone()))
    );
    assert_eq!(storage.pending_audit_events(), Ok(vec![fact.clone()]));

    let replay = storage.commit(&commit).expect("exact audit replay");
    assert!(replay.idempotent_replay);
    assert_eq!(
        storage.load_pending_audit_event(&receipt.receipt_identity),
        Ok(Some(fact.clone()))
    );
    storage
        .mark_audit_event_persisted(fact.event_id())
        .expect("mark audit event persisted");
    assert!(
        storage
            .pending_audit_events()
            .expect("pending audit events")
            .is_empty()
    );
    assert_eq!(
        storage.load_pending_audit_event(&receipt.receipt_identity),
        Ok(Some(fact.clone()))
    );

    let changed = StateCommit::new(
        receipt.receipt_identity.clone(),
        receipt.command_digest.clone(),
        "audit-stream",
        0,
        b"audit-state".to_vec(),
        vec![NewOutboxEvent::internal(
            "audit-event",
            "control-plane.state.changed",
            b"event".to_vec(),
        )],
    )
    .with_pending_audit_event(
        PendingAuditEvent::new("aud_changed", b"{\"kind\":\"changed\"}".to_vec())
            .expect("changed pending audit event"),
    );
    assert_eq!(
        storage
            .commit(&changed)
            .expect_err("changed pending audit event must conflict")
            .kind(),
        StorageErrorKind::RequestConflict
    );
    Box::new(storage).close().expect("audit storage close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn pending_audit_event_survives_restart_until_the_canonical_store_marks_it() {
    let root = temporary_directory("audit-outbox-restart");
    let fact = PendingAuditEvent::new("aud_restart", b"canonical-audit-event".to_vec())
        .expect("pending audit event");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let commit = state_commit(
        ("restart-actor", "restart-scope", "restart-request"),
        &format!("sha256:{}", "1".repeat(64)),
        "restart-stream",
        0,
        b"restart-state",
        "restart-event",
    )
    .with_pending_audit_event(fact.clone());
    storage.commit(&commit).expect("atomic commit");
    Box::new(storage).close().expect("first storage close");

    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    assert_eq!(
        restarted
            .pending_audit_events()
            .expect("pending after restart"),
        vec![fact.clone()]
    );
    restarted
        .mark_audit_event_persisted(fact.event_id())
        .expect("mark after restart");
    assert!(
        restarted
            .pending_audit_events()
            .expect("pending after mark")
            .is_empty()
    );
    Box::new(restarted).close().expect("restart close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn pending_audit_event_failure_rolls_back_state_receipt_and_outbox_together() {
    let root = temporary_directory("audit-fact-rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("injector open");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_pending_audit_event BEFORE INSERT ON audit_outbox
             BEGIN SELECT RAISE(ABORT, 'injected pending audit event failure'); END;",
        )
        .expect("audit trigger");
    connection.close().expect("injector close");
    let commit = state_commit(
        ("rollback-actor", "rollback-scope", "rollback-request"),
        &format!("sha256:{}", "f".repeat(64)),
        "rollback-stream",
        0,
        b"rollback-state",
        "rollback-event",
    )
    .with_pending_audit_event(
        PendingAuditEvent::new("aud_rollback", b"rollback-fact".to_vec()).expect("fact"),
    );
    assert_eq!(
        storage
            .commit(&commit)
            .expect_err("audit trigger must abort commit")
            .kind(),
        StorageErrorKind::Adapter
    );
    assert!(
        storage
            .load_state("rollback-stream")
            .expect("state read")
            .is_none()
    );
    assert!(
        storage
            .load_receipt(&commit.receipt_identity, &commit.command_digest)
            .expect("receipt read")
            .is_none()
    );
    assert!(
        storage
            .load_pending_audit_event(&commit.receipt_identity)
            .expect("audit read")
            .is_none()
    );
    assert!(storage.pending_events().expect("outbox read").is_empty());
    Box::new(storage).close().expect("rollback storage close");
    fs::remove_dir_all(root).expect("database directory release");
}

fn write_interleaved_projection_events(
    storage: &mut SqliteStorage,
    delivery_one: &ProjectionEventStream,
    delivery_two: &ProjectionEventStream,
    session_one: &ProjectionEventStream,
    session_two: &ProjectionEventStream,
) -> ProjectionEventCursor {
    let writes = [
        (
            "req_projection_00000001",
            "state:d1:1",
            "evt_delivery_one_0001",
            delivery_one,
        ),
        (
            "req_projection_00000002",
            "state:d2:1",
            "evt_delivery_two_0001",
            delivery_two,
        ),
        (
            "req_projection_00000003",
            "state:s1:1",
            "evt_session_one_0001",
            session_one,
        ),
        (
            "req_projection_00000004",
            "state:d1:2",
            "evt_delivery_one_0002",
            delivery_one,
        ),
        (
            "req_projection_00000005",
            "state:s2:1",
            "evt_session_two_0001",
            session_two,
        ),
        (
            "req_projection_00000006",
            "state:s1:2",
            "evt_session_one_0002",
            session_one,
        ),
    ];
    let mut first_delivery_cursor = None;
    for (request_id, state_stream, event_id, stream) in writes {
        let receipt = storage
            .commit(&projection_commit(
                request_id,
                state_stream,
                event_id,
                stream.clone(),
            ))
            .expect("projection event should commit");
        let cursor = receipt.events[0]
            .projection_cursor
            .clone()
            .expect("projection cursor");
        if event_id == "evt_delivery_one_0001" {
            first_delivery_cursor = Some(cursor);
        }
    }
    first_delivery_cursor.expect("first Delivery cursor")
}

fn assert_projection_stream_heads(
    storage: &SqliteStorage,
    expected: &[(ProjectionEventStream, u64, &str)],
) {
    for (stream, sequence, event_id) in expected {
        let cursor = storage
            .load_projection_event_cursor(&projection_key(stream.clone()), None)
            .expect("latest stream cursor");
        assert_eq!(cursor.sequence(), *sequence);
        assert_eq!(cursor.event_id().expect("event id").0, *event_id);
    }
}

fn assert_projection_cursor_key_is_exact(
    storage: &SqliteStorage,
    cursor: &ProjectionEventCursor,
    another_delivery: ProjectionEventStream,
    another_session: ProjectionEventStream,
) {
    let error = storage
        .load_projection_event_cursor(&projection_key(another_delivery), Some(cursor))
        .expect_err("another Delivery stream must reject this cursor");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    let foreign_scope_key = ProjectionEventStreamKey::new(
        ReceiptScopeKey::from_encoded(b"scope:repository-two".to_vec()).expect("scope key"),
        cursor.key().stream().clone(),
    )
    .expect("foreign scope stream key");
    let error = storage
        .load_projection_event_cursor(&foreign_scope_key, Some(cursor))
        .expect_err("another repository scope must reject this cursor");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    let error = storage
        .load_projection_event_cursor(&projection_key(another_session), Some(cursor))
        .expect_err("another resource stream kind must reject this cursor");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
}

fn assert_never_issued_projection_cursors_are_invalid(
    storage: &SqliteStorage,
    first_delivery_key: &ProjectionEventStreamKey,
) {
    let future = ProjectionEventCursor::try_new(
        first_delivery_key.clone(),
        3,
        Some(ControlPlaneEventId("evt_delivery_one_future".into())),
    )
    .expect("shape-valid future cursor");
    let error = storage
        .load_projection_event_cursor(first_delivery_key, Some(&future))
        .expect_err("a cursor beyond the durable stream head was never retained");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);

    let empty_key = projection_key(ProjectionEventStream::Delivery(DeliveryId(
        "dlv_01J00000000000000000000007".into(),
    )));
    let never_issued = ProjectionEventCursor::try_new(
        empty_key.clone(),
        1,
        Some(ControlPlaneEventId("evt_delivery_never_written".into())),
    )
    .expect("shape-valid never-issued cursor");
    let error = storage
        .load_projection_event_cursor(&empty_key, Some(&never_issued))
        .expect_err("a positive cursor for an empty stream was never retained");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
}

fn journal_key() -> AggregateJournalKey {
    AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
        .expect("aggregate journal key")
}

fn journal_create(digest: &str, payload: &[u8]) -> AggregateJournalPublication {
    AggregateJournalPublication::Create {
        key: journal_key(),
        manifest: b"delivery-manifest".to_vec(),
        first_record: AggregateJournalRecord::new(1, digest, payload.to_vec()),
    }
}

fn journal_append(
    expected_tail_digest: &str,
    digest: &str,
    payload: &[u8],
) -> AggregateJournalPublication {
    AggregateJournalPublication::Append {
        key: journal_key(),
        expected_tail_sequence: 1,
        expected_tail_digest: expected_tail_digest.to_owned(),
        record: AggregateJournalRecord::new(2, digest, payload.to_vec()),
    }
}

fn create_v1_fixture(database_path: &Path) {
    let connection = Connection::open(database_path).expect("v1 database should open");
    connection
        .execute_batch(
            "CREATE TABLE product_state (
                 stream_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 payload BLOB NOT NULL
             );
             CREATE TABLE command_receipts (
                 request_id TEXT PRIMARY KEY NOT NULL,
                 command_signature BLOB NOT NULL,
                 stream_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0)
             );
             CREATE TABLE outbox (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 request_id TEXT NOT NULL,
                 topic TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1))
             );
             CREATE INDEX outbox_pending_sequence ON outbox (published, sequence);
             INSERT INTO product_state VALUES ('legacy-stream', 1, X'6C6567616379');
             INSERT INTO command_receipts VALUES (
                 'req_01J00000000000000000000002', X'010203', 'legacy-stream', 1
             );
             INSERT INTO outbox
                 (event_id, request_id, topic, payload, published)
                 VALUES (
                     'legacy-published-event', 'req_01J00000000000000000000002',
                     'control-plane.state.changed', X'7075626C6973686564', 1
                 );
             INSERT INTO outbox
                 (event_id, request_id, topic, payload, published)
                 VALUES (
                     'legacy-event', 'req_01J00000000000000000000002',
                     'control-plane.state.changed', X'6576656E74', 0
                 );
             PRAGMA user_version = 1;",
        )
        .expect("v1 fixture should be created");
    connection.close().expect("v1 fixture should close");
}

fn create_v2_fixture(database_path: &Path) {
    let connection = Connection::open(database_path).expect("v2 database should open");
    connection
        .execute_batch(
            "CREATE TABLE product_state (
                 stream_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 payload BLOB NOT NULL
             );
             CREATE TABLE command_receipts (
                 actor_key BLOB NOT NULL,
                 scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 command_digest TEXT NOT NULL,
                 stream_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 PRIMARY KEY (actor_key, scope_key, request_id)
             );
             CREATE TABLE outbox (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 receipt_actor_key BLOB NOT NULL,
                 receipt_scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 topic TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1)),
                 FOREIGN KEY (receipt_actor_key, receipt_scope_key, request_id)
                     REFERENCES command_receipts (actor_key, scope_key, request_id)
                     DEFERRABLE INITIALLY DEFERRED
             );
             CREATE INDEX outbox_pending_sequence ON outbox (published, sequence);
             INSERT INTO product_state VALUES ('v2-stream', 1, X'76322D7374617465');
             INSERT INTO command_receipts VALUES (
                 X'76322D6163746F72', X'76322D73636F7065',
                 'req_01J00000000000000000000009',
                 'sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                 'v2-stream', 1
             );
             INSERT INTO outbox
                 (event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload)
                 VALUES (
                     'v2-event', X'76322D6163746F72', X'76322D73636F7065',
                     'req_01J00000000000000000000009',
                     'control-plane.state.changed', X'76322D6576656E74'
                 );
             PRAGMA user_version = 2;",
        )
        .expect("v2 fixture should be created");
    connection.close().expect("v2 fixture should close");
}

fn assert_current_receipt_schema(database_path: &Path) {
    let connection = Connection::open(database_path).expect("migrated database should open");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version should be readable");
    assert_eq!(version, 6);
    let receipt_columns = {
        let mut statement = connection
            .prepare("PRAGMA table_info(command_receipts)")
            .expect("receipt schema should be readable");
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("receipt columns should be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("receipt columns should collect")
    };
    assert_eq!(
        receipt_columns,
        [
            "actor_key",
            "scope_key",
            "request_id",
            "command_digest",
            "stream_id",
            "revision",
        ]
    );
    let legacy_digest: String = connection
        .query_row(
            "SELECT command_digest FROM command_receipts WHERE stream_id = 'legacy-stream'",
            [],
            |row| row.get(0),
        )
        .expect("legacy receipt should have one migrated digest");
    assert!(legacy_digest.starts_with("sha256:"));
    assert_eq!(legacy_digest.len(), 71);
    connection.close().expect("migrated database should close");
}

#[test]
fn startup_rejects_a_database_from_a_newer_schema_version() {
    let root = temporary_directory("newer-schema");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database_path).expect("test database should open");
    connection
        .pragma_update(None, "user_version", 7)
        .expect("test schema version should be written");
    connection.close().expect("test database should close");

    let Err(error) = SqliteStorage::open(&root) else {
        panic!("a newer schema must not be silently downgraded");
    };

    assert!(error.to_string().contains("unsupported schema version 7"));
    fs::remove_dir_all(root).expect("rejected database should have no open connection");
}

#[test]
fn v4_public_rows_without_envelope_context_fail_closed_and_rollback_migration() {
    let root = temporary_directory("v4-unbound-public-event");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v2_fixture(&database_path);
    let connection = Connection::open(&database_path).expect("v4 fixture database");
    connection
        .execute_batch(
            "ALTER TABLE outbox ADD COLUMN projection_stream_kind TEXT;\
             ALTER TABLE outbox ADD COLUMN projection_resource_id TEXT;\
             ALTER TABLE outbox ADD COLUMN projection_stream_sequence INTEGER;\
             CREATE TABLE projection_event_stream_heads (\
               scope_key BLOB NOT NULL,stream_kind TEXT NOT NULL,resource_id TEXT NOT NULL,\
               sequence INTEGER NOT NULL,event_id TEXT NOT NULL,\
               PRIMARY KEY(scope_key,stream_kind,resource_id));\
             UPDATE outbox SET projection_stream_kind='delivery',\
               projection_resource_id='dlv_01J00000000000000000000000',\
               projection_stream_sequence=1 WHERE event_id='v2-event';\
             PRAGMA user_version=4;",
        )
        .expect("v4 public fixture should be created");
    connection.close().expect("v4 fixture should close");

    let Err(error) = SqliteStorage::open(&root) else {
        panic!("unbound public row must fail closed");
    };
    assert!(
        error
            .to_string()
            .contains("legacy public outbox rows have no durable envelope context")
    );
    let connection = Connection::open(&database_path).expect("failed migration database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("rolled-back schema version");
    assert_eq!(version, 4);
    let context_columns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('outbox') \
             WHERE name IN ('public_scope_json','public_stream_json',\
                            'public_occurred_at_json','public_source_json')",
            [],
            |row| row.get(0),
        )
        .expect("context column count");
    assert_eq!(context_columns, 0);
    connection
        .close()
        .expect("failed migration connection close");
    fs::remove_dir_all(root).expect("remove v4 fixture");
}

#[test]
fn startup_migrates_v1_receipts_once_without_a_legacy_runtime_lookup_path() {
    let root = temporary_directory("v1-receipt-migration");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v1_fixture(&database_path);

    let mut storage = SqliteStorage::open(&root).expect("v1 storage should migrate to v2");
    assert_eq!(
        storage
            .load_state("legacy-stream")
            .expect("migrated state read")
            .expect("migrated state")
            .payload,
        b"legacy"
    );
    assert_eq!(
        storage.pending_events().expect("migrated outbox read"),
        [OutboxEvent {
            sequence: 2,
            event_id: "legacy-event".to_owned(),
            topic: "control-plane.state.changed".to_owned(),
            payload: b"event".to_vec(),
            projection_cursor: None,
            public_context: None,
        }]
    );

    storage
        .commit(&state_commit(
            (
                "user:new",
                "organization:new",
                "req_01J00000000000000000000002",
            ),
            &format!("sha256:{}", "4".repeat(64)),
            "new-stream",
            0,
            b"new",
            "new-event",
        ))
        .expect("a canonical identity may reuse a v1 globally-scoped request id");
    Box::new(storage).close().expect("storage should close");

    assert_current_receipt_schema(&database_path);
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn startup_migrates_v2_to_the_single_journal_schema_before_serving() {
    let root = temporary_directory("v2-journal-migration");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v2_fixture(&database_path);

    let storage = SqliteStorage::open(&root).expect("v2 storage should migrate to v4");
    assert_eq!(
        storage
            .load_state("v2-stream")
            .expect("migrated state read")
            .expect("migrated state")
            .payload,
        b"v2-state"
    );
    assert_eq!(
        storage.pending_events().expect("migrated outbox")[0].event_id,
        "v2-event"
    );
    assert!(
        storage
            .load_journal(&journal_key())
            .expect("new journal read")
            .is_none()
    );
    Box::new(storage).close().expect("storage should close");

    let connection = Connection::open(&database_path).expect("migrated database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 6);
    for table in ["aggregate_journals", "aggregate_journal_records"] {
        let exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("journal table lookup");
        assert_eq!(exists, 1, "{table} must exist after v2 migration");
    }
    connection
        .close()
        .expect("inspection connection should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn interrupted_v2_migration_rolls_back_before_serving() {
    let root = temporary_directory("v2-migration-rollback");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v2_fixture(&database_path);
    let connection = Connection::open(&database_path).expect("v2 database");
    connection
        .execute_batch("CREATE VIEW aggregate_journal_records AS SELECT 1 AS incompatible_fixture;")
        .expect("migration blocker should install");
    connection.close().expect("v2 database should close");

    let error = SqliteStorage::open(&root)
        .err()
        .expect("migration failure must prevent storage startup");
    assert!(
        error
            .to_string()
            .contains("aggregate journal record schema is not canonical")
    );

    let connection = Connection::open(&database_path).expect("rolled back database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("rolled back version");
    assert_eq!(version, 2);
    let journal_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema \
             WHERE type = 'table' AND name = 'aggregate_journals'",
            [],
            |row| row.get(0),
        )
        .expect("rolled back journal table lookup");
    assert_eq!(journal_table_count, 0);
    let state: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = 'v2-stream'",
            [],
            |row| row.get(0),
        )
        .expect("v2 state must remain intact");
    assert_eq!(state, b"v2-state");
    connection
        .close()
        .expect("inspection connection should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn all_four_public_event_streams_round_trip_across_restart() {
    let root = temporary_directory("four-public-streams");
    let streams = [
        ProjectionEventStream::Scope,
        ProjectionEventStream::Delivery(DeliveryId("dlv_01J00000000000000000000000".into())),
        ProjectionEventStream::ProductSession(ProductSessionId(
            "psn_01J00000000000000000000000".into(),
        )),
        ProjectionEventStream::Lease {
            worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
            lease_id: LeaseId("lse_01J00000000000000000000000".into()),
        },
    ];
    let mut storage = SqliteStorage::open(&root).expect("storage should open");
    for (index, stream) in streams.iter().enumerate() {
        storage
            .commit(&projection_commit(
                &format!("req_four_streams_{index:02}"),
                &format!("state:four-streams:{index}"),
                &format!("evt_four_streams_event_{index:02}"),
                stream.clone(),
            ))
            .expect("each generated stream kind should commit");
    }
    Box::new(storage).close().expect("first storage close");

    let storage = SqliteStorage::open(&root).expect("storage should restart");
    for (index, stream) in streams.iter().enumerate() {
        let cursor = storage
            .load_projection_event_cursor(&projection_key(stream.clone()), None)
            .expect("restarted stream cursor");
        assert_eq!(cursor.sequence(), 1);
        assert_eq!(
            cursor.event_id().expect("stream event id").0,
            format!("evt_four_streams_event_{index:02}")
        );
        let event = storage
            .load_outbox_event(&format!("evt_four_streams_event_{index:02}"))
            .expect("event lookup")
            .expect("durable event");
        assert_eq!(
            event
                .event()
                .public_context
                .as_ref()
                .expect("public context")
                .stream(),
            stream
        );
    }
    Box::new(storage).close().expect("restarted storage close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn write_scope_and_lease_authority_events(
    storage: &mut SqliteStorage,
    first_scope: &PublicEventScope,
    second_scope: &PublicEventScope,
    first_lease: &ProjectionEventStream,
    second_lease: &ProjectionEventStream,
) {
    let writes = [
        (
            "req_scope_first_01",
            "state:scope:first:1",
            "evt_scope_first_event_01",
            ProjectionEventStream::Scope,
            first_scope.clone(),
        ),
        (
            "req_scope_second_01",
            "state:scope:second:1",
            "evt_scope_second_event_01",
            ProjectionEventStream::Scope,
            second_scope.clone(),
        ),
        (
            "req_scope_first_02",
            "state:scope:first:2",
            "evt_scope_first_event_02",
            ProjectionEventStream::Scope,
            first_scope.clone(),
        ),
        (
            "req_lease_first_01",
            "state:lease:first:1",
            "evt_lease_first_event_01",
            first_lease.clone(),
            first_scope.clone(),
        ),
        (
            "req_lease_second_01",
            "state:lease:second:1",
            "evt_lease_second_event_01",
            second_lease.clone(),
            first_scope.clone(),
        ),
        (
            "req_lease_first_02",
            "state:lease:first:2",
            "evt_lease_first_event_02",
            first_lease.clone(),
            first_scope.clone(),
        ),
    ];
    for (request_id, state_stream, event_id, stream, scope) in writes {
        storage
            .commit(&projection_commit_for_scope(
                request_id,
                state_stream,
                event_id,
                stream,
                scope,
            ))
            .expect("authority-bound stream event should commit");
    }
}

#[test]
fn scope_and_lease_stream_positions_are_bound_to_exact_authority() {
    let root = temporary_directory("scope-lease-authority");
    let first_scope = projection_scope();
    let second_scope = PublicEventScope::Repository {
        organization_id: OrganizationId("org_01J00000000000000000000000".into()),
        workspace_id: WorkspaceId("wsp_01J00000000000000000000000".into()),
        project_id: ProjectId("prj_01J00000000000000000000000".into()),
        repository_id: RepositoryId("rep_01J00000000000000000000001".into()),
    };
    let first_lease = ProjectionEventStream::Lease {
        worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
        lease_id: LeaseId("lse_01J00000000000000000000000".into()),
    };
    let second_lease = ProjectionEventStream::Lease {
        worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
        lease_id: LeaseId("lse_01J00000000000000000000001".into()),
    };
    let mut storage = SqliteStorage::open(&root).expect("storage should open");
    write_scope_and_lease_authority_events(
        &mut storage,
        &first_scope,
        &second_scope,
        &first_lease,
        &second_lease,
    );
    let assertions = [
        (
            projection_key_for_scope(ProjectionEventStream::Scope, &first_scope),
            2,
            "evt_scope_first_event_02",
        ),
        (
            projection_key_for_scope(ProjectionEventStream::Scope, &second_scope),
            1,
            "evt_scope_second_event_01",
        ),
        (
            projection_key_for_scope(first_lease, &first_scope),
            2,
            "evt_lease_first_event_02",
        ),
        (
            projection_key_for_scope(second_lease, &first_scope),
            1,
            "evt_lease_second_event_01",
        ),
    ];
    for (key, sequence, event_id) in assertions {
        let cursor = storage
            .load_projection_event_cursor(&key, None)
            .expect("exact authority stream cursor");
        assert_eq!(cursor.sequence(), sequence);
        assert_eq!(cursor.event_id().expect("event id").0, event_id);
    }
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn malformed_scope_and_lease_stream_storage_fails_closed() {
    let invalid = NewOutboxEvent::public_projection(
        ControlPlaneEventId("evt_invalid_lease_stream".into()),
        "projection.invalidated",
        b"{}".to_vec(),
        ProjectionEventStream::Lease {
            worker_id: WorkerId("worker-not-canonical".into()),
            lease_id: LeaseId("lse_01J00000000000000000000000".into()),
        },
        projection_scope(),
        Instant("2026-08-27T00:00:00.000Z".into()),
        PublicEventSource::ControlPlane {
            actor: projection_actor(),
            component: "storage-test".into(),
        },
    )
    .expect_err("non-canonical Lease stream must be rejected");
    assert_eq!(invalid.kind(), StorageErrorKind::InvalidInput);
    let mismatched_worker = NewOutboxEvent::public_projection(
        ControlPlaneEventId("evt_mismatched_lease_authority".into()),
        "projection.invalidated",
        b"{}".to_vec(),
        ProjectionEventStream::Lease {
            worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
            lease_id: LeaseId("lse_01J00000000000000000000000".into()),
        },
        projection_scope(),
        Instant("2026-08-27T00:00:00.000Z".into()),
        PublicEventSource::ExecutionWorker {
            worker_id: WorkerId("wrk_01J00000000000000000000001".into()),
            worker_session_id: WorkerSessionId("wsn_01J00000000000000000000000".into()),
            lease_id: LeaseId("lse_01J00000000000000000000000".into()),
            codex_thread_id: CodexThreadId("cdx_01J00000000000000000000000".into()),
        },
    )
    .expect_err("Lease stream must equal its Worker authority");
    assert_eq!(mismatched_worker.kind(), StorageErrorKind::InvalidInput);

    let root = temporary_directory("malformed-lease-storage");
    let stream = ProjectionEventStream::Lease {
        worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
        lease_id: LeaseId("lse_01J00000000000000000000000".into()),
    };
    let mut storage = SqliteStorage::open(&root).expect("storage should open");
    storage
        .commit(&projection_commit(
            "req_malformed_lease_storage",
            "state:malformed-lease",
            "evt_malformed_lease_storage",
            stream,
        ))
        .expect("valid Lease stream should commit");
    Box::new(storage).close().expect("storage should close");
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("raw database");
    connection
        .execute(
            "UPDATE outbox SET projection_resource_id = 'wrk_bad/lse_bad/extra'
             WHERE event_id = 'evt_malformed_lease_storage'",
            [],
        )
        .expect("corrupt stored Lease identity");
    connection.close().expect("raw database close");
    let storage = SqliteStorage::open(&root).expect("storage should reopen");
    let error = storage
        .load_outbox_event("evt_malformed_lease_storage")
        .expect_err("corrupt stored Lease stream must fail closed");
    assert_eq!(error.kind(), StorageErrorKind::Adapter);
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

type ProjectionHeadRow = (String, String, String, i64, String);
type OutboxSequenceRow = (i64, String, Option<i64>);

fn install_v5_projection_stream_fixture(
    root: &Path,
) -> (Vec<ProjectionHeadRow>, Vec<OutboxSequenceRow>) {
    let delivery =
        ProjectionEventStream::Delivery(DeliveryId("dlv_01J00000000000000000000000".into()));
    let product_session = ProjectionEventStream::ProductSession(ProductSessionId(
        "psn_01J00000000000000000000000".into(),
    ));
    let mut storage = SqliteStorage::open(root).expect("fresh v6 storage should open");
    storage
        .commit(&projection_commit(
            "req_v5_delivery_01",
            "state:v5:delivery:1",
            "evt_v5_delivery_event_01",
            delivery.clone(),
        ))
        .expect("Delivery head seed");
    storage
        .commit(&projection_commit(
            "req_v5_delivery_02",
            "state:v5:delivery:2",
            "evt_v5_delivery_event_02",
            delivery,
        ))
        .expect("Delivery head advance");
    storage
        .commit(&projection_commit(
            "req_v5_product_session_01",
            "state:v5:product-session:1",
            "evt_v5_product_session_01",
            product_session,
        ))
        .expect("ProductSession head seed");
    Box::new(storage).close().expect("seed storage close");

    let database_path = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database_path).expect("migration fixture database");
    let before_heads = projection_head_rows(&connection);
    let before_outbox_sequences = outbox_sequences(&connection);
    connection
        .execute_batch(
            "ALTER TABLE projection_event_stream_heads
                 RENAME TO projection_event_stream_heads_v6_fixture;
             CREATE TABLE projection_event_stream_heads (
                 scope_key BLOB NOT NULL,
                 stream_kind TEXT NOT NULL CHECK (
                     stream_kind IN ('delivery', 'product-session')
                 ),
                 resource_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 event_id TEXT NOT NULL,
                 PRIMARY KEY (scope_key, stream_kind, resource_id),
                 UNIQUE (scope_key, stream_kind, resource_id, sequence),
                 FOREIGN KEY (event_id) REFERENCES outbox (event_id)
             );
             INSERT INTO projection_event_stream_heads
                 (scope_key, stream_kind, resource_id, sequence, event_id)
                 SELECT scope_key, stream_kind, resource_id, sequence, event_id
                 FROM projection_event_stream_heads_v6_fixture;
             DROP TABLE projection_event_stream_heads_v6_fixture;
             PRAGMA user_version = 5;",
        )
        .expect("v5 heads fixture should be installed");
    connection.close().expect("migration fixture close");
    (before_heads, before_outbox_sequences)
}

#[test]
fn v5_to_v6_migration_preserves_existing_heads_and_enables_scope_and_lease() {
    let root = temporary_directory("v5-to-v6-streams");
    let (before_heads, before_outbox_sequences) = install_v5_projection_stream_fixture(&root);
    let database_path = root.join("control-plane.sqlite3");

    let mut storage = SqliteStorage::open(&root).expect("v5 storage should migrate to v6");
    let connection = Connection::open(&database_path).expect("migrated database inspection");
    assert_eq!(projection_head_rows(&connection), before_heads);
    assert_eq!(outbox_sequences(&connection), before_outbox_sequences);
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("migrated schema version");
    assert_eq!(version, 6);
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type='table' AND name='projection_event_stream_heads'",
            [],
            |row| row.get(0),
        )
        .expect("migrated heads schema");
    assert!(schema.contains("'scope'"));
    assert!(schema.contains("'lease'"));
    connection.close().expect("migration inspection close");

    for (request_id, state_stream, event_id, stream) in [
        (
            "req_v6_scope_01",
            "state:v6:scope:1",
            "evt_v6_scope_event_01",
            ProjectionEventStream::Scope,
        ),
        (
            "req_v6_lease_01",
            "state:v6:lease:1",
            "evt_v6_lease_event_01",
            ProjectionEventStream::Lease {
                worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
                lease_id: LeaseId("lse_01J00000000000000000000000".into()),
            },
        ),
    ] {
        storage
            .commit(&projection_commit(
                request_id,
                state_stream,
                event_id,
                stream,
            ))
            .expect("new v6 stream should commit after migration");
    }
    Box::new(storage).close().expect("migrated storage close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn empty_v4_projection_schema_migrates_through_public_context_to_v6() {
    let root = temporary_directory("empty-v4-to-v6");
    fs::create_dir_all(&root).expect("fixture directory");
    let database_path = root.join("control-plane.sqlite3");
    create_v2_fixture(&database_path);
    let connection = Connection::open(&database_path).expect("v4 fixture database");
    connection
        .execute_batch(
            "CREATE TABLE aggregate_journals (
                 aggregate_type TEXT NOT NULL,
                 aggregate_id TEXT NOT NULL,
                 manifest BLOB NOT NULL,
                 PRIMARY KEY (aggregate_type, aggregate_id)
             );
             CREATE TABLE aggregate_journal_records (
                 aggregate_type TEXT NOT NULL,
                 aggregate_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 digest TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 PRIMARY KEY (aggregate_type, aggregate_id, sequence),
                 FOREIGN KEY (aggregate_type, aggregate_id)
                     REFERENCES aggregate_journals (aggregate_type, aggregate_id)
                     ON DELETE CASCADE
             );
             ALTER TABLE outbox ADD COLUMN projection_stream_kind TEXT;
             ALTER TABLE outbox ADD COLUMN projection_resource_id TEXT;
             ALTER TABLE outbox ADD COLUMN projection_stream_sequence INTEGER;
             CREATE TABLE projection_event_stream_heads (
                 scope_key BLOB NOT NULL,
                 stream_kind TEXT NOT NULL CHECK (
                     stream_kind IN ('delivery', 'product-session')
                 ),
                 resource_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 event_id TEXT NOT NULL,
                 PRIMARY KEY (scope_key, stream_kind, resource_id),
                 UNIQUE (scope_key, stream_kind, resource_id, sequence),
                 FOREIGN KEY (event_id) REFERENCES outbox (event_id)
             );
             CREATE UNIQUE INDEX outbox_projection_stream_sequence
                 ON outbox (receipt_scope_key, projection_stream_kind,
                            projection_resource_id, projection_stream_sequence)
                 WHERE projection_stream_kind IS NOT NULL;
             PRAGMA user_version = 4;",
        )
        .expect("empty v4 projection schema fixture");
    connection.close().expect("v4 fixture close");

    let storage = SqliteStorage::open(&root).expect("empty v4 should migrate to v6");
    assert_eq!(
        storage
            .load_state("v2-stream")
            .expect("legacy state read")
            .expect("legacy state")
            .payload,
        b"v2-state"
    );
    Box::new(storage).close().expect("migrated storage close");
    let connection = Connection::open(&database_path).expect("migrated v6 database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("migrated version");
    assert_eq!(version, 6);
    let context_columns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('outbox')
             WHERE name IN ('public_scope_json','public_stream_json',
                            'public_occurred_at_json','public_source_json')",
            [],
            |row| row.get(0),
        )
        .expect("public context columns");
    assert_eq!(context_columns, 4);
    connection.close().expect("migration inspection close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn projection_head_rows(connection: &Connection) -> Vec<ProjectionHeadRow> {
    let mut statement = connection
        .prepare(
            "SELECT hex(scope_key), stream_kind, resource_id, sequence, event_id
             FROM projection_event_stream_heads
             ORDER BY scope_key, stream_kind, resource_id",
        )
        .expect("projection head query");
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("projection head rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("projection head collection")
}

fn outbox_sequences(connection: &Connection) -> Vec<OutboxSequenceRow> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, event_id, projection_stream_sequence
             FROM outbox ORDER BY sequence",
        )
        .expect("outbox sequence query");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("outbox sequence rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("outbox sequence collection")
}

#[test]
fn sqlite_adapter_uses_no_dynamic_savepoint_or_sql_identifier_path() {
    let source = include_str!("../src/lib.rs");
    for forbidden in [
        ".savepoint(",
        "savepoint_with_name",
        "new_unchecked",
        "unchecked_transaction",
        "execute(&format!",
        "execute_batch(&format!",
        "prepare(&format!",
        "query_row(&format!",
    ] {
        assert!(
            !source.contains(forbidden),
            "SQLite adapter introduced forbidden dynamic SQL path: {forbidden}"
        );
    }
    assert!(source.contains("params!["));
}

#[test]
fn projection_event_positions_are_stream_local_durable_and_exact() {
    let root = temporary_directory("projection-event-streams");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let delivery_one = delivery_projection_stream("dlv_14P4FYB8C2ZEFAWXKNB3A9TXZR");
    let delivery_two = delivery_projection_stream("dlv_6KSS07PEY1TVMHGNR5DT4GN9KX");
    let session_one = ProjectionEventStream::ProductSession(ProductSessionId("session-one".into()));
    let session_two = ProjectionEventStream::ProductSession(ProductSessionId("session-two".into()));
    let first_delivery_cursor = write_interleaved_projection_events(
        &mut storage,
        &delivery_one,
        &delivery_two,
        &session_one,
        &session_two,
    );
    assert_projection_stream_heads(
        &storage,
        &[
            (delivery_one.clone(), 2, "evt_delivery_one_0002"),
            (delivery_two.clone(), 1, "evt_delivery_two_0001"),
            (session_one.clone(), 2, "evt_session_one_0002"),
            (session_two.clone(), 1, "evt_session_two_0001"),
        ],
    );
    let global = storage.pending_events().expect("global outbox");
    assert_eq!(
        global
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        global
            .iter()
            .map(|event| {
                event
                    .projection_cursor
                    .as_ref()
                    .expect("projection cursor")
                    .sequence()
            })
            .collect::<Vec<_>>(),
        vec![1, 1, 1, 2, 1, 2]
    );

    let first_delivery_key = projection_key(delivery_one);
    assert_projection_cursor_key_is_exact(
        &storage,
        &first_delivery_cursor,
        delivery_two,
        session_two,
    );
    assert_eq!(
        storage
            .load_projection_event_cursor(&first_delivery_key, Some(&first_delivery_cursor),)
            .expect("retained historical cursor"),
        first_delivery_cursor
    );
    Box::new(storage).close().expect("storage should close");

    let storage = SqliteStorage::open(&root).expect("storage should restart");
    assert_eq!(
        storage
            .load_projection_event_cursor(&first_delivery_key, None)
            .expect("restarted stream head")
            .sequence(),
        2
    );
    assert_eq!(
        storage
            .load_projection_event_cursor(&first_delivery_key, Some(&first_delivery_cursor),)
            .expect("restarted retained cursor"),
        first_delivery_cursor
    );
    Box::new(storage).close().expect("storage should close");

    let connection =
        Connection::open(root.join("control-plane.sqlite3")).expect("retention fixture database");
    connection
        .execute(
            "DELETE FROM outbox WHERE event_id = 'evt_delivery_one_0001'",
            [],
        )
        .expect("old retained event can be compacted");
    connection.close().expect("fixture should close");
    let storage = SqliteStorage::open(&root).expect("storage after compaction");
    let error = storage
        .load_projection_event_cursor(&first_delivery_key, Some(&first_delivery_cursor))
        .expect_err("compacted exact cursor must expire");
    assert_eq!(error.kind(), StorageErrorKind::EventCursorExpired);

    let forged = ProjectionEventCursor::try_new(
        first_delivery_key.clone(),
        2,
        Some(ControlPlaneEventId("evt_delivery_one_fake".into())),
    )
    .expect("shape-valid forged cursor");
    let error = storage
        .load_projection_event_cursor(&first_delivery_key, Some(&forged))
        .expect_err("eventId mismatch is not retention loss");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    assert_never_issued_projection_cursors_are_invalid(&storage, &first_delivery_key);
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sql_looking_business_values_remain_bound_values() {
    let root = temporary_directory("bound-values");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let stream_id = "stream'); DROP TABLE product_state; --";
    let event_id = "event'); DROP TABLE outbox; --";
    let commit = StateCommit::new(
        receipt_identity(
            "actor'); DROP TABLE command_receipts; --",
            "scope'); DROP TABLE outbox; --",
            "request'); DROP TABLE command_receipts; --",
        ),
        Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        stream_id,
        0,
        b"state".to_vec(),
        vec![NewOutboxEvent::internal(
            event_id,
            "topic'); PRAGMA writable_schema=ON; --",
            b"event".to_vec(),
        )],
    );

    storage
        .commit(&commit)
        .expect("SQL-looking values should commit as data");

    let state = storage
        .load_state(stream_id)
        .expect("the state table should remain readable")
        .expect("the SQL-looking stream id should be stored literally");
    assert_eq!((state.revision, state.payload), (1, b"state".to_vec()));
    let events = storage
        .pending_events()
        .expect("the outbox table should remain readable");
    assert_eq!(events[0].event_id, event_id);
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn receipt_identity_scopes_one_request_id_by_actor_and_full_scope() {
    let root = temporary_directory("receipt-scope");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let request_id = "req_01J00000000000000000000000";
    let digest = format!("sha256:{}", "1".repeat(64));

    let first = storage
        .commit(&state_commit(
            (
                "user:one",
                "repository:org:workspace:project:repository-one",
                request_id,
            ),
            &digest,
            "stream-one",
            0,
            b"one",
            "event-one",
        ))
        .expect("first identity should commit");
    let second_actor = storage
        .commit(&state_commit(
            (
                "user:two",
                "repository:org:workspace:project:repository-one",
                request_id,
            ),
            &digest,
            "stream-two",
            0,
            b"two",
            "event-two",
        ))
        .expect("another actor may reuse the request id");
    let second_scope = storage
        .commit(&state_commit(
            (
                "user:one",
                "repository:org:workspace:project:repository-two",
                request_id,
            ),
            &digest,
            "stream-three",
            0,
            b"three",
            "event-three",
        ))
        .expect("another full scope may reuse the request id");

    assert!(!first.idempotent_replay);
    assert!(!second_actor.idempotent_replay);
    assert!(!second_scope.idempotent_replay);
    assert_eq!(
        storage
            .load_state("stream-three")
            .expect("third stream read")
            .expect("third stream state")
            .payload,
        b"three"
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn receipt_identity_replays_only_the_same_digest_and_rejects_a_changed_digest() {
    let root = temporary_directory("receipt-digest");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let request_id = "req_01J00000000000000000000001";
    let first_digest = format!("sha256:{}", "2".repeat(64));
    let changed_digest = format!("sha256:{}", "3".repeat(64));
    let first = state_commit(
        ("user:one", "project:org:workspace:project", request_id),
        &first_digest,
        "stream-one",
        0,
        b"one",
        "event-original",
    );

    storage.commit(&first).expect("first commit should succeed");
    let replay = storage
        .commit(&state_commit(
            ("user:one", "project:org:workspace:project", request_id),
            &first_digest,
            "ignored-retry-stream",
            99,
            b"ignored retry state",
            "ignored-retry-event",
        ))
        .expect("the same receipt identity and digest should replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.stream_id, "stream-one");
    assert_eq!(replay.events[0].event_id, "event-original");

    let error = storage
        .commit(&state_commit(
            ("user:one", "project:org:workspace:project", request_id),
            &changed_digest,
            "stream-one",
            1,
            b"changed",
            "event-changed",
        ))
        .expect_err("the same receipt identity cannot represent another digest");
    assert_eq!(error.kind(), StorageErrorKind::RequestConflict);

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn one_commit_makes_state_journal_receipt_and_outbox_authoritative_together() {
    let root = temporary_directory("atomic-journal-create");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let commit = state_commit(
        (
            "user:one",
            "repository:org:workspace:project:repository-one",
            "req_01J00000000000000000000003",
        ),
        &format!("sha256:{}", "5".repeat(64)),
        "delivery:one",
        0,
        b"canonical-delivery-v1",
        "job-event-one",
    )
    .with_journal_publication(journal_create("record-one", b"journal-record-one"));

    let receipt = storage
        .commit(&commit)
        .expect("one transaction should commit");

    assert_eq!(receipt.revision, 1);
    assert!(!receipt.idempotent_replay);
    assert_eq!(receipt.events.len(), 1);
    assert_eq!(receipt.events[0].event_id, "job-event-one");
    assert_eq!(receipt.events[0].payload, b"event");
    let state = storage
        .load_state("delivery:one")
        .expect("state read")
        .expect("committed state");
    assert_eq!(
        (state.revision, state.payload),
        (1, b"canonical-delivery-v1".to_vec())
    );
    let journal = storage
        .load_journal(&journal_key())
        .expect("journal read")
        .expect("committed journal");
    assert_eq!(journal.manifest, b"delivery-manifest");
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].sequence, 1);
    assert_eq!(journal.records[0].digest, "record-one");
    assert_eq!(journal.records[0].payload, b"journal-record-one");

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sqlite_state_commit_accepts_matching_secondary_revision_and_missing_zero_guard() {
    let root = temporary_directory("state-revision-guard-success");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&state_commit(
            ("guard-seed-actor", "guard-seed-scope", "guard-seed-request"),
            &format!("sha256:{}", "a".repeat(64)),
            "secondary-stream",
            0,
            b"secondary-v1",
            "guard-seed-event",
        ))
        .expect("secondary stream seed should commit");

    let guarded = state_commit(
        ("guarded-actor", "guarded-scope", "guarded-request"),
        &format!("sha256:{}", "b".repeat(64)),
        "primary-stream",
        0,
        b"primary-v1",
        "guarded-event",
    )
    .with_state_guard(
        StateRevisionGuard::new("secondary-stream", 1).expect("valid secondary guard"),
    )
    .with_state_guard(
        StateRevisionGuard::new("missing-secondary-stream", 0).expect("valid missing-stream guard"),
    );

    let receipt = storage
        .commit(&guarded)
        .expect("matching secondary revisions should commit");
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        storage
            .load_state("primary-stream")
            .expect("primary state read")
            .expect("primary state")
            .revision,
        1
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sqlite_state_revision_guard_conflict_writes_none_of_the_four_commit_members() {
    let root = temporary_directory("state-revision-guard-conflict");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&state_commit(
            ("guard-conflict-seed", "guard-conflict", "seed-request"),
            &format!("sha256:{}", "c".repeat(64)),
            "secondary-stream",
            0,
            b"secondary-v1",
            "guard-conflict-seed-event",
        ))
        .expect("secondary stream seed should commit");

    let before_counts = [
        table_count(&root.join("control-plane.sqlite3"), "product_state"),
        table_count(&root.join("control-plane.sqlite3"), "command_receipts"),
        table_count(&root.join("control-plane.sqlite3"), "outbox"),
        table_count(&root.join("control-plane.sqlite3"), "audit_outbox"),
    ];
    let identity = receipt_identity(
        "guard-conflict-actor",
        "guard-conflict-scope",
        "guard-conflict-request",
    );
    let digest = Sha256Digest(format!("sha256:{}", "d".repeat(64)));
    let commit = StateCommit::new(
        identity.clone(),
        digest.clone(),
        "primary-stream",
        0,
        b"primary-must-not-write".to_vec(),
        vec![NewOutboxEvent::internal(
            "guard-conflict-event",
            "control-plane.state.changed",
            b"event".to_vec(),
        )],
    )
    .with_state_guard(StateRevisionGuard::new("secondary-stream", 0).expect("valid stale guard"))
    .with_pending_audit_event(
        PendingAuditEvent::new("guard-conflict-audit", b"audit".to_vec())
            .expect("pending audit event"),
    );

    let error = storage
        .commit(&commit)
        .expect_err("stale secondary revision must reject the commit");
    assert_eq!(error.kind(), StorageErrorKind::RevisionConflict);
    assert!(error.is_state_guard_conflict());
    assert_eq!(
        [
            table_count(&root.join("control-plane.sqlite3"), "product_state"),
            table_count(&root.join("control-plane.sqlite3"), "command_receipts"),
            table_count(&root.join("control-plane.sqlite3"), "outbox"),
            table_count(&root.join("control-plane.sqlite3"), "audit_outbox"),
        ],
        before_counts,
        "a guard conflict must not write state, receipt, outbox, or audit rows"
    );
    assert!(
        storage
            .load_state("primary-stream")
            .expect("primary state read")
            .is_none()
    );
    assert!(
        storage
            .load_receipt(&identity, &digest)
            .expect("receipt read")
            .is_none()
    );
    assert!(
        storage
            .load_pending_audit_event(&identity)
            .expect("audit read")
            .is_none()
    );
    assert!(
        storage
            .pending_events()
            .expect("outbox read")
            .iter()
            .all(|event| event.event_id != "guard-conflict-event")
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn state_revision_guard_rejects_invalid_identity_range_duplicate_and_primary_stream() {
    assert_eq!(
        StateRevisionGuard::new("", 0)
            .expect_err("an empty guard stream must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    assert_eq!(
        StateRevisionGuard::new("out-of-range", i64::MAX as u64 + 1)
            .expect_err("a guard revision outside SQLite's range must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let root = temporary_directory("state-revision-guard-validation");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let duplicate = state_commit(
        ("guard-validation", "guard-validation", "duplicate-request"),
        &format!("sha256:{}", "e".repeat(64)),
        "primary-stream",
        0,
        b"must-not-write",
        "duplicate-event",
    )
    .with_state_guard(StateRevisionGuard::new("secondary-stream", 0).expect("guard"))
    .with_state_guard(StateRevisionGuard::new("secondary-stream", 0).expect("duplicate guard"));
    assert_eq!(
        storage
            .commit(&duplicate)
            .expect_err("duplicate guard streams must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let primary = state_commit(
        ("guard-validation", "guard-validation", "primary-request"),
        &format!("sha256:{}", "f".repeat(64)),
        "primary-stream",
        0,
        b"must-not-write",
        "primary-event",
    )
    .with_state_guard(StateRevisionGuard::new("primary-stream", 0).expect("primary guard"));
    assert_eq!(
        storage
            .commit(&primary)
            .expect_err("the primary stream must not also be a secondary guard")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let database_path = root.join("control-plane.sqlite3");
    assert_eq!(table_count(&database_path, "product_state"), 0);
    assert_eq!(table_count(&database_path, "command_receipts"), 0);
    assert_eq!(table_count(&database_path, "outbox"), 0);
    assert_eq!(table_count(&database_path, "audit_outbox"), 0);

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sqlite_state_revision_guard_replay_wins_before_a_later_guard_revision_change() {
    let root = temporary_directory("state-revision-guard-replay");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");

    let identity = receipt_identity(
        "guard-replay-actor",
        "guard-replay-scope",
        "guard-replay-request",
    );
    let digest = Sha256Digest(format!("sha256:{}", "2".repeat(64)));
    let original = StateCommit::new(
        identity.clone(),
        digest.clone(),
        "primary-stream",
        0,
        b"primary-v1".to_vec(),
        vec![NewOutboxEvent::internal(
            "guard-replay-original-event",
            "control-plane.state.changed",
            b"original-event".to_vec(),
        )],
    )
    .with_state_guard(StateRevisionGuard::new("secondary-stream", 0).expect("guard"))
    .with_pending_audit_event(
        PendingAuditEvent::new("guard-replay-audit", b"original-audit".to_vec())
            .expect("pending audit event"),
    );
    let first_receipt = storage.commit(&original).expect("original guarded commit");
    assert!(!first_receipt.idempotent_replay);

    storage
        .commit(&state_commit(
            ("guard-replay-change", "guard-replay", "change-request"),
            &format!("sha256:{}", "3".repeat(64)),
            "secondary-stream",
            0,
            b"secondary-v1",
            "guard-replay-change-event",
        ))
        .expect("secondary stream revision change should commit");
    assert_eq!(
        storage
            .load_state("secondary-stream")
            .expect("secondary state read")
            .expect("secondary state")
            .revision,
        1
    );

    let database_path = root.join("control-plane.sqlite3");
    let before_replay_counts = [
        table_count(&database_path, "product_state"),
        table_count(&database_path, "command_receipts"),
        table_count(&database_path, "outbox"),
        table_count(&database_path, "audit_outbox"),
    ];
    let replay = storage
        .commit(&original)
        .expect("exact receipt replay must precede guard evaluation");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.receipt_identity, first_receipt.receipt_identity);
    assert_eq!(replay.command_digest, first_receipt.command_digest);
    assert_eq!(replay.stream_id, "primary-stream");
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.events, first_receipt.events);
    assert_eq!(
        [
            table_count(&database_path, "product_state"),
            table_count(&database_path, "command_receipts"),
            table_count(&database_path, "outbox"),
            table_count(&database_path, "audit_outbox"),
        ],
        before_replay_counts,
        "receipt replay must not write after the guarded stream changes"
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sqlite_state_commit_persists_primary_two_secondary_states_and_members_across_restart() {
    let root = temporary_directory("state-mutation-atomic-restart");
    let identity = receipt_identity(
        "mutation-actor",
        "mutation-scope",
        "mutation-atomic-request",
    );
    let digest = Sha256Digest(format!("sha256:{}", "7".repeat(64)));
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&state_commit(
            (
                "mutation-update-seed",
                "mutation-scope",
                "mutation-update-seed-request",
            ),
            &format!("sha256:{}", "6".repeat(64)),
            "mutation-secondary-one",
            0,
            b"secondary-one-v1",
            "mutation-update-seed-event",
        ))
        .expect("secondary update seed should commit");
    let commit = StateCommit::new(
        identity.clone(),
        digest.clone(),
        "mutation-primary",
        0,
        b"primary-v1".to_vec(),
        vec![NewOutboxEvent::internal(
            "mutation-atomic-event",
            "control-plane.state.changed",
            b"event-v1".to_vec(),
        )],
    )
    .with_state_mutation(
        StateMutation::new("mutation-secondary-one", 1, b"secondary-one-v2".to_vec())
            .expect("first mutation"),
    )
    .with_state_mutation(
        StateMutation::new("mutation-secondary-two", 0, b"secondary-two-v1".to_vec())
            .expect("second mutation"),
    )
    .with_journal_publication(journal_create(
        "mutation-record-one",
        b"mutation-journal-record",
    ))
    .with_pending_audit_event(
        PendingAuditEvent::new("mutation-audit", b"mutation-audit-v1".to_vec())
            .expect("pending audit event"),
    );

    let receipt = storage
        .commit(&commit)
        .expect("all transaction members should commit");
    assert_eq!(receipt.revision, 1);
    assert!(!receipt.idempotent_replay);
    Box::new(storage).close().expect("storage should close");

    let storage = SqliteStorage::open(&root).expect("storage should reopen");
    for (stream_id, revision, payload) in [
        ("mutation-primary", 1, b"primary-v1".as_slice()),
        ("mutation-secondary-one", 2, b"secondary-one-v2".as_slice()),
        ("mutation-secondary-two", 1, b"secondary-two-v1".as_slice()),
    ] {
        let state = storage
            .load_state(stream_id)
            .expect("state read")
            .expect("committed state");
        assert_eq!(
            (state.revision, state.payload.as_slice()),
            (revision, payload)
        );
    }
    let replay = storage
        .load_receipt(&identity, &digest)
        .expect("receipt read")
        .expect("durable receipt");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.events, receipt.events);
    assert_eq!(
        storage
            .load_pending_audit_event(&identity)
            .expect("audit read")
            .expect("durable audit")
            .payload(),
        b"mutation-audit-v1"
    );
    assert_eq!(
        storage
            .load_journal(&journal_key())
            .expect("journal read")
            .expect("durable journal")
            .records[0]
            .payload,
        b"mutation-journal-record"
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn stale_state_mutation_rejects_every_transaction_member_before_any_write() {
    let root = temporary_directory("state-mutation-conflict");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&state_commit(
            ("mutation-seed", "mutation-scope", "mutation-seed-request"),
            &format!("sha256:{}", "8".repeat(64)),
            "mutation-stale-secondary",
            0,
            b"secondary-v1",
            "mutation-seed-event",
        ))
        .expect("secondary seed should commit");

    let database_path = root.join("control-plane.sqlite3");
    let before_counts = atomic_member_counts(&database_path);
    let identity = receipt_identity(
        "mutation-conflict-actor",
        "mutation-conflict-scope",
        "mutation-conflict-request",
    );
    let digest = Sha256Digest(format!("sha256:{}", "9".repeat(64)));
    let commit = StateCommit::new(
        identity.clone(),
        digest.clone(),
        "mutation-conflict-primary",
        0,
        b"must-not-write-primary".to_vec(),
        vec![NewOutboxEvent::internal(
            "mutation-conflict-event",
            "control-plane.state.changed",
            b"must-not-write-event".to_vec(),
        )],
    )
    .with_state_mutation(
        StateMutation::new(
            "mutation-missing-secondary",
            0,
            b"must-not-write-secondary".to_vec(),
        )
        .expect("valid missing secondary"),
    )
    .with_state_mutation(
        StateMutation::new(
            "mutation-stale-secondary",
            0,
            b"must-not-overwrite-secondary".to_vec(),
        )
        .expect("valid stale secondary"),
    )
    .with_journal_publication(journal_create(
        "mutation-conflict-record",
        b"must-not-write-journal",
    ))
    .with_pending_audit_event(
        PendingAuditEvent::new("mutation-conflict-audit", b"must-not-write-audit".to_vec())
            .expect("pending audit event"),
    );

    let error = storage
        .commit(&commit)
        .expect_err("a stale secondary mutation must reject the transaction");
    assert_eq!(error.kind(), StorageErrorKind::RevisionConflict);
    assert!(!error.is_state_guard_conflict());
    assert_eq!(
        atomic_member_counts(&database_path),
        before_counts,
        "all revisions must be checked before the first write"
    );
    assert!(
        storage
            .load_state("mutation-conflict-primary")
            .expect("primary state read")
            .is_none()
    );
    assert!(
        storage
            .load_state("mutation-missing-secondary")
            .expect("secondary state read")
            .is_none()
    );
    assert_eq!(
        storage
            .load_state("mutation-stale-secondary")
            .expect("stale state read")
            .expect("seed state")
            .payload,
        b"secondary-v1"
    );
    assert!(
        storage
            .load_receipt(&identity, &digest)
            .expect("receipt read")
            .is_none()
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn corrupt_secondary_revision_is_detected_before_any_state_upsert() {
    let root = temporary_directory("state-mutation-corrupt-revision");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&state_commit(
            (
                "mutation-corrupt-seed",
                "mutation-corrupt",
                "mutation-corrupt-seed-request",
            ),
            &format!("sha256:{}", "0".repeat(64)),
            "mutation-corrupt-secondary",
            0,
            b"secondary-v1",
            "mutation-corrupt-seed-event",
        ))
        .expect("secondary seed should commit");
    let database_path = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database_path).expect("corruption fixture database");
    connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON; \
             UPDATE product_state SET revision = -1 \
             WHERE stream_id = 'mutation-corrupt-secondary';",
        )
        .expect("corrupt revision fixture should install");
    connection.close().expect("corruption fixture should close");
    let before_counts = state_receipt_outbox_counts(&database_path);

    let commit = state_commit(
        (
            "mutation-corrupt",
            "mutation-corrupt",
            "mutation-corrupt-request",
        ),
        &format!("sha256:{}", "1".repeat(64)),
        "mutation-corrupt-primary",
        0,
        b"must-not-write-primary",
        "mutation-corrupt-event",
    )
    .with_state_mutation(
        StateMutation::new(
            "mutation-corrupt-missing",
            0,
            b"must-not-write-first-secondary".to_vec(),
        )
        .expect("first mutation"),
    )
    .with_state_mutation(
        StateMutation::new(
            "mutation-corrupt-secondary",
            1,
            b"must-not-write-corrupt-secondary".to_vec(),
        )
        .expect("corrupt mutation"),
    );
    assert_eq!(
        storage
            .commit(&commit)
            .expect_err("a corrupt stored revision must fail closed")
            .kind(),
        StorageErrorKind::Adapter
    );
    assert_eq!(state_receipt_outbox_counts(&database_path), before_counts);
    assert!(
        storage
            .load_state("mutation-corrupt-primary")
            .expect("primary read")
            .is_none()
    );
    assert!(
        storage
            .load_state("mutation-corrupt-missing")
            .expect("secondary read")
            .is_none()
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn state_mutation_rejects_invalid_duplicate_and_reserved_stream_inputs() {
    assert_eq!(
        StateMutation::new("", 0, Vec::new())
            .expect_err("an empty mutation stream must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    assert_eq!(
        StateMutation::new("mutation-range", i64::MAX as u64, Vec::new())
            .expect_err("an out-of-range mutation revision must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let root = temporary_directory("state-mutation-validation");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let base = || {
        state_commit(
            (
                "mutation-validation",
                "mutation-validation",
                "mutation-validation-request",
            ),
            &format!("sha256:{}", "a".repeat(64)),
            "mutation-primary-reserved",
            0,
            b"must-not-write",
            "mutation-validation-event",
        )
    };
    let mutation = |stream: String, payload: Vec<u8>| {
        StateMutation::new(stream, 0, payload).expect("valid mutation")
    };

    let duplicate = base()
        .with_state_mutation(mutation("mutation-duplicate".into(), b"one".to_vec()))
        .with_state_mutation(mutation("mutation-duplicate".into(), b"two".to_vec()));
    assert_eq!(
        storage
            .commit(&duplicate)
            .expect_err("duplicate mutation streams must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let primary_collision = base().with_state_mutation(mutation(
        "mutation-primary-reserved".into(),
        b"collision".to_vec(),
    ));
    assert_eq!(
        storage
            .commit(&primary_collision)
            .expect_err("the primary stream is reserved for its primary state")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let guard_collision = base()
        .with_state_guard(StateRevisionGuard::new("mutation-guarded", 0).expect("guard"))
        .with_state_mutation(mutation("mutation-guarded".into(), b"collision".to_vec()));
    assert_eq!(
        storage
            .commit(&guard_collision)
            .expect_err("a guarded stream cannot also be mutated")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let database_path = root.join("control-plane.sqlite3");
    for table in [
        "product_state",
        "command_receipts",
        "outbox",
        "audit_outbox",
    ] {
        assert_eq!(table_count(&database_path, table), 0);
    }
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn state_mutation_enforces_count_single_payload_and_total_payload_bounds() {
    let root = temporary_directory("state-mutation-bounds");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let base = || {
        state_commit(
            (
                "mutation-bounds",
                "mutation-bounds",
                "mutation-bounds-request",
            ),
            &format!("sha256:{}", "a".repeat(64)),
            "mutation-bounds-primary",
            0,
            b"must-not-write",
            "mutation-bounds-event",
        )
    };
    let mutation = |stream: String, payload: Vec<u8>| {
        StateMutation::new(stream, 0, payload).expect("valid mutation")
    };

    let mut too_many = base();
    for index in 0..=MAX_STATE_MUTATIONS_PER_COMMIT {
        too_many = too_many.with_state_mutation(mutation(
            format!("mutation-count-{index}"),
            vec![u8::try_from(index).expect("bounded mutation index")],
        ));
    }
    assert_eq!(
        storage
            .commit(&too_many)
            .expect_err("too many mutations must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let oversized_single = base().with_state_mutation(mutation(
        "mutation-oversized-single".into(),
        vec![0; MAX_STATE_MUTATION_PAYLOAD_BYTES + 1],
    ));
    assert_eq!(
        storage
            .commit(&oversized_single)
            .expect_err("one oversized mutation must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let oversized_total = base()
        .with_state_mutation(mutation(
            "mutation-total-one".into(),
            vec![0; MAX_STATE_MUTATION_BYTES_PER_COMMIT / 2 + 1],
        ))
        .with_state_mutation(mutation(
            "mutation-total-two".into(),
            vec![0; MAX_STATE_MUTATION_BYTES_PER_COMMIT / 2],
        ));
    assert_eq!(
        storage
            .commit(&oversized_total)
            .expect_err("oversized combined mutations must be rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    let database_path = root.join("control-plane.sqlite3");
    assert_eq!(atomic_member_counts(&database_path), [0; 6]);
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn exact_receipt_replay_does_not_recheck_or_rewrite_secondary_state() {
    let root = temporary_directory("state-mutation-replay");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let identity = receipt_identity(
        "mutation-replay-actor",
        "mutation-replay-scope",
        "mutation-replay-request",
    );
    let digest = Sha256Digest(format!("sha256:{}", "b".repeat(64)));
    let original = StateCommit::new(
        identity.clone(),
        digest.clone(),
        "mutation-replay-primary",
        0,
        b"primary-v1".to_vec(),
        vec![NewOutboxEvent::internal(
            "mutation-replay-event",
            "control-plane.state.changed",
            b"event-v1".to_vec(),
        )],
    )
    .with_state_mutation(
        StateMutation::new("mutation-replay-secondary", 0, b"secondary-v1".to_vec())
            .expect("secondary mutation"),
    );
    let first = storage.commit(&original).expect("first commit");

    storage
        .commit(&state_commit(
            (
                "mutation-replay-advance",
                "mutation-replay-scope",
                "mutation-replay-advance-request",
            ),
            &format!("sha256:{}", "c".repeat(64)),
            "mutation-replay-secondary",
            1,
            b"secondary-v2",
            "mutation-replay-advance-event",
        ))
        .expect("secondary stream should advance independently");
    let database_path = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database_path).expect("failure injection database");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_replay_secondary_update \
             BEFORE UPDATE ON product_state \
             WHEN OLD.stream_id = 'mutation-replay-secondary' \
             BEGIN SELECT RAISE(ABORT, 'replay attempted secondary update'); END;",
        )
        .expect("secondary update trigger should install");
    connection.close().expect("failure injector should close");
    let before_counts = state_receipt_outbox_counts(&database_path);

    let replay = storage
        .commit(&original)
        .expect("exact receipt replay must precede mutation CAS checks");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.events, first.events);
    assert_eq!(
        storage
            .load_state("mutation-replay-secondary")
            .expect("secondary state read")
            .expect("advanced secondary")
            .payload,
        b"secondary-v2"
    );
    assert_eq!(
        state_receipt_outbox_counts(&database_path),
        before_counts,
        "exact replay must not perform secondary writes"
    );

    let changed = StateCommit::new(
        identity,
        Sha256Digest(format!("sha256:{}", "d".repeat(64))),
        "mutation-replay-primary",
        1,
        b"changed".to_vec(),
        vec![NewOutboxEvent::internal(
            "mutation-replay-changed-event",
            "control-plane.state.changed",
            b"changed".to_vec(),
        )],
    )
    .with_state_mutation(
        StateMutation::new("mutation-replay-secondary", 2, b"changed".to_vec())
            .expect("changed mutation"),
    );
    assert_eq!(
        storage
            .commit(&changed)
            .expect_err("changed digest must conflict before state writes")
            .kind(),
        StorageErrorKind::RequestConflict
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn default_single_state_adapter_rejects_secondary_mutations_before_delegation() {
    struct SingleStateOnly {
        delegated: bool,
    }

    impl ProductStateStorage for SingleStateOnly {
        fn commit_adapter(&mut self, _commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
            self.delegated = true;
            Err(StorageError::adapter("single-state test adapter"))
        }

        fn load_receipt(
            &self,
            _identity: &ReceiptIdentity,
            _command_digest: &Sha256Digest,
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
            Err(StorageError::adapter("single-state test adapter"))
        }

        fn load_journal(
            &self,
            _key: &AggregateJournalKey,
        ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
            Ok(None)
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

    let mut storage = SingleStateOnly { delegated: false };
    let commit = state_commit(
        (
            "single-state-adapter",
            "single-state-adapter",
            "single-state-adapter-request",
        ),
        &format!("sha256:{}", "e".repeat(64)),
        "single-state-primary",
        0,
        b"primary",
        "single-state-event",
    )
    .with_state_mutation(
        StateMutation::new("single-state-secondary", 0, b"secondary".to_vec())
            .expect("secondary mutation"),
    );

    assert_eq!(
        storage
            .commit(&commit)
            .expect_err("default adapter must fail closed")
            .kind(),
        StorageErrorKind::Adapter
    );
    assert!(!storage.delegated);
}

fn install_failing_insert_trigger(database_path: &Path, table: &str) {
    let connection = Connection::open(database_path).expect("failure injection database");
    let trigger = match table {
        "product_state" => {
            "CREATE TRIGGER fail_product_state_insert BEFORE INSERT ON product_state \
             BEGIN SELECT RAISE(ABORT, 'injected product_state failure'); END;"
        }
        "aggregate_journal_records" => {
            "CREATE TRIGGER fail_aggregate_journal_records_insert \
             BEFORE INSERT ON aggregate_journal_records \
             BEGIN SELECT RAISE(ABORT, 'injected aggregate_journal_records failure'); END;"
        }
        "command_receipts" => {
            "CREATE TRIGGER fail_command_receipts_insert BEFORE INSERT ON command_receipts \
             BEGIN SELECT RAISE(ABORT, 'injected command_receipts failure'); END;"
        }
        "outbox" => {
            "CREATE TRIGGER fail_outbox_insert BEFORE INSERT ON outbox \
             BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;"
        }
        _ => panic!("unsupported failure injection table"),
    };
    connection
        .execute_batch(trigger)
        .expect("failure trigger should install");
    connection.close().expect("failure injector should close");
}

fn table_count(database_path: &Path, table: &str) -> i64 {
    let connection = Connection::open(database_path).expect("inspection database");
    let query = match table {
        "product_state" => "SELECT COUNT(*) FROM product_state",
        "aggregate_journals" => "SELECT COUNT(*) FROM aggregate_journals",
        "aggregate_journal_records" => "SELECT COUNT(*) FROM aggregate_journal_records",
        "command_receipts" => "SELECT COUNT(*) FROM command_receipts",
        "outbox" => "SELECT COUNT(*) FROM outbox",
        "audit_outbox" => "SELECT COUNT(*) FROM audit_outbox",
        "projection_event_stream_heads" => "SELECT COUNT(*) FROM projection_event_stream_heads",
        _ => panic!("unsupported inspection table"),
    };
    let count = connection
        .query_row(query, [], |row| row.get(0))
        .expect("table count");
    connection
        .close()
        .expect("inspection connection should close");
    count
}

fn atomic_member_counts(database_path: &Path) -> [i64; 6] {
    [
        table_count(database_path, "product_state"),
        table_count(database_path, "aggregate_journals"),
        table_count(database_path, "aggregate_journal_records"),
        table_count(database_path, "command_receipts"),
        table_count(database_path, "outbox"),
        table_count(database_path, "audit_outbox"),
    ]
}

fn state_receipt_outbox_counts(database_path: &Path) -> [i64; 3] {
    [
        table_count(database_path, "product_state"),
        table_count(database_path, "command_receipts"),
        table_count(database_path, "outbox"),
    ]
}

#[test]
fn failure_at_each_atomic_member_rolls_back_every_member() {
    for failing_table in [
        "product_state",
        "aggregate_journal_records",
        "command_receipts",
        "outbox",
    ] {
        let root = temporary_directory(&format!("rollback-{failing_table}"));
        let storage = SqliteStorage::open(&root).expect("schema should be created");
        let database_path = storage.database_path().to_path_buf();
        Box::new(storage)
            .close()
            .expect("bootstrap storage should close");
        install_failing_insert_trigger(&database_path, failing_table);

        let mut storage = SqliteStorage::open(&root).expect("storage with trigger should open");
        let commit = state_commit(
            (
                "user:rollback",
                "repository:rollback",
                "req_01J00000000000000000000004",
            ),
            &format!("sha256:{}", "6".repeat(64)),
            "delivery:rollback",
            0,
            b"must-not-commit",
            "must-not-commit-event",
        )
        .with_journal_publication(journal_create(
            "must-not-commit-record",
            b"must-not-commit-record",
        ));

        let error = storage
            .commit(&commit)
            .expect_err("injected failure must abort the transaction");
        assert_eq!(error.kind(), StorageErrorKind::Adapter);
        Box::new(storage)
            .close()
            .expect("failed storage should close");

        for table in [
            "product_state",
            "aggregate_journals",
            "aggregate_journal_records",
            "command_receipts",
            "outbox",
        ] {
            assert_eq!(
                table_count(&database_path, table),
                0,
                "{failing_table} failure left a partial row in {table}"
            );
        }
        fs::remove_dir_all(root).expect("database directory should be released");
    }
}

#[test]
fn failure_on_a_later_secondary_state_rolls_back_primary_and_every_member() {
    let root = temporary_directory("rollback-secondary-state");
    let storage = SqliteStorage::open(&root).expect("schema should be created");
    let database_path = storage.database_path().to_path_buf();
    Box::new(storage)
        .close()
        .expect("bootstrap storage should close");
    let connection = Connection::open(&database_path).expect("failure injection database");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_second_state_mutation \
             BEFORE INSERT ON product_state \
             WHEN NEW.stream_id = 'rollback-secondary-two' \
             BEGIN SELECT RAISE(ABORT, 'injected secondary state failure'); END;",
        )
        .expect("secondary failure trigger should install");
    connection.close().expect("failure injector should close");

    let mut storage = SqliteStorage::open(&root).expect("storage with trigger should open");
    let commit = state_commit(
        (
            "mutation-rollback",
            "mutation-rollback",
            "mutation-rollback-request",
        ),
        &format!("sha256:{}", "f".repeat(64)),
        "rollback-primary",
        0,
        b"must-not-commit-primary",
        "rollback-mutation-event",
    )
    .with_state_mutation(
        StateMutation::new(
            "rollback-secondary-one",
            0,
            b"must-not-commit-secondary-one".to_vec(),
        )
        .expect("first mutation"),
    )
    .with_state_mutation(
        StateMutation::new(
            "rollback-secondary-two",
            0,
            b"must-not-commit-secondary-two".to_vec(),
        )
        .expect("second mutation"),
    )
    .with_journal_publication(journal_create(
        "rollback-mutation-record",
        b"must-not-commit-journal",
    ))
    .with_pending_audit_event(
        PendingAuditEvent::new("rollback-mutation-audit", b"must-not-commit-audit".to_vec())
            .expect("pending audit event"),
    );

    let error = storage
        .commit(&commit)
        .expect_err("later secondary failure must abort the transaction");
    assert_eq!(error.kind(), StorageErrorKind::Adapter);
    Box::new(storage)
        .close()
        .expect("failed storage should close");
    for table in [
        "product_state",
        "aggregate_journals",
        "aggregate_journal_records",
        "command_receipts",
        "outbox",
        "audit_outbox",
    ] {
        assert_eq!(
            table_count(&database_path, table),
            0,
            "secondary mutation failure left a partial row in {table}"
        );
    }
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn later_outbox_failure_rolls_back_the_first_projection_cursor() {
    let root = temporary_directory("projection-cursor-rollback");
    let storage = SqliteStorage::open(&root).expect("schema should be created");
    let database_path = storage.database_path().to_path_buf();
    Box::new(storage)
        .close()
        .expect("bootstrap storage should close");

    let connection = Connection::open(&database_path).expect("failure injection database");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_second_projection_event BEFORE INSERT ON outbox \
             WHEN NEW.event_id = 'evt_projection_rollback_0002' \
             BEGIN SELECT RAISE(ABORT, 'injected second event failure'); END;",
        )
        .expect("failure trigger should install");
    connection.close().expect("failure injector should close");

    let stream = delivery_projection_stream("dlv_7Q71DWFHAZN7S6NPCS27WTTX4V");
    let mut storage = SqliteStorage::open(&root).expect("storage with trigger should open");
    let commit = StateCommit::new(
        projection_receipt_identity("req_projection_rollback_0001"),
        Sha256Digest(format!("sha256:{}", "e".repeat(64))),
        "state:projection-rollback",
        0,
        b"must-not-commit".to_vec(),
        vec![
            public_projection_event("evt_projection_rollback_0001", stream.clone()),
            public_projection_event("evt_projection_rollback_0002", stream.clone()),
        ],
    );
    let error = storage
        .commit(&commit)
        .expect_err("the second outbox insert must abort the whole transaction");
    assert_eq!(error.kind(), StorageErrorKind::Adapter);
    let cursor = storage
        .load_projection_event_cursor(&projection_key(stream), None)
        .expect("rolled-back stream remains empty");
    assert_eq!(cursor.sequence(), 0);
    assert_eq!(cursor.event_id(), None);
    Box::new(storage).close().expect("storage should close");

    for table in [
        "product_state",
        "command_receipts",
        "outbox",
        "projection_event_stream_heads",
    ] {
        assert_eq!(
            table_count(&database_path, table),
            0,
            "late event failure left a partial row in {table}"
        );
    }
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn journal_tail_compare_and_append_allows_only_one_concurrent_winner() {
    let root = temporary_directory("journal-tail-cas");
    let mut bootstrap = SqliteStorage::open(&root).expect("SQLite storage should open");
    bootstrap
        .commit(
            &state_commit(
                (
                    "user:bootstrap",
                    "repository:bootstrap",
                    "req_01J00000000000000000000005",
                ),
                &format!("sha256:{}", "7".repeat(64)),
                "bootstrap",
                0,
                b"bootstrap",
                "bootstrap-event",
            )
            .with_journal_publication(journal_create("record-one", b"record-one")),
        )
        .expect("first journal record");
    Box::new(bootstrap).close().expect("bootstrap should close");

    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for ordinal in 0..2 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            let mut storage = SqliteStorage::open(&root).expect("contender storage should open");
            let stream_id = format!("contender-{ordinal}");
            let state = format!("state-{ordinal}");
            let event_id = format!("event-{ordinal}");
            let record_digest = format!("record-{ordinal}");
            let record_payload = format!("record-{ordinal}");
            let digest_nibble = if ordinal == 0 { "8" } else { "9" };
            let commit = state_commit(
                (
                    if ordinal == 0 { "user:one" } else { "user:two" },
                    "repository:race",
                    if ordinal == 0 {
                        "req_01J00000000000000000000006"
                    } else {
                        "req_01J00000000000000000000007"
                    },
                ),
                &format!("sha256:{}", digest_nibble.repeat(64)),
                &stream_id,
                0,
                state.as_bytes(),
                &event_id,
            )
            .with_journal_publication(journal_append(
                "record-one",
                &record_digest,
                record_payload.as_bytes(),
            ));
            barrier.wait();
            let result = storage.commit(&commit);
            Box::new(storage).close().expect("contender should close");
            result
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("contender thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one contender should lose");
    assert_eq!(error.kind(), StorageErrorKind::JournalConflict);

    let storage = SqliteStorage::open(&root).expect("inspection storage should open");
    let journal = storage
        .load_journal(&journal_key())
        .expect("journal read")
        .expect("journal");
    assert_eq!(journal.records.len(), 2);
    assert_eq!(storage.pending_events().expect("pending events").len(), 2);
    Box::new(storage)
        .close()
        .expect("inspection storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn scoped_request_replay_returns_the_original_event_without_duplicate_rows() {
    let root = temporary_directory("journal-request-replay");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let identity = (
        "user:one",
        "repository:one",
        "req_01J00000000000000000000008",
    );
    let digest = format!("sha256:{}", "a".repeat(64));
    storage
        .commit(
            &state_commit(
                identity,
                &digest,
                "delivery:original",
                0,
                b"original-state",
                "original-job-event",
            )
            .with_journal_publication(journal_create("record-one", b"original-record")),
        )
        .expect("original commit");

    let replay = storage
        .commit(
            &state_commit(
                identity,
                &digest,
                "delivery:retry-must-be-ignored",
                99,
                b"retry-state-must-be-ignored",
                "retry-event-must-be-ignored",
            )
            .with_journal_publication(journal_append(
                "record-one",
                "retry-record",
                b"retry-record-must-be-ignored",
            )),
        )
        .expect("same scoped command should replay");

    assert!(replay.idempotent_replay);
    assert_eq!(replay.stream_id, "delivery:original");
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.events.len(), 1);
    assert_eq!(replay.events[0].event_id, "original-job-event");
    assert_eq!(replay.events[0].payload, b"event");
    assert_eq!(storage.pending_events().expect("pending events").len(), 1);
    let journal = storage
        .load_journal(&journal_key())
        .expect("journal read")
        .expect("journal");
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].payload, b"original-record");
    assert!(
        storage
            .load_state("delivery:retry-must-be-ignored")
            .expect("retry state query")
            .is_none()
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn replay_only_commit_refuses_to_fill_a_journal_without_its_original_receipt() {
    let root = temporary_directory("partial-journal-replay");
    let storage = SqliteStorage::open(&root).expect("schema should be created");
    let database_path = storage.database_path().to_path_buf();
    Box::new(storage).close().expect("bootstrap storage close");
    let connection = Connection::open(&database_path).expect("partial journal fixture");
    connection
        .execute(
            "INSERT INTO aggregate_journals (aggregate_type, aggregate_id, manifest) \
             VALUES ('delivery', 'dlv_01J00000000000000000000000', X'6D616E6966657374')",
            [],
        )
        .expect("partial journal manifest");
    connection
        .execute(
            "INSERT INTO aggregate_journal_records \
             (aggregate_type, aggregate_id, sequence, digest, payload) \
             VALUES (
                 'delivery', 'dlv_01J00000000000000000000000', 1,
                 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                 X'7265636F7264'
             )",
            [],
        )
        .expect("partial journal record");
    connection.close().expect("fixture connection close");

    let mut storage = SqliteStorage::open(&root).expect("partial storage should open");
    let commit = state_commit(
        (
            "user:partial",
            "repository:partial",
            "req_01J00000000000000000000010",
        ),
        &format!("sha256:{}", "c".repeat(64)),
        "delivery:partial",
        0,
        b"recomputed-state-must-not-commit",
        "recomputed-event-must-not-commit",
    )
    .require_receipt_replay();

    let error = storage
        .commit(&commit)
        .expect_err("missing original receipt must fail closed");
    assert_eq!(error.kind(), StorageErrorKind::RequestReplayMissing);
    assert!(
        storage
            .load_state("delivery:partial")
            .expect("partial state read")
            .is_none()
    );
    assert!(
        storage
            .pending_events()
            .expect("partial outbox read")
            .is_empty()
    );
    let journal = storage
        .load_journal(&journal_key())
        .expect("partial journal read")
        .expect("partial journal remains for diagnosis");
    assert_eq!(journal.records.len(), 1);

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn revision_conflict_constructor_preserves_expected_and_actual_revisions() {
    let error = StorageError::revision_conflict(7, 9);

    assert_eq!(error.kind(), StorageErrorKind::RevisionConflict);
    assert!(!error.is_state_guard_conflict());
    assert_eq!(
        error.to_string(),
        "expected revision 7, but current revision is 9"
    );
}

#[test]
fn revision_token_conflict_accepts_only_the_review_set_stale_token() {
    let stale = StorageError::revision_token_conflict("reviewSetSha256");
    assert_eq!(stale.kind(), StorageErrorKind::RevisionConflict);
    assert_eq!(
        stale.to_string(),
        "reviewSetSha256 no longer identifies the current solution review"
    );

    let unsupported = StorageError::revision_token_conflict("candidateDigest");
    assert_eq!(unsupported.kind(), StorageErrorKind::InvalidInput);
    assert_eq!(
        unsupported.to_string(),
        "revision token conflict field is unsupported"
    );
}
