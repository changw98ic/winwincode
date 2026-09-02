// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind,
    RepositoryScope, RepositoryScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CredentialReferenceService, DurableProviderRetrySettlement, FrozenModelRetryPlan,
    FrozenModelRouteAuthority, ModelAttemptCharge, ModelAttemptCompletionCommand,
    ModelAttemptFailureCommand, ModelAttemptFailureFact, ModelAttemptFailureKind,
    ModelAttemptStartCommand, ModelAttemptStartReceipt, ModelCapability, ModelExecutionCertainty,
    ModelReservationReceipt, ModelReservationTerminalOutcome, ModelReservationTerminalReceipt,
    ModelRetryAction, ModelRetrySettlementContext, ModelRetrySettlementContextError,
    ModelRetrySettlementContextPort, ModelRetrySettlementErrorKind, ModelRetrySettlementReceipt,
    ModelRetryStep, ModelRetryUsageErrorKind, ModelRetryUsageRequest, ModelRetryUsageService,
    ModelSettingsProjection, ModelSettingsTarget, ModelToolSupport, ModelUsageAttribution,
    ModelUsageFilter, ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor,
    ProviderEnterpriseUsageErrorKind, ProviderEnterpriseUsageReconciler, ProviderGatewayIdentity,
    ProviderGatewaySettlement, ProviderGatewayTerminalOutcome, ProviderStreamFailureKind,
    ProviderTokenUsage,
};
use winwincode_domain::{
    CredentialReferenceId, DeliveryId, Instant, ModelExchangeId, OrganizationId, ProductSessionId,
    ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    AggregateJournalKey, CommitReceipt, EnterpriseUsageFilter as EnterpriseLedgerFilter,
    LoadedAggregateJournal, OutboxEvent, ProductStateStorage, ProjectionEventCursor,
    ProjectionEventStreamKey, ProjectionReadCut, ReceiptIdentity, SqliteStorage, StorageError,
    StoredState,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-model-retry-usage-{name}-{}-{suffix}",
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

fn route(seed: u64) -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", seed)),
        provider_id: format!("provider-{seed}"),
        model_id: format!("model-{seed}"),
    }
}

fn register_provider(storage: &mut SqliteStorage, seed: u64, expected_version: u64) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", seed)),
                expected_catalog_version: expected_version,
            },
            &ProviderDescriptor {
                provider_id: format!("provider-{seed}"),
                display_name: format!("Provider {seed}"),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", seed)),
                models: vec![ModelCapability {
                    model_id: format!("model-{seed}"),
                    display_name: format!("Model {seed}"),
                    context_window_tokens: 128_000,
                    max_output_tokens: 16_000,
                    tool_support: ModelToolSupport::Parallel,
                    reasoning_efforts: vec!["high".to_owned()],
                }],
            },
            Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("register Provider");
}

fn create_credential(storage: &mut SqliteStorage, seed: u64) {
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", seed)),
                    display_name: format!("Provider {seed} credential"),
                    provider_id: format!("provider-{seed}"),
                    vault_locator: format!("local-fixture://provider-{seed}"),
                },
                request_id: RequestId(id("req", seed + 10)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_000,
        )
        .expect("create Credential reference");
}

fn authority(storage: &mut SqliteStorage, seed: u64) -> FrozenModelRouteAuthority {
    let scope = Scope::OrganizationScope(organization_scope());
    let capability = ProviderCatalogService::new(storage)
        .resolve_model(
            &scope,
            &format!("provider-{seed}"),
            &format!("model-{seed}"),
        )
        .expect("resolve model");
    let credential = CredentialReferenceService::new(storage)
        .resolve(&scope, &CredentialReferenceId(id("crd", seed)))
        .expect("resolve Credential reference");
    let target = ModelSettingsTarget::ProductSession {
        repository_scope: repository_scope(),
        product_session_id: ProductSessionId(id("psn", 1)),
    };
    let projection = ModelSettingsProjection {
        target: target.clone(),
        selection: None,
        default_model_route: Some(route(seed)),
        worker_concurrency_limit: 100,
        revision: seed,
    };
    FrozenModelRouteAuthority::from_resolved_authority(
        &ProviderGatewayIdentity::product_session(
            repository_scope(),
            ProductSessionId(id("psn", 1)),
        ),
        &projection,
        &capability,
        &credential,
    )
    .expect("freeze route authority")
}

struct Fixture {
    root: PathBuf,
    storage: SqliteStorage,
    primary: FrozenModelRouteAuthority,
    fallback: FrozenModelRouteAuthority,
}

struct SnapshotStorage {
    inner: SqliteStorage,
    catalog_loaded: Arc<Barrier>,
    insertion_finished: Arc<Barrier>,
    paused: AtomicBool,
}

