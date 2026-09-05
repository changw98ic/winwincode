// SPDX-License-Identifier: Apache-2.0

//! The FLOW-100.3 `ProductSession` continue permission gate over the real
//! composed Server application: a session bound to device execution through
//! its durable launch anchor continues only for the current occupancy holder
//! (`ACCESS_DENIED` otherwise) while the client stays occupied or draining
//! (`OCCUPANCY_REQUIRED` otherwise) and the repository binding stays visible
//! under the plan 13.4 dual-authorization projection (`BINDING_NOT_VISIBLE`
//! otherwise). A session without a device anchor is not gated, so pure
//! supervised local execution is unchanged.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use winwincode_api::generated::{
    Actor, CommandRequest, OrganizationScope, OrganizationScopeKind, Scope, UserActor,
    UserActorKind,
};
use winwincode_control_plane::device_session_gate::DeviceSessionGateDenialKind;
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DurableWorkerInteractionOutbound, EventPublishError,
    EventPublisher, OutboxEvent, ProductSessionExecutionConfig, WorkerLaunchGrantService,
};
use winwincode_domain::{Instant, UserId};
use winwincode_server::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    DurableEventHubConfig, StandaloneApplicationClock, StandaloneControlPlaneApplication,
    TypedControlPlaneApiPort,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, LaunchGrantIssuance, OccupancyClaim, OccupancyLeaseState,
    RepositoryAccessGrantIssuance, RepositoryAvailability, RepositoryBindingProjection,
    RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage, WorkerOutboundQueueConfig,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct FixedClock;

impl StandaloneApplicationClock for FixedClock {
    fn now_millis(&self) -> u64 {
        1_800_000_000_000
    }

    fn now_instant(&self) -> Instant {
        Instant("2027-01-15T08:00:00.000Z".to_owned())
    }
}

fn temporary_root(label: &str) -> PathBuf {
    let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-server-device-session-gate-{label}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn suffix(seed: u64) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut identity = String::with_capacity(26);
    let mut value = seed;
    for _ in 0..26 {
        let digit = usize::try_from(value % 32).expect("digit fits");
        identity.push(ALPHABET[digit] as char);
        value /= 32;
    }
    identity
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn repository_scope_json(seed: u64) -> serde_json::Value {
    serde_json::json!({
        "kind": "repository",
        "organizationId": id("org", seed),
        "workspaceId": id("wsp", seed),
        "projectId": id("prj", seed),
        "repositoryId": id("rep", seed)
    })
}

fn actor(user: &str) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(user.to_owned()),
        kind: UserActorKind::User,
    })
}

fn principal(user: &str) -> AuthenticatedPrincipal {
    let scopes = vec![
        Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: winwincode_domain::OrganizationId(id("org", 1)),
        }),
        serde_json::from_value::<Scope>(repository_scope_json(1)).expect("repository scope"),
    ];
    AuthenticatedPrincipal::new(actor(user), scopes).expect("principal")
}

