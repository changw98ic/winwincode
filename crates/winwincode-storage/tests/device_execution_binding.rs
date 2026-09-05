// SPDX-License-Identifier: Apache-2.0

//! Durable `DeviceExecutionBinding`, execution-reservation device facts, and
//! the per-node reservation capacity ledger contract tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, UserId, WorkspaceId,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState,
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRelease, DeviceExecutionBindingState,
    DeviceExecutionBindingStoreErrorKind, DeviceExecutionFactsAttachment,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest, GrantPermissions,
    GrantSource, GrantTrustMode, LaunchAckSettlement, LaunchGrantIssuance, OccupancyClaim,
    OccupancyLeaseState, RepositoryAccessGrantIssuance, RepositoryAvailability,
    RepositoryBindingProjection, RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage,
    WorkerLaunchGrantRecord, WorkerPoolId,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-device-execution-binding-{name}-{}-{suffix}-{nanos}",
        std::process::id()
    ))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T3: &str = "2026-01-01T00:03:00.000Z";
const GRANT_EXPIRES: &str = "2026-01-01T01:00:00.000Z";

const DIGEST: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// Every authoritative identity one binding needs.
struct Fixture {
    node: String,
    instance: String,
    holder: String,
    lease_id: String,
    fencing_token: u64,
    binding_id: String,
}

/// Seeds the registry, access grants, occupancy lease, and repository
/// binding; the caller stays responsible for the launch grant.
#[allow(clippy::too_many_lines)]
fn seed_fixture(storage: &mut SqliteStorage, seed: u64) -> Fixture {
    let node = id("cnd", seed);
    let instance = id("cix", seed + 2);
    let holder = id("usr", seed + 1);
    {
        let registration = ClientNodeRegistration::try_new(
            node.clone(),
            format!("{seed:010}"),
            "Binding Test Device".to_owned(),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(instance.clone()),
            4,
        )
        .expect("registration");
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &instant(T0))
            .expect("register");
        registry
            .update_presence(&node, ClientPresenceState::Online, 1)
            .expect("presence");
    }
    {
        let issuance = AccessGrantIssuance::try_new(
            id("cag", seed + 3),
            &node,
            &holder,
            &holder,
            GrantTrustMode::Trusted,
            None,
        )
        .expect("issuance");
        let mut ledger = storage.client_connect_ledger().expect("ledger");
        ledger
            .create_grant(
                &issuance,
                GrantSource::Administrator,
                GrantPermissions::USE,
                &instant("2026-01-01T00:00:10.000Z"),
            )
            .expect("grant");
    }
    let (lease_id, fencing_token) = {
        let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
        let claim =
            OccupancyClaim::try_new(id("ocl", seed + 4), &node, &holder, id("req", seed + 5))
                .expect("claim");
        let lease = occupancy
            .atomic_claim(&claim, &instant("2026-01-01T00:01:00.000Z"))
            .expect("claim");
        let occupied = occupancy
            .record_acknowledgement(
                &lease.occupancy_lease_id,
                lease.fencing_token,
                None,
                &instant("2026-01-01T00:01:01.000Z"),
            )
            .expect("ack");
        assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
        (occupied.occupancy_lease_id, occupied.fencing_token)
    };
    let binding_id = id("rbd", seed + 6);
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding_id.clone(),
            &node,
            "winwincode",
            Some("main".to_owned()),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{seed:064}"),
        )
        .expect("projection");
        ledger
            .upsert(&projection, None, 0, &instant("2026-01-01T00:00:30.000Z"))
            .expect("upsert");
        let issuance = RepositoryAccessGrantIssuance::try_new(
            id("rag", seed + 7),
            &binding_id,
            &holder,
            &holder,
        )
        .expect("repo issuance");
        ledger
            .create_grant(
                &issuance,
                RepositoryGrantPermissions::Use,
                &instant("2026-01-01T00:00:31.000Z"),
            )
            .expect("repo grant");
    }
    Fixture {
        node,
        instance,
        holder,
        lease_id,
        fencing_token,
        binding_id,
    }
}

/// Issues one live launch grant over the seeded fixture.
fn issue_launch_grant(
    storage: &mut SqliteStorage,
    seed: u64,
    fixture: &Fixture,
) -> WorkerLaunchGrantRecord {
    issue_launch_grant_for_session(storage, seed, fixture, &id("ws", seed + 8))
}