impl ProductStateStorage for SnapshotStorage {
    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        self.inner.load_receipt(identity, command_digest)
    }

    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        let state = self.inner.load_state(stream_id)?;
        if stream_id == "model-usage-catalog:v1" && !self.paused.swap(true, Ordering::Relaxed) {
            self.catalog_loaded.wait();
            self.insertion_finished.wait();
        }
        Ok(state)
    }

    fn load_projection_read_cut(
        &self,
        state_stream_ids: &[String],
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionReadCut, StorageError> {
        self.inner
            .load_projection_read_cut(state_stream_ids, key, expected)
    }

    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        self.inner.load_journal(key)
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        self.inner.pending_events()
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        self.inner.mark_published(event_id)
    }

    fn close(self: Box<Self>) -> Result<(), StorageError> {
        Box::new(self.inner).close()
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = temporary_directory(name);
        let mut storage = SqliteStorage::open(&root).expect("open storage");
        register_provider(&mut storage, 1, 0);
        register_provider(&mut storage, 2, 1);
        create_credential(&mut storage, 1);
        create_credential(&mut storage, 2);
        let primary = authority(&mut storage, 1);
        let fallback = authority(&mut storage, 2);
        Self {
            root,
            storage,
            primary,
            fallback,
        }
    }

    fn cleanup(self) {
        drop(self.storage);
        fs::remove_dir_all(self.root).expect("remove fixture");
    }
}

fn plan(
    primary: FrozenModelRouteAuthority,
    primary_attempts: u64,
    fallback: Option<FrozenModelRouteAuthority>,
) -> FrozenModelRetryPlan {
    let mut steps = vec![ModelRetryStep::try_new(primary, primary_attempts).expect("primary step")];
    if let Some(fallback) = fallback {
        steps.push(ModelRetryStep::try_new(fallback, 1).expect("fallback step"));
    }
    FrozenModelRetryPlan::freeze("retry-policy".to_owned(), 7, steps).expect("retry plan")
}

fn request(
    plan: FrozenModelRetryPlan,
    seed: u64,
    delivery_seed: Option<u64>,
) -> ModelRetryUsageRequest {
    ModelRetryUsageRequest {
        request_id: RequestId(id("req", seed)),
        attribution: ModelUsageAttribution::from_request_authority(
            plan.steps()[0].authority(),
            delivery_seed.map(|seed| DeliveryId(id("dlv", seed))),
            &actor(),
        )
        .expect("attribution"),
        plan,
        enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts {
            tokens: 100,
            provider_cost_micros: 10,
            operations: 1,
            ..winwincode_storage::EnterpriseQuotaAmounts::default()
        },
        enterprise_quota_requested_at: Instant("2027-08-01T00:00:00.000Z".to_owned()),
    }
}

fn admission(authority: &FrozenModelRouteAuthority, seed: u64) -> ModelReservationReceipt {
    ModelReservationReceipt {
        request_id: RequestId(id("req", seed + 100)),
        model_exchange_id: ModelExchangeId(id("mdl", seed)),
        route_authority_fingerprint: authority.fingerprint().to_owned(),
        denial: None,
        unix_minute: 1,
        revision: 1,
        idempotent_replay: false,
    }
}

fn gateway(
    authority: &FrozenModelRouteAuthority,
    exchange_seed: u64,
    outcome: ModelReservationTerminalOutcome,
    failure: Option<ModelAttemptFailureFact>,
    charge: Option<ModelAttemptCharge>,
) -> ProviderGatewaySettlement {
    let (actual_tokens, actual_cost_micros) = charge.as_ref().map_or((0, 0), |charge| {
        (
            charge.usage.input_tokens + charge.usage.output_tokens,
            charge.cost_micros,
        )
    });
    ProviderGatewaySettlement {
        model_exchange_id: ModelExchangeId(id("mdl", exchange_seed)),
        request_id: RequestId(id("req", exchange_seed + 100)),
        provider_id: authority.route().provider_id.clone(),
        model_id: authority.route().model_id.clone(),
        adapter_request_id: format!("adapter-{exchange_seed}"),
        settled_at: Instant(format!("2027-02-15T08:00:{:02}.000Z", exchange_seed % 60)),
        outcome: match outcome {
            ModelReservationTerminalOutcome::Completed if failure.is_none() => {
                ProviderGatewayTerminalOutcome::Succeeded
            }
            ModelReservationTerminalOutcome::Cancelled => ProviderGatewayTerminalOutcome::Cancelled,
            ModelReservationTerminalOutcome::ProviderFailed
            | ModelReservationTerminalOutcome::Completed => ProviderGatewayTerminalOutcome::Failed,
        },
        admission_terminal: ModelReservationTerminalReceipt {
            request_id: RequestId(id("req", exchange_seed + 100)),
            model_exchange_id: ModelExchangeId(id("mdl", exchange_seed)),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome,
            actual_tokens,
            actual_cost_micros,
            revision: 2,
            idempotent_replay: false,
        },
        failure,
        charge,
    }
}

fn charge(
    seed: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_micros: u64,
) -> ModelAttemptCharge {
    ModelAttemptCharge {
        provider_usage_id: format!("provider-usage-{seed}"),
        usage: ProviderTokenUsage {
            input_tokens,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens,
            reasoning_output_tokens: 0,
        },
        cost_micros,
    }
}

fn start(
    storage: &mut SqliteStorage,
    request: &ModelRetryUsageRequest,
    authority: &FrozenModelRouteAuthority,
    exchange_seed: u64,
    command_seed: u64,
) -> ModelAttemptStartReceipt {
    let receipt = ModelRetryUsageService::new(storage)
        .start_attempt(
            request,
            &ModelAttemptStartCommand {
                command_request_id: RequestId(id("req", command_seed)),
                admission: admission(authority, exchange_seed),
            },
        )
        .expect("start attempt");
    assert_eq!(
        receipt.model_exchange_id,
        ModelExchangeId(id("mdl", exchange_seed))
    );
    assert_eq!(receipt.route_fingerprint, authority.fingerprint());
    receipt
}

