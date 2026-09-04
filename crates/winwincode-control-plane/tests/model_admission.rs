// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind,
    RepositoryScope, RepositoryScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CredentialReferenceResolution, CredentialReferenceService, DurableProviderGatewayAdmission,
    EnterpriseModelPolicyCeiling, FrozenModelAdmissionPolicy, FrozenModelRouteAuthority,
    LocalModelPolicyAuthority, LocalModelPolicyAuthorityConfig, ModelAdmissionClock,
    ModelAdmissionClockError, ModelAdmissionDenialReason, ModelAdmissionErrorKind,
    ModelAdmissionLimits, ModelAdmissionPolicyLayer, ModelAdmissionService, ModelCapability,
    ModelPolicyAuthorityError, ModelPolicyAuthorityPort, ModelPolicyAuthoritySnapshot,
    ModelPolicyResolutionErrorKind, ModelPolicyRouteKey, ModelRequestAdmission,
    ModelReservationCompletion, ModelReservationReceipt, ModelReservationRelease,
    ModelReservationReleaseReason, ModelReservationRequest, ModelReservationTerminalOutcome,
    ModelRoutePolicyDecision, ModelSettingsProjection, ModelSettingsRequest, ModelSettingsService,
    ModelSettingsTarget, ModelSettingsValues, ModelToolSupport, ProductionModelPolicySource,
    ProviderAdmissionOpenRequest, ProviderAdmissionReservationConfig, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor, ProviderGatewayAdmissionPort,
    ProviderGatewayIdentity, ProviderTokenUsage, ResolvedModelCapability,
};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, ExecutionJobId, ExecutionMessageId, FencingToken,
    Instant, LeaseId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, SessionIdentity, Sha256Digest, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionLeaseStamp, ModelGatewayRoute, ModelOpenMessage, ModelOpenMessageKind,
};
use winwincode_storage::SqliteStorage;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-model-admission-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        provider_id: "provider-a".to_owned(),
        model_id: "model-a".to_owned(),
    }
}

fn model_capability() -> ModelCapability {
    ModelCapability {
        model_id: "model-a".to_owned(),
        display_name: "Model A".to_owned(),
        context_window_tokens: 128_000,
        max_output_tokens: 16_000,
        tool_support: ModelToolSupport::Parallel,
        reasoning_efforts: vec!["high".to_owned()],
    }
}

fn register_provider(storage: &mut SqliteStorage, request_seed: u64, expected_version: u64) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", request_seed)),
                expected_catalog_version: expected_version,
            },
            &ProviderDescriptor {
                provider_id: "provider-a".to_owned(),
                display_name: "Provider A".to_owned(),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                models: vec![model_capability()],
            },
            Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("register Provider");
}

