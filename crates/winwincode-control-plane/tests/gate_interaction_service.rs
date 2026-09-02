// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::too_many_arguments, clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_api::generated::ModelRoute;
use winwincode_control_plane::{
    ContinueProductSessionCommand, CreateProductSessionCommand, ExpireGateInteractionCommand,
    GateCandidateIdentity, GateDecisionFact, GateHumanDecision, GateInteractionActor,
    GateInteractionAuthority, GateInteractionCommandContext, GateInteractionService,
    GateInteractionServiceErrorCode, GateInteractionState, GateInteractionSubject,
    ProductSessionCommandContext, ProductSessionService, RegisterGateInteractionCommand,
    RespondGateInteractionCommand, RoutableGateDecision,
};
use winwincode_domain::{
    ApprovalId, AttentionItemId, CodexThreadId, ControlPlaneEventId, CredentialReferenceId,
    DeliveryId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant,
    LeaseId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::action_gateway::GateDecision;
use winwincode_session::SessionBindingIdentity;
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus, PublicEventActor,
    PublicEventScope, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-gate-interaction-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-15T08:00:{second:02}.000Z"))
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn actor_key() -> ReceiptActorKey {
    winwincode_storage::receipt_actor_key(&public_actor()).expect("actor key")
}

fn repository_scope_key() -> ReceiptScopeKey {
    winwincode_storage::receipt_scope_key(&public_scope(1)).expect("repository scope")
}

fn other_scope_key() -> ReceiptScopeKey {
    winwincode_storage::receipt_scope_key(&public_scope(2)).expect("other scope")
}

fn public_actor() -> PublicEventActor {
    PublicEventActor::User {
        id: UserId(id("usr", 1)),
    }
}

fn public_scope(repository: u64) -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", repository)),
    }
}

fn assert_gate_receipts_are_internal(directory: &TestDirectory) {
    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("outbox database");
    let public_gate_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox \
             WHERE topic LIKE 'gate-interaction.%' AND projection_stream_kind IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("public Gate row count");
    assert_eq!(public_gate_rows, 0);
    let internal_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox \
             WHERE topic = 'gate-interaction.receipt.internal.v1' \
               AND projection_stream_kind IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("internal Gate row count");
    assert_eq!(internal_rows, 2);
}

fn model_route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        model_id: "fixture-model".into(),
        provider_id: "fixture-provider".into(),
    }
}

fn receipt(scope: &ReceiptScopeKey, request: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(actor_key(), scope.clone(), RequestId(id("req", request)))
        .expect("receipt identity")
}

fn product_context(
    scope: &ReceiptScopeKey,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> ProductSessionCommandContext {
    ProductSessionCommandContext {
        receipt_identity: receipt(scope, request),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
        public_actor: public_actor(),
        public_scope: if scope == &other_scope_key() {
            public_scope(2)
        } else {
            public_scope(1)
        },
    }
}

fn gate_context(
    scope: &ReceiptScopeKey,
    request: u64,
    second: u64,
) -> GateInteractionCommandContext {
    GateInteractionCommandContext {
        receipt_identity: receipt(scope, request),
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
    }
}

fn execution_scope(delivery: bool) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        product_session_id: ProductSessionId(id("psn", 1)),
        delivery_id: delivery.then(|| DeliveryId(id("dlv", 1))),
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", 1))
}

fn worker_registration() -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "fixture-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into()],
        capability_digest: digest('a'),
        security_zone: "local".into(),
        max_slots: 2,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: RequestId(id("req", 1_001)),
        sent_at: at(2),
        started_at: at(1),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn heartbeat() -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 2,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 2,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 2)),
        observed_at: at(3),
        sent_at: at(3),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let mut values = vec![
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool(),
        },
    ];
    if let Some(delivery_id) = &scope.delivery_id {
        values.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    values
}