#[test]
fn safe_transient_failure_falls_back_but_ambiguous_failure_stops() {
    let mut fixture = Fixture::new("fallback-safety");
    let request = request(
        plan(fixture.primary.clone(), 1, Some(fixture.fallback.clone())),
        500,
        Some(1),
    );
    start(&mut fixture.storage, &request, &fixture.primary, 1, 501);

    let first = ModelRetryUsageService::new(&mut fixture.storage)
        .fail_attempt(
            &request,
            &ModelAttemptFailureCommand {
                command_request_id: RequestId(id("req", 502)),
                gateway: gateway(
                    &fixture.primary,
                    1,
                    ModelReservationTerminalOutcome::ProviderFailed,
                    Some(ModelAttemptFailureFact::from_stream(
                        ProviderStreamFailureKind::RateLimit,
                        ModelExecutionCertainty::RejectedBeforeAcceptance,
                    )),
                    None,
                ),
            },
        )
        .expect("record safe failure");
    assert_eq!(first.action, ModelRetryAction::Fallback);
    assert_eq!(first.next_provider_id.as_deref(), Some("provider-2"));

    start(&mut fixture.storage, &request, &fixture.fallback, 2, 504);
    let stopped = ModelRetryUsageService::new(&mut fixture.storage)
        .fail_attempt(
            &request,
            &ModelAttemptFailureCommand {
                command_request_id: RequestId(id("req", 505)),
                gateway: gateway(
                    &fixture.fallback,
                    2,
                    ModelReservationTerminalOutcome::ProviderFailed,
                    Some(ModelAttemptFailureFact::from_stream(
                        ProviderStreamFailureKind::Timeout,
                        ModelExecutionCertainty::AcceptanceUnknown,
                    )),
                    None,
                ),
            },
        )
        .expect("record ambiguous failure");
    assert_eq!(stopped.action, ModelRetryAction::Stop);

    let error = ModelRetryUsageService::new(&mut fixture.storage)
        .start_attempt(
            &request,
            &ModelAttemptStartCommand {
                command_request_id: RequestId(id("req", 507)),
                admission: admission(&fixture.fallback, 3),
            },
        )
        .expect_err("terminal request cannot retry");
    assert_eq!(error.kind(), ModelRetryUsageErrorKind::InvalidState);
    fixture.cleanup();
}

#[test]
fn admitted_receipt_is_bound_to_the_exact_planned_route_before_any_write() {
    let mut fixture = Fixture::new("swapped-route");
    let request = request(plan(fixture.primary.clone(), 1, None), 600, None);
    let before = fixture
        .storage
        .pending_events()
        .expect("events before")
        .len();
    let error = ModelRetryUsageService::new(&mut fixture.storage)
        .start_attempt(
            &request,
            &ModelAttemptStartCommand {
                command_request_id: RequestId(id("req", 601)),
                admission: admission(&fixture.fallback, 1),
            },
        )
        .expect_err("swapped route receipt is rejected");
    assert_eq!(error.kind(), ModelRetryUsageErrorKind::IdentityMismatch);
    assert_eq!(
        fixture
            .storage
            .pending_events()
            .expect("events after")
            .len(),
        before
    );
    assert!(
        fixture
            .storage
            .last_state_stream_id("model-retry-usage:")
            .expect("request state lookup")
            .is_none()
    );
    fixture.cleanup();
}

#[test]
fn same_route_retry_and_duplicate_terminal_receipt_are_deterministic() {
    let mut fixture = Fixture::new("same-route-replay");
    let request = request(plan(fixture.primary.clone(), 2, None), 700, None);
    start(&mut fixture.storage, &request, &fixture.primary, 1, 701);
    let command = ModelAttemptFailureCommand {
        command_request_id: RequestId(id("req", 702)),
        gateway: gateway(
            &fixture.primary,
            1,
            ModelReservationTerminalOutcome::ProviderFailed,
            Some(ModelAttemptFailureFact {
                kind: ModelAttemptFailureKind::Transport,
                certainty: ModelExecutionCertainty::NotSent,
            }),
            None,
        ),
    };
    let first = ModelRetryUsageService::new(&mut fixture.storage)
        .fail_attempt(&request, &command)
        .expect("first terminal");
    let replay = ModelRetryUsageService::new(&mut fixture.storage)
        .fail_attempt(&request, &command)
        .expect("terminal replay");
    assert_eq!(first.action, ModelRetryAction::RetrySameRoute);
    assert!(!first.idempotent_replay);
    assert!(replay.idempotent_replay);
    assert_eq!(first.revision, replay.revision);
    let mut changed = command.clone();
    changed.gateway.failure.as_mut().expect("failure fact").kind = ModelAttemptFailureKind::Timeout;
    assert_eq!(
        ModelRetryUsageService::new(&mut fixture.storage)
            .fail_attempt(&request, &changed)
            .expect_err("changed terminal command conflicts")
            .kind(),
        ModelRetryUsageErrorKind::RequestConflict
    );
    fixture.cleanup();
}