/// Issues one live launch grant over the seeded fixture with an explicit
/// worker session identity (the recovery path reuses the session).
fn issue_launch_grant_for_session(
    storage: &mut SqliteStorage,
    seed: u64,
    fixture: &Fixture,
    worker_session_id: &str,
) -> WorkerLaunchGrantRecord {
    let issuance = LaunchGrantIssuance::try_new(
        id("wlg", seed),
        &fixture.node,
        &fixture.instance,
        &fixture.holder,
        &fixture.lease_id,
        fixture.fencing_token,
        &fixture.binding_id,
        worker_session_id,
        id("wkr", seed + 9),
        id("winst", seed + 10),
        DIGEST,
        Some(id("ps", seed + 11)),
        Some(id("run", seed + 12)),
        instant(GRANT_EXPIRES),
    )
    .expect("grant issuance");
    storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .issue(&issuance, &instant(T0))
        .expect("issue")
}

/// Echoes every grant field into a validated bind command.
fn bind_command(seed: u64, grant: &WorkerLaunchGrantRecord) -> DeviceExecutionBindingIssuance {
    DeviceExecutionBindingIssuance::try_new(
        id("deb", seed),
        id("req", seed + 1),
        &grant.worker_launch_grant_id,
        &grant.client_node_id,
        &grant.client_instance_id,
        &grant.holder_user_id,
        &grant.occupancy_lease_id,
        grant.occupancy_fencing_token,
        &grant.repository_binding_id,
        &grant.worker_session_id,
        grant.product_session_id.clone(),
        grant.stage_run_id.clone(),
    )
    .expect("bind command")
}

/// Configures every admission boundary and reserves one queued Job for the
/// fixture holder. The reservation scope carries the `psn_`-prefixed session
/// identity the admission ledger validates.
fn seed_reservation(storage: &mut SqliteStorage, seed: u64, fixture: &Fixture) -> String {
    seed_reservation_for_user(storage, seed, &fixture.holder)
}

/// Configures every admission boundary and reserves one queued Job for an
/// arbitrary reservation user.
fn seed_reservation_for_user(
    storage: &mut SqliteStorage,
    seed: u64,
    reservation_user: &str,
) -> String {
    let scope = ExecutionQueueScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        product_session_id: ProductSessionId(id("psn", seed)),
        delivery_id: Some(DeliveryId(id("dlv", seed))),
    };
    let pool = WorkerPoolId(id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 4,
        max_queued: 4,
        token_budget: 10_000,
        cost_budget_microunits: 10_000,
        max_runtime_millis: 60_000,
    };
    {
        let mut admission = storage.execution_admission().expect("admission");
        for boundary in [
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
            ExecutionAdmissionBoundary::Delivery {
                organization_id: scope.organization_id.clone(),
                delivery_id: scope.delivery_id.clone().expect("delivery"),
            },
            ExecutionAdmissionBoundary::ProductSession {
                organization_id: scope.organization_id.clone(),
                project_id: scope.project_id.clone(),
                product_session_id: scope.product_session_id.clone(),
            },
            ExecutionAdmissionBoundary::WorkerPool {
                organization_id: scope.organization_id.clone(),
                worker_pool_id: pool.clone(),
            },
        ] {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("policy configure");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope,
                user_id: UserId(reservation_user.to_owned()),
                worker_pool_id: pool,
                job_id: ExecutionJobId(id("job", seed)),
                request_id: RequestId(id("req", seed + 13)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: 100,
                reserved_cost_microunits: 1_000,
                runtime_limit_millis: 30_000,
                submitted_at: instant(T1),
            })
            .expect("reserve");
    }
    id("job", seed)
}

/// Seeds fixture, grant, binding, and one queued reservation; returns the
/// job id for the attachment tests.
fn seed_bound_job(
    storage: &mut SqliteStorage,
    seed: u64,
) -> (Fixture, WorkerLaunchGrantRecord, String) {
    let fixture = seed_fixture(storage, seed);
    let grant = issue_launch_grant(storage, seed + 100, &fixture);
    {
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        ledger
            .bind(&bind_command(seed + 200, &grant), &instant(T2))
            .expect("bind");
    }
    let job = seed_reservation(storage, seed + 300, &fixture);
    (fixture, grant, job)
}

