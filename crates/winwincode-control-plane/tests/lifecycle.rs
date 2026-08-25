use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, OrganizationScope, ProjectScope, RepositoryScope,
    SchemaVersion, Scope, ServiceAccountActor, UserActor, WorkspaceScope,
};
use winwincode_control_plane::{
    AggregateJournalKey, CommitError, CommitReceipt, ControlPlane, ControlPlaneConfig,
    EventPublishError, EventPublisher, LoadedAggregateJournal, NewOutboxEvent, OutboxEvent,
    ProductStateStorage, ShutdownError, ShutdownReport, StartError, StateChange, StorageError,
    StorageErrorKind, StoredState,
};
use winwincode_domain::{
    OrganizationId, ProjectId, RepositoryId, RequestId, ServiceAccountId, UserId, WorkspaceId,
};
use winwincode_storage::{ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, StateCommit};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct RecordingPublisher {
    trace: Arc<Mutex<Vec<String>>>,
    sent_events: Arc<Mutex<Vec<OutboxEvent>>>,
    fail_publish: bool,
    closed: Arc<AtomicBool>,
}

impl RecordingPublisher {
    fn successful() -> (Self, Arc<Mutex<Vec<OutboxEvent>>>) {
        let publisher = Self::default();
        let sent_events = Arc::clone(&publisher.sent_events);
        (publisher, sent_events)
    }

    fn failing() -> (Self, Arc<AtomicBool>) {
        let publisher = Self {
            fail_publish: true,
            ..Self::default()
        };
        let closed = Arc::clone(&publisher.closed);
        (publisher, closed)
    }
}

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("publisher.publish:{}", event.event_id));
        if self.fail_publish {
            return Err(EventPublishError::new("publisher is unavailable"));
        }
        self.sent_events
            .lock()
            .expect("published event lock")
            .push(event.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<(), EventPublishError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("publisher.close".to_owned());
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

struct TraceStorage {
    trace: Arc<Mutex<Vec<String>>>,
    state: Mutex<Option<StoredState>>,
    committed_events: Mutex<Vec<OutboxEvent>>,
    pending_schedule: Mutex<VecDeque<Vec<OutboxEvent>>>,
    owned_directory: Option<PathBuf>,
    closed: Arc<AtomicBool>,
}

impl TraceStorage {
    fn new(
        trace: Arc<Mutex<Vec<String>>>,
        pending_schedule: Vec<Vec<OutboxEvent>>,
        owned_directory: Option<PathBuf>,
    ) -> (Self, Arc<AtomicBool>) {
        let closed = Arc::new(AtomicBool::new(false));
        (
            Self {
                trace,
                state: Mutex::new(None),
                committed_events: Mutex::new(Vec::new()),
                pending_schedule: Mutex::new(pending_schedule.into()),
                owned_directory,
                closed: Arc::clone(&closed),
            },
            closed,
        )
    }
}

impl ProductStateStorage for TraceStorage {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("storage.commit".to_owned());
        let revision = commit.expected_revision + 1;
        *self.state.lock().expect("state lock") = Some(StoredState {
            stream_id: commit.stream_id.clone(),
            revision,
            payload: commit.state.clone(),
        });
        *self.committed_events.lock().expect("event lock") = commit
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| OutboxEvent {
                sequence: u64::try_from(index + 1).expect("test event sequence"),
                event_id: event.event_id.clone(),
                topic: event.topic.clone(),
                payload: event.payload.clone(),
                projection_cursor: None,
            })
            .collect();
        Ok(CommitReceipt {
            receipt_identity: commit.receipt_identity.clone(),
            stream_id: commit.stream_id.clone(),
            revision,
            events: commit
                .events
                .iter()
                .enumerate()
                .map(|(index, event)| OutboxEvent {
                    sequence: u64::try_from(index + 1).expect("test event sequence"),
                    event_id: event.event_id.clone(),
                    topic: event.topic.clone(),
                    payload: event.payload.clone(),
                    projection_cursor: None,
                })
                .collect(),
            idempotent_replay: false,
        })
    }

    fn load_receipt(
        &self,
        _identity: &ReceiptIdentity,
        _command_digest: &winwincode_domain::Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        Ok(None)
    }

    fn load_state(&self, _stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        Ok(self.state.lock().expect("state lock").clone())
    }

    fn load_journal(
        &self,
        _key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        Ok(None)
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("storage.pending".to_owned());
        if let Some(events) = self
            .pending_schedule
            .lock()
            .expect("pending schedule lock")
            .pop_front()
        {
            return Ok(events);
        }
        Ok(self.committed_events.lock().expect("event lock").clone())
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("storage.mark:{event_id}"));
        self.committed_events
            .lock()
            .expect("event lock")
            .retain(|event| event.event_id != event_id);
        Ok(())
    }

    fn close(self: Box<Self>) -> Result<(), StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("storage.close".to_owned());
        self.closed.store(true, Ordering::Release);
        if let Some(directory) = &self.owned_directory {
            fs::remove_dir_all(directory).map_err(|error| {
                StorageError::adapter(format!("failed to release test directory: {error}"))
            })?;
        }
        Ok(())
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn event(event_id: &str) -> NewOutboxEvent {
    NewOutboxEvent::internal(event_id, "control-plane.state.changed", b"event".to_vec())
}