fn record_fallback_and_charged_failure(fixture: &mut Fixture) {
    let main_request = request(
        plan(fixture.primary.clone(), 1, Some(fixture.fallback.clone())),
        800,
        Some(8),
    );
    start(
        &mut fixture.storage,
        &main_request,
        &fixture.primary,
        1,
        801,
    );
    let failed = ModelAttemptFailureCommand {
        command_request_id: RequestId(id("req", 802)),
        gateway: gateway(
            &fixture.primary,
            1,
            ModelReservationTerminalOutcome::ProviderFailed,
            Some(ModelAttemptFailureFact {
                kind: ModelAttemptFailureKind::RateLimit,
                certainty: ModelExecutionCertainty::RejectedBeforeAcceptance,
            }),
            None,
        ),
    };
    assert_eq!(
        ModelRetryUsageService::new(&mut fixture.storage)
            .fail_attempt(&main_request, &failed)
            .expect("safe failure")
            .action,
        ModelRetryAction::Fallback
    );
    start(
        &mut fixture.storage,
        &main_request,
        &fixture.fallback,
        2,
        804,
    );
    let successful_charge = charge(2, 20, 3, 230);
    let completed = ModelAttemptCompletionCommand {
        command_request_id: RequestId(id("req", 805)),
        gateway: gateway(
            &fixture.fallback,
            2,
            ModelReservationTerminalOutcome::Completed,
            None,
            Some(successful_charge),
        ),
    };
    let first = ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(&main_request, &completed)
        .expect("settle success");
    let replay = ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(&main_request, &completed)
        .expect("settlement replay");
    assert!(!first.idempotent_replay);
    assert!(replay.idempotent_replay);

    let charged_failure_request = request(plan(fixture.primary.clone(), 1, None), 810, Some(8));
    start(
        &mut fixture.storage,
        &charged_failure_request,
        &fixture.primary,
        3,
        811,
    );
    let failed_charge = charge(1, 10, 2, 120);
    let stopped = ModelRetryUsageService::new(&mut fixture.storage)
        .fail_attempt(
            &charged_failure_request,
            &ModelAttemptFailureCommand {
                command_request_id: RequestId(id("req", 812)),
                gateway: gateway(
                    &fixture.primary,
                    3,
                    ModelReservationTerminalOutcome::Completed,
                    Some(ModelAttemptFailureFact {
                        kind: ModelAttemptFailureKind::Timeout,
                        certainty: ModelExecutionCertainty::OutputObserved,
                    }),
                    Some(failed_charge),
                ),
            },
        )
        .expect("charged ambiguous failure");
    assert_eq!(stopped.action, ModelRetryAction::Stop);
}

#[test]
fn fallback_and_charged_failures_reconcile_once_across_every_dimension_after_restart() {
    let mut fixture = Fixture::new("reconcile-restart");
    record_fallback_and_charged_failure(&mut fixture);

    drop(fixture.storage);
    let mut storage = SqliteStorage::open(&fixture.root).expect("restart storage");
    let filters = [
        ModelUsageFilter {
            organization_id: Some(OrganizationId(id("org", 1))),
            ..ModelUsageFilter::default()
        },
        ModelUsageFilter {
            workspace_id: Some(WorkspaceId(id("wsp", 1))),
            ..ModelUsageFilter::default()
        },
        ModelUsageFilter {
            project_id: Some(ProjectId(id("prj", 1))),
            ..ModelUsageFilter::default()
        },
        ModelUsageFilter {
            repository_id: Some(RepositoryId(id("rep", 1))),
            ..ModelUsageFilter::default()
        },
        ModelUsageFilter {
            product_session_id: Some(ProductSessionId(id("psn", 1))),
            ..ModelUsageFilter::default()
        },
        ModelUsageFilter {
            delivery_id: Some(DeliveryId(id("dlv", 8))),
            ..ModelUsageFilter::default()
        },
        ModelUsageFilter {
            user_id: Some(UserId(id("usr", 1))),
            ..ModelUsageFilter::default()
        },
    ];
    for filter in filters {
        let totals = ModelRetryUsageService::new(&mut storage)
            .reconcile(&filter)
            .expect("reconcile dimension")
            .totals;
        assert_eq!(totals.entries, 2);
        assert_eq!(totals.total_tokens, 35);
        assert_eq!(totals.cost_micros, 350);
    }
    let provider = ModelRetryUsageService::new(&mut storage)
        .reconcile(&ModelUsageFilter {
            provider_id: Some("provider-2".to_owned()),
            ..ModelUsageFilter::default()
        })
        .expect("reconcile Provider");
    assert_eq!(provider.totals.entries, 1);
    assert_eq!(provider.totals.total_tokens, 23);
    assert_eq!(provider.by_provider.len(), 1);

    drop(storage);
    fs::remove_dir_all(fixture.root).expect("remove fixture");
}