#[test]
fn bind_persists_a_durable_traceable_binding_across_restart() {
    let directory = temporary_directory("restart");
    let (grant, seeded) = {
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        let fixture = seed_fixture(&mut storage, 1000);
        let grant = issue_launch_grant(&mut storage, 1100, &fixture);
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        let receipt = ledger
            .bind(&bind_command(1200, &grant), &instant(T2))
            .expect("bind");
        assert!(!receipt.replayed);
        let binding = &receipt.binding;
        assert_eq!(binding.state, DeviceExecutionBindingState::Bound);
        assert_eq!(binding.revision, 1);
        // Every identity is traceable to the ProductSession/StageRun stamp.
        assert_eq!(
            binding.product_session_id.as_deref(),
            grant.product_session_id.as_deref()
        );
        assert_eq!(
            binding.stage_run_id.as_deref(),
            grant.stage_run_id.as_deref()
        );
        assert_eq!(binding.worker_session_id, id("ws", 1100 + 8));
        let snapshot = ledger
            .snapshot(&binding.worker_session_id)
            .expect("snapshot")
            .expect("binding");
        assert_eq!(snapshot, *binding);
        (grant, receipt.binding)
    };
    // Reopen: the binding survives the restart and still names the grant.
    let mut storage = SqliteStorage::open(&directory).expect("reopen");
    let ledger = storage.device_execution_binding_ledger().expect("ledger");
    let snapshot = ledger
        .snapshot(&seeded.worker_session_id)
        .expect("snapshot")
        .expect("binding");
    assert_eq!(snapshot, seeded);
    assert_eq!(
        snapshot.worker_launch_grant_id,
        grant.worker_launch_grant_id
    );
    let by_id = ledger
        .snapshot_by_binding_id(&seeded.device_execution_binding_id)
        .expect("snapshot by id")
        .expect("binding");
    assert_eq!(by_id, seeded);
}

#[test]
fn bind_replays_idempotently_and_refuses_request_reuse() {
    let mut storage = SqliteStorage::open(temporary_directory("replay")).expect("storage");
    let fixture = seed_fixture(&mut storage, 2000);
    let grant = issue_launch_grant(&mut storage, 2100, &fixture);
    let command = bind_command(2200, &grant);
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    let first = ledger.bind(&command, &instant(T2)).expect("bind");
    let replay = ledger.bind(&command, &instant(T3)).expect("replay");
    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(first.binding, replay.binding);
    // The same request id with a different body is a fixed conflict.
    let conflicting = DeviceExecutionBindingIssuance::try_new(
        id("deb", 2300),
        command.request_id.clone(),
        &grant.worker_launch_grant_id,
        &grant.client_node_id,
        &grant.client_instance_id,
        &grant.holder_user_id,
        &grant.occupancy_lease_id,
        grant.occupancy_fencing_token,
        &grant.repository_binding_id,
        &grant.worker_session_id,
        grant.product_session_id.clone(),
        grant.stage_run_id.clone(),
    )
    .expect("conflicting command");
    let error = ledger.bind(&conflicting, &instant(T3)).expect_err("reuse");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::RequestConflict
    );
}