fn compose_application(root: &Path) -> StandaloneControlPlaneApplication {
    let hub = Arc::new(
        DurableEventHub::open(root.join("events"), DurableEventHubConfig::default())
            .expect("open event hub"),
    );
    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .expect("open Control Plane");
    let storage = SqliteStorage::open(root).expect("open application storage");
    let worker_outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(root).expect("open Worker outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("open Worker outbound adapter");
    let repository_scope =
        serde_json::from_value(repository_scope_json(1)).expect("repository scope");
    let execution = ProductSessionExecutionConfig::try_new(
        repository_scope,
        "fixture-checkout-revision",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config");
    StandaloneControlPlaneApplication::new_with_clock(
        control_plane,
        storage,
        worker_outbound,
        hub,
        Arc::new(FixedClock),
        execution,
    )
    .expect("compose application")
}

// ---- durable staging over the same product-state database ------------------

/// Registers one online client node with four worker-session slots.
fn stage_node(storage: &mut SqliteStorage, seed: u64) -> String {
    let node = format!("cnd_{}", suffix(seed));
    let registration = ClientNodeRegistration::try_new(
        node.clone(),
        format!("{seed:010}"),
        "Gate Test Device".to_owned(),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        Some(format!("cix_{}", suffix(seed + 40))),
        4,
    )
    .expect("registration");
    let mut registry = storage.client_node_registry().expect("registry");
    registry
        .register(&registration, 0, &instant("2026-09-04T12:00:00.000Z"))
        .expect("register");
    registry
        .update_presence(&node, ClientPresenceState::Online, 1)
        .expect("presence");
    node
}

/// Stages one active client `use` grant for the user.
fn stage_client_grant(storage: &mut SqliteStorage, seed: u64, node: &str, user: &str) {
    let issuance = AccessGrantIssuance::try_new(
        format!("cag_{}", suffix(seed)),
        node,
        user,
        user,
        GrantTrustMode::Trusted,
        None,
    )
    .expect("issuance");
    storage
        .client_connect_ledger()
        .expect("ledger")
        .create_grant(
            &issuance,
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant("2026-09-04T12:00:10.000Z"),
        )
        .expect("grant");
}

/// Stages one repository binding on the node with an active `use` grant for
/// the user; returns the binding id.
fn stage_visible_binding(storage: &mut SqliteStorage, seed: u64, node: &str, user: &str) -> String {
    let binding = format!("rbd_{}", suffix(seed));
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    let projection = RepositoryBindingProjection::try_new(
        binding.clone(),
        node,
        "winwincode",
        Some("main".to_owned()),
        Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        RepositoryDirtyState::Clean,
        RepositoryAvailability::Available,
        format!("sha256:{seed:064}"),
    )
    .expect("projection");
    ledger
        .upsert(&projection, None, 0, &instant("2026-09-04T12:00:30.000Z"))
        .expect("upsert");
    let issuance = RepositoryAccessGrantIssuance::try_new(
        format!("rag_{}", suffix(seed + 20)),
        &binding,
        user,
        user,
    )
    .expect("repo issuance");
    ledger
        .create_grant(
            &issuance,
            RepositoryGrantPermissions::Use,
            &instant("2026-09-04T12:00:31.000Z"),
        )
        .expect("repo grant");
    binding
}

/// Claims occupancy for the user and walks it to `occupied`.
fn stage_occupied_lease(
    storage: &mut SqliteStorage,
    seed: u64,
    node: &str,
    user: &str,
) -> (String, u64) {
    let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
    let claim = OccupancyClaim::try_new(
        format!("ocl_{}", suffix(seed)),
        node,
        user,
        format!("req_{}", suffix(seed + 10)),
    )
    .expect("claim");
    let lease = occupancy
        .atomic_claim(&claim, &instant("2026-09-04T12:01:00.000Z"))
        .expect("claim");
    let occupied = occupancy
        .record_acknowledgement(
            &lease.occupancy_lease_id,
            lease.fencing_token,
            None,
            &instant("2026-09-04T12:01:01.000Z"),
        )
        .expect("ack");
    assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
    (occupied.occupancy_lease_id, occupied.fencing_token)
}

/// Issues one launch grant anchored to the product session.
#[allow(clippy::too_many_arguments)]
fn stage_anchor(
    storage: &mut SqliteStorage,
    seed: u64,
    node: &str,
    user: &str,
    lease_id: &str,
    fencing_token: u64,
    binding: &str,
    product_session_id: &str,
) {
    let issuance = LaunchGrantIssuance::try_new(
        format!("wlg_{}", suffix(seed)),
        node,
        format!("cix_{}", suffix(seed + 40)),
        user,
        lease_id,
        fencing_token,
        binding,
        format!("ws_{}", suffix(seed + 50)),
        format!("wkr_{}", suffix(seed + 51)),
        format!("winst_{}", suffix(seed + 52)),
        "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        Some(product_session_id.to_owned()),
        Some(format!("run_{}", suffix(seed + 53))),
        instant("2100-01-01T00:00:00.000Z"),
    )
    .expect("issuance");
    WorkerLaunchGrantService::new(storage)
        .issue(&issuance, &instant("2026-09-04T12:02:00.000Z"))
        .expect("issue anchor grant");
}

// ---- generated command helpers ---------------------------------------------

fn session_create_request(request: u64, session: u64, user: &str) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": "session.create",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(1),
        "expectedRevision": 0,
        "payload": {
            "productSessionId": id("psn", session),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1),
            "title": "Device-anchored session",
            "modelRoute": {
                "providerId": "provider-main",
                "modelId": "model-main",
                "credentialReferenceId": id("crd", 1)
            }
        }
    }))
    .expect("generated session.create command")
}

fn chat_submit_request(
    request: u64,
    session: u64,
    user: &str,
    expected_revision: i64,
) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": "chat.submit",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(1),
        "expectedRevision": expected_revision,
        "payload": {
            "productSessionId": id("psn", session),
            "message": "Continue on the device"
        }
    }))
    .expect("generated chat.submit command")
}

fn completed(response: CommandDispatchResponse) -> serde_json::Value {
    let CommandDispatchResponse::Completed(response) = response else {
        panic!("ProductSession command must complete synchronously");
    };
    serde_json::to_value(response).expect("encode completed response")
}

/// Everything one staged device-anchored session exposes to the test.
struct AnchoredSession {
    holder: String,
    binding: String,
    occupancy_lease_id: String,
}

/// Stages the authorized device world, creates the session as the holder, and
/// anchors it through a real launch grant.
fn stage_anchored_session(
    application: &StandaloneControlPlaneApplication,
    root: &Path,
    seed: u64,
    session: u64,
    holder: &str,
) -> AnchoredSession {
    let mut storage = SqliteStorage::open(root).expect("open staging storage");
    let node = stage_node(&mut storage, seed);
    stage_client_grant(&mut storage, seed + 1, &node, holder);
    let binding = stage_visible_binding(&mut storage, seed + 2, &node, holder);
    let (occupancy_lease_id, occupancy_fencing_token) =
        stage_occupied_lease(&mut storage, seed + 3, &node, holder);
    drop(storage);

    application
        .command(
            &principal(holder),
            CommandFamily::Session,
            session_create_request(10 * seed, session, holder),
        )
        .expect("create ProductSession");

    let mut storage = SqliteStorage::open(root).expect("reopen staging storage");
    stage_anchor(
        &mut storage,
        seed + 4,
        &node,
        holder,
        &occupancy_lease_id,
        occupancy_fencing_token,
        &binding,
        &id("psn", session),
    );
    AnchoredSession {
        holder: holder.to_owned(),
        binding,
        occupancy_lease_id,
    }
}