#[test]
fn settled_source_catalog_pages_and_replays_exact_frozen_authority_after_restart() {
    let mut fixture = Fixture::new("source-catalog");
    record_fallback_and_charged_failure(&mut fixture);
    let filter = ModelUsageFilter::default();
    let first = ModelRetryUsageService::new(&mut fixture.storage)
        .scan_usage_sources(&filter, None, 1)
        .expect("first source page");
    assert_eq!(first.snapshot_sequence, 2);
    assert_eq!(first.entries.len(), 1);
    let first_entry = first.entries[0].clone();
    assert_eq!(first_entry.usage.provider_usage_id, "provider-usage-2");
    assert_eq!(first_entry.model_exchange_id, ModelExchangeId(id("mdl", 2)));
    assert_eq!(
        first_entry.route_authority_fingerprint,
        fixture.fallback.fingerprint()
    );
    assert_eq!(
        first_entry.settled_at,
        Instant("2027-02-15T08:00:02.000Z".to_owned())
    );
    let loaded = ModelRetryUsageService::new(&mut fixture.storage)
        .usage_source("provider-usage-2")
        .expect("load source")
        .expect("source exists");
    assert_eq!(loaded, first_entry);
    let cursor = first.next.expect("second source page");
    let bytes = serde_json::to_vec(&first_entry).expect("canonical source bytes");
    let changed_filter = ModelUsageFilter {
        user_id: Some(UserId(id("usr", 2))),
        ..ModelUsageFilter::default()
    };
    assert_eq!(
        ModelRetryUsageService::new(&mut fixture.storage)
            .scan_usage_sources(&changed_filter, Some(&cursor), 1)
            .expect_err("cursor is bound to its frozen filter")
            .kind(),
        ModelRetryUsageErrorKind::InvalidRequest
    );
    let later_request = request(plan(fixture.primary.clone(), 1, None), 820, Some(8));
    start(
        &mut fixture.storage,
        &later_request,
        &fixture.primary,
        4,
        821,
    );
    ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(
            &later_request,
            &ModelAttemptCompletionCommand {
                command_request_id: RequestId(id("req", 822)),
                gateway: gateway(
                    &fixture.primary,
                    4,
                    ModelReservationTerminalOutcome::Completed,
                    None,
                    Some(charge(3, 4, 1, 50)),
                ),
            },
        )
        .expect("later source settlement");

    drop(fixture.storage);
    let mut restarted = SqliteStorage::open(&fixture.root).expect("restart source storage");
    let replay = ModelRetryUsageService::new(&mut restarted)
        .usage_source("provider-usage-2")
        .expect("load source after restart")
        .expect("source after restart");
    assert_eq!(serde_json::to_vec(&replay).expect("restart bytes"), bytes);
    let second = ModelRetryUsageService::new(&mut restarted)
        .scan_usage_sources(&filter, Some(&cursor), 1)
        .expect("second source page");
    assert_eq!(second.snapshot_sequence, 2);
    assert_eq!(second.entries.len(), 1);
    assert_eq!(
        second.entries[0].usage.provider_usage_id,
        "provider-usage-1"
    );
    assert!(second.next.is_none());
    assert_eq!(
        ModelRetryUsageService::new(&mut restarted)
            .scan_usage_sources(&filter, None, 10)
            .expect("new source snapshot")
            .snapshot_sequence,
        3
    );
    drop(restarted);
    fs::remove_dir_all(fixture.root).expect("remove fixture");
}

#[test]
fn enterprise_projection_recovers_a_post_settlement_crash_gap_exactly_once() {
    let mut fixture = Fixture::new("enterprise-recovery");
    let request = request(plan(fixture.primary.clone(), 1, None), 880, Some(8));
    start(&mut fixture.storage, &request, &fixture.primary, 1, 881);
    ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(
            &request,
            &ModelAttemptCompletionCommand {
                command_request_id: RequestId(id("req", 882)),
                gateway: gateway(
                    &fixture.primary,
                    1,
                    ModelReservationTerminalOutcome::Completed,
                    None,
                    Some(charge(88, 10, 2, 120)),
                ),
            },
        )
        .expect("source settlement");
    fixture
        .storage
        .enterprise_usage_ledger()
        .expect("prepare enterprise schema");
    let database = fixture.root.join("control-plane.sqlite3");
    let fault = rusqlite::Connection::open(&database).expect("fault connection");
    fault
        .execute_batch(
            "CREATE TRIGGER fail_enterprise_projection
             BEFORE INSERT ON enterprise_usage_entries
             BEGIN SELECT RAISE(ABORT, 'injected projection crash'); END;",
        )
        .expect("install fault");
    let error = ProviderEnterpriseUsageReconciler::new(&mut fixture.storage)
        .reconcile_provider_page(None, 10)
        .expect_err("projection insert fails");
    assert_eq!(error.kind(), ProviderEnterpriseUsageErrorKind::Ledger);
    assert_eq!(
        fixture
            .storage
            .enterprise_usage_ledger()
            .expect("ledger after fault")
            .reconcile(&EnterpriseLedgerFilter::default())
            .expect("zero enterprise totals")
            .entries,
        0
    );
    fault
        .execute_batch("DROP TRIGGER fail_enterprise_projection;")
        .expect("remove fault");
    drop(fault);
    drop(fixture.storage);

    let mut restarted = SqliteStorage::open(&fixture.root).expect("restart storage");
    let applied = ProviderEnterpriseUsageReconciler::new(&mut restarted)
        .reconcile_provider_page(None, 10)
        .expect("rebuild projection");
    assert_eq!((applied.inserted_entries, applied.replayed_entries), (1, 0));
    let replay = ProviderEnterpriseUsageReconciler::new(&mut restarted)
        .reconcile_provider_page(None, 10)
        .expect("exact projection replay");
    assert_eq!((replay.inserted_entries, replay.replayed_entries), (0, 1));
    assert_eq!(
        restarted
            .enterprise_usage_ledger()
            .expect("rebuilt ledger")
            .reconcile(&EnterpriseLedgerFilter::default())
            .expect("rebuilt totals")
            .entries,
        1
    );
    drop(restarted);
    fs::remove_dir_all(fixture.root).expect("remove fixture");
}