fn committed_event(sequence: u64, event_id: &str) -> OutboxEvent {
    OutboxEvent {
        sequence,
        event_id: event_id.to_owned(),
        topic: "control-plane.state.changed".to_owned(),
        payload: b"event".to_vec(),
        projection_cursor: None,
    }
}

fn command(
    request_id: &str,
    expected_revision: i64,
    actor: Actor,
    scope: Scope,
    payload: serde_json::Value,
) -> CommandEnvelope {
    CommandEnvelope {
        actor,
        command: CommandName::SettingsUpdate,
        expected_revision: winwincode_domain::Revision(expected_revision),
        payload,
        request_id: RequestId(request_id.to_owned()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn user_actor(value: &str) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(value.to_owned()),
        kind: winwincode_api::generated::UserActorKind::User,
    })
}

fn service_actor(value: &str) -> Actor {
    Actor::ServiceAccountActor(ServiceAccountActor {
        id: ServiceAccountId(value.to_owned()),
        kind: winwincode_api::generated::ServiceAccountActorKind::ServiceAccount,
    })
}

fn repository_scope(
    organization_id: &str,
    workspace_id: &str,
    project_id: &str,
    repository_id: &str,
) -> Scope {
    Scope::RepositoryScope(RepositoryScope {
        kind: winwincode_api::generated::RepositoryScopeKind::Repository,
        organization_id: OrganizationId(organization_id.to_owned()),
        workspace_id: WorkspaceId(workspace_id.to_owned()),
        project_id: ProjectId(project_id.to_owned()),
        repository_id: RepositoryId(repository_id.to_owned()),
    })
}

fn organization_scope(organization_id: &str) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: winwincode_api::generated::OrganizationScopeKind::Organization,
        organization_id: OrganizationId(organization_id.to_owned()),
    })
}

fn workspace_scope(organization_id: &str, workspace_id: &str) -> Scope {
    Scope::WorkspaceScope(WorkspaceScope {
        kind: winwincode_api::generated::WorkspaceScopeKind::Workspace,
        organization_id: OrganizationId(organization_id.to_owned()),
        workspace_id: WorkspaceId(workspace_id.to_owned()),
    })
}

fn project_scope(organization_id: &str, workspace_id: &str, project_id: &str) -> Scope {
    Scope::ProjectScope(ProjectScope {
        kind: winwincode_api::generated::ProjectScopeKind::Project,
        organization_id: OrganizationId(organization_id.to_owned()),
        workspace_id: WorkspaceId(workspace_id.to_owned()),
        project_id: ProjectId(project_id.to_owned()),
    })
}

fn default_command(request_id: &str, expected_revision: i64) -> CommandEnvelope {
    command(
        request_id,
        expected_revision,
        user_actor("usr_01J00000000000000000000000"),
        repository_scope(
            "org_01J00000000000000000000000",
            "wsp_01J00000000000000000000000",
            "prj_01J00000000000000000000000",
            "rep_01J00000000000000000000000",
        ),
        serde_json::json!({"operation": "advance"}),
    )
}

