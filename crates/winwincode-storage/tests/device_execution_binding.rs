// SPDX-License-Identifier: Apache-2.0

//! Durable `DeviceExecutionBinding`, execution-reservation device facts, and
//! the per-node reservation capacity ledger contract tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, UserId, WorkRunId, WorkspaceId,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState,
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRelease, DeviceExecutionBindingState,
    DeviceExecutionBindingStoreErrorKind, DeviceExecutionFactsAttachment,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest, GrantPermissions,
    GrantSource, GrantTrustMode, LaunchAckSettlement, LaunchGrantIssuance, OccupancyClaim,
    OccupancyLeaseState, ProductStateStorage, RepositoryAccessGrantIssuance,
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
    RepositoryGrantPermissions, SqliteStorage, WorkerLaunchGrantRecord, WorkerPoolId,
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
    issue_launch_grant_for_session(storage, seed, fixture, &id("wsn", seed + 8))
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
        id("wrk", seed + 9),
        id("wki", seed + 10),
        DIGEST,
        Some(id("ps", seed + 11)),
        Some(WorkRunId(id("wrn", seed + 12))),
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
        grant.work_run_id.as_ref().map(|value| value.0.clone()),
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
    seed_reservation_with_scope(storage, seed, reservation_user, false)
}

fn seed_reservation_with_scope(
    storage: &mut SqliteStorage,
    seed: u64,
    reservation_user: &str,
    chat: bool,
) -> String {
    let scope = ExecutionQueueScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        product_session_id: ProductSessionId(id("psn", seed)),
        delivery_id: (!chat).then(|| DeliveryId(id("dlv", seed))),
    };
    let pool = WorkerPoolId(id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 4,
        max_queued: 4,
        token_budget: Some(10_000),
        cost_budget_microunits: Some(10_000),
        max_runtime_millis: Some(60_000),
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
                delivery_id: DeliveryId(id("dlv", seed)),
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
                reserved_tokens: Some(100),
                reserved_cost_microunits: Some(1_000),
                runtime_limit_millis: Some(30_000),
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
        // Every identity is traceable to the ProductSession/WorkRun stamp.
        assert_eq!(
            binding.product_session_id.as_deref(),
            grant.product_session_id.as_deref()
        );
        assert_eq!(
            binding.work_run_id.as_deref(),
            grant.work_run_id.as_ref().map(|value| value.0.as_str())
        );
        assert_eq!(binding.worker_session_id, id("wsn", 1100 + 8));
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
        grant.work_run_id.as_ref().map(|value| value.0.clone()),
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
            ("session", "expected_worker_session_id", id("wsn", 3999)),
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
        dropped_stamp.expected_work_run_id = None;
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
            id("wsn", 3302),
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
    let root = temporary_directory("attach");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let (fixture, grant, job) = seed_bound_job(&mut storage, 6000);
    {
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
        assert_eq!(
            facts.work_run_id,
            grant.work_run_id.as_ref().map(|value| value.0.clone())
        );
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
    let job_id = ExecutionJobId(job);
    let projected = ProductStateStorage::load_work_run_device_binding_facts(&storage, &job_id)
        .expect("projection read")
        .expect("projected binding");
    assert_eq!(projected.public_client_id, "0000006000");
    assert_eq!(projected.repository_binding_id, fixture.binding_id);
    assert_eq!(projected.worker_session_id, grant.worker_session_id);
    drop(storage);
    let restarted = SqliteStorage::open(&root).expect("restarted storage");
    let replayed = ProductStateStorage::load_work_run_device_binding_facts(&restarted, &job_id)
        .expect("restarted projection read")
        .expect("restarted projected binding");
    assert_eq!(replayed, projected);
    drop(restarted);
    std::fs::remove_dir_all(root).expect("temporary storage directory");
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
                actual_cost_microunits: Some(1_000),
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
        id("wsn", 9508),
        None,
        None,
    )
    .expect_err("non-canonical binding id");
    assert_eq!(
        command.kind(),
        DeviceExecutionBindingStoreErrorKind::InvalidInput
    );
    let release =
        DeviceExecutionBindingRelease::try_new(id("wsn", 9510), id("req", 9511), 0, instant(T0))
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
            .snapshot(&id("wsn", 9600))
            .expect("snapshot")
            .is_none()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn credential_renewal_preserves_current_launch_authority_and_audit_across_workers() {
    use rusqlite::params;
    use winwincode_execution_port::generated::WorkerHeartbeatMessage;
    use winwincode_storage::{CredentialAuditAction, CredentialIssuance, CredentialRotation};
    let root = temporary_directory("credential-renewal");
    let mut storage = SqliteStorage::open(&root).unwrap();
    storage.execution_registry().unwrap();
    for (seed, occupancy_state) in [(20000, "draining"), (22000, "recovery_pending")] {
        let (fixture, grant, job) = seed_bound_job(&mut storage, seed);
        storage
            .worker_launch_grant_ledger()
            .unwrap()
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
                .unwrap(),
                &instant(T2),
            )
            .unwrap();
        let record = storage
            .worker_session_credential_ledger()
            .unwrap()
            .issue(
                &CredentialIssuance::try_new(
                    id("wcred", seed),
                    &grant.worker_session_id,
                    &grant.worker_id,
                    &grant.worker_instance_id,
                    &grant.worker_launch_grant_id,
                    DIGEST,
                    instant("2026-01-01T00:30:00.000Z"),
                )
                .unwrap(),
                &instant(T0),
            )
            .unwrap();
        let heartbeat: WorkerHeartbeatMessage = serde_json::from_value(serde_json::json!({
            "schemaVersion":"winwincode/v1", "kind":"worker.heartbeat",
            "messageId":id("xmsg",7), "sentAt":T2, "observedAt":T2,
            "workerId":grant.worker_id, "workerInstanceId":grant.worker_instance_id,
            "heartbeatSequence":7, "capacity":{"availableSlots":0,"runningJobs":1},
            "activeLeases":[{"jobId":job,"leaseId":id("lease",seed),"attempt":1,
                "fencingToken":"1", "expiresAt":GRANT_EXPIRES,"lastEventSequence":0}]
        }))
        .unwrap();
        let db = rusqlite::Connection::open(storage.database_path()).unwrap();
        db.execute("UPDATE client_occupancy_leases SET idle_expires_at=?1, state=?3 WHERE occupancy_lease_id=?2",
            params![T1,fixture.lease_id,occupancy_state]).unwrap();
        db.execute("INSERT INTO execution_workers VALUES (?1,?2,?3,'auth','v1','{}','{}','digest','local','healthy',?3,7,1,1,0)",
            params![grant.worker_id,grant.worker_instance_id,T0]).unwrap();
        db.execute(
            "INSERT INTO execution_heartbeats VALUES (?1,?2,7,'digest','{}')",
            params![grant.worker_id, grant.worker_instance_id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO execution_leases VALUES (?1,?2,'digest',?3,?4,1,'1',?5,?6)",
            params![
                job,
                id("lease", seed),
                grant.worker_id,
                grant.worker_instance_id,
                T0,
                GRANT_EXPIRES
            ],
        )
        .unwrap();
        // Dispatch session is deliberately different from the launch credential session.
        db.execute("INSERT INTO execution_dispatch_authorities VALUES (?1,?2,'digest',?3,?4,?5,1,'1',?6,?7,?8,?6)",
            params![job,id("lease",seed),grant.worker_id,grant.worker_instance_id,id("wsn",seed+900),T0,GRANT_EXPIRES,id("req",seed+901)]).unwrap();
        let lease = &heartbeat.active_leases[0];
        let renewal_time = instant("2026-01-01T00:20:00.000Z");
        let expiry = instant("2026-01-01T00:50:00.000Z");
        // A same-Worker lease alone is insufficient without trusted launch reservation facts.
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .renew_after_accepted_heartbeat(&record, &heartbeat, lease, &expiry, &renewal_time)
                .unwrap()
                .is_none()
        );
        storage
            .device_execution_binding_ledger()
            .unwrap()
            .attach_facts(
                &DeviceExecutionFactsAttachment::try_new(
                    id("req", seed + 902),
                    &job,
                    &grant.worker_launch_grant_id,
                )
                .unwrap(),
                &instant(T2),
            )
            .unwrap();
        let mut wrong_fence = lease.clone();
        wrong_fence.fencing_token.0 = "2".into();
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .renew_after_accepted_heartbeat(
                    &record,
                    &heartbeat,
                    &wrong_fence,
                    &expiry,
                    &renewal_time
                )
                .unwrap()
                .is_none()
        );
        for forbidden_state in ["recovery_pending_issued", "released"] {
            if forbidden_state == "released" {
                db.execute("UPDATE client_occupancy_leases SET state='released' WHERE occupancy_lease_id=?1", [&fixture.lease_id]).unwrap();
            } else {
                db.execute("UPDATE worker_launch_grants SET state='issued' WHERE worker_launch_grant_id=?1", [&grant.worker_launch_grant_id]).unwrap();
            }
            assert!(
                storage
                    .worker_session_credential_ledger()
                    .unwrap()
                    .renew_after_accepted_heartbeat(
                        &record,
                        &heartbeat,
                        lease,
                        &expiry,
                        &renewal_time
                    )
                    .unwrap()
                    .is_none(),
                "{forbidden_state} must not renew credentials"
            );
            db.execute(
                "UPDATE client_occupancy_leases SET state=?2 WHERE occupancy_lease_id=?1",
                params![fixture.lease_id, occupancy_state],
            )
            .unwrap();
            db.execute(
                "UPDATE worker_launch_grants SET state='consumed' WHERE worker_launch_grant_id=?1",
                [&grant.worker_launch_grant_id],
            )
            .unwrap();
        }
        let renewed = storage
            .worker_session_credential_ledger()
            .unwrap()
            .renew_after_accepted_heartbeat(&record, &heartbeat, lease, &expiry, &renewal_time)
            .unwrap()
            .unwrap();
        assert_eq!(renewed.revision, 2);
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .renew_after_accepted_heartbeat(&record, &heartbeat, lease, &expiry, &renewal_time)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .audit_trail(&record.worker_session_credential_id)
                .unwrap()
                .iter()
                .filter(|entry| entry.action == CredentialAuditAction::Renewed)
                .count(),
            1
        );
        // Rotation changes the active digest, while the consumed grant retains its initial digest.
        let rotated = storage
            .worker_session_credential_ledger()
            .unwrap()
            .rotate(
                &CredentialRotation::try_new(
                    &grant.worker_session_id,
                    id("wcred", seed + 903),
                    format!("sha256:{:064}", 1),
                    expiry.clone(),
                    None,
                )
                .unwrap(),
                &renewal_time,
            )
            .unwrap()
            .issued;
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .renew_after_accepted_heartbeat(
                    &renewed,
                    &heartbeat,
                    lease,
                    &instant(GRANT_EXPIRES),
                    &renewal_time
                )
                .unwrap()
                .is_none()
        );
        let rotated_renewed = storage
            .worker_session_credential_ledger()
            .unwrap()
            .renew_after_accepted_heartbeat(
                &rotated,
                &heartbeat,
                lease,
                &instant(GRANT_EXPIRES),
                &renewal_time,
            )
            .unwrap()
            .unwrap();
        storage
            .worker_session_credential_ledger()
            .unwrap()
            .revoke_for_session(
                &grant.worker_session_id,
                &fixture.holder,
                None,
                &renewal_time,
            )
            .unwrap();
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .renew_after_accepted_heartbeat(
                    &rotated_renewed,
                    &heartbeat,
                    lease,
                    &instant("2026-01-01T01:10:00.000Z"),
                    &renewal_time
                )
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .renew_after_accepted_heartbeat(
                    &rotated_renewed,
                    &heartbeat,
                    lease,
                    &instant("2026-01-01T01:10:00.000Z"),
                    &instant(GRANT_EXPIRES)
                )
                .unwrap()
                .is_none()
        );
    }
    drop(storage);
    let mut reopened = SqliteStorage::open(&root).unwrap();
    let audits = reopened
        .worker_session_credential_ledger()
        .unwrap()
        .audit_trail(&id("wcred", 20000))
        .unwrap();
    assert_eq!(
        audits
            .iter()
            .filter(|entry| entry.action == CredentialAuditAction::Renewed)
            .count(),
        1
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn trusted_exit_replaces_a_running_attempt_in_the_same_server_and_retains_history() {
    exercise_trusted_recovery(false, false);
}

#[test]
fn trusted_exit_replaces_an_unclaimed_route_without_inventing_a_lease() {
    exercise_trusted_recovery(true, false);
}

#[test]
fn trusted_exit_recovers_active_chat_without_changing_the_original_job() {
    exercise_trusted_recovery(false, true);
}

#[test]
fn trusted_exit_recovers_queued_chat_without_manufacturing_a_lease() {
    exercise_trusted_recovery(true, true);
}

#[allow(clippy::too_many_lines)]
fn exercise_trusted_recovery(replace_queued: bool, chat: bool) {
    use winwincode_domain::{ExecutionMessageId, ExecutionSequence, WorkerId, WorkerInstanceId};
    use winwincode_storage::{
        EXECUTION_PROTOCOL_VERSION, ExecutionJobSubmission, RepositorySchedulerClaimRequest,
        RepositorySchedulerScope, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
        WorkerPlatform, WorkerRegistrationRequest,
    };
    let directory = temporary_directory("trusted-replacement");
    let mut storage = SqliteStorage::open(&directory).unwrap();
    let seed = 8000;
    let fixture = seed_fixture(&mut storage, seed);
    let job_id = ExecutionJobId(seed_reservation_with_scope(
        &mut storage,
        seed + 300,
        &fixture.holder,
        chat,
    ));
    let issuance = LaunchGrantIssuance::try_new(
        id("wlg", seed + 100),
        &fixture.node,
        &fixture.instance,
        &fixture.holder,
        &fixture.lease_id,
        fixture.fencing_token,
        &fixture.binding_id,
        id("wsn", seed + 100),
        id("wrk", seed + 100),
        id("wki", seed + 100),
        DIGEST,
        Some(id("psn", seed + 300)),
        (!chat).then(|| WorkRunId(id("wrn", seed + 100))),
        instant(GRANT_EXPIRES),
    )
    .unwrap();
    let mut initial = storage
        .worker_launch_grant_ledger()
        .unwrap()
        .issue(&issuance, &instant(T1))
        .unwrap();
    let consume = |storage: &mut SqliteStorage, grant: &WorkerLaunchGrantRecord| {
        let ack = LaunchAckSettlement::try_new(
            &grant.worker_launch_grant_id,
            &grant.occupancy_lease_id,
            grant.occupancy_fencing_token,
            &grant.worker_session_id,
            &grant.worker_id,
            &grant.worker_instance_id,
            true,
            None,
        )
        .unwrap();
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .settle_launch_ack(&ack, &instant(T2))
            .unwrap();
        storage
            .device_execution_binding_ledger()
            .unwrap()
            .bind(
                &bind_command(
                    9000 + grant.worker_instance_id[4..].parse::<u64>().unwrap(),
                    grant,
                ),
                &instant(T2),
            )
            .unwrap();
    };
    consume(&mut storage, &initial);
    storage
        .device_execution_binding_ledger()
        .unwrap()
        .attach_facts(
            &DeviceExecutionFactsAttachment::try_new_with_role(
                id("req", 9000),
                &job_id.0,
                &initial.worker_launch_grant_id,
                (!chat).then(|| "executor".into()),
            )
            .unwrap(),
            &instant(T2),
        )
        .unwrap();
    let reservation = storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job_id)
        .unwrap()
        .unwrap();
    storage.execution_queue().unwrap().submit(&ExecutionJobSubmission {
        scope:reservation.scope.clone(),job_id:job_id.clone(),request_id:RequestId(id("req",9001)),payload_digest:winwincode_domain::Sha256Digest(format!("sha256:{}","a".repeat(64))),
        dispatch_payload:serde_json::to_vec(&serde_json::json!({
            "jobId":job_id,"attempt":1,"executionProfile":"executor","goal":"trusted recovery fixture",
            "payloadDigest":format!("sha256:{}","a".repeat(64)),
            "limits":{"deadlineAt":null,"maxRuntimeSeconds":null,"maxArtifactBytes":0},
            "workspace":{"repositoryId":reservation.scope.repository_id,"checkoutRevision":"main","writeMode":"read-only"},
            "scope":if chat {serde_json::json!({"kind":"product-session","productSessionId":reservation.scope.product_session_id})}
                else {serde_json::json!({"kind":"work-run","workRunId":initial.work_run_id,"productSessionId":reservation.scope.product_session_id,
                    "attempt":1,"workContractId":id("wct",seed),"workContractRevision":1,"workItemId":id("wit",seed),"workItemRevision":1})}
        })).unwrap(),attempt:1,dependencies:vec![],work_run_id:initial.work_run_id.clone(),submitted_at:instant(T2),
    }).unwrap();
    let register = |storage: &mut SqliteStorage, grant: &WorkerLaunchGrantRecord, n: u64| {
        storage
            .execution_registry()
            .unwrap()
            .register_worker(&WorkerRegistrationRequest {
                authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                    control_plane_principal: "trusted-native-fixture".into(),
                },
                protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["codex".into()],
                capability_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{}",
                    "b".repeat(64)
                )),
                security_zone: "local".into(),
                max_slots: 4,
                message_id: ExecutionMessageId(id("xmsg", n)),
                request_id: RequestId(id("req", n)),
                sent_at: instant(T2),
                started_at: instant(T1),
                worker_id: WorkerId(grant.worker_id.clone()),
                worker_instance_id: WorkerInstanceId(grant.worker_instance_id.clone()),
            })
            .unwrap();
        storage
            .execution_registry()
            .unwrap()
            .record_heartbeat(&WorkerHeartbeatRequest {
                active_leases: vec![],
                available_slots: 4,
                heartbeat_sequence: ExecutionSequence(1),
                max_slots: 4,
                running_slots: 0,
                message_id: ExecutionMessageId(id("xmsg", n + 1)),
                observed_at: instant(T2),
                sent_at: instant(T2),
                worker_id: WorkerId(grant.worker_id.clone()),
                worker_instance_id: WorkerInstanceId(grant.worker_instance_id.clone()),
            })
            .unwrap();
    };
    let scope = RepositorySchedulerScope {
        organization_id: reservation.scope.organization_id.clone(),
        workspace_id: reservation.scope.workspace_id.clone(),
        project_id: reservation.scope.project_id.clone(),
        repository_id: reservation.scope.repository_id.clone(),
    };
    let claim = |grant: &WorkerLaunchGrantRecord, n: u64, issued: &str, expires: &str| {
        RepositorySchedulerClaimRequest {
            scope: scope.clone(),
            request_id: RequestId(id("req", n)),
            scheduler_generation: "same-server-process".into(),
            worker_id: WorkerId(grant.worker_id.clone()),
            worker_instance_id: WorkerInstanceId(grant.worker_instance_id.clone()),
            issued_at: instant(issued),
            expires_at: instant(expires),
        }
    };
    if replace_queued {
        assert!(
            storage
                .worker_launch_grant_ledger()
                .unwrap()
                .observe_trusted_exit(
                    &fixture.node,
                    &initial.worker_session_id,
                    &initial.worker_instance_id,
                    &fixture.lease_id,
                    &instant(T2)
                )
                .unwrap()
        );
        let old = storage
            .device_execution_binding_ledger()
            .unwrap()
            .snapshot(&initial.worker_session_id)
            .unwrap()
            .unwrap();
        storage
            .device_execution_binding_ledger()
            .unwrap()
            .release(
                &DeviceExecutionBindingRelease::try_new(
                    &initial.worker_session_id,
                    id("req", 9010),
                    old.revision,
                    instant(T2),
                )
                .unwrap(),
                &instant(T2),
            )
            .unwrap();
        let route = LaunchGrantIssuance::try_new(
            id("wlg", 8101),
            &fixture.node,
            &fixture.instance,
            &fixture.holder,
            &fixture.lease_id,
            fixture.fencing_token,
            &fixture.binding_id,
            id("wsn", 8101),
            &initial.worker_id,
            id("wki", 8101),
            DIGEST,
            initial.product_session_id.clone(),
            initial.work_run_id.clone(),
            instant(GRANT_EXPIRES),
        )
        .unwrap();
        let grant = storage
            .worker_launch_grant_ledger()
            .unwrap()
            .issue_replacement(&route, Some(&initial.worker_launch_grant_id), &instant(T2))
            .unwrap();
        consume(&mut storage, &grant);
        winwincode_storage::stage_device_execution_recovery(
            &mut storage,
            &job_id,
            &grant.worker_launch_grant_id,
            if chat { None } else { Some("executor") },
            &instant(T2),
        )
        .unwrap();
        assert!(
            storage
                .execution_registry()
                .unwrap()
                .load_lease(&job_id)
                .unwrap()
                .is_none()
        );
        assert!(!storage.execution_has_unknown_predecessor(&job_id).unwrap());
        initial = grant;
    }
    register(&mut storage, &initial, 9100);
    let first = storage
        .repository_scheduler()
        .unwrap()
        .claim_next(&claim(&initial, 9200, T2, T3))
        .unwrap()
        .unwrap();
    assert_eq!(first.lease.attempt, 1);
    let mut predecessor = initial.clone();
    for (n, at, expires) in [
        (1, "2026-01-01T00:03:01.000Z", "2026-01-01T00:04:00.000Z"),
        (2, "2026-01-01T00:04:01.000Z", "2026-01-01T00:05:00.000Z"),
    ] {
        let current = storage
            .device_execution_binding_ledger()
            .unwrap()
            .facts(&job_id.0)
            .unwrap()
            .unwrap();
        assert!(
            storage
                .worker_launch_grant_ledger()
                .unwrap()
                .observe_trusted_exit(
                    &fixture.node,
                    &predecessor.worker_session_id,
                    &predecessor.worker_instance_id,
                    &fixture.lease_id,
                    &instant(T2)
                )
                .unwrap()
        );
        let old_binding = storage
            .device_execution_binding_ledger()
            .unwrap()
            .snapshot(&predecessor.worker_session_id)
            .unwrap()
            .unwrap();
        storage
            .device_execution_binding_ledger()
            .unwrap()
            .release(
                &DeviceExecutionBindingRelease::try_new(
                    &predecessor.worker_session_id,
                    id("req", 9300 + n),
                    old_binding.revision,
                    instant(T2),
                )
                .unwrap(),
                &instant(T2),
            )
            .unwrap();
        let successor_issuance = LaunchGrantIssuance::try_new(
            id("wlg", 8200 + n),
            &fixture.node,
            &fixture.instance,
            &fixture.holder,
            &fixture.lease_id,
            fixture.fencing_token,
            &fixture.binding_id,
            id("wsn", 8200 + n),
            &initial.worker_id,
            id("wki", 8200 + n),
            DIGEST,
            initial.product_session_id.clone(),
            current.work_run_id.clone().map(WorkRunId),
            instant(GRANT_EXPIRES),
        )
        .unwrap();
        let successor = storage
            .worker_launch_grant_ledger()
            .unwrap()
            .issue_replacement(
                &successor_issuance,
                Some(&predecessor.worker_launch_grant_id),
                &instant(T2),
            )
            .unwrap();
        consume(&mut storage, &successor);
        register(&mut storage, &successor, 9400 + n * 10);
        assert!(
            storage
                .repository_scheduler()
                .unwrap()
                .claim_next(&claim(&successor, 9500 + n, T2, expires))
                .unwrap()
                .is_none(),
            "must not replace without ready intent"
        );
        winwincode_storage::stage_device_execution_recovery(
            &mut storage,
            &job_id,
            &successor.worker_launch_grant_id,
            if chat { None } else { Some("executor") },
            &instant(T2),
        )
        .unwrap();
        assert!(
            storage
                .repository_scheduler()
                .unwrap()
                .claim_next(&claim(&successor, 9600 + n, T2, expires))
                .unwrap()
                .is_none(),
            "old lease remains fenced until expiry"
        );
        drop(storage);
        storage = SqliteStorage::open(&directory).unwrap();
        let next = storage
            .repository_scheduler()
            .unwrap()
            .claim_next(&claim(&successor, 9700 + n, at, expires))
            .unwrap()
            .unwrap();
        assert_eq!(next.lease.attempt, 1 + n);
        let facts = storage
            .device_execution_binding_ledger()
            .unwrap()
            .facts(&job_id.0)
            .unwrap()
            .unwrap();
        assert_eq!(
            facts.work_run_id,
            next.job.work_run_id.as_ref().map(|id| id.0.clone())
        );
        assert_eq!(facts.worker_session_id, successor.worker_session_id);
        if chat {
            assert!(facts.role.is_none());
            assert!(facts.work_run_id.is_none());
        } else {
            let old_projection = ProductStateStorage::load_device_binding_facts_for_work_run(
                &storage,
                &job_id,
                &WorkRunId(current.work_run_id.unwrap()),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                old_projection.worker_instance_id,
                predecessor.worker_instance_id
            );
        }

        assert!(storage.execution_has_unknown_predecessor(&job_id).unwrap());
        predecessor = successor;
    }
    assert_eq!(
        storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&job_id)
            .unwrap()
            .unwrap()
            .state,
        winwincode_storage::ExecutionReservationState::Queued,
        "replacement keeps execution admission open"
    );
}