fn prepare_runtime(storage: &mut SqliteStorage, delivery: bool) -> WorkerSlotAuthority {
    let scope = execution_scope(delivery);
    {
        let mut registry = storage.execution_registry().expect("registry");
        registry
            .register_worker(&worker_registration())
            .expect("register Worker");
        assert_eq!(
            registry
                .record_heartbeat(&heartbeat())
                .expect("heartbeat")
                .status,
            LeaseWriteStatus::Accepted
        );
    }
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: 10_000,
        cost_budget_microunits: 10_000,
        max_runtime_millis: 60_000,
    };
    {
        let mut admission = storage.execution_admission().expect("admission");
        for boundary in boundaries(&scope) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("configure admission");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: UserId(id("usr", 1)),
                worker_pool_id: pool(),
                job_id: ExecutionJobId(id("job", 1)),
                request_id: RequestId(id("req", 1_010)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: 100,
                reserved_cost_microunits: 100,
                runtime_limit_millis: 30_000,
                submitted_at: at(4),
            })
            .expect("reserve");
        admission
            .start(&ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: pool(),
                job_id: ExecutionJobId(id("job", 1)),
                request_id: RequestId(id("req", 1_011)),
                expected_revision: 1,
                started_at: at(5),
            })
            .expect("start");
    }
    let lease = ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken("1".into()),
        issued_at: at(5),
        job_id: ExecutionJobId(id("job", 1)),
        lease_id: LeaseId(id("lse", 1)),
        message_id: ExecutionMessageId(id("xmsg", 12)),
        payload_digest: digest('b'),
        request_id: RequestId(id("req", 1_012)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        attempt: 1,
    };
    assert_eq!(
        storage
            .execution_registry()
            .expect("registry")
            .claim_execution_job(&lease)
            .expect("claim lease")
            .status,
        LeaseWriteStatus::Accepted
    );
    let authority = WorkerSlotAuthority {
        worker_id: lease.worker_id,
        worker_instance_id: lease.worker_instance_id,
        worker_session_id: WorkerSessionId(id("wsn", 1)),
        codex_thread_id: CodexThreadId(id("cdx", 1)),
        job_id: lease.job_id,
        lease_id: lease.lease_id,
        attempt: lease.attempt,
        fencing_token: lease.fencing_token,
    };
    let mut slots = storage.worker_session_slots().expect("slots");
    slots
        .configure_resources(
            &authority.worker_id,
            &authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("slot limits");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: authority.clone(),
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 1_013)),
            opened_at: at(6),
        })
        .expect("open slot");
    authority
}

fn prepare_product_session(
    storage: &mut SqliteStorage,
    scope_key: &ReceiptScopeKey,
    runtime: &WorkerSlotAuthority,
    delivery: bool,
) -> u64 {
    let mut service = ProductSessionService::new(storage);
    service
        .create(&CreateProductSessionCommand {
            context: product_context(scope_key, 1, 0, 7),
            product_session_id: ProductSessionId(id("psn", 1)),
            project_id: ProjectId(id("prj", 1)),
            repository_id: RepositoryId(id("rep", 1)),
            title: "Gate route fixture".into(),
            model_route: model_route(),
        })
        .expect("create ProductSession");
    let binding_identity = if delivery {
        SessionBindingIdentity::delivery_stage(
            DeliveryId(id("dlv", 1)),
            None,
            StageRunId(id("run", 1)),
            ProductSessionId(id("psn", 1)),
            runtime.job_id.clone(),
        )
        .expect("Delivery binding")
    } else {
        SessionBindingIdentity::product_session(
            ProductSessionId(id("psn", 1)),
            runtime.job_id.clone(),
        )
        .expect("ProductSession binding")
    };
    service
        .continue_session(&ContinueProductSessionCommand {
            context: product_context(scope_key, 2, 1, 8),
            product_session_id: ProductSessionId(id("psn", 1)),
            binding_identity,
            runtime_authority: runtime.clone(),
            execution_scope: execution_scope(delivery),
            worker_pool_id: pool(),
            model_exchange_id: ModelExchangeId(id("mdl", 1)),
        })
        .expect("continue ProductSession")
        .record
        .session()
        .revision()
}