fn request_id_with_ordinal(ordinal: usize) -> String {
    format!("req_{ordinal:026}")
}

fn state_change(state: &[u8], event_id: &str) -> StateChange {
    state_change_for("product-session:one", state, event_id)
}

fn state_change_for(stream_id: &str, state: &[u8], event_id: &str) -> StateChange {
    StateChange::new(stream_id, state.to_vec(), vec![event(event_id)])
}

fn alternate_receipt_identities(
    base_actor: &Actor,
    base_scope: &Scope,
) -> Vec<(Actor, Scope, &'static str, &'static str)> {
    vec![
        (
            service_actor("svc_01J00000000000000000000000"),
            base_scope.clone(),
            "stream-actor",
            "event-actor",
        ),
        (
            base_actor.clone(),
            organization_scope("org_01J00000000000000000000000"),
            "stream-scope-kind",
            "event-scope-kind",
        ),
        (
            base_actor.clone(),
            repository_scope(
                "org_01J00000000000000000000001",
                "wsp_01J00000000000000000000000",
                "prj_01J00000000000000000000000",
                "rep_01J00000000000000000000000",
            ),
            "stream-organization",
            "event-organization",
        ),
        (
            base_actor.clone(),
            repository_scope(
                "org_01J00000000000000000000000",
                "wsp_01J00000000000000000000001",
                "prj_01J00000000000000000000000",
                "rep_01J00000000000000000000000",
            ),
            "stream-workspace",
            "event-workspace",
        ),
        (
            base_actor.clone(),
            repository_scope(
                "org_01J00000000000000000000000",
                "wsp_01J00000000000000000000000",
                "prj_01J00000000000000000000001",
                "rep_01J00000000000000000000000",
            ),
            "stream-project",
            "event-project",
        ),
        (
            base_actor.clone(),
            repository_scope(
                "org_01J00000000000000000000000",
                "wsp_01J00000000000000000000000",
                "prj_01J00000000000000000000000",
                "rep_01J00000000000000000000001",
            ),
            "stream-repository",
            "event-repository",
        ),
    ]
}

fn assert_receipt_excludes_sensitive_command_values(root: &std::path::Path) {
    let connection = Connection::open(root.join("control-plane.sqlite3"))
        .expect("receipt database should open for inspection");
    let persisted_receipt_text: String = connection
        .query_row(
            "SELECT CAST(actor_key AS TEXT) || CAST(scope_key AS TEXT) || request_id || \
                    command_digest || stream_id || CAST(revision AS TEXT) \
             FROM command_receipts WHERE stream_id = 'stream-base'",
            [],
            |row| row.get(0),
        )
        .expect("the durable receipt should be readable");
    assert!(!persisted_receipt_text.contains("proof-must-not-be-persisted"));
    assert!(!persisted_receipt_text.contains("credential-must-not-be-persisted"));
    connection
        .close()
        .expect("inspection connection should close");
}

#[test]
fn startup_migrates_storage_before_the_control_plane_accepts_commits() {
    let root = temporary_directory("startup");
    let config = ControlPlaneConfig::local(&root);
    let (publisher, _) = RecordingPublisher::successful();

    let control_plane = ControlPlane::start_local(config, Box::new(publisher))
        .expect("the local Control Plane should start after applying SQLite migrations");

    assert!(root.join("control-plane.sqlite3").is_file());
    let temporary_root = control_plane.temporary_root().to_path_buf();
    assert!(
        temporary_root
            .join(".winwincode-control-plane-owner")
            .is_file(),
        "the running instance must mark its owned temporary root"
    );

    control_plane
        .shutdown()
        .expect("the local Control Plane should stop cleanly");
    assert!(!temporary_root.exists());
    fs::remove_dir_all(root).expect("shutdown should release the temporary database directory");
}