fn create_credential(storage: &mut SqliteStorage) {
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
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

fn configure_session(storage: &mut SqliteStorage, session_seed: u64) {
    ModelSettingsService::new(storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(),
                target: ModelSettingsTarget::ProductSession {
                    repository_scope: repository_scope(),
                    product_session_id: ProductSessionId(id("psn", session_seed)),
                },
                request_id: RequestId(id("req", 10 + session_seed)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(route()),
                worker_concurrency_limit: 100,
            },
            Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("configure session route");
}

fn authority(storage: &mut SqliteStorage, session_seed: u64) -> FrozenModelRouteAuthority {
    let catalog_scope = Scope::OrganizationScope(organization_scope());
    let capability = ProviderCatalogService::new(storage)
        .resolve_model(&catalog_scope, "provider-a", "model-a")
        .expect("resolve Catalog route");
    let credential = CredentialReferenceService::new(storage)
        .resolve(&catalog_scope, &CredentialReferenceId(id("crd", 1)))
        .expect("resolve Credential reference");
    let target = ModelSettingsTarget::ProductSession {
        repository_scope: repository_scope(),
        product_session_id: ProductSessionId(id("psn", session_seed)),
    };
    let settings = ModelSettingsService::new(storage)
        .project(&target)
        .expect("project session settings");
    FrozenModelRouteAuthority::from_resolved_authority(
        &ProviderGatewayIdentity::product_session(
            repository_scope(),
            ProductSessionId(id("psn", session_seed)),
        ),
        &settings,
        &capability,
        &credential,
    )
    .expect("freeze route authority")
}

fn setup(name: &str) -> (PathBuf, SqliteStorage, FrozenModelRouteAuthority) {
    let root = temporary_directory(name);
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    register_provider(&mut storage, 1, 0);
    create_credential(&mut storage);
    configure_session(&mut storage, 1);
    let authority = authority(&mut storage, 1);
    (root, storage, authority)
}

fn admission_message(seed: u64) -> ModelOpenMessage {
    let worker_session_id = WorkerSessionId(id("wsn", 1));
    ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2030-01-01T00:05:00Z".to_owned()),
            fencing_token: FencingToken(id("fence", 1)),
            issued_at: Instant("2030-01-01T00:00:00Z".to_owned()),
            job_id: ExecutionJobId(id("job", 1)),
            lease_id: LeaseId(id("lea", 1)),
            worker_id: WorkerId(id("wrk", 1)),
            worker_instance_id: WorkerInstanceId(id("wri", 1)),
        },
        message_id: ExecutionMessageId(id("xmsg", seed)),
        model_exchange_id: ModelExchangeId(id("mdl", seed)),
        request: EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: "e30=".to_owned(),
            payload_digest: Sha256Digest(
                "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
                    .to_owned(),
            ),
        },
        request_id: RequestId(id("req", 800 + seed)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "configured-session-route".to_owned(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:00Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", 1)),
            product_session_id: ProductSessionId(id("psn", 1)),
            stage_run_id: None,
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

struct ProviderAdmissionFacts {
    settings: ModelSettingsProjection,
    capability: ResolvedModelCapability,
    credential: CredentialReferenceResolution,
    identity: ProviderGatewayIdentity,
    message: ModelOpenMessage,
}

fn provider_admission_facts(storage: &mut SqliteStorage) -> ProviderAdmissionFacts {
    let target = ModelSettingsTarget::ProductSession {
        repository_scope: repository_scope(),
        product_session_id: ProductSessionId(id("psn", 1)),
    };
    let settings = ModelSettingsService::new(storage)
        .project(&target)
        .expect("project exact settings");
    let scope = Scope::OrganizationScope(organization_scope());
    let capability = ProviderCatalogService::new(storage)
        .resolve_model(&scope, "provider-a", "model-a")
        .expect("resolve exact capability");
    let credential = CredentialReferenceService::new(storage)
        .resolve(&scope, &CredentialReferenceId(id("crd", 1)))
        .expect("resolve exact Credential reference");
    ProviderAdmissionFacts {
        settings,
        capability,
        credential,
        identity: ProviderGatewayIdentity::product_session(
            repository_scope(),
            ProductSessionId(id("psn", 1)),
        ),
        message: admission_message(90),
    }
}

fn durable_admission<'authority, 'clock>(
    root: &std::path::Path,
    clock: &'clock dyn ModelAdmissionClock,
    policy_authority: &'authority dyn ModelPolicyAuthorityPort,
    reservation: ProviderAdmissionReservationConfig,
) -> DurableProviderGatewayAdmission<'authority, 'clock> {
    DurableProviderGatewayAdmission::new(
        SqliteStorage::open(root).expect("open admission connection"),
        clock,
        policy_authority,
        reservation,
    )
}

fn limits() -> ModelAdmissionLimits {
    ModelAdmissionLimits {
        requests_per_minute: 100,
        tokens_per_minute: 10_000,
        concurrent_requests: 100,
        token_budget: 100_000,
        cost_budget_micros: 100_000,
    }
}

fn policy_with(
    base_limits: ModelAdmissionLimits,
    base_decision: ModelRoutePolicyDecision,
    enterprise: Option<(ModelAdmissionLimits, ModelRoutePolicyDecision)>,
) -> FrozenModelAdmissionPolicy {
    let base = ModelAdmissionPolicyLayer::try_new(
        "base-policy".to_owned(),
        7,
        "budget-2030-01".to_owned(),
        base_decision,
        base_limits,
    )
    .expect("base policy");
    let enterprise = enterprise.map(|(limits, decision)| {
        ModelAdmissionPolicyLayer::try_new(
            "enterprise-policy".to_owned(),
            11,
            "budget-2030-01".to_owned(),
            decision,
            limits,
        )
        .expect("enterprise policy")
    });
    FrozenModelAdmissionPolicy::freeze(base, enterprise).expect("effective policy")
}

fn policy(policy_limits: ModelAdmissionLimits) -> FrozenModelAdmissionPolicy {
    policy_with(policy_limits, ModelRoutePolicyDecision::Allow, None)
}

fn reservation(
    authority: &FrozenModelRouteAuthority,
    seed: u64,
    tokens: u64,
    cost: u64,
) -> ModelReservationRequest {
    let admission = ModelRequestAdmission::from_gateway_route(
        &ProviderGatewayIdentity::product_session(
            repository_scope(),
            ProductSessionId(id("psn", 1)),
        ),
        &route(),
        ModelExchangeId(id("mdl", seed)),
        RequestId(id("req", seed + 100)),
    )
    .expect("pool admission");
    assert_eq!(&admission.route, authority.route_key());
    ModelReservationRequest::try_new(admission, tokens, cost).expect("reservation")
}

struct FixedClock(u64);

impl ModelAdmissionClock for FixedClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        Ok(self.0)
    }
}

struct OneShotClock {
    minute: u64,
    used: AtomicBool,
    calls: AtomicU64,
}

impl ModelAdmissionClock for OneShotClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.used.swap(true, Ordering::Relaxed) {
            Err(ModelAdmissionClockError)
        } else {
            Ok(self.minute)
        }
    }
}