fn gate_fact(decision: &GateDecision) -> GateDecisionFact {
    GateDecisionFact {
        decision: RoutableGateDecision::from_gate(decision).expect("routable Gate decision"),
        action_id: "action:file/write:1".into(),
        action_digest: digest('c'),
        envelope_version: 3,
        envelope_digest: digest('d'),
        decision_revision: 4,
        candidate: Some(GateCandidateIdentity {
            candidate_ref: format!("git-candidate:sha256:{}", "e".repeat(64)),
            candidate_digest: digest('e'),
            candidate_revision: 2,
        }),
    }
}

fn authority(
    runtime: WorkerSlotAuthority,
    product_session_revision: u64,
    delivery: bool,
    gate: GateDecisionFact,
) -> GateInteractionAuthority {
    GateInteractionAuthority {
        execution_scope: execution_scope(delivery),
        worker_pool_id: pool(),
        product_session_revision,
        stage_run_id: delivery.then(|| StageRunId(id("run", 1))),
        job_revision: 2,
        worker_slot_revision: 1,
        runtime,
        lease_expires_at: at(50),
        gate,
    }
}

fn actor() -> GateInteractionActor {
    GateInteractionActor::User(UserId(id("usr", 1)))
}

fn approval() -> GateInteractionSubject {
    GateInteractionSubject::Approval(ApprovalId(id("apr", 1)))
}

fn attention() -> GateInteractionSubject {
    GateInteractionSubject::Attention(AttentionItemId(id("att", 1)))
}

fn register_command(
    scope: &ReceiptScopeKey,
    subject: &GateInteractionSubject,
    authority: GateInteractionAuthority,
    request: u64,
) -> RegisterGateInteractionCommand {
    RegisterGateInteractionCommand {
        context: gate_context(scope, request, 10),
        subject: subject.clone(),
        authority,
        authorized_actor: actor(),
        expires_at: at(40),
        attention_decisions: match subject {
            GateInteractionSubject::Approval(_) => Vec::new(),
            GateInteractionSubject::Attention(_) => vec!["retry".into(), "stop".into()],
        },
    }
}

fn setup(
    label: &str,
    delivery: bool,
    decision: &GateDecision,
) -> (
    TestDirectory,
    SqliteStorage,
    ReceiptScopeKey,
    GateInteractionAuthority,
) {
    let directory = TestDirectory::new(label);
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let scope = repository_scope_key();
    let runtime = prepare_runtime(&mut storage, delivery);
    let session_revision = prepare_product_session(&mut storage, &scope, &runtime, delivery);
    let gate = gate_fact(decision);
    let authority = authority(runtime, session_revision, delivery, gate);
    (directory, storage, scope, authority)
}