#[test]
fn commit_persists_state_and_outbox_before_publishing_the_event() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (storage, _) = TraceStorage::new(Arc::clone(&trace), vec![vec![]], None);
    let publisher = RecordingPublisher {
        trace: Arc::clone(&trace),
        ..RecordingPublisher::default()
    };
    let mut control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("the Control Plane should start");
    trace.lock().expect("trace lock").clear();

    let receipt = control_plane
        .commit(
            &default_command("req_01J00000000000000000000010", 0),
            state_change(b"state-v1", "event-1"),
        )
        .expect("state and event should commit");

    assert_eq!(receipt.revision, 1);
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.commit",
            "storage.pending",
            "publisher.publish:event-1",
            "storage.mark:event-1",
        ]
    );
    control_plane.shutdown().expect("shutdown should succeed");
}

#[test]
fn command_receipts_use_canonical_actor_full_scope_request_and_payload_digest() {
    let root = temporary_directory("canonical-receipt-identity");
    let (publisher, _) = RecordingPublisher::successful();
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("the local Control Plane should start");
    let request_id = "req_01J00000000000000000000020";
    let base_actor = user_actor("usr_01J00000000000000000000000");
    let base_scope = repository_scope(
        "org_01J00000000000000000000000",
        "wsp_01J00000000000000000000000",
        "prj_01J00000000000000000000000",
        "rep_01J00000000000000000000000",
    );
    let base = command(
        request_id,
        0,
        base_actor.clone(),
        base_scope.clone(),
        serde_json::json!({
            "actorProof": "proof-must-not-be-persisted",
            "credential": "credential-must-not-be-persisted",
            "operation": "advance",
        }),
    );
    let first = control_plane
        .commit(
            &base,
            state_change_for("stream-base", b"base", "event-base"),
        )
        .expect("base command should commit");
    assert!(!first.idempotent_replay);

    for (actor, scope, stream_id, event_id) in
        alternate_receipt_identities(&base_actor, &base_scope)
    {
        let receipt = control_plane
            .commit(
                &command(
                    request_id,
                    0,
                    actor,
                    scope,
                    serde_json::json!({"operation": "advance"}),
                ),
                state_change_for(stream_id, stream_id.as_bytes(), event_id),
            )
            .expect("another actor or complete scope may reuse the request id");
        assert!(!receipt.idempotent_replay);
    }

    let replay = control_plane
        .commit(
            &base,
            state_change_for(
                "ignored-retry-stream",
                b"ignored retry state",
                "ignored-retry-event",
            ),
        )
        .expect("the same command digest should replay its durable receipt");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.stream_id, "stream-base");
    assert_eq!(replay.events[0].event_id, "event-base");

    let changed = command(
        request_id,
        0,
        base_actor,
        base_scope,
        serde_json::json!({"operation": "different"}),
    );
    let error = control_plane
        .commit(
            &changed,
            state_change_for("stream-changed", b"changed", "event-changed"),
        )
        .expect_err("the same receipt identity cannot represent another command digest");
    assert!(matches!(
        error,
        CommitError::Storage(ref source) if source.kind() == StorageErrorKind::RequestConflict
    ));

    control_plane.shutdown().expect("shutdown should succeed");
    assert_receipt_excludes_sensitive_command_values(&root);
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn command_digest_is_stable_when_json_object_keys_arrive_in_another_order() {
    let root = temporary_directory("canonical-command-digest");
    let (publisher, _) = RecordingPublisher::successful();
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("the local Control Plane should start");
    let request_id = "req_01J00000000000000000000021";
    let first: serde_json::Value = serde_json::from_str(r#"{"z":3,"nested":{"b":2,"a":1},"a":1}"#)
        .expect("first JSON payload");
    let reordered: serde_json::Value =
        serde_json::from_str(r#"{"a":1,"nested":{"a":1,"b":2},"z":3}"#)
            .expect("reordered JSON payload");
    let first_command = command(
        request_id,
        0,
        user_actor("usr_01J00000000000000000000000"),
        repository_scope(
            "org_01J00000000000000000000000",
            "wsp_01J00000000000000000000000",
            "prj_01J00000000000000000000000",
            "rep_01J00000000000000000000000",
        ),
        first,
    );
    let reordered_command = CommandEnvelope {
        payload: reordered,
        ..first_command.clone()
    };

    control_plane
        .commit(
            &first_command,
            state_change_for("stream-json", b"first", "event-json"),
        )
        .expect("first command should commit");
    let replay = control_plane
        .commit(
            &reordered_command,
            state_change_for("ignored", b"ignored", "ignored-event"),
        )
        .expect("JSON key order must not change the semantic command digest");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.events[0].event_id, "event-json");

    control_plane.shutdown().expect("shutdown should succeed");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn invalid_scope_ids_fail_before_the_storage_port_is_called() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (storage, _) = TraceStorage::new(Arc::clone(&trace), vec![vec![]], None);
    let publisher = RecordingPublisher {
        trace: Arc::clone(&trace),
        ..RecordingPublisher::default()
    };
    let mut control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("the Control Plane should start");
    trace.lock().expect("trace lock").clear();
    let valid_org = "org_01J00000000000000000000000";
    let valid_workspace = "wsp_01J00000000000000000000000";
    let valid_project = "prj_01J00000000000000000000000";
    let valid_repository = "rep_01J00000000000000000000000";
    let invalid_scopes = [
        organization_scope(""),
        organization_scope("organization-not-canonical"),
        workspace_scope("", valid_workspace),
        workspace_scope("organization-not-canonical", valid_workspace),
        workspace_scope(valid_org, ""),
        workspace_scope(valid_org, "workspace-not-canonical"),
        project_scope("", valid_workspace, valid_project),
        project_scope("organization-not-canonical", valid_workspace, valid_project),
        project_scope(valid_org, "", valid_project),
        project_scope(valid_org, "workspace-not-canonical", valid_project),
        project_scope(valid_org, valid_workspace, ""),
        project_scope(valid_org, valid_workspace, "project-not-canonical"),
        repository_scope("", valid_workspace, valid_project, valid_repository),
        repository_scope(
            "organization-not-canonical",
            valid_workspace,
            valid_project,
            valid_repository,
        ),
        repository_scope(valid_org, "", valid_project, valid_repository),
        repository_scope(
            valid_org,
            "workspace-not-canonical",
            valid_project,
            valid_repository,
        ),
        repository_scope(valid_org, valid_workspace, "", valid_repository),
        repository_scope(
            valid_org,
            valid_workspace,
            "project-not-canonical",
            valid_repository,
        ),
        repository_scope(valid_org, valid_workspace, valid_project, ""),
        repository_scope(
            valid_org,
            valid_workspace,
            valid_project,
            "repository-not-canonical",
        ),
    ];

    for (index, scope) in invalid_scopes.into_iter().enumerate() {
        let error = control_plane
            .commit(
                &command(
                    &request_id_with_ordinal(100 + index),
                    0,
                    user_actor("usr_01J00000000000000000000000"),
                    scope,
                    serde_json::json!({"operation": "advance"}),
                ),
                state_change_for(
                    &format!("invalid-scope-{index}"),
                    b"must not commit",
                    &format!("invalid-event-{index}"),
                ),
            )
            .expect_err("an invalid scope id must fail closed");
        assert!(matches!(
            error,
            CommitError::Storage(ref source) if source.kind() == StorageErrorKind::InvalidInput
        ));
    }
    assert!(
        trace.lock().expect("trace lock").is_empty(),
        "invalid scope values must not reach storage or publication"
    );
    control_plane.shutdown().expect("shutdown should succeed");
}

#[test]
fn failed_outbox_insert_rolls_back_the_state_write() {
    let root = temporary_directory("rollback");
    let (publisher, _) = RecordingPublisher::successful();
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("the local Control Plane should start");
    control_plane
        .commit(
            &default_command("req_01J00000000000000000000011", 0),
            state_change(b"state-v1", "duplicate-event"),
        )
        .expect("the first commit should succeed");

    let error = control_plane
        .commit(
            &default_command("req_01J00000000000000000000012", 1),
            state_change(b"state-v2", "duplicate-event"),
        )
        .expect_err("the duplicate outbox event should fail the transaction");
    assert!(matches!(error, CommitError::Storage(_)));
    let state = control_plane
        .load_state("product-session:one")
        .expect("state should remain readable")
        .expect("the first state should remain");
    assert_eq!((state.revision, state.payload), (1, b"state-v1".to_vec()));

    control_plane.shutdown().expect("shutdown should succeed");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn publish_failure_keeps_committed_state_and_pending_outbox_for_restart() {
    let root = temporary_directory("restart");
    let (failing_publisher, failing_closed) = RecordingPublisher::failing();
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(failing_publisher),
    )
    .expect("the first Control Plane should start");

    let error = control_plane
        .commit(
            &default_command("req_01J00000000000000000000013", 0),
            state_change(b"state-v1", "event-for-restart"),
        )
        .expect_err("publication should fail after the database commit");
    let receipt = error
        .committed_receipt()
        .expect("the error must identify the durable commit");
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        control_plane
            .load_state("product-session:one")
            .expect("committed state should remain readable")
            .expect("committed state should exist")
            .payload,
        b"state-v1"
    );
    control_plane
        .shutdown()
        .expect_err("shutdown should report that publication is still pending");
    assert!(failing_closed.load(Ordering::Acquire));

    let (successful_publisher, sent_events) = RecordingPublisher::successful();
    let restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(successful_publisher),
    )
    .expect("restart should replay the pending outbox event");
    assert_eq!(
        sent_events
            .lock()
            .expect("published event lock")
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        ["event-for-restart"]
    );
    restarted.shutdown().expect("restart should stop cleanly");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn restart_replays_committed_but_unpublished_outbox_events() {
    let root = temporary_directory("sequence");
    let mut storage =
        winwincode_storage::SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                ReceiptActorKey::from_encoded(b"test-actor".to_vec()).expect("actor key"),
                ReceiptScopeKey::from_encoded(b"test-scope".to_vec()).expect("scope key"),
                RequestId("req_01J00000000000000000000014".to_owned()),
            )
            .expect("receipt identity"),
            winwincode_domain::Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            "stream-1",
            0,
            b"one".to_vec(),
            vec![event("event-z"), event("event-a")],
        ))
        .expect("outbox events should commit in supplied order");
    Box::new(storage)
        .close()
        .expect("direct storage should close");

    let (publisher, sent_events) = RecordingPublisher::successful();
    let control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("startup should replay the durable outbox");
    assert_eq!(
        sent_events
            .lock()
            .expect("published event lock")
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        ["event-z", "event-a"]
    );
    control_plane.shutdown().expect("shutdown should succeed");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn shutdown_flushes_outbox_then_closes_publisher_and_storage() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let shutdown_event = committed_event(7, "shutdown-event");
    let (storage, _) =
        TraceStorage::new(Arc::clone(&trace), vec![vec![], vec![shutdown_event]], None);
    let publisher = RecordingPublisher {
        trace: Arc::clone(&trace),
        ..RecordingPublisher::default()
    };
    let control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("the Control Plane should start");
    trace.lock().expect("trace lock").clear();

    let report: ShutdownReport = control_plane.shutdown().expect("shutdown should succeed");

    assert_eq!(report.published_event_count, 1);
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.pending",
            "publisher.publish:shutdown-event",
            "storage.mark:shutdown-event",
            "publisher.close",
            "storage.close",
        ]
    );
}

