// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
    ServiceAccountActor, ServiceAccountActorKind,
};
use winwincode_control_plane::{
    ConfiguredModelRetryPlanAuthority, CredentialReferenceService, DurableModelRetryPreOpenPlanner,
    FrozenModelRetryPlan, FrozenModelRouteAuthority, ModelCapability, ModelReservationReceipt,
    ModelRetryPlanAuthorityPort, ModelRetryPlannerError, ModelRetryPlannerErrorKind,
    ModelRetryPreOpenPlannerPort, ModelRetryStep, ModelSettingsProjection, ModelSettingsTarget,
    ModelToolSupport, ProviderAdmissionOpenReceipt, ProviderCatalogRequest, ProviderCatalogService,
    ProviderDescriptor, ProviderGatewayIdentity, StructuredOutputSupport, command_receipt_identity,
};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, DeliveryId, ExecutionJobId, ExecutionMessageId,
    FencingToken, Instant, LeaseId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RequestId, Revision, SchemaVersion, ServiceAccountId, SessionIdentity,
    Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, EncodedPayload, ExecutionJob,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
    ExecutionWorkspaceWriteMode, ModelGatewayRoute, ModelOpenMessage, ModelOpenMessageKind,
};
use winwincode_storage::{NewOutboxEvent, ProductStateStorage, SqliteStorage, StateCommit};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-model-retry-planner-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn user_actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn service_actor(seed: u64) -> Actor {
    Actor::ServiceAccountActor(ServiceAccountActor {
        id: ServiceAccountId(id("svc", seed)),
        kind: ServiceAccountActorKind::ServiceAccount,
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn register_provider(storage: &mut SqliteStorage) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: user_actor(1),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 1)),
                expected_catalog_version: 0,
            },
            &ProviderDescriptor {
                provider_id: "provider-a".to_owned(),
                display_name: "Provider A".to_owned(),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                models: vec![ModelCapability {
                    model_id: "model-a".to_owned(),
                    display_name: "Model A".to_owned(),
                    context_window_tokens: 128_000,
                    max_output_tokens: 16_000,
                    tool_support: ModelToolSupport::Parallel,
                    structured_output_support: StructuredOutputSupport::Unsupported,
                    reasoning_efforts: vec!["high".to_owned()],
                }],
            },
        )
        .expect("register Provider");
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: user_actor(1),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    display_name: "Provider A credential".to_owned(),
                    provider_id: "provider-a".to_owned(),
                    vault_locator: "local-fixture://provider-a".to_owned(),
                },
                request_id: RequestId(id("req", 2)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_000,
        )
        .expect("create Credential reference");
}

fn authority(storage: &mut SqliteStorage, repository_seed: u64) -> FrozenModelRouteAuthority {
    let catalog_scope = Scope::OrganizationScope(organization_scope());
    let capability = ProviderCatalogService::new(storage)
        .resolve_model(&catalog_scope, "provider-a", "model-a")
        .expect("resolve capability");
    let credential = CredentialReferenceService::new(storage)
        .resolve(&catalog_scope, &CredentialReferenceId(id("crd", 1)))
        .expect("resolve Credential reference");
    let route = ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        provider_id: "provider-a".to_owned(),
        model_id: "model-a".to_owned(),
    };
    let target = ModelSettingsTarget::ProductSession {
        repository_scope: repository_scope(repository_seed),
        product_session_id: ProductSessionId(id("psn", 1)),
    };
    FrozenModelRouteAuthority::from_resolved_authority(
        &ProviderGatewayIdentity::product_session(
            repository_scope(repository_seed),
            ProductSessionId(id("psn", 1)),
        ),
        &ModelSettingsProjection {
            target,
            selection: None,
            default_model_route: Some(route),
            worker_concurrency_limit: 8,
            revision: 1,
        },
        &capability,
        &credential,
    )
    .expect("freeze route authority")
}