#[test]
fn plan_delta_routes_to_approval_and_replays_across_restart() {
    let (directory, mut storage, scope, authority) = setup(
        "approval",
        false,
        &GateDecision::RequestPlanDelta {
            reason: "plan exceeds the sealed scope".into(),
        },
    );
    let registration = register_command(&scope, &approval(), authority.clone(), 100);
    let registered = GateInteractionService::new(&mut storage)
        .register(&registration)
        .expect("register Approval");
    assert_eq!(registered.record.state, GateInteractionState::Pending);
    assert_eq!(registered.record.authority.stage_run_id, None);

    let replay = GateInteractionService::new(&mut storage)
        .register(&registration)
        .expect("registration replay");
    assert!(replay.replayed);
    assert_eq!(replay.record, registered.record);

    let response = RespondGateInteractionCommand {
        context: gate_context(&scope, 101, 20),
        subject: approval(),
        authority: authority.clone(),
        actor: actor(),
        decision: GateHumanDecision::Approve {
            reason_sha256: digest('f'),
        },
        responded_at: at(20),
    };
    let approved = GateInteractionService::new(&mut storage)
        .respond(&response)
        .expect("approve");
    assert_eq!(approved.record.state, GateInteractionState::Approved);
    assert_eq!(approved.record.current_revision, 5);
    assert!(
        GateInteractionService::new(&mut storage)
            .respond(&response)
            .expect("response replay")
            .replayed
    );

    let mut conflicting = response.clone();
    conflicting.decision = GateHumanDecision::Reject {
        reason_sha256: digest('f'),
    };
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&conflicting)
            .expect_err("changed request body")
            .code(),
        GateInteractionServiceErrorCode::RequestConflict
    );

    drop(storage);
    assert_gate_receipts_are_internal(&directory);
    let mut reopened = SqliteStorage::open(&directory.0).expect("reopen storage");
    let recovered = GateInteractionService::new(&mut reopened)
        .get(&scope, &approval())
        .expect("read recovered Approval")
        .expect("Approval fact");
    assert_eq!(recovered, approved.record);
    assert_eq!(recovered.authority.gate.action_id, "action:file/write:1");
}

#[test]
fn pause_routes_only_to_attention_with_sealed_choices() {
    let (_directory, mut storage, scope, authority) = setup(
        "attention",
        false,
        &GateDecision::PauseForHuman {
            reason: "human decision required".into(),
        },
    );
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .register(&register_command(
                &scope,
                &approval(),
                authority.clone(),
                110
            ))
            .expect_err("Pause cannot become Approval")
            .code(),
        GateInteractionServiceErrorCode::SubjectMismatch
    );
    let registration = register_command(&scope, &attention(), authority.clone(), 111);
    GateInteractionService::new(&mut storage)
        .register(&registration)
        .expect("register Attention");

    let mut response = RespondGateInteractionCommand {
        context: gate_context(&scope, 112, 20),
        subject: attention(),
        authority,
        actor: actor(),
        decision: GateHumanDecision::ResolveAttention {
            decision: "invented".into(),
            resolution_sha256: digest('a'),
        },
        responded_at: at(20),
    };
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&response)
            .expect_err("unsealed choice")
            .code(),
        GateInteractionServiceErrorCode::SubjectMismatch
    );
    response.context = gate_context(&scope, 113, 21);
    response.decision = GateHumanDecision::ResolveAttention {
        decision: "retry".into(),
        resolution_sha256: digest('a'),
    };
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&response)
            .expect("resolve Attention")
            .record
            .state,
        GateInteractionState::AttentionResolved
    );
}

#[test]
fn stale_scope_action_candidate_and_fence_fail_before_a_write() {
    let (_directory, mut storage, scope, authority) = setup(
        "stale",
        false,
        &GateDecision::RequestPlanDelta {
            reason: "plan change".into(),
        },
    );
    GateInteractionService::new(&mut storage)
        .register(&register_command(
            &scope,
            &approval(),
            authority.clone(),
            120,
        ))
        .expect("register Approval");

    let base = RespondGateInteractionCommand {
        context: gate_context(&scope, 121, 20),
        subject: approval(),
        authority: authority.clone(),
        actor: actor(),
        decision: GateHumanDecision::Reject {
            reason_sha256: digest('a'),
        },
        responded_at: at(20),
    };
    let mut stale_fence = base.clone();
    stale_fence.authority.runtime.fencing_token = FencingToken("2".into());
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&stale_fence)
            .expect_err("stale fence")
            .code(),
        GateInteractionServiceErrorCode::AuthorityMismatch
    );

    let mut stale_revision = base.clone();
    stale_revision.authority.job_revision = 3;
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&stale_revision)
            .expect_err("stale job revision")
            .code(),
        GateInteractionServiceErrorCode::AuthorityMismatch
    );

    let mut foreign_actor = base.clone();
    foreign_actor.actor = GateInteractionActor::User(UserId(id("usr", 2)));
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&foreign_actor)
            .expect_err("foreign actor")
            .code(),
        GateInteractionServiceErrorCode::ActorMismatch
    );

    let mut stale_candidate = base.clone();
    stale_candidate
        .authority
        .gate
        .candidate
        .as_mut()
        .expect("candidate")
        .candidate_revision = 3;
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&stale_candidate)
            .expect_err("stale candidate")
            .code(),
        GateInteractionServiceErrorCode::AuthorityMismatch
    );

    let mut cross_scope = base.clone();
    cross_scope.context = gate_context(&other_scope_key(), 122, 20);
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .respond(&cross_scope)
            .expect_err("cross-scope response")
            .code(),
        GateInteractionServiceErrorCode::NotFound
    );
    let record = GateInteractionService::new(&mut storage)
        .get(&scope, &approval())
        .expect("read pending")
        .expect("pending record");
    assert_eq!(record.state, GateInteractionState::Pending);
    assert_eq!(record.current_revision, 4);
}