#[test]
fn bind_refuses_mismatched_facts_unknown_or_terminal_grants() {
    let mut storage = SqliteStorage::open(temporary_directory("gate")).expect("storage");
    let fixture = seed_fixture(&mut storage, 3000);
    let grant = issue_launch_grant(&mut storage, 3100, &fixture);
    {
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        // A projection that guesses any field is refused.
        let guesses = [
            ("client node", "expected_client_node_id", id("cnd", 3999)),
            (
                "client instance",
                "expected_client_instance_id",
                id("cix", 3999),
            ),
            ("holder", "expected_holder_user_id", id("usr", 3999)),
            ("lease", "expected_occupancy_lease_id", id("ocl", 3999)),
            (
                "repository binding",
                "expected_repository_binding_id",
                id("rbd", 3999),
            ),
            ("session", "expected_worker_session_id", id("ws", 3999)),
        ];
        for (label, field, value) in guesses {
            let mut command = bind_command(3200, &grant);
            match field {
                "expected_client_node_id" => command.expected_client_node_id = value,
                "expected_client_instance_id" => command.expected_client_instance_id = value,
                "expected_holder_user_id" => command.expected_holder_user_id = value,
                "expected_occupancy_lease_id" => command.expected_occupancy_lease_id = value,
                "expected_repository_binding_id" => command.expected_repository_binding_id = value,
                "expected_worker_session_id" => command.expected_worker_session_id = value,
                _ => unreachable!("covered field"),
            }
            let error = ledger.bind(&command, &instant(T2)).expect_err(label);
            assert_eq!(
                error.kind(),
                DeviceExecutionBindingStoreErrorKind::FieldMismatch,
                "{label}"
            );
        }
        let mut stale_token = bind_command(3201, &grant);
        stale_token.expected_occupancy_fencing_token += 1;
        let error = ledger.bind(&stale_token, &instant(T2)).expect_err("token");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingStoreErrorKind::FieldMismatch
        );
        let mut dropped_stamp = bind_command(3202, &grant);
        dropped_stamp.expected_stage_run_id = None;
        let error = ledger
            .bind(&dropped_stamp, &instant(T2))
            .expect_err("stamp");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingStoreErrorKind::FieldMismatch
        );
        // An unknown grant names the unknown category.
        let unknown = DeviceExecutionBindingIssuance::try_new(
            id("deb", 3300),
            id("req", 3301),
            id("wlg", 3999),
            &fixture.node,
            &fixture.instance,
            &fixture.holder,
            &fixture.lease_id,
            fixture.fencing_token,
            &fixture.binding_id,
            id("ws", 3302),
            None,
            None,
        )
        .expect("unknown command");
        let error = ledger.bind(&unknown, &instant(T2)).expect_err("unknown");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingStoreErrorKind::UnknownLaunchGrant
        );
    }
    // A revoked grant is terminal and refuses the binding.
    storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .revoke(
            &grant.worker_launch_grant_id,
            &fixture.holder,
            None,
            &instant(T1),
        )
        .expect("revoke");
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    let error = ledger
        .bind(&bind_command(3400, &grant), &instant(T2))
        .expect_err("terminal");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::LaunchGrantNotLive
    );
}

#[test]
fn bind_enforces_one_bound_binding_per_session_and_per_grant() {
    let mut storage = SqliteStorage::open(temporary_directory("unique")).expect("storage");
    let fixture = seed_fixture(&mut storage, 4000);
    let grant = issue_launch_grant(&mut storage, 4100, &fixture);
    {
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        ledger
            .bind(&bind_command(4200, &grant), &instant(T2))
            .expect("bind");
        // The same grant cannot bind twice under any binding identity.
        let error = ledger
            .bind(&bind_command(4300, &grant), &instant(T2))
            .expect_err("second grant binding");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingStoreErrorKind::BindingConflict
        );
    }
    // After the grant terminates and the binding releases, a fresh grant may
    // bind the same worker session again (the recovery path).
    storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .revoke(
            &grant.worker_launch_grant_id,
            &fixture.holder,
            None,
            &instant(T2),
        )
        .expect("revoke");
    {
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        ledger
            .release(
                &DeviceExecutionBindingRelease::try_new(
                    &grant.worker_session_id,
                    id("req", 4400),
                    1,
                    instant(T2),
                )
                .expect("release"),
                &instant(T2),
            )
            .expect("release");
    }
    let revived =
        issue_launch_grant_for_session(&mut storage, 4500, &fixture, &grant.worker_session_id);
    assert_ne!(revived.worker_launch_grant_id, grant.worker_launch_grant_id);
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    let receipt = ledger
        .bind(&bind_command(4600, &revived), &instant(T3))
        .expect("rebind");
    assert_eq!(receipt.binding.state, DeviceExecutionBindingState::Bound);
    let snapshot = ledger
        .snapshot(&grant.worker_session_id)
        .expect("snapshot")
        .expect("binding");
    assert_eq!(snapshot, receipt.binding);
}