#[test]
fn enterprise_denial_and_stricter_ceiling_apply_before_provider_invocation() {
    let (root, mut storage, authority) = setup("policy-denial");
    let mut enterprise_limits = limits();
    enterprise_limits.concurrent_requests = 1;
    let denied_policy = policy_with(
        limits(),
        ModelRoutePolicyDecision::Allow,
        Some((enterprise_limits, ModelRoutePolicyDecision::Deny)),
    );
    assert!(!denied_policy.route_allowed());
    assert_eq!(denied_policy.limits().concurrent_requests, 1);
    assert_eq!(denied_policy.sources().len(), 2);
    assert_eq!(denied_policy.sources()[1].authority_id, "enterprise-policy");

    let mut provider_invocations = 0_u64;
    let receipt = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_000))
        .reserve(
            &authority,
            &denied_policy,
            &reservation(&authority, 1, 10, 2),
        )
        .expect("durable denial");
    if receipt.admitted() {
        provider_invocations += 1;
    }
    assert_eq!(
        receipt.denial,
        Some(ModelAdmissionDenialReason::PolicyDenied)
    );
    assert_eq!(provider_invocations, 0);

    let ceiling_policy = policy_with(
        limits(),
        ModelRoutePolicyDecision::Allow,
        Some((enterprise_limits, ModelRoutePolicyDecision::Allow)),
    );
    for seed in [2, 3] {
        let receipt = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_000))
            .reserve(
                &authority,
                &ceiling_policy,
                &reservation(&authority, seed, 10, 2),
            )
            .expect("enterprise concurrency decision");
        if receipt.admitted() {
            provider_invocations += 1;
        } else {
            assert_eq!(
                receipt.denial,
                Some(ModelAdmissionDenialReason::Concurrency)
            );
        }
    }
    assert_eq!(provider_invocations, 1);

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn every_limit_has_a_durable_fail_closed_denial() {
    let cases = [
        (
            "rpm",
            ModelAdmissionLimits {
                requests_per_minute: 1,
                ..limits()
            },
            (1, 1),
            (1, 1),
            ModelAdmissionDenialReason::RequestsPerMinute,
        ),
        (
            "tpm",
            ModelAdmissionLimits {
                tokens_per_minute: 10,
                ..limits()
            },
            (6, 1),
            (5, 1),
            ModelAdmissionDenialReason::TokensPerMinute,
        ),
        (
            "concurrency",
            ModelAdmissionLimits {
                concurrent_requests: 1,
                ..limits()
            },
            (1, 1),
            (1, 1),
            ModelAdmissionDenialReason::Concurrency,
        ),
        (
            "token-budget",
            ModelAdmissionLimits {
                token_budget: 10,
                ..limits()
            },
            (6, 1),
            (5, 1),
            ModelAdmissionDenialReason::TokenBudget,
        ),
        (
            "cost-budget",
            ModelAdmissionLimits {
                cost_budget_micros: 10,
                ..limits()
            },
            (1, 6),
            (1, 5),
            ModelAdmissionDenialReason::CostBudget,
        ),
    ];

    for (name, policy_limits, first, second, expected) in cases {
        let (root, mut storage, authority) = setup(name);
        let frozen = policy(policy_limits);
        let clock = FixedClock(31_556_001);
        let mut service = ModelAdmissionService::new(&mut storage, &clock);
        assert!(
            service
                .reserve(
                    &authority,
                    &frozen,
                    &reservation(&authority, 10, first.0, first.1),
                )
                .expect("first reservation")
                .admitted()
        );
        let denied = service
            .reserve(
                &authority,
                &frozen,
                &reservation(&authority, 11, second.0, second.1),
            )
            .expect("durable limit denial");
        assert_eq!(denied.denial, Some(expected), "{name}");
        let replay = service
            .reserve(
                &authority,
                &frozen,
                &reservation(&authority, 11, second.0, second.1),
            )
            .expect("denial replay");
        assert_eq!(replay.denial, Some(expected), "{name} replay");
        assert!(replay.idempotent_replay);
        drop(storage);
        fs::remove_dir_all(root).expect("remove fixture");
    }
}