fn alternate_authority(root: &std::path::Path) -> FrozenModelRouteAuthority {
    let mut storage = SqliteStorage::open(root).expect("open alternate authority storage");
    ProviderCatalogService::new(&mut storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: user_actor(1),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 3)),
                expected_catalog_version: 1,
            },
            &ProviderDescriptor {
                provider_id: "provider-b".to_owned(),
                display_name: "Provider B".to_owned(),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 2)),
                models: vec![ModelCapability {
                    model_id: "model-b".to_owned(),
                    display_name: "Model B".to_owned(),
                    context_window_tokens: 64_000,
                    max_output_tokens: 8_000,
                    tool_support: ModelToolSupport::Serial,
                    structured_output_support: StructuredOutputSupport::Unsupported,
                    reasoning_efforts: vec!["medium".to_owned()],
                }],
            },
        )
        .expect("register alternate Provider");
    CredentialReferenceService::new(&mut storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: user_actor(1),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 2)),
                    display_name: "Provider B credential".to_owned(),
                    provider_id: "provider-b".to_owned(),
                    vault_locator: "local-fixture://provider-b".to_owned(),
                },
                request_id: RequestId(id("req", 4)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_001,
        )
        .expect("create alternate Credential reference");
    let catalog_scope = Scope::OrganizationScope(organization_scope());
    let capability = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "provider-b", "model-b")
        .expect("resolve alternate capability");
    let credential = CredentialReferenceService::new(&mut storage)
        .resolve(&catalog_scope, &CredentialReferenceId(id("crd", 2)))
        .expect("resolve alternate Credential reference");
    FrozenModelRouteAuthority::from_resolved_authority(
        &ProviderGatewayIdentity::product_session(
            repository_scope(1),
            ProductSessionId(id("psn", 1)),
        ),
        &ModelSettingsProjection {
            target: ModelSettingsTarget::ProductSession {
                repository_scope: repository_scope(1),
                product_session_id: ProductSessionId(id("psn", 1)),
            },
            selection: None,
            default_model_route: Some(ModelRoute {
                credential_reference_id: CredentialReferenceId(id("crd", 2)),
                provider_id: "provider-b".to_owned(),
                model_id: "model-b".to_owned(),
            }),
            worker_concurrency_limit: 8,
            revision: 2,
        },
        &capability,
        &credential,
    )
    .expect("freeze alternate route authority")
}

fn execution_job(seed: u64, repository_seed: u64) -> ExecutionJob {
    ExecutionJob {
        attempt: 1,
        execution_profile: "executor".to_owned(),
        goal: "execute the frozen Delivery task".to_owned(),
        job_id: ExecutionJobId(id("job", seed)),
        limits: ExecutionLimits {
            deadline_at: Instant("2030-01-01T00:04:00.000Z".to_owned()),
            max_artifact_bytes: 1_000_000,
            max_runtime_seconds: 240,
        },
        payload_digest: Sha256Digest(format!("sha256:{seed:064x}")),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: DeliveryId(id("dlv", 1)),
            delivery_task_id: None,
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: ProductSessionId(id("psn", 1)),
            rework_authorization: None,
            stage_run_id: StageRunId(id("run", 1)),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            repository_id: RepositoryId(id("rep", repository_seed)),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    }
}

fn commit_job(storage: &mut SqliteStorage, actor: &Actor, repository_seed: u64) {
    let job = execution_job(1, repository_seed);
    let request_id = RequestId(id("req", 100));
    let identity = command_receipt_identity(
        actor,
        &Scope::RepositoryScope(repository_scope(repository_seed)),
        request_id,
    )
    .expect("job receipt identity");
    let event_id = format!("execution-job:{}", job.job_id.0);
    storage
        .commit(&StateCommit::new(
            identity,
            Sha256Digest(format!("sha256:{:064x}", 100)),
            "planner-execution-job:1",
            0,
            br#"{"schema":"planner-job.v1"}"#.to_vec(),
            vec![NewOutboxEvent::internal(
                event_id,
                "execution.job.dispatch",
                serde_json::to_vec(&job).expect("job bytes"),
            )],
        ))
        .expect("commit ExecutionJob");
}