#[test]
fn duplicate_provider_usage_id_rolls_back_the_second_request() {
    let mut fixture = Fixture::new("duplicate-usage");
    let plan = plan(fixture.primary.clone(), 1, None);
    let first_request = request(plan.clone(), 900, None);
    let second_request = request(plan, 901, None);
    start(
        &mut fixture.storage,
        &first_request,
        &fixture.primary,
        1,
        902,
    );
    start(
        &mut fixture.storage,
        &second_request,
        &fixture.primary,
        2,
        903,
    );

    let first_charge = charge(9, 5, 1, 60);
    ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(
            &first_request,
            &ModelAttemptCompletionCommand {
                command_request_id: RequestId(id("req", 904)),
                gateway: gateway(
                    &fixture.primary,
                    1,
                    ModelReservationTerminalOutcome::Completed,
                    None,
                    Some(first_charge),
                ),
            },
        )
        .expect("first usage");

    let duplicate_charge = charge(9, 7, 2, 90);
    let error = ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(
            &second_request,
            &ModelAttemptCompletionCommand {
                command_request_id: RequestId(id("req", 906)),
                gateway: gateway(
                    &fixture.primary,
                    2,
                    ModelReservationTerminalOutcome::Completed,
                    None,
                    Some(duplicate_charge),
                ),
            },
        )
        .expect_err("duplicate Provider Usage identity");
    assert_eq!(error.kind(), ModelRetryUsageErrorKind::UsageConflict);

    let totals = ModelRetryUsageService::new(&mut fixture.storage)
        .reconcile(&ModelUsageFilter::default())
        .expect("reconcile after conflict")
        .totals;
    assert_eq!(totals.entries, 1);
    assert_eq!(totals.total_tokens, 6);

    let replacement = charge(10, 7, 2, 90);
    ModelRetryUsageService::new(&mut fixture.storage)
        .complete_attempt(
            &second_request,
            &ModelAttemptCompletionCommand {
                command_request_id: RequestId(id("req", 908)),
                gateway: gateway(
                    &fixture.primary,
                    2,
                    ModelReservationTerminalOutcome::Completed,
                    None,
                    Some(replacement),
                ),
            },
        )
        .expect("second request remained active");
    fixture.cleanup();
}