#[test]
fn exact_reservation_replay_precedes_clock_and_reuses_original_minute() {
    let (root, mut storage, authority) = setup("replay-clock");
    let request = reservation(&authority, 20, 20, 2);
    let frozen = policy(limits());
    let clock = OneShotClock {
        minute: 31_556_010,
        used: AtomicBool::new(false),
        calls: AtomicU64::new(0),
    };
    let first = ModelAdmissionService::new(&mut storage, &clock)
        .reserve(&authority, &frozen, &request)
        .expect("first reservation");
    assert!(first.admitted());
    assert_eq!(first.route_authority_fingerprint, authority.fingerprint());
    drop(storage);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let replay = ModelAdmissionService::new(&mut reopened, &clock)
        .reserve(&authority, &frozen, &request)
        .expect("receipt-first replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.unix_minute, first.unix_minute);
    assert_eq!(clock.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        ModelAdmissionService::new(&mut reopened, &clock)
            .reserve(&authority, &frozen, &reservation(&authority, 20, 21, 2),)
            .expect_err("changed-body replay conflicts before the clock")
            .kind(),
        ModelAdmissionErrorKind::RequestConflict
    );
    assert_eq!(clock.calls.load(Ordering::Relaxed), 1);

    drop(reopened);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn clock_rollback_fails_closed_without_resetting_the_minute_ledger() {
    let (root, mut storage, authority) = setup("clock-rollback");
    let frozen = policy(limits());
    let first = reservation(&authority, 25, 5, 1);
    ModelAdmissionService::new(&mut storage, &FixedClock(31_556_100))
        .reserve(&authority, &frozen, &first)
        .expect("first reservation");
    let before = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_100))
        .snapshot(&authority, "budget-2030-01")
        .expect("snapshot before rollback");

    assert_eq!(
        ModelAdmissionService::new(&mut storage, &FixedClock(31_556_099))
            .reserve(&authority, &frozen, &reservation(&authority, 26, 5, 1),)
            .expect_err("backward clock is rejected")
            .kind(),
        ModelAdmissionErrorKind::ClockUnavailable
    );
    let after = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_100))
        .snapshot(&authority, "budget-2030-01")
        .expect("snapshot after rollback");
    assert_eq!(after, before);

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn release_and_completion_settle_once_and_recover_after_restart() {
    let (root, mut storage, authority) = setup("terminal-restart");
    let frozen = policy(limits());
    let first = reservation(&authority, 30, 20, 5);
    let second = reservation(&authority, 31, 30, 7);
    {
        let mut service = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_020));
        assert!(
            service
                .reserve(&authority, &frozen, &first)
                .expect("first")
                .admitted()
        );
        assert!(
            service
                .reserve(&authority, &frozen, &second)
                .expect("second")
                .admitted()
        );
    }
    let release = ModelReservationRelease {
        request_id: RequestId(id("req", 300)),
        model_exchange_id: first.model_exchange_id().clone(),
        reason: ModelReservationReleaseReason::Cancelled,
    };
    let completion = ModelReservationCompletion {
        request_id: RequestId(id("req", 301)),
        model_exchange_id: second.model_exchange_id().clone(),
        usage: ProviderTokenUsage {
            input_tokens: 5,
            cached_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 3,
            reasoning_output_tokens: 1,
        },
        actual_cost_micros: 4,
    };
    {
        let mut service = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_020));
        assert_eq!(
            service
                .release(&authority, &release)
                .expect("release")
                .outcome,
            ModelReservationTerminalOutcome::Cancelled
        );
        let settled = service.complete(&authority, &completion).expect("settle");
        assert_eq!(settled.route_authority_fingerprint, authority.fingerprint());
        assert_eq!(settled.actual_tokens, 8);
        assert_eq!(settled.actual_cost_micros, 4);
        let snapshot = service
            .snapshot(&authority, "budget-2030-01")
            .expect("snapshot");
        assert_eq!(snapshot.active_reservations, 0);
        assert_eq!(snapshot.budget_reserved_tokens, 0);
        assert_eq!(snapshot.budget_reserved_cost_micros, 0);
        assert_eq!(snapshot.budget_settled_tokens, 8);
        assert_eq!(snapshot.budget_settled_cost_micros, 4);
    }
    drop(storage);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let mut service = ModelAdmissionService::new(&mut reopened, &FixedClock(31_556_999));
    assert!(
        service
            .release(&authority, &release)
            .expect("release replay")
            .idempotent_replay
    );
    assert!(
        service
            .complete(&authority, &completion)
            .expect("settlement replay")
            .idempotent_replay
    );
    let conflicting_terminal = ModelReservationRelease {
        request_id: RequestId(id("req", 302)),
        model_exchange_id: second.model_exchange_id().clone(),
        reason: ModelReservationReleaseReason::ProviderFailed,
    };
    assert_eq!(
        service
            .release(&authority, &conflicting_terminal)
            .expect_err("another terminal is rejected")
            .kind(),
        ModelAdmissionErrorKind::TerminalConflict
    );

    drop(reopened);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn catalog_change_cannot_terminate_an_older_authority_reservation() {
    let (root, mut storage, authority_v1) = setup("changed-authority");
    let frozen = policy(limits());
    let request = reservation(&authority_v1, 40, 10, 1);
    ModelAdmissionService::new(&mut storage, &FixedClock(31_556_030))
        .reserve(&authority_v1, &frozen, &request)
        .expect("reserve v1");
    let revision_before = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_030))
        .snapshot(&authority_v1, "budget-2030-01")
        .expect("snapshot before")
        .revision;

    register_provider(&mut storage, 3, 1);
    let authority_v2 = authority(&mut storage, 1);
    assert_ne!(
        authority_v1.catalog_version(),
        authority_v2.catalog_version()
    );
    let release = ModelReservationRelease {
        request_id: RequestId(id("req", 400)),
        model_exchange_id: request.model_exchange_id().clone(),
        reason: ModelReservationReleaseReason::ProviderFailed,
    };
    assert_eq!(
        ModelAdmissionService::new(&mut storage, &FixedClock(31_556_030))
            .release(&authority_v2, &release)
            .expect_err("changed authority fails closed")
            .kind(),
        ModelAdmissionErrorKind::IdentityMismatch
    );
    let after = ModelAdmissionService::new(&mut storage, &FixedClock(31_556_030))
        .snapshot(&authority_v1, "budget-2030-01")
        .expect("snapshot after");
    assert_eq!(after.revision, revision_before);
    assert_eq!(after.active_reservations, 1);

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn concurrent_sqlite_reservations_never_exceed_the_frozen_limit() {
    let (root, storage, authority) = setup("concurrent");
    drop(storage);
    let frozen = policy(ModelAdmissionLimits {
        concurrent_requests: 2,
        ..limits()
    });
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0_u64..8)
        .map(|index| {
            let root = root.clone();
            let authority = authority.clone();
            let frozen = frozen.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("open concurrent storage");
                let request = reservation(&authority, 500 + index, 1, 1);
                barrier.wait();
                ModelAdmissionService::new(&mut storage, &FixedClock(31_556_040))
                    .reserve(&authority, &frozen, &request)
                    .expect("concurrent durable decision")
                    .admitted()
            })
        })
        .collect::<Vec<_>>();
    let admitted = handles
        .into_iter()
        .map(|handle| handle.join().expect("join reservation"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(admitted, 2);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let snapshot = ModelAdmissionService::new(&mut reopened, &FixedClock(31_556_040))
        .snapshot(&authority, "budget-2030-01")
        .expect("concurrent snapshot");
    assert_eq!(snapshot.active_reservations, 2);
    assert_eq!(snapshot.minute_requests, 2);

    drop(reopened);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn concurrent_exact_replay_creates_one_reservation_and_one_rate_count() {
    let (root, storage, authority) = setup("concurrent-replay");
    drop(storage);
    let frozen = policy(ModelAdmissionLimits {
        requests_per_minute: 1,
        concurrent_requests: 1,
        ..limits()
    });
    let request = reservation(&authority, 550, 2, 1);
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let root = root.clone();
            let authority = authority.clone();
            let frozen = frozen.clone();
            let request = request.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("open replay storage");
                barrier.wait();
                ModelAdmissionService::new(&mut storage, &FixedClock(31_556_045))
                    .reserve(&authority, &frozen, &request)
                    .expect("concurrent exact replay")
            })
        })
        .collect::<Vec<_>>();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("join exact replay"))
        .collect::<Vec<_>>();
    assert!(receipts.iter().all(ModelReservationReceipt::admitted));
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| !receipt.idempotent_replay)
            .count(),
        1
    );

    let mut reopened = SqliteStorage::open(&root).expect("reopen replay storage");
    let snapshot = ModelAdmissionService::new(&mut reopened, &FixedClock(31_556_045))
        .snapshot(&authority, "budget-2030-01")
        .expect("exact replay snapshot");
    assert_eq!(snapshot.active_reservations, 1);
    assert_eq!(snapshot.minute_requests, 1);
    assert_eq!(snapshot.minute_tokens, 2);

    drop(reopened);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn budget_and_scope_ledgers_remain_isolated() {
    let (root, mut storage, authority_one) = setup("scope-isolation");
    configure_session(&mut storage, 2);
    let authority_two = authority(&mut storage, 2);
    let frozen = policy(ModelAdmissionLimits {
        token_budget: 5,
        ..limits()
    });
    assert!(
        ModelAdmissionService::new(&mut storage, &FixedClock(31_556_050))
            .reserve(
                &authority_one,
                &frozen,
                &reservation(&authority_one, 600, 5, 1)
            )
            .expect("scope one")
            .admitted()
    );
    assert!(
        ModelAdmissionService::new(&mut storage, &FixedClock(31_556_050))
            .reserve(
                &authority_two,
                &frozen,
                &reservation_for_session(2, 601, 5, 1)
            )
            .expect("scope two")
            .admitted()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

fn reservation_for_session(
    session_seed: u64,
    exchange_seed: u64,
    tokens: u64,
    cost: u64,
) -> ModelReservationRequest {
    ModelReservationRequest::try_new(
        ModelRequestAdmission::from_gateway_route(
            &ProviderGatewayIdentity::product_session(
                repository_scope(),
                ProductSessionId(id("psn", session_seed)),
            ),
            &route(),
            ModelExchangeId(id("mdl", exchange_seed)),
            RequestId(id("req", exchange_seed + 100)),
        )
        .expect("session admission"),
        tokens,
        cost,
    )
    .expect("session reservation")
}

struct PolicyAuthorityFixture {
    base: ModelAdmissionPolicyLayer,
    enterprise: Option<ModelAdmissionPolicyLayer>,
    returned_key: Option<ModelPolicyRouteKey>,
    unavailable: AtomicBool,
    queries: Mutex<Vec<ModelPolicyRouteKey>>,
}

impl ModelPolicyAuthorityPort for PolicyAuthorityFixture {
    fn snapshot(
        &self,
        key: &ModelPolicyRouteKey,
    ) -> Result<ModelPolicyAuthoritySnapshot, ModelPolicyAuthorityError> {
        self.queries
            .lock()
            .expect("policy query lock")
            .push(key.clone());
        if self.unavailable.load(Ordering::Relaxed) {
            return Err(ModelPolicyAuthorityError::unavailable());
        }
        ModelPolicyAuthoritySnapshot::freeze(
            self.returned_key.clone().unwrap_or_else(|| key.clone()),
            self.base.clone(),
            self.enterprise.clone(),
        )
        .map_err(|_| ModelPolicyAuthorityError::unavailable())
    }
}

fn policy_layer(
    authority_id: &str,
    revision: u64,
    decision: ModelRoutePolicyDecision,
    layer_limits: ModelAdmissionLimits,
) -> ModelAdmissionPolicyLayer {
    ModelAdmissionPolicyLayer::try_new(
        authority_id.to_owned(),
        revision,
        "budget-2030-01".to_owned(),
        decision,
        layer_limits,
    )
    .expect("policy authority layer")
}

#[test]
fn durable_provider_admission_replays_reserve_and_actual_completion_after_restart() {
    let (root, mut state_storage, route_authority) = setup("production-admission-restart");
    let facts = provider_admission_facts(&mut state_storage);
    let policy_authority = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: policy_layer(
            "base-policy-authority",
            31,
            ModelRoutePolicyDecision::Allow,
            limits(),
        ),
        enterprise_ceilings: Vec::new(),
    })
    .expect("production policy authority");
    let reservation = ProviderAdmissionReservationConfig::try_new(100, 10)
        .expect("production reservation config");
    let clock = FixedClock(31_556_200);

    let mut admission = durable_admission(&root, &clock, &policy_authority, reservation);
    assert_eq!(admission.database_path(), state_storage.database_path());
    let first = admission
        .reserve(&ProviderAdmissionOpenRequest {
            identity: &facts.identity,
            settings: &facts.settings,
            capability: &facts.capability,
            credential: &facts.credential,
            message: &facts.message,
        })
        .expect("first durable Provider reservation");
    assert!(first.reservation.admitted());
    assert!(!first.reservation.idempotent_replay);
    assert_eq!(first.route_authority, route_authority);
    assert!(!format!("{admission:?}").contains(root.to_string_lossy().as_ref()));
    admission.close().expect("close admission connection");

    let mut admission = durable_admission(&root, &clock, &policy_authority, reservation);
    let replay = admission
        .reserve(&ProviderAdmissionOpenRequest {
            identity: &facts.identity,
            settings: &facts.settings,
            capability: &facts.capability,
            credential: &facts.credential,
            message: &facts.message,
        })
        .expect("restart reserve replay");
    assert!(replay.reservation.idempotent_replay);
    let before_terminal = ModelAdmissionService::new(&mut state_storage, &clock)
        .snapshot(&route_authority, "budget-2030-01")
        .expect("snapshot one active reservation");
    assert_eq!(before_terminal.active_reservations, 1);

    let actual_usage = ProviderTokenUsage {
        input_tokens: 5,
        cached_input_tokens: 1,
        cache_write_input_tokens: 0,
        output_tokens: 3,
        reasoning_output_tokens: 1,
    };
    let completed = admission
        .complete(
            &replay.route_authority,
            &facts.message.request_id,
            &facts.message.model_exchange_id,
            actual_usage,
            4,
        )
        .expect("complete exact reservation");
    assert_eq!(completed.actual_tokens, 8);
    assert_eq!(completed.actual_cost_micros, 4);
    admission.close().expect("close completed connection");

    let mut admission = durable_admission(&root, &clock, &policy_authority, reservation);
    assert!(
        admission
            .complete(
                &replay.route_authority,
                &facts.message.request_id,
                &facts.message.model_exchange_id,
                actual_usage,
                4,
            )
            .expect("completion replay")
            .idempotent_replay
    );
    let settled = ModelAdmissionService::new(&mut state_storage, &clock)
        .snapshot(&route_authority, "budget-2030-01")
        .expect("settled snapshot");
    assert_eq!(settled.active_reservations, 0);
    assert_eq!(settled.budget_settled_tokens, 8);
    assert_eq!(settled.budget_settled_cost_micros, 4);

    admission.close().expect("close replay connection");
    drop(state_storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn frozen_route_authority_durable_json_rehydrates_only_exact_fingerprint() {
    let (root, storage, authority) = setup("durable-route-authority");
    let encoded = authority
        .to_durable_json()
        .expect("encode frozen authority");
    let restored = FrozenModelRouteAuthority::from_durable_json(&encoded)
        .expect("rehydrate exact frozen authority");
    assert_eq!(restored, authority);
    restored
        .validate_fingerprint()
        .expect("validate restored fingerprint");

    let mut changed: serde_json::Value =
        serde_json::from_slice(&encoded).expect("decode fixture JSON");
    changed["providerVersion"] = serde_json::json!(999);
    assert!(
        FrozenModelRouteAuthority::from_durable_json(
            &serde_json::to_vec(&changed).expect("encode changed authority")
        )
        .is_err()
    );
    changed["unexpected"] = serde_json::json!(true);
    assert!(
        FrozenModelRouteAuthority::from_durable_json(
            &serde_json::to_vec(&changed).expect("encode unknown field")
        )
        .is_err()
    );
    let mut whitespace = encoded.clone();
    whitespace.insert(1, b' ');
    assert!(FrozenModelRouteAuthority::from_durable_json(&whitespace).is_err());
    let reordered = serde_json::to_vec(
        &serde_json::from_slice::<serde_json::Value>(&encoded).expect("decode authority value"),
    )
    .expect("reorder authority fields");
    assert_ne!(reordered, encoded);
    assert!(FrozenModelRouteAuthority::from_durable_json(&reordered).is_err());

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn production_policy_source_intersects_only_auditable_base_and_enterprise_layers() {
    let (root, storage, authority) = setup("production-policy-source");
    let enterprise_limits = ModelAdmissionLimits {
        requests_per_minute: 40,
        tokens_per_minute: 4_000,
        concurrent_requests: 4,
        token_budget: 40_000,
        cost_budget_micros: 40_000,
    };
    let fixture = PolicyAuthorityFixture {
        base: policy_layer(
            "base-policy-authority",
            7,
            ModelRoutePolicyDecision::Allow,
            limits(),
        ),
        enterprise: Some(policy_layer(
            "enterprise-policy-authority",
            11,
            ModelRoutePolicyDecision::Allow,
            enterprise_limits,
        )),
        returned_key: None,
        unavailable: AtomicBool::new(false),
        queries: Mutex::new(Vec::new()),
    };
    let resolution = ProductionModelPolicySource::new(&fixture)
        .resolve(&authority)
        .expect("resolve production policy");
    assert_eq!(resolution.policy().limits(), enterprise_limits);
    assert!(resolution.policy().route_allowed());
    assert_eq!(resolution.policy().budget_period_id(), "budget-2030-01");
    assert_eq!(
        resolution
            .policy()
            .sources()
            .iter()
            .map(|source| (source.authority_id.as_str(), source.revision))
            .collect::<Vec<_>>(),
        [
            ("base-policy-authority", 7),
            ("enterprise-policy-authority", 11),
        ]
    );
    let queries = fixture.queries.lock().expect("policy query lock");
    assert_eq!(queries.as_slice(), [resolution.key().clone()]);
    assert_eq!(
        resolution.key().organization_id(),
        &OrganizationId(id("org", 1))
    );
    assert_eq!(resolution.key().provider_id(), "provider-a");
    assert_eq!(resolution.key().model_id(), "model-a");
    assert_eq!(
        resolution.key().catalog_version(),
        authority.catalog_version()
    );
    assert_eq!(
        resolution.key().provider_version(),
        authority.provider_version()
    );
    assert_eq!(
        resolution.key().credential_rotation_version(),
        authority.credential_rotation_version()
    );
    let debug = format!("{:?}", resolution.key());
    assert!(!debug.contains("psn_"));
    assert!(!debug.contains("usr_"));

    drop(queries);
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn enterprise_deny_snapshot_mismatch_and_unavailability_fail_closed() {
    let (root, mut storage, route_authority) = setup("production-policy-deny");
    configure_session(&mut storage, 2);
    let other_authority = authority(&mut storage, 2);
    let mismatch_key =
        ModelPolicyRouteKey::try_from_authority(&other_authority).expect("other policy key");
    let fixture = PolicyAuthorityFixture {
        base: policy_layer(
            "base-policy-authority",
            7,
            ModelRoutePolicyDecision::Allow,
            limits(),
        ),
        enterprise: Some(policy_layer(
            "enterprise-policy-authority",
            11,
            ModelRoutePolicyDecision::Deny,
            limits(),
        )),
        returned_key: None,
        unavailable: AtomicBool::new(false),
        queries: Mutex::new(Vec::new()),
    };
    let denied = ProductionModelPolicySource::new(&fixture)
        .resolve(&route_authority)
        .expect("resolve enterprise denial");
    assert!(!denied.policy().route_allowed());

    let mismatch = PolicyAuthorityFixture {
        returned_key: Some(mismatch_key),
        ..fixture
    };
    assert_eq!(
        ProductionModelPolicySource::new(&mismatch)
            .resolve(&route_authority)
            .expect_err("another route snapshot is rejected")
            .kind(),
        ModelPolicyResolutionErrorKind::SnapshotMismatch
    );
    mismatch.unavailable.store(true, Ordering::Relaxed);
    assert_eq!(
        ProductionModelPolicySource::new(&mismatch)
            .resolve(&route_authority)
            .expect_err("unavailable authority fails closed")
            .kind(),
        ModelPolicyResolutionErrorKind::AuthorityUnavailable
    );

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn local_production_policy_config_rejects_duplicates_and_freezes_enterprise_ceiling() {
    let (root, storage, route_authority) = setup("local-production-policy");
    let organization_id = OrganizationId(id("org", 1));
    let enterprise_limits = ModelAdmissionLimits {
        requests_per_minute: 20,
        tokens_per_minute: 2_000,
        concurrent_requests: 2,
        token_budget: 20_000,
        cost_budget_micros: 20_000,
    };
    let local = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: policy_layer(
            "base-policy-authority",
            20,
            ModelRoutePolicyDecision::Allow,
            limits(),
        ),
        enterprise_ceilings: vec![
            EnterpriseModelPolicyCeiling::try_new(
                organization_id.clone(),
                policy_layer(
                    "enterprise-policy-authority",
                    21,
                    ModelRoutePolicyDecision::Allow,
                    enterprise_limits,
                ),
            )
            .expect("enterprise ceiling"),
        ],
    })
    .expect("local production policy config");
    let resolution = ProductionModelPolicySource::new(&local)
        .resolve(&route_authority)
        .expect("local production policy");
    assert_eq!(resolution.policy().limits(), enterprise_limits);
    assert_eq!(resolution.policy().sources().len(), 2);

    let duplicate_policy = || {
        policy_layer(
            "enterprise-policy-authority",
            22,
            ModelRoutePolicyDecision::Allow,
            enterprise_limits,
        )
    };
    let duplicate = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: policy_layer(
            "base-policy-authority",
            20,
            ModelRoutePolicyDecision::Allow,
            limits(),
        ),
        enterprise_ceilings: vec![
            EnterpriseModelPolicyCeiling::try_new(organization_id.clone(), duplicate_policy())
                .expect("first duplicate"),
            EnterpriseModelPolicyCeiling::try_new(organization_id, duplicate_policy())
                .expect("second duplicate"),
        ],
    })
    .expect_err("duplicate enterprise organization");
    assert_eq!(
        duplicate.kind(),
        ModelPolicyResolutionErrorKind::InvalidAuthority
    );

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}
