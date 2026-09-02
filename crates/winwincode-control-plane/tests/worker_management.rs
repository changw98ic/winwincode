// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use winwincode_api::generated::{
    Actor, PageRequest, RepositoryScope, RepositoryScopeKind, Scope, SystemActor, SystemActorKind,
    WorkerDrainCommand, WorkerDrainCommandCommand, WorkerDrainPayload, WorkerEnableCommand,
    WorkerEnableCommandCommand, WorkerEnablePayload, WorkerGetParameters, WorkerGetQuery,
    WorkerGetQueryQuery, WorkerListParameters, WorkerListQuery, WorkerListQueryQuery,
};
use winwincode_control_plane::{
    ScopeWorkerHealthEventPort, WorkerHealthEventPort, WorkerHealthEventPortError,
    WorkerHealthEventRequest, WorkerManagementService, WorkerManagementServiceErrorKind,
};
use winwincode_domain::{
    ExecutionMessageId, Instant, OpaqueCursor, OrganizationId, ProjectId, RepositoryId, RequestId,
    Revision, SchemaVersion, Sha256Digest, SystemActorId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, NewOutboxEvent, SqliteStorage, WorkerAuthenticationIdentity,
    WorkerManagementState, WorkerPlatform, WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-worker-management-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn actor() -> Actor {
    Actor::SystemActor(SystemActor {
        id: SystemActorId(id("sys", 1)),
        kind: SystemActorKind::System,
    })
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn registry_scope(scope: &RepositoryScope) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn registration(worker: u64, request: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::TransportPrincipal {
            issuer: "fixture-issuer".into(),
            subject: format!("worker-{worker}"),
            credential_fingerprint: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".into(), "shell".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "build-local".into(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", worker)),
    }
}

fn drain_command(scope: &RepositoryScope, worker: u64, request: u64) -> WorkerDrainCommand {
    WorkerDrainCommand {
        actor: actor(),
        command: WorkerDrainCommandCommand::WorkerDrain,
        expected_revision: Revision(0),
        payload: WorkerDrainPayload {
            reason: "planned maintenance".into(),
            worker_id: WorkerId(id("wrk", worker)),
        },
        request_id: RequestId(id("req", request)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope.clone()),
    }
}

fn enable_command(
    scope: &RepositoryScope,
    worker: u64,
    request: u64,
    revision: i64,
) -> WorkerEnableCommand {
    WorkerEnableCommand {
        actor: actor(),
        command: WorkerEnableCommandCommand::WorkerEnable,
        expected_revision: Revision(revision),
        payload: WorkerEnablePayload {
            worker_id: WorkerId(id("wrk", worker)),
        },
        request_id: RequestId(id("req", request)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope.clone()),
    }
}

#[derive(Default)]
struct CapturingPort {
    requests: Mutex<Vec<WorkerHealthEventRequest>>,
}

impl WorkerHealthEventPort for CapturingPort {
    fn prepare_worker_health_event(
        &self,
        request: &WorkerHealthEventRequest,
    ) -> Result<NewOutboxEvent, WorkerHealthEventPortError> {
        self.requests
            .lock()
            .expect("capture lock")
            .push(request.clone());
        ScopeWorkerHealthEventPort.prepare_worker_health_event(request)
    }
}

#[derive(Default)]
struct FailingPort {
    calls: AtomicUsize,
}

impl WorkerHealthEventPort for FailingPort {
    fn prepare_worker_health_event(
        &self,
        _request: &WorkerHealthEventRequest,
    ) -> Result<NewOutboxEvent, WorkerHealthEventPortError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(WorkerHealthEventPortError::unavailable())
    }
}