#[test]
fn independent_launch_bundles_reserve_distinct_downlink_positions() {
    let directory = temporary_directory("parallel-launch-bundles");
    let mut storage = SqliteStorage::open(&directory).unwrap();
    let f = seed_fixture(&mut storage, 32000);
    // Compose both launches before either publication reserves a stream position.
    let launches = [32000, 32001].map(|seed| {
        let digest = format!("sha256:{seed:064x}");
        let issuance = LaunchGrantIssuance::try_new(
            id("wlg", seed), &f.node, &f.instance, &f.holder, &f.lease_id,
            f.fencing_token, &f.binding_id, id("wsn", seed), id("wrk", seed),
            id("wki", seed), &digest, Some(id("psn", seed)), None,
            instant(GRANT_EXPIRES),
        ).unwrap();
        let credential = winwincode_storage::CredentialIssuance::try_new(
            id("wcred", seed * 100), id("wsn", seed), id("wrk", seed), id("wki", seed),
            id("wlg", seed), &digest, instant(GRANT_EXPIRES),
        ).unwrap();
        let frame = serde_json::json!({"schemaVersion":"winwincode/v1","kind":"client.worker.launch",
            "messageId":id("msg",seed),"clientNodeId":f.node,"clientInstanceId":f.instance,"occurredAt":T1,
            "payload":{"expectedRevision":1,"idempotencyKey":format!("launch-{seed}"),"occupancyLeaseId":f.lease_id,"occupancyFencingToken":f.fencing_token.to_string(),"launchGrant":{
            "workerLaunchGrantId":id("wlg",seed),"clientNodeId":f.node,"clientInstanceId":f.instance,"occupancyLeaseId":f.lease_id,"occupancyFencingToken":f.fencing_token.to_string(),
            "repositoryBindingId":f.binding_id,"productSessionId":id("psn",seed),"workerSessionId":id("wsn",seed),"workerId":id("wrk",seed),"workerInstanceId":id("wki",seed),"credentialDigest":&digest,"expiresAt":GRANT_EXPIRES,"state":"issued","revision":1}}});
        (issuance, credential, frame)
    });
    for (issuance, credential, frame) in &launches {
        winwincode_storage::issue_worker_launch_bundle(
            &mut storage,
            issuance,
            None,
            credential,
            |sequence| {
                let mut frame = frame.clone();
                frame["sequence"] = sequence.into();
                winwincode_storage::ClientDownlinkAppend::try_new(
                    &f.node,
                    frame["messageId"].as_str().unwrap(),
                    sequence,
                    frame.to_string(),
                )
                .map_err(|_| {
                    winwincode_storage::StorageError::invalid_input("launch frame invalid")
                })
            },
            &instant(T1),
        )
        .expect("independent Session launch must not lose its publication");
    }
    let frames = storage
        .client_downlink_outbox()
        .unwrap()
        .deliverable(&f.node, 0, 4)
        .unwrap();
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    for (frame, seed) in frames.iter().zip([32000, 32001]) {
        let envelope: serde_json::Value = serde_json::from_str(&frame.frame).unwrap();
        assert_eq!(envelope["sequence"], frame.sequence);
        let grant_id = envelope["payload"]["launchGrant"]["workerLaunchGrantId"]
            .as_str()
            .unwrap();
        assert_eq!(grant_id, id("wlg", seed));
        let grant = storage
            .worker_launch_grant_ledger()
            .unwrap()
            .snapshot(grant_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.product_session_id,
            envelope["payload"]["launchGrant"]["productSessionId"]
                .as_str()
                .map(str::to_owned)
        );
        assert!(
            storage
                .worker_session_credential_ledger()
                .unwrap()
                .active_for_session(&grant.worker_session_id)
                .unwrap()
                .is_some()
        );
    }
    let db = rusqlite::Connection::open(storage.database_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM worker_launch_publications",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    drop(db);
    drop(storage);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn launch_bundle_rolls_back_all_public_facts_and_retains_publication_after_ack() {
    let directory = temporary_directory("launch-bundle");
    let mut storage = SqliteStorage::open(&directory).unwrap();
    let seed = 30000;
    let f = seed_fixture(&mut storage, seed);
    let issuance = LaunchGrantIssuance::try_new(
        id("wlg", seed),
        &f.node,
        &f.instance,
        &f.holder,
        &f.lease_id,
        f.fencing_token,
        &f.binding_id,
        id("wsn", seed),
        id("wrk", seed),
        id("wki", seed),
        DIGEST,
        Some(id("psn", seed)),
        None,
        instant(GRANT_EXPIRES),
    )
    .unwrap();
    let credential = winwincode_storage::CredentialIssuance::try_new(
        id("wcred", seed),
        id("wsn", seed),
        id("wrk", seed),
        id("wki", seed),
        id("wlg", seed),
        DIGEST,
        instant(GRANT_EXPIRES),
    )
    .unwrap();
    let frame = serde_json::json!({"schemaVersion":"winwincode/v1","kind":"client.worker.launch","messageId":id("msg",seed),"clientNodeId":f.node,"clientInstanceId":f.instance,"sequence":1,"occurredAt":T1,
        "payload":{"expectedRevision":1,"idempotencyKey":"launch-bundle","occupancyLeaseId":f.lease_id,"occupancyFencingToken":f.fencing_token.to_string(),"launchGrant":{
        "workerLaunchGrantId":id("wlg",seed),"clientNodeId":f.node,"clientInstanceId":f.instance,"occupancyLeaseId":f.lease_id,"occupancyFencingToken":f.fencing_token.to_string(),
        "repositoryBindingId":f.binding_id,"productSessionId":id("psn",seed),"workerSessionId":id("wsn",seed),"workerId":id("wrk",seed),"workerInstanceId":id("wki",seed),"credentialDigest":DIGEST,"expiresAt":GRANT_EXPIRES,"state":"issued","revision":1}}});
    for pointer in [
        "/payload/occupancyFencingToken",
        "/payload/launchGrant/occupancyFencingToken",
    ] {
        let mut numeric = frame.clone();
        *numeric.pointer_mut(pointer).unwrap() = serde_json::json!(f.fencing_token);
        let numeric = winwincode_storage::ClientDownlinkAppend::try_new(
            &f.node,
            id("msg", seed),
            1,
            numeric.to_string(),
        )
        .unwrap();
        assert!(
            winwincode_storage::issue_worker_launch_bundle(
                &mut storage,
                &issuance,
                None,
                &credential,
                |_| Ok(numeric.clone()),
                &instant(T1),
            )
            .is_err(),
            "fencing tokens must use the formal string encoding"
        );
    }
    let mut invalid_frame = frame.clone();
    invalid_frame["sequence"] = serde_json::json!(2);
    let invalid = winwincode_storage::ClientDownlinkAppend::try_new(
        &f.node,
        id("msg", seed),
        2,
        invalid_frame.to_string(),
    )
    .unwrap();
    assert!(
        winwincode_storage::issue_worker_launch_bundle(
            &mut storage,
            &issuance,
            None,
            &credential,
            |_| Ok(invalid.clone()),
            &instant(T1)
        )
        .is_err()
    );
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .snapshot(&id("wlg", seed))
            .unwrap()
            .is_none()
    );
    assert!(
        storage
            .worker_session_credential_ledger()
            .unwrap()
            .active_for_session(&id("wsn", seed))
            .unwrap()
            .is_none()
    );
    {
        let db = rusqlite::Connection::open(storage.database_path()).unwrap();
        for table in [
            "worker_launch_grant_audit",
            "worker_session_credentials_audit",
            "client_downlink_frames",
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "failed final append rolls back every ledger"
            );
        }
        db.execute(
            "UPDATE client_nodes SET current_instance_id=?2 WHERE client_node_id=?1",
            rusqlite::params![f.node, id("cix", seed + 100)],
        )
        .unwrap();
    }
    let stale = winwincode_storage::ClientDownlinkAppend::try_new(
        &f.node,
        id("msg", seed),
        1,
        frame.to_string(),
    )
    .unwrap();
    assert!(
        matches!(
            winwincode_storage::issue_worker_launch_bundle(
                &mut storage,
                &issuance,
                None,
                &credential,
                |_| Ok(stale.clone()),
                &instant(T1)
            ),
            Err(winwincode_storage::WorkerLaunchBundleError::Launch(_))
        ),
        "Device reboot CAS is checked inside bundle transaction"
    );
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .snapshot(&id("wlg", seed))
            .unwrap()
            .is_none()
    );
    rusqlite::Connection::open(storage.database_path())
        .unwrap()
        .execute(
            "UPDATE client_nodes SET current_instance_id=?2 WHERE client_node_id=?1",
            rusqlite::params![f.node, f.instance],
        )
        .unwrap();
    let valid = winwincode_storage::ClientDownlinkAppend::try_new(
        &f.node,
        id("msg", seed),
        1,
        frame.to_string(),
    )
    .unwrap();
    winwincode_storage::issue_worker_launch_bundle(
        &mut storage,
        &issuance,
        None,
        &credential,
        |_| Ok(valid.clone()),
        &instant(T1),
    )
    .unwrap();
    storage
        .client_downlink_outbox()
        .unwrap()
        .retain_through(&f.node, 1)
        .unwrap();
    drop(storage);
    let reopened = SqliteStorage::open(&directory).unwrap();
    let connection = rusqlite::Connection::open(reopened.database_path()).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM worker_launch_publications WHERE grant_id=?1",
                [id("wlg", seed)],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    drop(connection);
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn process_closure_is_exact_and_survives_multiple_device_reboots() {
    use winwincode_storage::DeviceLaunchProcessClosure;
    let directory = temporary_directory("closure-reboots");
    let mut storage = SqliteStorage::open(&directory).unwrap();
    let seed = 31000;
    let mut f = seed_fixture(&mut storage, seed);
    let grant = issue_launch_grant(&mut storage, seed, &f);
    let ack = LaunchAckSettlement::try_new(
        &grant.worker_launch_grant_id,
        &grant.occupancy_lease_id,
        grant.occupancy_fencing_token,
        &grant.worker_session_id,
        &grant.worker_id,
        &grant.worker_instance_id,
        true,
        None,
    )
    .unwrap();
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .settle_device_launch_ack(&id("cnd", seed + 999), &ack, &instant(T2))
            .is_err()
    );
    assert_eq!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .snapshot(&grant.worker_launch_grant_id)
            .unwrap()
            .unwrap()
            .state,
        winwincode_storage::WorkerLaunchGrantState::Issued
    );
    storage
        .worker_launch_grant_ledger()
        .unwrap()
        .settle_device_launch_ack(&f.node, &ack, &instant(T2))
        .unwrap();
    let revision = storage
        .client_node_registry()
        .unwrap()
        .snapshot(&f.node)
        .unwrap()
        .unwrap()
        .revision;
    storage
        .client_node_registry()
        .unwrap()
        .update_presence(&f.node, ClientPresenceState::Offline, revision)
        .unwrap();
    storage
        .client_occupancy_ledger()
        .unwrap()
        .mark_recovery_pending(&f.lease_id, &instant(GRANT_EXPIRES), &instant(T2))
        .unwrap();
    let mut closure = DeviceLaunchProcessClosure {
        worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
        client_instance_id: f.instance.clone(),
        reporting_client_instance_id: id("cix", seed + 20),
        occupancy_fencing_token: f.fencing_token,
        never_started: true,
        process_boot_digest: None,
    };
    let update = |storage: &SqliteStorage, instance: &str| {
        let db = rusqlite::Connection::open(storage.database_path()).unwrap();
        db.execute(
            "UPDATE client_nodes SET current_instance_id=?2 WHERE client_node_id=?1",
            rusqlite::params![f.node, instance],
        )
        .unwrap();
    };
    update(&storage, &closure.reporting_client_instance_id);
    assert!(
        !storage
            .worker_launch_grant_ledger()
            .unwrap()
            .observe_device_process_closure(
                &f.node,
                &grant.worker_session_id,
                &grant.worker_instance_id,
                &f.lease_id,
                &closure,
                &instant(T2)
            )
            .unwrap(),
        "consumed worker cannot be declared never started"
    );
    closure.never_started = false;
    closure.process_boot_digest = Some(format!("sha256:{}", "a".repeat(64)));
    let mut wrong = closure.clone();
    wrong.occupancy_fencing_token += 1;
    assert!(
        !storage
            .worker_launch_grant_ledger()
            .unwrap()
            .observe_device_process_closure(
                &f.node,
                &grant.worker_session_id,
                &grant.worker_instance_id,
                &f.lease_id,
                &wrong,
                &instant(T2)
            )
            .unwrap()
    );
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .observe_device_process_closure(
                &f.node,
                &grant.worker_session_id,
                &grant.worker_instance_id,
                &f.lease_id,
                &closure,
                &instant(T2)
            )
            .unwrap()
    );
    drop(storage);
    storage = SqliteStorage::open(&directory).unwrap();
    closure.reporting_client_instance_id = id("cix", seed + 21);
    update(&storage, &closure.reporting_client_instance_id);
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .observe_device_process_closure(
                &f.node,
                &grant.worker_session_id,
                &grant.worker_instance_id,
                &f.lease_id,
                &closure,
                &instant(T3)
            )
            .unwrap()
    );
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .closed_for_device_instance(
                &grant.worker_launch_grant_id,
                &closure.reporting_client_instance_id
            )
            .unwrap()
    );
    let mut changed = closure.clone();
    changed.process_boot_digest = Some(format!("sha256:{}", "b".repeat(64)));
    assert!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .observe_device_process_closure(
                &f.node,
                &grant.worker_session_id,
                &grant.worker_instance_id,
                &f.lease_id,
                &changed,
                &instant(T3)
            )
            .is_err()
    );
    let revision = storage
        .client_node_registry()
        .unwrap()
        .snapshot(&f.node)
        .unwrap()
        .unwrap()
        .revision;
    storage
        .client_node_registry()
        .unwrap()
        .update_presence(&f.node, ClientPresenceState::Online, revision)
        .unwrap();
    storage
        .client_occupancy_ledger()
        .unwrap()
        .reconcile_resume(
            &f.lease_id,
            winwincode_storage::OccupancyReconcileTarget::ResumeOccupied,
            None,
            &instant(T3),
        )
        .unwrap();
    f.instance = closure.reporting_client_instance_id;
    let successor = LaunchGrantIssuance::try_new(
        id("wlg", seed + 1),
        &f.node,
        &f.instance,
        &f.holder,
        &f.lease_id,
        f.fencing_token,
        &f.binding_id,
        id("wsn", seed + 1),
        &grant.worker_id,
        id("wki", seed + 1),
        DIGEST,
        grant.product_session_id.clone(),
        grant.work_run_id.clone(),
        instant(GRANT_EXPIRES),
    )
    .unwrap();
    storage
        .worker_launch_grant_ledger()
        .unwrap()
        .issue_replacement(
            &successor,
            Some(&grant.worker_launch_grant_id),
            &instant(T3),
        )
        .unwrap();
    drop(storage);
    std::fs::remove_dir_all(directory).unwrap();
}