#[test]
fn reconciliation_uses_one_catalog_snapshot_while_another_usage_is_inserted() {
    let fixture = Fixture::new("catalog-snapshot");
    let root = fixture.root.clone();
    let authority = fixture.primary.clone();
    let retry_plan = plan(authority.clone(), 1, None);
    let first_request = request(retry_plan.clone(), 950, None);
    let second_request = request(retry_plan, 951, None);
    let mut storage = fixture.storage;
    start(&mut storage, &first_request, &authority, 1, 952);
    let first_usage = charge(95, 5, 1, 60);
    ModelRetryUsageService::new(&mut storage)
        .complete_attempt(
            &first_request,
            &ModelAttemptCompletionCommand {
                command_request_id: RequestId(id("req", 953)),
                gateway: gateway(
                    &authority,
                    1,
                    ModelReservationTerminalOutcome::Completed,
                    None,
                    Some(first_usage),
                ),
            },
        )
        .expect("first Usage");
    start(&mut storage, &second_request, &authority, 2, 955);
    drop(storage);

    let second_usage = charge(96, 7, 2, 90);
    let second_command = ModelAttemptCompletionCommand {
        command_request_id: RequestId(id("req", 956)),
        gateway: gateway(
            &authority,
            2,
            ModelReservationTerminalOutcome::Completed,
            None,
            Some(second_usage),
        ),
    };
    let catalog_loaded = Arc::new(Barrier::new(2));
    let insertion_finished = Arc::new(Barrier::new(2));
    let insert_handle = {
        let root = root.clone();
        let second_request = second_request.clone();
        let catalog_loaded = Arc::clone(&catalog_loaded);
        let insertion_finished = Arc::clone(&insertion_finished);
        thread::spawn(move || {
            catalog_loaded.wait();
            let mut storage = SqliteStorage::open(&root).expect("open insertion storage");
            ModelRetryUsageService::new(&mut storage)
                .complete_attempt(&second_request, &second_command)
                .expect("insert concurrent Usage");
            insertion_finished.wait();
        })
    };
    let mut snapshot_storage = SnapshotStorage {
        inner: SqliteStorage::open(&root).expect("open snapshot storage"),
        catalog_loaded,
        insertion_finished,
        paused: AtomicBool::new(false),
    };
    let snapshot = ModelRetryUsageService::new(&mut snapshot_storage)
        .reconcile(&ModelUsageFilter::default())
        .expect("stable catalog snapshot");
    assert_eq!(snapshot.totals.entries, 1);
    assert_eq!(snapshot.totals.total_tokens, 6);
    insert_handle.join().expect("join insertion");
    drop(snapshot_storage);

    let mut reopened = SqliteStorage::open(&root).expect("open later snapshot");
    let later = ModelRetryUsageService::new(&mut reopened)
        .reconcile(&ModelUsageFilter::default())
        .expect("later catalog snapshot");
    assert_eq!(later.totals.entries, 2);
    assert_eq!(later.totals.total_tokens, 15);
    drop(reopened);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn concurrent_exact_completion_has_one_usage_fact_and_one_replay() {
    let fixture = Fixture::new("concurrent-terminal");
    let root = fixture.root.clone();
    let authority = fixture.primary.clone();
    let mut setup_storage = fixture.storage;
    for round in 0..10 {
        verify_concurrent_exact_completion(
            &root,
            &mut setup_storage,
            &authority,
            1_000 + round * 10,
        );
    }
    drop(setup_storage);

    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    let totals = ModelRetryUsageService::new(&mut storage)
        .reconcile(&ModelUsageFilter::default())
        .expect("reconcile")
        .totals;
    assert_eq!(totals.entries, 10);
    assert_eq!(totals.input_tokens, 90);
    assert_eq!(totals.output_tokens, 10);
    assert_eq!(totals.total_tokens, 100);
    assert_eq!(totals.cost_micros, 1_000);
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

fn verify_concurrent_exact_completion(
    root: &PathBuf,
    setup_storage: &mut SqliteStorage,
    authority: &FrozenModelRouteAuthority,
    seed: u64,
) {
    let request = request(plan(authority.clone(), 1, None), seed, None);
    start(setup_storage, &request, authority, seed, seed + 1);

    let usage = charge(seed, 9, 1, 100);
    let command = ModelAttemptCompletionCommand {
        command_request_id: RequestId(id("req", seed + 2)),
        gateway: gateway(
            authority,
            seed,
            ModelReservationTerminalOutcome::Completed,
            None,
            Some(usage),
        ),
    };
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let request = request.clone();
            let command = command.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("open concurrent storage");
                barrier.wait();
                ModelRetryUsageService::new(&mut storage).complete_attempt(&request, &command)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("join completion").expect("complete"))
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| receipt.idempotent_replay)
            .count(),
        1
    );
    assert_eq!(receipts[0].revision, receipts[1].revision);
    assert_eq!(receipts[0].usage, receipts[1].usage);

    let mut changed = command.clone();
    let changed_charge = changed.gateway.charge.as_mut().expect("completion charge");
    changed_charge.cost_micros += 1;
    changed.gateway.admission_terminal.actual_cost_micros += 1;
    let mut storage = SqliteStorage::open(root).expect("restart storage");
    assert_eq!(
        ModelRetryUsageService::new(&mut storage)
            .complete_attempt(&request, &changed)
            .expect_err("changed completion conflicts")
            .kind(),
        ModelRetryUsageErrorKind::RequestConflict
    );
    drop(storage);
}

#[derive(Clone)]
struct FixedSettlementContext {
    result: Result<Option<ModelRetrySettlementContext>, ModelRetrySettlementContextError>,
}