#[test]
fn shutdown_releases_the_sqlite_connection_and_temporary_directory() {
    let root = temporary_directory("release");
    let (publisher, _) = RecordingPublisher::successful();
    let control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("the local Control Plane should start");
    let temporary_root = control_plane.temporary_root().to_path_buf();

    control_plane.shutdown().expect("shutdown should succeed");

    assert!(!temporary_root.exists());
    fs::remove_dir_all(&root).expect("all SQLite handles should be released after shutdown");
    assert!(!root.exists());
}

#[test]
fn startup_does_not_delete_a_preexisting_temporary_root_without_a_proven_stale_lease() {
    let root = temporary_directory("preexisting-temporary-root");
    let temporary_parent = root.join("runtime");
    let preexisting_root = temporary_parent.join("instance-stale-candidate");
    fs::create_dir_all(&preexisting_root).expect("preexisting root should exist");
    fs::write(
        preexisting_root.join(".winwincode-control-plane-owner"),
        b"winwincode-control-plane\npid=1\ninstance=old\n",
    )
    .expect("preexisting marker should exist");
    let (publisher, _) = RecordingPublisher::successful();

    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root.join("data")).with_temporary_parent(&temporary_parent),
        Box::new(publisher),
    )
    .expect("startup should create a separate owned root");
    let current_root = control_plane.temporary_root().to_path_buf();

    assert_ne!(current_root, preexisting_root);
    assert!(preexisting_root.exists());
    control_plane.shutdown().expect("shutdown should succeed");
    assert!(!current_root.exists());
    assert!(
        preexisting_root.exists(),
        "a PID or old-looking marker is not proof of a stale lease"
    );

    fs::remove_dir_all(root).expect("test should remove the deliberately retained root");
}