fn message() -> ModelOpenMessage {
    let worker_session_id = WorkerSessionId(id("wsn", 1));
    ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2030-01-01T00:05:00.000Z".to_owned()),
            fencing_token: FencingToken("1".to_owned()),
            issued_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
            job_id: ExecutionJobId(id("job", 1)),
            lease_id: LeaseId(id("lse", 1)),
            worker_id: WorkerId(id("wrk", 1)),
            worker_instance_id: WorkerInstanceId(id("wki", 1)),
        },
        message_id: ExecutionMessageId(id("xmsg", 1)),
        model_exchange_id: ModelExchangeId(id("mdl", 1)),
        request: EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: "e30=".to_owned(),
            payload_digest: Sha256Digest(
                "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
                    .to_owned(),
            ),
        },
        request_id: RequestId(id("req", 900)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "control-plane".to_owned(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:01.000Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", 1)),
            product_session_id: ProductSessionId(id("psn", 1)),
            stage_run_id: Some(StageRunId(id("run", 1))),
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

fn admission(
    route_authority: FrozenModelRouteAuthority,
    replay: bool,
) -> ProviderAdmissionOpenReceipt {
    ProviderAdmissionOpenReceipt {
        reservation: ModelReservationReceipt {
            request_id: RequestId(id("req", 900)),
            model_exchange_id: ModelExchangeId(id("mdl", 1)),
            route_authority_fingerprint: route_authority.fingerprint().to_owned(),
            denial: None,
            unix_minute: 31_557_600,
            revision: 1,
            idempotent_replay: replay,
        },
        route_authority,
        enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts {
            tokens: 100,
            provider_cost_micros: 10,
            operations: 1,
            ..winwincode_storage::EnterpriseQuotaAmounts::default()
        },
    }
}

struct ChangedRoutePolicy {
    authority: FrozenModelRouteAuthority,
}

impl ModelRetryPlanAuthorityPort for ChangedRoutePolicy {
    fn freeze_plan(
        &self,
        _primary: FrozenModelRouteAuthority,
    ) -> Result<FrozenModelRetryPlan, ModelRetryPlannerError> {
        let step = ModelRetryStep::try_new(self.authority.clone(), 1).expect("changed route step");
        Ok(
            FrozenModelRetryPlan::freeze("changed-route-policy".to_owned(), 1, vec![step])
                .expect("changed route plan"),
        )
    }
}

fn setup(name: &str, actor: &Actor, repository_seed: u64) -> (PathBuf, FrozenModelRouteAuthority) {
    let root = temporary_directory(name);
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    register_provider(&mut storage);
    let route_authority = authority(&mut storage, repository_seed);
    commit_job(&mut storage, actor, 1);
    drop(storage);
    (root, route_authority)
}

#[test]
fn authenticated_execution_job_freezes_full_context_before_open_and_replays_after_restart() {
    let (root, route_authority) = setup("restart", &user_actor(7), 1);
    let policy = ConfiguredModelRetryPlanAuthority::try_new("production-retry".to_owned(), 3, 2)
        .expect("policy");
    DurableModelRetryPreOpenPlanner::open(&root, &policy)
        .expect("open before simulated crash")
        .close()
        .expect("close before prepare");
    let mut planner =
        DurableModelRetryPreOpenPlanner::open(&root, &policy).expect("restart after reserve");
    let context = planner
        .prepare(&message(), &admission(route_authority.clone(), true))
        .expect("recover exact reservation after crash before prepare");
    let attribution = &context.request().attribution;
    assert_eq!(attribution.organization_id, OrganizationId(id("org", 1)));
    assert_eq!(attribution.workspace_id, WorkspaceId(id("wsp", 1)));
    assert_eq!(attribution.project_id, ProjectId(id("prj", 1)));
    assert_eq!(attribution.repository_id, RepositoryId(id("rep", 1)));
    assert_eq!(
        attribution.product_session_id,
        ProductSessionId(id("psn", 1))
    );
    assert_eq!(attribution.delivery_id, Some(DeliveryId(id("dlv", 1))));
    assert_eq!(attribution.user_id, UserId(id("usr", 7)));
    let bytes = context.encode_json().expect("context bytes");
    let replay = planner
        .prepare(&message(), &admission(route_authority.clone(), true))
        .expect("exact admission replay");
    assert_eq!(replay.encode_json().expect("replay bytes"), bytes);
    planner.close().expect("close planner");

    let mut restarted =
        DurableModelRetryPreOpenPlanner::open(&root, &policy).expect("restart planner");
    let restarted_context = restarted
        .prepare(&message(), &admission(route_authority, true))
        .expect("restart replay");
    assert_eq!(
        restarted_context.encode_json().expect("restart bytes"),
        bytes
    );
    restarted.close().expect("close restarted planner");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn deterministic_policy_rejection_authorizes_only_the_current_admission_release() {
    let (root, route_authority) = setup("policy", &user_actor(1), 1);
    let changed_route = alternate_authority(&root);
    let policy = ChangedRoutePolicy {
        authority: changed_route,
    };
    let mut planner =
        DurableModelRetryPreOpenPlanner::open(&root, &policy).expect("open policy planner");
    let current_admission = admission(route_authority, false);
    let error = planner
        .prepare(&message(), &current_admission)
        .expect_err("changed policy route is rejected");
    assert_eq!(error.kind(), ModelRetryPlannerErrorKind::Policy);
    assert!(
        error
            .release_authority()
            .is_some_and(|authority| authority.authorizes(&current_admission))
    );
    assert_eq!(retry_context_rows(&root), 0);
    assert_eq!(failure_marker_rows(&root), 0);
    planner.close().expect("close policy planner");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn non_user_job_or_changed_repository_authority_is_rejected_before_retry_state() {
    let (service_root, service_authority) = setup("service", &service_actor(1), 1);
    let policy = ConfiguredModelRetryPlanAuthority::try_new("production-retry".to_owned(), 1, 1)
        .expect("policy");
    let mut service_planner =
        DurableModelRetryPreOpenPlanner::open(&service_root, &policy).expect("service planner");
    let service_admission = admission(service_authority, false);
    let service_error = service_planner
        .prepare(&message(), &service_admission)
        .expect_err("Worker cannot supply a User");
    assert_eq!(
        service_error.kind(),
        ModelRetryPlannerErrorKind::IdentityMismatch,
    );
    assert!(
        service_error
            .release_authority()
            .is_some_and(|authority| authority.authorizes(&service_admission))
    );
    assert_eq!(retry_context_rows(&service_root), 0);
    service_planner.close().expect("close service planner");
    fs::remove_dir_all(service_root).expect("remove service fixture");

    let (scope_root, changed_scope_authority) = setup("scope", &user_actor(1), 2);
    let mut scope_planner =
        DurableModelRetryPreOpenPlanner::open(&scope_root, &policy).expect("scope planner");
    let scope_admission = admission(changed_scope_authority, false);
    let scope_error = scope_planner
        .prepare(&message(), &scope_admission)
        .expect_err("changed repository is rejected");
    assert_eq!(
        scope_error.kind(),
        ModelRetryPlannerErrorKind::IdentityMismatch,
    );
    assert!(
        scope_error
            .release_authority()
            .is_some_and(|authority| authority.authorizes(&scope_admission))
    );
    assert_eq!(retry_context_rows(&scope_root), 0);
    scope_planner.close().expect("close scope planner");
    fs::remove_dir_all(scope_root).expect("remove scope fixture");

    let (route_root, primary) = setup("route", &user_actor(1), 1);
    let changed_route = alternate_authority(&route_root);
    let mut route_planner =
        DurableModelRetryPreOpenPlanner::open(&route_root, &policy).expect("route planner");
    route_planner
        .prepare(&message(), &admission(primary, false))
        .expect("freeze original route");
    let route_error = route_planner
        .prepare(&message(), &admission(changed_route, false))
        .expect_err("changed route is rejected");
    assert_eq!(route_error.kind(), ModelRetryPlannerErrorKind::Ledger);
    assert!(route_error.release_authority().is_none());
    assert_eq!(retry_context_rows(&route_root), 2);
    route_planner.close().expect("close route planner");
    fs::remove_dir_all(route_root).expect("remove route fixture");
}

#[test]
fn context_commit_fault_persists_release_marker_and_replay_stays_fail_closed() {
    let (root, route_authority) = setup("fault", &user_actor(1), 1);
    let database = root.join("control-plane.sqlite3");
    let fault = rusqlite::Connection::open(&database).expect("fault connection");
    fault
        .execute_batch(
            "CREATE TRIGGER fail_retry_context
             BEFORE INSERT ON product_state
             WHEN NEW.stream_id LIKE 'model-retry-context:%'
             BEGIN SELECT RAISE(ABORT, 'injected context crash'); END;",
        )
        .expect("install fault");
    let policy = ConfiguredModelRetryPlanAuthority::try_new("production-retry".to_owned(), 1, 1)
        .expect("policy");
    let mut planner = DurableModelRetryPreOpenPlanner::open(&root, &policy).expect("open planner");
    let original_admission = admission(route_authority.clone(), false);
    let failure = planner
        .prepare(&message(), &original_admission)
        .expect_err("context transaction fails");
    assert_eq!(failure.kind(), ModelRetryPlannerErrorKind::Ledger);
    assert!(
        failure
            .release_authority()
            .is_some_and(|authority| authority.authorizes(&original_admission))
    );
    assert_eq!(retry_context_rows(&root), 0);
    assert_eq!(failure_marker_rows(&root), 1);
    fault
        .execute_batch("DROP TRIGGER fail_retry_context;")
        .expect("remove fault");
    drop(fault);

    planner.close().expect("close failed planner");
    let mut restarted =
        DurableModelRetryPreOpenPlanner::open(&root, &policy).expect("restart failed planner");
    let replay_admission = admission(route_authority, true);
    let replay = restarted
        .prepare(&message(), &replay_admission)
        .expect_err("durable failure marker prevents reservation reuse");
    assert_eq!(replay.kind(), ModelRetryPlannerErrorKind::Ledger);
    assert!(
        replay
            .release_authority()
            .is_some_and(|authority| authority.authorizes(&replay_admission))
    );
    assert_eq!(retry_context_rows(&root), 0);
    assert_eq!(failure_marker_rows(&root), 1);
    restarted.close().expect("close restarted planner");

    let changed_route = alternate_authority(&root);
    let mut changed =
        DurableModelRetryPreOpenPlanner::open(&root, &policy).expect("open changed planner");
    let changed_admission = admission(changed_route, true);
    let mismatch = changed
        .prepare(&message(), &changed_admission)
        .expect_err("failure marker cannot release a foreign route reservation");
    assert_eq!(
        mismatch.kind(),
        ModelRetryPlannerErrorKind::IdentityMismatch
    );
    assert!(mismatch.release_authority().is_none());
    assert_eq!(retry_context_rows(&root), 0);
    assert_eq!(failure_marker_rows(&root), 1);
    changed.close().expect("close changed planner");
    fs::remove_dir_all(root).expect("remove fixture");
}

fn retry_context_rows(root: &std::path::Path) -> u64 {
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("inspect database");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM product_state
             WHERE stream_id LIKE 'model-retry-usage:%'
                OR stream_id LIKE 'model-retry-context:%'",
            [],
            |row| row.get(0),
        )
        .expect("retry state count");
    u64::try_from(count).expect("non-negative retry state count")
}

fn failure_marker_rows(root: &std::path::Path) -> u64 {
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("inspect database");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM product_state
             WHERE stream_id LIKE 'model-retry-pre-open-failure:%'",
            [],
            |row| row.get(0),
        )
        .expect("failure marker count");
    u64::try_from(count).expect("non-negative failure marker count")
}