impl ModelRetrySettlementContextPort for FixedSettlementContext {
    fn load_context(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelRetrySettlementContext>, ModelRetrySettlementContextError> {
        self.result.clone().map(|context| {
            context
                .filter(|context| context.start_receipt().model_exchange_id == *model_exchange_id)
        })
    }
}

#[test]
fn production_settlement_rehydrates_exact_context_and_records_usage_once_after_restart() {
    let fixture = Fixture::new("production-settlement");
    let request = request(plan(fixture.primary.clone(), 1, None), 1_100, Some(11));
    let root = fixture.root.clone();
    let authority = fixture.primary.clone();
    let mut setup = fixture.storage;
    let started = start(&mut setup, &request, &authority, 1, 1_101);
    let context = ModelRetrySettlementContext::try_new(request.clone(), started)
        .expect("freeze exact retry context");
    let encoded = context.encode_json().expect("encode context");
    let encoded_text = std::str::from_utf8(&encoded).expect("context UTF-8");
    for exact_id in [id("wsp", 1), id("rep", 1), id("usr", 1)] {
        assert!(encoded_text.contains(&exact_id));
    }
    let decoded = ModelRetrySettlementContext::decode_json(&encoded).expect("decode context");
    assert_eq!(decoded.encode_json().expect("re-encode context"), encoded);
    assert_eq!(decoded.request_fingerprint(), context.request_fingerprint());
    assert_eq!(decoded.context_fingerprint(), context.context_fingerprint());
    let mut corrupted = encoded.clone();
    let offset = corrupted
        .windows("provider-1".len())
        .position(|window| window == b"provider-1")
        .expect("Provider identity in authority JSON");
    corrupted[offset + "provider-".len()] = b'9';
    assert_eq!(
        ModelRetrySettlementContext::decode_json(&corrupted)
            .expect_err("changed authority is rejected")
            .kind(),
        ModelRetrySettlementErrorKind::CorruptContext
    );
    let mut changed_user = encoded.clone();
    let user_offset = changed_user
        .windows(id("usr", 1).len())
        .position(|window| window == id("usr", 1).as_bytes())
        .expect("User identity in attribution");
    changed_user[user_offset + id("usr", 1).len() - 1] = b'2';
    assert_eq!(
        ModelRetrySettlementContext::decode_json(&changed_user)
            .expect_err("changed original User is rejected")
            .kind(),
        ModelRetrySettlementErrorKind::CorruptContext
    );
    drop(setup);

    let contexts = FixedSettlementContext {
        result: Ok(Some(decoded)),
    };
    let settlement = gateway(
        &authority,
        1,
        ModelReservationTerminalOutcome::Completed,
        None,
        Some(charge(110, 13, 2, 150)),
    );
    let adapter = DurableProviderRetrySettlement::open(&root, &contexts)
        .expect("open durable settlement adapter");
    let first = adapter.apply(&settlement).expect("settle Usage");
    let replay = adapter.apply(&settlement).expect("replay settlement");
    assert!(matches!(first, ModelRetrySettlementReceipt::Completed(_)));
    assert!(!first.idempotent_replay());
    assert!(replay.idempotent_replay());
    drop(adapter);

    let restarted = DurableProviderRetrySettlement::open(&root, &contexts)
        .expect("restart durable settlement adapter");
    assert!(
        restarted
            .apply(&settlement)
            .expect("replay after restart")
            .idempotent_replay()
    );
    drop(restarted);
    let mut storage = SqliteStorage::open(&root).expect("open reconciliation storage");
    let totals = ModelRetryUsageService::new(&mut storage)
        .reconcile(&ModelUsageFilter::default())
        .expect("reconcile exact settlement")
        .totals;
    assert_eq!(totals.entries, 1);
    assert_eq!(totals.total_tokens, 15);
    assert_eq!(totals.cost_micros, 150);
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn missing_corrupt_or_changed_settlement_context_fails_closed_without_usage_write() {
    let fixture = Fixture::new("settlement-fail-closed");
    let request = request(plan(fixture.primary.clone(), 1, None), 1_200, None);
    let root = fixture.root.clone();
    let authority = fixture.primary.clone();
    let mut setup = fixture.storage;
    let started = start(&mut setup, &request, &authority, 1, 1_201);
    let context = ModelRetrySettlementContext::try_new(request.clone(), started.clone())
        .expect("valid frozen context");
    let before_events = setup
        .pending_events()
        .expect("events before failures")
        .len();

    let mut wrong_attempt = started.clone();
    wrong_attempt.attempt = 2;
    assert_eq!(
        ModelRetrySettlementContext::try_new(request.clone(), wrong_attempt)
            .expect_err("changed attempt")
            .kind(),
        ModelRetrySettlementErrorKind::IdentityMismatch
    );
    let mut wrong_route = started;
    wrong_route.provider_id = "provider-9".to_owned();
    assert_eq!(
        ModelRetrySettlementContext::try_new(request, wrong_route)
            .expect_err("changed route")
            .kind(),
        ModelRetrySettlementErrorKind::IdentityMismatch
    );
    drop(setup);

    let valid = gateway(
        &authority,
        1,
        ModelReservationTerminalOutcome::Completed,
        None,
        Some(charge(120, 4, 1, 50)),
    );
    let missing = FixedSettlementContext { result: Ok(None) };
    let missing_adapter = DurableProviderRetrySettlement::open(&root, &missing)
        .expect("open missing-context adapter");
    assert_eq!(
        missing_adapter
            .apply(&valid)
            .expect_err("missing context")
            .kind(),
        ModelRetrySettlementErrorKind::MissingContext
    );
    drop(missing_adapter);

    let corrupt = FixedSettlementContext {
        result: Err(ModelRetrySettlementContextError::corrupt()),
    };
    let corrupt_adapter = DurableProviderRetrySettlement::open(&root, &corrupt)
        .expect("open corrupt-context adapter");
    assert_eq!(
        corrupt_adapter
            .apply(&valid)
            .expect_err("corrupt context")
            .kind(),
        ModelRetrySettlementErrorKind::CorruptContext
    );
    drop(corrupt_adapter);

    let exact = FixedSettlementContext {
        result: Ok(Some(context)),
    };
    let exact_adapter =
        DurableProviderRetrySettlement::open(&root, &exact).expect("open exact-context adapter");
    let mut changed = valid;
    changed.provider_id = "provider-9".to_owned();
    assert_eq!(
        exact_adapter
            .apply(&changed)
            .expect_err("changed terminal route")
            .kind(),
        ModelRetrySettlementErrorKind::IdentityMismatch
    );
    drop(exact_adapter);

    let mut storage = SqliteStorage::open(&root).expect("reopen storage");
    assert_eq!(
        storage
            .pending_events()
            .expect("events after failures")
            .len(),
        before_events
    );
    assert_eq!(
        ModelRetryUsageService::new(&mut storage)
            .reconcile(&ModelUsageFilter::default())
            .expect("reconcile after failures")
            .totals
            .entries,
        0
    );
    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}