#[test]
fn exact_replay_and_changed_body_are_resolved_before_event_adapter() {
    let root = temporary_directory("command-replay");
    let scope = repository_scope(1);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    storage
        .execution_registry()
        .expect("registry")
        .register_worker_for_scope(&registration(1, 1), &registry_scope(&scope))
        .expect("registration");
    let command = drain_command(&scope, 1, 10);
    let capture = CapturingPort::default();
    let first = WorkerManagementService::new(&mut storage, &capture)
        .drain(&command, &instant(2))
        .expect("drain");
    assert_eq!(first.previous_revision, Revision(0));
    assert_eq!(first.current_revision, Revision(1));
    assert_eq!(first.result.state, "draining");
    assert_eq!(first.result.capacity, 4);
    let requests = capture.requests.lock().expect("capture lock");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].event.status, "draining");
    assert_eq!(requests[0].event.available_capacity, 0);
    assert!(
        !serde_json::to_string(&requests[0].event)
            .expect("event serialization")
            .contains("planned maintenance")
    );
    drop(requests);

    let failing = FailingPort::default();
    let replay = WorkerManagementService::new(&mut storage, &failing)
        .drain(&command, &instant(9))
        .expect("exact replay");
    assert_eq!(replay, first);
    assert_eq!(failing.calls.load(Ordering::Relaxed), 0);

    let mut changed = command.clone();
    changed.payload.reason = "changed body".into();
    let conflict = WorkerManagementService::new(&mut storage, &failing)
        .drain(&changed, &instant(9))
        .expect_err("changed command body");
    assert_eq!(
        conflict.kind(),
        WorkerManagementServiceErrorKind::RequestConflict
    );
    assert_eq!(failing.calls.load(Ordering::Relaxed), 0);

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn event_unavailability_does_not_change_worker_revision() {
    let root = temporary_directory("event-failure");
    let scope = repository_scope(2);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    storage
        .execution_registry()
        .expect("registry")
        .register_worker_for_scope(&registration(2, 20), &registry_scope(&scope))
        .expect("registration");
    let failing = FailingPort::default();
    let error = WorkerManagementService::new(&mut storage, &failing)
        .drain(&drain_command(&scope, 2, 21), &instant(2))
        .expect_err("event stream failure");
    assert_eq!(
        error.kind(),
        WorkerManagementServiceErrorKind::EventUnavailable
    );
    assert_eq!(failing.calls.load(Ordering::Relaxed), 1);
    let current = storage
        .execution_registry()
        .expect("registry")
        .load_managed_worker(
            &registry_scope(&scope),
            &WorkerId(id("wrk", 2)),
            &instant(2),
        )
        .expect("Worker read")
        .expect("Worker");
    assert_eq!(current.revision, 0);
    assert_eq!(current.management_state, WorkerManagementState::Enabled);

    let default_port = ScopeWorkerHealthEventPort;
    let drain = WorkerManagementService::new(&mut storage, &default_port)
        .drain(&drain_command(&scope, 2, 22), &instant(2))
        .expect("drain after recovery");
    assert_eq!(drain.result.state, "draining");
    let enable = WorkerManagementService::new(&mut storage, &default_port)
        .enable(&enable_command(&scope, 2, 23, 1), &instant(3))
        .expect("enable");
    assert_eq!(enable.result.state, "enabled");
    assert_eq!(enable.current_revision, Revision(2));

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn list_cursor_is_bound_to_scope_and_filters_and_get_is_exact() {
    let root = temporary_directory("queries");
    let scope = repository_scope(3);
    let foreign_scope = repository_scope(4);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    {
        let mut registry = storage.execution_registry().expect("registry");
        for worker in [30, 32] {
            registry
                .register_worker_for_scope(&registration(worker, worker), &registry_scope(&scope))
                .expect("registration");
        }
        registry
            .register_worker_for_scope(&registration(31, 31), &registry_scope(&foreign_scope))
            .expect("foreign registration");
    }
    let port = ScopeWorkerHealthEventPort;
    let first_query = WorkerListQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: WorkerListParameters {
            states: vec!["enabled".into()],
        },
        query: WorkerListQueryQuery::WorkerList,
        request_id: RequestId(id("req", 40)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope.clone()),
    };
    let first = WorkerManagementService::new(&mut storage, &port)
        .list(&first_query, &instant(2))
        .expect("first page");
    assert_eq!(first.result.items.len(), 1);
    assert_eq!(first.result.items[0].id, WorkerId(id("wrk", 30)));
    assert!(first.page.has_more);
    let cursor = first.page.next_cursor.expect("next cursor");

    let mut second_query = first_query.clone();
    second_query.request_id = RequestId(id("req", 41));
    second_query.page.cursor = Some(cursor.clone());
    let second = WorkerManagementService::new(&mut storage, &port)
        .list(&second_query, &instant(2))
        .expect("second page");
    assert_eq!(second.result.items.len(), 1);
    assert_eq!(second.result.items[0].id, WorkerId(id("wrk", 32)));
    assert!(!second.page.has_more);

    let mut foreign_cursor_query = second_query.clone();
    foreign_cursor_query.request_id = RequestId(id("req", 42));
    foreign_cursor_query.scope = Scope::RepositoryScope(foreign_scope);
    assert_eq!(
        WorkerManagementService::new(&mut storage, &port)
            .list(&foreign_cursor_query, &instant(2))
            .expect_err("foreign cursor")
            .kind(),
        WorkerManagementServiceErrorKind::InvalidRequest
    );
    let mut changed_filter_query = second_query;
    changed_filter_query.request_id = RequestId(id("req", 43));
    changed_filter_query.parameters.states = Vec::new();
    assert_eq!(
        WorkerManagementService::new(&mut storage, &port)
            .list(&changed_filter_query, &instant(2))
            .expect_err("filter cursor")
            .kind(),
        WorkerManagementServiceErrorKind::InvalidRequest
    );

    let get_query = WorkerGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: WorkerGetParameters {
            worker_id: WorkerId(id("wrk", 32)),
        },
        query: WorkerGetQueryQuery::WorkerGet,
        request_id: RequestId(id("req", 44)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope),
    };
    let get = WorkerManagementService::new(&mut storage, &port)
        .get(&get_query, &instant(2))
        .expect("get Worker");
    assert_eq!(get.result.id, WorkerId(id("wrk", 32)));
    assert_eq!(get.result.revision, Revision(0));
    assert_eq!(get.page.next_cursor, None);

    let mut malformed = first_query;
    malformed.request_id = RequestId(id("req", 45));
    malformed.page.cursor = Some(OpaqueCursor("not-base64-cursor".into()));
    assert_eq!(
        WorkerManagementService::new(&mut storage, &port)
            .list(&malformed, &instant(2))
            .expect_err("malformed cursor")
            .kind(),
        WorkerManagementServiceErrorKind::InvalidRequest
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