#[test]
fn failed_startup_closes_storage_and_releases_temporary_directory() {
    let root = temporary_directory("failed-startup");
    fs::create_dir_all(&root).expect("test directory should exist");
    let trace = Arc::new(Mutex::new(Vec::new()));
    let pending = committed_event(1, "startup-event");
    let (storage, storage_closed) =
        TraceStorage::new(Arc::clone(&trace), vec![vec![pending]], Some(root.clone()));
    let (mut publisher, publisher_closed) = RecordingPublisher::failing();
    publisher.trace = Arc::clone(&trace);

    let error: StartError = match ControlPlane::start(Box::new(storage), Box::new(publisher)) {
        Ok(control_plane) => {
            control_plane.shutdown().ok();
            panic!("startup should fail when durable outbox replay fails");
        }
        Err(error) => error,
    };

    assert!(error.to_string().contains("durable outbox"));
    assert!(publisher_closed.load(Ordering::Acquire));
    assert!(storage_closed.load(Ordering::Acquire));
    assert!(!root.exists());
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.pending",
            "publisher.publish:startup-event",
            "publisher.close",
            "storage.close",
        ]
    );
}

#[test]
fn local_storage_open_failure_closes_the_event_publisher() {
    let root = temporary_directory("blocked-data-directory");
    fs::create_dir_all(&root).expect("test root should exist");
    let blocked_data_directory = root.join("not-a-directory");
    fs::write(&blocked_data_directory, b"file").expect("blocking file should exist");
    let (publisher, publisher_closed) = RecordingPublisher::failing();
    let temporary_parent = root.join("runtime");

    let result = ControlPlane::start_local(
        ControlPlaneConfig::local(&blocked_data_directory).with_temporary_parent(&temporary_parent),
        Box::new(publisher),
    );

    assert!(result.is_err());
    assert!(
        publisher_closed.load(Ordering::Acquire),
        "startup must explicitly close the event publisher when storage cannot open"
    );
    assert_eq!(
        fs::read_dir(&temporary_parent)
            .expect("temporary parent should remain readable")
            .count(),
        0,
        "failed startup must release the instance-owned temporary root"
    );
    fs::remove_dir_all(root).expect("failed startup should leave no open file handles");
}

#[test]
fn shutdown_publish_failure_still_closes_storage_and_releases_temporary_directory() {
    let root = temporary_directory("failed-shutdown");
    fs::create_dir_all(&root).expect("test directory should exist");
    let trace = Arc::new(Mutex::new(Vec::new()));
    let pending = committed_event(1, "shutdown-event");
    let (storage, storage_closed) = TraceStorage::new(
        Arc::clone(&trace),
        vec![vec![], vec![pending]],
        Some(root.clone()),
    );
    let (mut publisher, publisher_closed) = RecordingPublisher::failing();
    publisher.trace = Arc::clone(&trace);
    let control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("startup has no pending event and should succeed");
    let temporary_root = control_plane.temporary_root().to_path_buf();
    trace.lock().expect("trace lock").clear();

    let error: ShutdownError = control_plane
        .shutdown()
        .expect_err("shutdown should report the publish failure");

    assert!(error.to_string().contains("outbox flush failed"));
    assert!(publisher_closed.load(Ordering::Acquire));
    assert!(storage_closed.load(Ordering::Acquire));
    assert!(!root.exists());
    assert!(!temporary_root.exists());
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.pending",
            "publisher.publish:shutdown-event",
            "publisher.close",
            "storage.close",
        ]
    );
}