#[test]
fn a_device_anchored_session_continues_for_its_current_holder() {
    let root = temporary_root("holder-continues");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    let staged = stage_anchored_session(&application, &root, 100, 1, &holder);
    assert_eq!(staged.holder, holder);

    let submitted = application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            chat_submit_request(11, 1, &holder, 1),
        )
        .expect("anchored turn continues for the holder");
    let body = completed(submitted);
    assert_eq!(body["currentRevision"], 2);
    assert_eq!(body["result"]["state"], "running");

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_anchored_turn_after_the_occupancy_ended_requires_occupancy() {
    let root = temporary_root("occupancy-required");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    let staged = stage_anchored_session(&application, &root, 200, 1, &holder);
    {
        // The holder releases the client after the session was bound; with
        // no active worker session the lease leaves the active set.
        let mut storage = SqliteStorage::open(&root).expect("reopen staging storage");
        let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
        let lease = occupancy
            .snapshot(&staged.occupancy_lease_id)
            .expect("lease snapshot")
            .expect("staged lease");
        occupancy
            .request_release(
                &lease.occupancy_lease_id,
                lease.fencing_token,
                0,
                &instant("2026-09-04T12:03:00.000Z"),
            )
            .expect("release");
    }

    let denial: ApiError = application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            chat_submit_request(12, 1, &holder, 1),
        )
        .expect_err("released occupancy must refuse the turn");
    assert_eq!(denial.status(), 409);
    assert_eq!(denial.code(), "OCCUPANCY_REQUIRED");

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_anchored_turn_by_any_other_user_is_access_denied() {
    let root = temporary_root("access-denied");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    let member = id("usr", 2);
    let _staged = stage_anchored_session(&application, &root, 300, 1, &holder);

    let denial: ApiError = application
        .command(
            &principal(&member),
            CommandFamily::Session,
            chat_submit_request(13, 1, &member, 1),
        )
        .expect_err("a non-holder must be refused");
    assert_eq!(denial.status(), 403);
    assert_eq!(denial.code(), "ACCESS_DENIED");

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_anchored_turn_after_a_repository_grant_revocation_is_not_visible() {
    let root = temporary_root("binding-not-visible");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    let staged = stage_anchored_session(&application, &root, 400, 1, &holder);
    {
        // Revoking the repository access grant ends the binding's visibility
        // immediately (plan 13.4): the next turn must be refused even though
        // the occupancy and the anchor grant are untouched.
        let mut storage = SqliteStorage::open(&root).expect("reopen staging storage");
        let record = storage
            .repository_binding_ledger()
            .expect("ledger")
            .active_grants_for_binding(&staged.binding)
            .expect("active grants")
            .into_iter()
            .next()
            .expect("one active grant");
        storage
            .repository_binding_ledger()
            .expect("ledger")
            .revoke_grant(&record.repository_access_grant_id, record.revision)
            .expect("revoke");
    }

    let denial: ApiError = application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            chat_submit_request(14, 1, &holder, 1),
        )
        .expect_err("an invisible binding must refuse the turn");
    assert_eq!(denial.status(), 403);
    assert_eq!(denial.code(), "BINDING_NOT_VISIBLE");

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_session_without_a_device_anchor_is_not_gated() {
    let root = temporary_root("pass-through");
    let application = compose_application(&root);
    let user = id("usr", 1);

    application
        .command(
            &principal(&user),
            CommandFamily::Session,
            session_create_request(15, 1, &user),
        )
        .expect("create ProductSession");
    let submitted = application
        .command(
            &principal(&user),
            CommandFamily::Session,
            chat_submit_request(16, 1, &user, 1),
        )
        .expect("an unanchored session continues unchanged");
    let body = completed(submitted);
    assert_eq!(body["result"]["state"], "running");

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_gate_denial_taxonomy_stays_on_the_central_wire_codes() {
    let denial = DeviceSessionGateDenialKind::OccupancyRequired;
    assert!(matches!(
        denial,
        DeviceSessionGateDenialKind::OccupancyRequired
    ));
    // The three canonical denials stay distinct from the transport fallbacks.
    let kinds = [
        DeviceSessionGateDenialKind::OccupancyRequired,
        DeviceSessionGateDenialKind::AccessDenied,
        DeviceSessionGateDenialKind::BindingNotVisible,
        DeviceSessionGateDenialKind::InvalidRequest,
        DeviceSessionGateDenialKind::Unavailable,
    ];
    for (index, kind) in kinds.iter().enumerate() {
        for other in &kinds[index + 1..] {
            assert_ne!(kind, other, "gate denial kinds must stay distinct");
        }
    }
}