#[test]
fn release_follows_the_fixed_cas_and_replay_rules() {
    let mut storage = SqliteStorage::open(temporary_directory("release")).expect("storage");
    let fixture = seed_fixture(&mut storage, 5000);
    let grant = issue_launch_grant(&mut storage, 5100, &fixture);
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    ledger
        .bind(&bind_command(5200, &grant), &instant(T2))
        .expect("bind");
    // A stale revision loses the compare-and-swap race.
    let stale = DeviceExecutionBindingRelease::try_new(
        &grant.worker_session_id,
        id("req", 5300),
        7,
        instant(T3),
    )
    .expect("stale release");
    let error = ledger.release(&stale, &instant(T3)).expect_err("stale");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::RevisionConflict
    );
    let release = DeviceExecutionBindingRelease::try_new(
        &grant.worker_session_id,
        id("req", 5400),
        1,
        instant(T3),
    )
    .expect("release");
    let receipt = ledger.release(&release, &instant(T3)).expect("release");
    assert!(!receipt.replayed);
    assert_eq!(receipt.binding.state, DeviceExecutionBindingState::Released);
    assert_eq!(receipt.binding.revision, 2);
    assert_eq!(
        receipt
            .binding
            .released_at
            .as_ref()
            .map(|value| value.0.as_str()),
        Some(T3)
    );
    // The replay is an accepted idempotent no-op.
    let replay = ledger.release(&release, &instant(T3)).expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.binding, receipt.binding);
    // A further release with a fresh request names the missing bound row.
    let repeat = DeviceExecutionBindingRelease::try_new(
        &grant.worker_session_id,
        id("req", 5500),
        2,
        instant(T3),
    )
    .expect("repeat release");
    let error = ledger.release(&repeat, &instant(T3)).expect_err("repeat");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::UnknownBinding
    );
}

#[test]
fn attach_copies_reservation_facts_from_the_launch_grant() {
    let mut storage = SqliteStorage::open(temporary_directory("attach")).expect("storage");
    let (fixture, grant, job) = seed_bound_job(&mut storage, 6000);
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    let command = DeviceExecutionFactsAttachment::try_new(
        id("req", 6100),
        &job,
        &grant.worker_launch_grant_id,
    )
    .expect("attachment");
    let receipt = ledger.attach_facts(&command, &instant(T2)).expect("attach");
    assert!(!receipt.replayed);
    let facts = &receipt.facts;
    assert_eq!(facts.job_id, job);
    assert_eq!(facts.client_node_id, fixture.node);
    assert_eq!(facts.client_instance_id, fixture.instance);
    assert_eq!(facts.holder_user_id, fixture.holder);
    assert_eq!(facts.repository_binding_id, fixture.binding_id);
    assert_eq!(facts.occupancy_lease_id, fixture.lease_id);
    assert_eq!(facts.occupancy_fencing_token, fixture.fencing_token);
    assert_eq!(facts.worker_launch_grant_id, grant.worker_launch_grant_id);
    assert_eq!(facts.worker_session_id, grant.worker_session_id);
    assert_eq!(facts.worker_id, grant.worker_id);
    assert_eq!(facts.worker_instance_id, grant.worker_instance_id);
    assert_eq!(facts.product_session_id, grant.product_session_id);
    assert_eq!(facts.stage_run_id, grant.stage_run_id);
    // The durable projection round-trips, and the replay is idempotent.
    assert_eq!(ledger.facts(&job).expect("facts").expect("stored"), *facts);
    let replay = ledger.attach_facts(&command, &instant(T3)).expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.facts, *facts);
    // A second attachment under a fresh request identity is refused.
    let repeat = DeviceExecutionFactsAttachment::try_new(
        id("req", 6200),
        &job,
        &grant.worker_launch_grant_id,
    )
    .expect("repeat attachment");
    let error = ledger
        .attach_facts(&repeat, &instant(T3))
        .expect_err("repeat");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::FactsAlreadyAttached
    );
}