#[test]
fn corrupt_durable_gate_seal_fails_closed_after_restart() {
    let (directory, mut storage, scope, authority) = setup(
        "corrupt-seal",
        false,
        &GateDecision::RequestPlanDelta {
            reason: "plan change".into(),
        },
    );
    GateInteractionService::new(&mut storage)
        .register(&register_command(&scope, &approval(), authority, 140))
        .expect("register Approval");
    drop(storage);

    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("state database");
    let (stream_id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT stream_id, payload FROM product_state WHERE stream_id LIKE 'gate-interaction:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("Gate state");
    let mut value: serde_json::Value = serde_json::from_slice(&payload).expect("Gate JSON");
    value["record"]["authority"]["gate"]["actionDigest"] =
        serde_json::Value::String("sha256:not-canonical".into());
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![serde_json::to_vec(&value).expect("Gate bytes"), stream_id],
        )
        .expect("corrupt Gate state");
    drop(connection);

    let mut reopened = SqliteStorage::open(&directory.0).expect("reopen storage");
    assert_eq!(
        GateInteractionService::new(&mut reopened)
            .get(&scope, &approval())
            .expect_err("corrupt Gate seal")
            .code(),
        GateInteractionServiceErrorCode::CorruptState
    );
}

#[test]
fn expiry_is_deterministic_and_delivery_stage_remains_exact() {
    let (_directory, mut storage, scope, authority) = setup(
        "expiry-stage",
        true,
        &GateDecision::ReplanRequired {
            reason: "candidate invalidated".into(),
        },
    );
    let registration = register_command(&scope, &attention(), authority.clone(), 130);
    GateInteractionService::new(&mut storage)
        .register(&registration)
        .expect("register staged Attention");

    let response_at_deadline = RespondGateInteractionCommand {
        context: gate_context(&scope, 131, 40),
        subject: attention(),
        authority: authority.clone(),
        actor: actor(),
        decision: GateHumanDecision::ResolveAttention {
            decision: "retry".into(),
            resolution_sha256: digest('a'),
        },
        responded_at: at(40),
    };
    let expired = GateInteractionService::new(&mut storage)
        .respond(&response_at_deadline)
        .expect("deadline response expires");
    assert_eq!(expired.record.state, GateInteractionState::Expired);
    assert_eq!(
        expired.record.authority.stage_run_id,
        Some(StageRunId(id("run", 1)))
    );
    assert!(
        GateInteractionService::new(&mut storage)
            .respond(&response_at_deadline)
            .expect("expiry replay")
            .replayed
    );

    let different_expiry = ExpireGateInteractionCommand {
        context: gate_context(&scope, 132, 41),
        subject: attention(),
        authority,
        expired_at: at(41),
    };
    assert_eq!(
        GateInteractionService::new(&mut storage)
            .expire(&different_expiry)
            .expect_err("another final command")
            .code(),
        GateInteractionServiceErrorCode::AlreadyResolved
    );
}