#[test]
fn attach_refuses_mismatched_unknown_or_terminal_reservations() {
    let mut storage = SqliteStorage::open(temporary_directory("attach-gate")).expect("storage");
    let fixture = seed_fixture(&mut storage, 7000);
    let grant = issue_launch_grant(&mut storage, 7100, &fixture);
    {
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        ledger
            .bind(&bind_command(7200, &grant), &instant(T2))
            .expect("bind");
        // Unknown Job.
        let unknown = DeviceExecutionFactsAttachment::try_new(
            id("req", 7300),
            id("job", 7999),
            &grant.worker_launch_grant_id,
        )
        .expect("unknown attachment");
        let error = ledger
            .attach_facts(&unknown, &instant(T2))
            .expect_err("unknown");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingStoreErrorKind::UnknownExecutionJob
        );
    }
    // The reservation user differs from the grant holder.
    let foreign_job = seed_reservation_for_user(&mut storage, 7500, &id("usr", 7999));
    let foreign = DeviceExecutionFactsAttachment::try_new(
        id("req", 7400),
        &foreign_job,
        &grant.worker_launch_grant_id,
    )
    .expect("mismatched attachment");
    // A reservation that will be settled (terminal) refuses the attachment.
    let settled_job = seed_reservation(&mut storage, 7800, &fixture);
    {
        let mut admission = storage.execution_admission().expect("admission");
        let scope = ExecutionQueueScope {
            organization_id: OrganizationId(id("org", 7800)),
            workspace_id: WorkspaceId(id("wsp", 7800)),
            project_id: ProjectId(id("prj", 7800)),
            repository_id: RepositoryId(id("rep", 7800)),
            product_session_id: ProductSessionId(id("psn", 7800)),
            delivery_id: Some(DeliveryId(id("dlv", 7800))),
        };
        admission
            .start(&winwincode_storage::ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: WorkerPoolId(id("wpl", 7800)),
                job_id: ExecutionJobId(id("job", 7800)),
                request_id: RequestId(id("req", 7810)),
                expected_revision: 1,
                started_at: instant(T2),
            })
            .expect("start");
        admission
            .settle(&winwincode_storage::ExecutionReservationSettlement {
                scope,
                worker_pool_id: WorkerPoolId(id("wpl", 7800)),
                job_id: ExecutionJobId(id("job", 7800)),
                request_id: RequestId(id("req", 7820)),
                expected_revision: 2,
                actual_tokens: 100,
                actual_cost_microunits: 1_000,
                actual_runtime_millis: 1_000,
                completed_at: instant(T3),
            })
            .expect("settle");
    }
    let terminal = DeviceExecutionFactsAttachment::try_new(
        id("req", 7900),
        &settled_job,
        &grant.worker_launch_grant_id,
    )
    .expect("terminal attachment");
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    let error = ledger
        .attach_facts(&foreign, &instant(T2))
        .expect_err("user mismatch");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::FieldMismatch
    );
    let error = ledger
        .attach_facts(&terminal, &instant(T3))
        .expect_err("terminal");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::IllegalStateTransition
    );
}

#[test]
fn attach_requires_the_bound_binding() {
    let mut storage = SqliteStorage::open(temporary_directory("unbound")).expect("storage");
    let fixture = seed_fixture(&mut storage, 8000);
    let grant = issue_launch_grant(&mut storage, 8100, &fixture);
    let job = seed_reservation(&mut storage, 8200, &fixture);
    let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
    let attachment = DeviceExecutionFactsAttachment::try_new(
        id("req", 8300),
        &job,
        &grant.worker_launch_grant_id,
    )
    .expect("attachment");
    let error = ledger
        .attach_facts(&attachment, &instant(T2))
        .expect_err("unbound");
    assert_eq!(
        error.kind(),
        DeviceExecutionBindingStoreErrorKind::UnknownBinding
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn capacity_ledger_drives_claim_and_launch_validation_from_one_durable_view() {
    let mut storage = SqliteStorage::open(temporary_directory("capacity")).expect("storage");
    let fixture = seed_fixture(&mut storage, 9000);
    {
        let ledger = storage.device_execution_binding_ledger().expect("ledger");
        // Unknown nodes have no ledger view.
        assert!(
            ledger
                .capacity_snapshot(&id("cnd", 9999))
                .expect("snapshot")
                .is_none()
        );
        let empty = ledger
            .capacity_snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node");
        assert_eq!(empty.max_worker_sessions, 4);
        assert_eq!(empty.reserved_worker_sessions, 0);
        assert_eq!(empty.bound_bindings, 0);
        assert_eq!(empty.free_worker_sessions, 4);
    }
    // An issued grant reserves one slot durably.
    let grant = issue_launch_grant(&mut storage, 9100, &fixture);
    {
        let mut ledger = storage.device_execution_binding_ledger().expect("ledger");
        let reserved = ledger
            .capacity_snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node");
        assert_eq!(reserved.reserved_worker_sessions, 1);
        assert_eq!(reserved.in_flight_worker_sessions, 1);
        assert_eq!(reserved.free_worker_sessions, 3);
        assert_eq!(
            ledger
                .reserved_worker_sessions_for_node(&fixture.node)
                .expect("reserved"),
            1
        );
        // The binding is visible as the bound ledger fact.
        ledger
            .bind(&bind_command(9200, &grant), &instant(T2))
            .expect("bind");
        let bound = ledger
            .capacity_snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node");
        assert_eq!(bound.bound_bindings, 1);
        assert_eq!(bound.reserved_worker_sessions, 1);
    }
    // Consuming the grant keeps it non-terminal: still reserved.
    storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .settle_launch_ack(
            &LaunchAckSettlement::try_new(
                &grant.worker_launch_grant_id,
                &fixture.lease_id,
                fixture.fencing_token,
                &grant.worker_session_id,
                &grant.worker_id,
                &grant.worker_instance_id,
                true,
                None,
            )
            .expect("settlement"),
            &instant(T2),
        )
        .expect("ack");
    // A reported running count above the reservation dominates in-flight.
    {
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .heartbeat(&fixture.node, 3, &instant(T2), 2)
            .expect("heartbeat");
    }
    {
        let ledger = storage.device_execution_binding_ledger().expect("ledger");
        let consumed = ledger
            .capacity_snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node");
        assert_eq!(consumed.reserved_worker_sessions, 1);
        let reported = ledger
            .capacity_snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node");
        assert_eq!(reported.reported_running_worker_sessions, 3);
        assert_eq!(reported.in_flight_worker_sessions, 3);
        assert_eq!(reported.free_worker_sessions, 1);
    }
    // A second issued grant reserves another slot.
    let second = issue_launch_grant(&mut storage, 9300, &fixture);
    {
        let ledger = storage.device_execution_binding_ledger().expect("ledger");
        let doubled = ledger
            .capacity_snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node");
        assert_eq!(doubled.reserved_worker_sessions, 2);
        assert_eq!(doubled.in_flight_worker_sessions, 3);
        assert_eq!(doubled.free_worker_sessions, 1);
    }
    // Revoking the issued grant releases exactly its reserved slot; the
    // consumed grant stays reserved until its own session ends.
    storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .revoke(
            &second.worker_launch_grant_id,
            &fixture.holder,
            None,
            &instant(T3),
        )
        .expect("revoke");
    let ledger = storage.device_execution_binding_ledger().expect("ledger");
    let released = ledger
        .capacity_snapshot(&fixture.node)
        .expect("snapshot")
        .expect("node");
    assert_eq!(released.reserved_worker_sessions, 1);
    assert_eq!(released.in_flight_worker_sessions, 3);
    assert_eq!(released.bound_bindings, 1);
    assert_eq!(released.free_worker_sessions, 1);
}

#[test]
fn a_non_canonical_command_is_rejected_before_any_durable_write() {
    let mut storage = SqliteStorage::open(temporary_directory("invalid")).expect("storage");
    let ledger = storage.device_execution_binding_ledger().expect("ledger");
    let command = DeviceExecutionBindingIssuance::try_new(
        "not-canonical",
        id("req", 9501),
        id("wlg", 9502),
        id("cnd", 9503),
        id("cix", 9504),
        id("usr", 9505),
        id("ocl", 9506),
        1,
        id("rbd", 9507),
        id("ws", 9508),
        None,
        None,
    )
    .expect_err("non-canonical binding id");
    assert_eq!(
        command.kind(),
        DeviceExecutionBindingStoreErrorKind::InvalidInput
    );
    let release =
        DeviceExecutionBindingRelease::try_new(id("ws", 9510), id("req", 9511), 0, instant(T0))
            .expect_err("zero revision");
    assert_eq!(
        release.kind(),
        DeviceExecutionBindingStoreErrorKind::InvalidInput
    );
    let attachment =
        DeviceExecutionFactsAttachment::try_new(id("req", 9512), "job_wrong", id("wlg", 9513))
            .expect_err("non-canonical job id");
    assert_eq!(
        attachment.kind(),
        DeviceExecutionBindingStoreErrorKind::InvalidInput
    );
    assert!(
        ledger
            .snapshot(&id("ws", 9600))
            .expect("snapshot")
            .is_none()
    );
}
