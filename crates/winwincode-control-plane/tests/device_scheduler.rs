// SPDX-License-Identifier: Apache-2.0

//! `DeviceSchedulerService` two-phase scheduling verticals (plan FLOW-100.2):
//! phase one validates the holder, occupancy semantics, dual authorization,
//! and the durable capacity, then reserves exactly one free slot; phase two
//! issues the Worker request through the frozen launch grant gate. Every
//! refusal and every failure path must leave the durable capacity unchanged.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    ClientRegistryService, DeviceSchedulerService, DeviceSchedulerServiceErrorKind,
    DeviceWorkerSchedulingRequest, OccupancyLeaseState, RepositoryAccessGrantService,
};
use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, DeviceSchedulerReleaseReason,
    DeviceSchedulerReservationRequest, DeviceSchedulerReservationState, GrantPermissions,
    GrantSource, GrantTrustMode, LaunchGrantIssuance, OccupancyClaim,
    RepositoryAccessGrantIssuance, RepositoryAvailability, RepositoryBindingProjection,
    RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    // Wall-clock nanos keep the directory unique even when the operating
    // system reuses a previous run's process id.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-device-scheduler-{name}-{}-{suffix}-{nanos}",
        std::process::id()
    ))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn public_client_id(seed: u64) -> String {
    format!("{seed:012}")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T3: &str = "2026-01-01T00:03:00.000Z";
const GRANT_EXPIRES: &str = "2026-01-01T01:00:00.000Z";

const DIGEST: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// Every identity one scheduling vertical needs against one client node.
#[derive(Clone)]
struct Fixture {
    directory: PathBuf,
    node: String,
    instance: String,
    holder: String,
    lease_id: String,
    fencing_token: u64,
    binding: String,
}

/// Seeds one `online` client node with `capacity` worker-session slots, a
/// `use` access grant for `holder`, an occupancy lease (device-confirmed
/// when `confirmed`), and one visible repository binding.
fn seed_fixture(seed: u64, holder: &str, capacity: u32, confirmed: bool) -> Fixture {
    let directory = temporary_directory(&format!("fixture-{seed}"));
    let node = id("cnd", seed);
    let instance = id("cix", seed + 2);
    let mut storage = open(&directory);
    {
        let registration = ClientNodeRegistration::try_new(
            node.clone(),
            public_client_id(seed),
            format!("Scheduler Device {seed}"),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(instance.clone()),
            capacity,
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
            holder,
            holder,
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
                &instant(T0),
            )
            .expect("grant");
    }
    let (lease_id, fencing_token) = {
        let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
        let claim =
            OccupancyClaim::try_new(id("ocl", seed + 4), &node, holder, id("req", seed + 5))
                .expect("claim");
        let lease = occupancy.atomic_claim(&claim, &instant(T1)).expect("claim");
        if confirmed {
            let occupied = occupancy
                .record_acknowledgement(
                    &lease.occupancy_lease_id,
                    lease.fencing_token,
                    None,
                    &instant(T1),
                )
                .expect("ack");
            assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
            (occupied.occupancy_lease_id, occupied.fencing_token)
        } else {
            assert_eq!(lease.state, OccupancyLeaseState::Reserving);
            (lease.occupancy_lease_id, lease.fencing_token)
        }
    };
    let binding = seed_visible_binding(&mut storage, seed, &node, holder);
    Fixture {
        directory,
        node,
        instance,
        holder: holder.to_owned(),
        lease_id,
        fencing_token,
        binding,
    }
}

/// Seeds one repository binding of `node` with an active `Use` grant for
/// `holder`, so the dual-authorization visibility gate accepts it.
fn seed_visible_binding(
    storage: &mut SqliteStorage,
    seed: u64,
    node: &str,
    holder: &str,
) -> String {
    let binding = id("rbd", seed + 6);
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
        .upsert(&projection, None, 0, &instant(T0))
        .expect("upsert");
    let issuance =
        RepositoryAccessGrantIssuance::try_new(id("rag", seed + 7), &binding, holder, holder)
            .expect("repo issuance");
    ledger
        .create_grant(&issuance, RepositoryGrantPermissions::Use, &instant(T0))
        .expect("repo grant");
    binding
}

fn open(directory: &PathBuf) -> SqliteStorage {
    SqliteStorage::open(directory).expect("storage")
}

fn scheduling_request(seed: u64, fixture: &Fixture) -> DeviceWorkerSchedulingRequest {
    DeviceWorkerSchedulingRequest::try_new(
        id("req", seed),
        fixture.holder.clone(),
        public_client_id_for(fixture),
        fixture.binding.clone(),
        id("ws", seed + 8),
        id("wkr", seed + 9),
        id("winst", seed + 10),
        DIGEST,
        Some(id("ps", seed + 11)),
        Some(id("run", seed + 12)),
        instant(GRANT_EXPIRES),
    )
    .expect("scheduling request")
}

fn scheduling_request_for(
    seed: u64,
    fixture: &Fixture,
    holder: &str,
) -> DeviceWorkerSchedulingRequest {
    DeviceWorkerSchedulingRequest::try_new(
        id("req", seed),
        holder,
        public_client_id_for(fixture),
        fixture.binding.clone(),
        id("ws", seed + 8),
        id("wkr", seed + 9),
        id("winst", seed + 10),
        DIGEST,
        Some(id("ps", seed + 11)),
        Some(id("run", seed + 12)),
        instant(GRANT_EXPIRES),
    )
    .expect("scheduling request")
}

/// The public Client ID of the fixture's node (12 digits, matching the seed).
fn public_client_id_for(fixture: &Fixture) -> String {
    let seed = fixture
        .node
        .strip_prefix("cnd_")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .expect("fixture node id shape");
    public_client_id(seed)
}

/// Directly issues one launch grant through the frozen ledger, bypassing the
/// scheduler — the crash-orphan and phase-two-conflict fixtures.
fn seed_orphan_grant(
    storage: &mut SqliteStorage,
    seed: u64,
    fixture: &Fixture,
    worker_session: &str,
    worker: &str,
    worker_instance: &str,
) -> String {
    let issuance = LaunchGrantIssuance::try_new(
        id("wlg", seed),
        fixture.node.clone(),
        fixture.instance.clone(),
        fixture.holder.clone(),
        fixture.lease_id.clone(),
        fixture.fencing_token,
        fixture.binding.clone(),
        worker_session.to_owned(),
        worker.to_owned(),
        worker_instance.to_owned(),
        DIGEST,
        None,
        None,
        instant(GRANT_EXPIRES),
    )
    .expect("orphan issuance");
    storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .issue(&issuance, &instant(T2))
        .expect("orphan grant")
        .worker_launch_grant_id
}

fn non_terminal_grant_count(storage: &mut SqliteStorage, node: &str) -> u64 {
    storage
        .device_execution_binding_ledger()
        .expect("ledger")
        .capacity_snapshot(node)
        .expect("capacity")
        .expect("node")
        .reserved_worker_sessions
}

fn stored_reservation(
    storage: &mut SqliteStorage,
    request_id: &str,
) -> Option<winwincode_storage::DeviceSchedulerReservationRecord> {
    storage
        .device_scheduler_reservation_ledger()
        .expect("ledger")
        .snapshot_by_request(request_id)
        .expect("snapshot")
}

#[test]
fn the_two_phase_schedule_reserves_a_slot_then_issues_the_worker_request() {
    let seed = 100;
    let fixture = seed_fixture(seed, &id("usr", seed + 1), 2, true);
    let request = scheduling_request(seed + 20, &fixture);
    let mut storage = open(&fixture.directory);
    let receipt = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect("schedule");
    assert_eq!(receipt.request_id, id("req", seed + 20));
    assert_eq!(receipt.client_node_id, fixture.node);
    assert_eq!(receipt.holder_user_id, fixture.holder);
    assert_eq!(receipt.occupancy_lease_id, fixture.lease_id);
    assert_eq!(receipt.occupancy_fencing_token, fixture.fencing_token);
    assert_eq!(receipt.repository_binding_id, fixture.binding);
    assert_eq!(receipt.worker_session_id, id("ws", seed + 28));
    assert_eq!(receipt.worker_id, id("wkr", seed + 29));
    assert_eq!(receipt.worker_instance_id, id("winst", seed + 30));
    assert_eq!(receipt.product_session_id, Some(id("ps", seed + 31)));
    assert_eq!(receipt.stage_run_id, Some(id("run", seed + 32)));
    assert!(!receipt.replayed);
    // Phase one left exactly one reservation and it settled onto the grant.
    let reservation = stored_reservation(&mut storage, &id("req", seed + 20)).expect("reservation");
    assert_eq!(reservation.state, DeviceSchedulerReservationState::Granted);
    assert_eq!(
        reservation.launch_grant_id.as_deref(),
        Some(receipt.worker_launch_grant_id.as_str())
    );
    // Phase two issued exactly one live grant: the durable capacity view.
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 1);
    // The holder can schedule a second concurrent WorkerSession on the same
    // device while the capacity lasts.
    let second = scheduling_request(seed + 40, &fixture);
    DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&second, &instant(T2))
        .expect("second concurrent session");
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 2);
}

#[test]
fn a_client_without_confirmed_occupancy_is_never_dispatched() {
    let seed = 200;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 2, true);
    let mut storage = open(&fixture.directory);
    // Drop the lease entirely: nothing is occupied, so nothing is dispatched.
    storage
        .client_occupancy_ledger()
        .expect("ledger")
        .request_release(&fixture.lease_id, fixture.fencing_token, 0, &instant(T2))
        .expect("release");
    let request = scheduling_request(seed + 20, &fixture);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("an unoccupied client must not be dispatched");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::OccupancyRequired
    );
    assert!(
        stored_reservation(&mut storage, &id("req", seed + 20)).is_none(),
        "a refusal must not reserve anything"
    );
    // A merely `reserving` (not device-confirmed) lease is equally not
    // dispatchable.
    let seed = seed + 50;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 2, false);
    let mut storage = open(&fixture.directory);
    let request = scheduling_request(seed + 20, &fixture);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("only confirmed occupancy may dispatch");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::OccupancyNotConfirmed
    );
    assert!(stored_reservation(&mut storage, &id("req", seed + 20)).is_none());
}

#[test]
fn a_non_holder_and_an_unreachable_client_are_refused() {
    let seed = 300;
    let holder = id("usr", seed + 1);
    let outsider = id("usr", seed + 13);
    let fixture = seed_fixture(seed, &holder, 2, true);
    let mut storage = open(&fixture.directory);
    let request = scheduling_request_for(seed + 20, &fixture, &outsider);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("only the occupancy holder may schedule");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::NotLeaseHolder
    );
    assert!(
        stored_reservation(&mut storage, &id("req", seed + 20)).is_none(),
        "a refusal must not reserve anything"
    );
    // The holder is refused once the device drops offline.
    let revision = {
        let mut registry = ClientRegistryService::new(&mut storage);
        registry
            .snapshot(&fixture.node)
            .expect("snapshot")
            .expect("node")
            .revision
    };
    {
        let mut registry = ClientRegistryService::new(&mut storage);
        registry
            .update_presence(&fixture.node, ClientPresenceState::Offline, revision)
            .expect("presence");
    }
    let request = scheduling_request(seed + 40, &fixture);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("an offline client is not schedulable");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::PresenceNotOnline
    );
    // An unknown public Client ID names no launchable client.
    let request = DeviceWorkerSchedulingRequest::try_new(
        id("req", seed + 60),
        &holder,
        "999999999",
        fixture.binding.clone(),
        id("ws", seed + 68),
        id("wkr", seed + 69),
        id("winst", seed + 70),
        DIGEST,
        None,
        None,
        instant(GRANT_EXPIRES),
    )
    .expect("request");
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("an unknown client is not schedulable");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::UnknownClientNode
    );
}

#[test]
fn foreign_unauthorized_and_unknown_bindings_are_refused_uniformly() {
    let seed = 400;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 2, true);
    // A binding of a different client node, visible to the same holder there.
    let other = seed_fixture(seed + 50, &holder, 2, true);
    let mut storage = open(&fixture.directory);
    let request = DeviceWorkerSchedulingRequest::try_new(
        id("req", seed + 20),
        &holder,
        public_client_id_for(&fixture),
        other.binding.clone(),
        id("ws", seed + 28),
        id("wkr", seed + 29),
        id("winst", seed + 30),
        DIGEST,
        None,
        None,
        instant(GRANT_EXPIRES),
    )
    .expect("request");
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("a binding of another client is not visible here");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::BindingNotVisible
    );
    // A known binding whose repository grant was revoked is equally invisible.
    {
        let mut grants = RepositoryAccessGrantService::new(&mut storage);
        let grant = grants
            .active_grants_for_user(&holder)
            .expect("grants")
            .into_iter()
            .find(|grant| grant.repository_binding_id == fixture.binding)
            .expect("binding grant");
        grants
            .revoke_grant(&grant.repository_access_grant_id, grant.revision)
            .expect("revoke");
    }
    let request = scheduling_request(seed + 40, &fixture);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("a revoked binding is not visible");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::BindingNotVisible
    );
    // An unknown binding id gets the same uniform refusal.
    let request = DeviceWorkerSchedulingRequest::try_new(
        id("req", seed + 60),
        &holder,
        public_client_id_for(&fixture),
        id("rbd", seed + 61),
        id("ws", seed + 68),
        id("wkr", seed + 69),
        id("winst", seed + 70),
        DIGEST,
        None,
        None,
        instant(GRANT_EXPIRES),
    )
    .expect("request");
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("an unknown binding is not visible");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::BindingNotVisible
    );
    assert!(stored_reservation(&mut storage, &id("req", seed + 60)).is_none());
}

#[test]
fn concurrent_schedules_never_oversell_the_durable_capacity() {
    let seed = 500;
    let capacity = 3;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, capacity, true);
    let attempts: Vec<u64> = (0..12).map(|offset| seed + 20 + offset * 10).collect();
    let handles: Vec<_> = attempts
        .iter()
        .map(|&attempt_seed| {
            let directory = fixture.directory.clone();
            let fixture = fixture.clone();
            std::thread::spawn(move || {
                let mut storage = open(&directory);
                let request = scheduling_request(attempt_seed, &fixture);
                match DeviceSchedulerService::new(&mut storage)
                    .select_and_request_worker(&request, &instant(T2))
                {
                    Ok(receipt) => Ok(receipt.worker_launch_grant_id),
                    Err(error) => Err(error.kind()),
                }
            })
        })
        .collect();
    let mut granted = Vec::new();
    let mut exhausted = 0;
    for handle in handles {
        match handle.join().expect("thread") {
            Ok(grant_id) => granted.push(grant_id),
            Err(DeviceSchedulerServiceErrorKind::CapacityExhausted) => exhausted += 1,
            Err(other) => panic!("unexpected scheduling failure: {other:?}"),
        }
    }
    let expected = usize::try_from(capacity).expect("small capacity");
    assert_eq!(granted.len(), expected);
    assert_eq!(exhausted, attempts.len() - expected);
    granted.sort();
    granted.dedup();
    assert_eq!(granted.len(), expected);
    let mut storage = open(&fixture.directory);
    assert_eq!(
        non_terminal_grant_count(&mut storage, &fixture.node),
        u64::from(capacity)
    );
}

#[test]
fn a_failed_phase_two_releases_the_reservation_immediately() {
    let seed = 600;
    let holder = id("usr", seed + 1);
    // Capacity 2: the orphan grant holds one slot, the scheduler the other.
    let fixture = seed_fixture(seed, &holder, 2, true);
    let mut storage = open(&fixture.directory);
    // A live grant already carries the requested worker session but for a
    // different worker identity: phase two must refuse without adopting it.
    seed_orphan_grant(
        &mut storage,
        seed + 15,
        &fixture,
        &id("ws", seed + 28),
        &id("wkr", seed + 79),
        &id("winst", seed + 80),
    );
    let request = scheduling_request(seed + 20, &fixture);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("a conflicting live grant must refuse the launch");
    assert_eq!(error.kind(), DeviceSchedulerServiceErrorKind::LaunchRefused);
    // The reservation was released durably: the scheduler holds no slot.
    let reservation = stored_reservation(&mut storage, &id("req", seed + 20)).expect("reservation");
    assert_eq!(reservation.state, DeviceSchedulerReservationState::Released);
    assert_eq!(
        reservation.release_reason,
        Some(DeviceSchedulerReleaseReason::LaunchFailed)
    );
    assert_eq!(
        storage
            .device_scheduler_reservation_ledger()
            .expect("ledger")
            .pending_count_for_node(&fixture.node)
            .expect("pending"),
        0
    );
    // The released slot is reusable by a fresh request.
    let request = scheduling_request(seed + 40, &fixture);
    DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect("the released slot is reusable");
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 2);
}

#[test]
fn an_orphan_grant_of_a_crashed_attempt_is_adopted_idempotently() {
    let seed = 700;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 1, true);
    let mut storage = open(&fixture.directory);
    // Touch the launch grant ledger so the durable capacity view exists on
    // this fresh database (the order production flows open their ledgers in).
    storage.worker_launch_grant_ledger().expect("ledger");
    // Simulate the crash window exactly: phase one reserved the slot, phase
    // two issued the grant with the exact request identities, and the
    // scheduler died before the settlement.
    let crashed = DeviceSchedulerReservationRequest::try_new(
        id("dsr", seed + 15),
        id("req", seed + 20),
        fixture.node.clone(),
        holder.clone(),
        fixture.lease_id.clone(),
        fixture.fencing_token,
        fixture.binding.clone(),
        id("ws", seed + 28),
    )
    .expect("reservation command");
    storage
        .device_scheduler_reservation_ledger()
        .expect("ledger")
        .reserve(&crashed, &instant(T0))
        .expect("reserve");
    let orphan = seed_orphan_grant(
        &mut storage,
        seed + 15,
        &fixture,
        &id("ws", seed + 28),
        &id("wkr", seed + 29),
        &id("winst", seed + 30),
    );
    let request = scheduling_request(seed + 20, &fixture);
    let receipt = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect("the crashed attempt is recovered");
    assert!(receipt.replayed);
    assert_eq!(receipt.worker_launch_grant_id, orphan);
    let reservation = stored_reservation(&mut storage, &id("req", seed + 20)).expect("reservation");
    assert_eq!(reservation.state, DeviceSchedulerReservationState::Granted);
    // No second slot was issued: the crash recovery stays at-most-once.
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 1);
}

#[test]
fn replaying_one_request_identity_returns_the_original_receipt() {
    let seed = 800;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 2, true);
    let mut storage = open(&fixture.directory);
    let request = scheduling_request(seed + 20, &fixture);
    let first = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect("schedule");
    let replay = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T3))
        .expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.worker_launch_grant_id, first.worker_launch_grant_id);
    assert_eq!(
        replay.device_scheduler_reservation_id,
        first.device_scheduler_reservation_id
    );
    // The replay issued no second slot.
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 1);
    // The same identity with a different body is a conflict, not a replay.
    let conflicting = DeviceWorkerSchedulingRequest::try_new(
        id("req", seed + 20),
        &holder,
        public_client_id_for(&fixture),
        fixture.binding.clone(),
        id("ws", seed + 48),
        id("wkr", seed + 49),
        id("winst", seed + 50),
        DIGEST,
        None,
        None,
        instant(GRANT_EXPIRES),
    )
    .expect("request");
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&conflicting, &instant(T3))
        .expect_err("a reused request identity with another body must conflict");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::RequestConflict
    );
}

#[test]
fn rolling_a_launch_back_frees_the_slot_after_a_timeout() {
    let seed = 900;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 1, true);
    let mut storage = open(&fixture.directory);
    let request = scheduling_request(seed + 20, &fixture);
    let receipt = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect("schedule");
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 1);
    // The bounded launch flow times out and rolls the attempt back: the
    // issued grant is revoked, which frees the durable slot.
    let reservation = DeviceSchedulerService::new(&mut storage)
        .rollback_launch(&id("req", seed + 20), &instant(T3))
        .expect("rollback");
    assert_eq!(reservation.state, DeviceSchedulerReservationState::Granted);
    let grant = storage
        .worker_launch_grant_ledger()
        .expect("ledger")
        .snapshot(&receipt.worker_launch_grant_id)
        .expect("snapshot")
        .expect("grant");
    assert_eq!(grant.state.as_str(), "revoked");
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 0);
    // Rolling back again is an idempotent no-op; an unknown request is
    // refused without a durable change.
    DeviceSchedulerService::new(&mut storage)
        .rollback_launch(&id("req", seed + 20), &instant(T3))
        .expect("idempotent rollback");
    let error = DeviceSchedulerService::new(&mut storage)
        .rollback_launch(&id("req", seed + 21), &instant(T3))
        .expect_err("an unknown request cannot roll back");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::UnknownReservation
    );
    // The freed slot schedules a replacement worker session.
    let request = scheduling_request(seed + 40, &fixture);
    DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T3))
        .expect("replacement schedule");
    assert_eq!(non_terminal_grant_count(&mut storage, &fixture.node), 1);
}

#[test]
fn the_sweep_reclaims_reservations_a_crashed_scheduler_never_settled() {
    let seed = 1000;
    let holder = id("usr", seed + 1);
    let fixture = seed_fixture(seed, &holder, 1, true);
    let mut storage = open(&fixture.directory);
    // Touch the launch grant ledger so the durable capacity view exists on
    // this fresh database (the order production flows open their ledgers in).
    storage.worker_launch_grant_ledger().expect("ledger");
    // A scheduler reserved a slot at T0 and died before phase two.
    let command = DeviceSchedulerReservationRequest::try_new(
        id("dsr", seed + 15),
        id("req", seed + 20),
        fixture.node.clone(),
        holder.clone(),
        fixture.lease_id.clone(),
        fixture.fencing_token,
        fixture.binding.clone(),
        id("ws", seed + 28),
    )
    .expect("reservation command");
    let pending = storage
        .device_scheduler_reservation_ledger()
        .expect("ledger")
        .reserve(&command, &instant(T0))
        .expect("reserve");
    assert!(matches!(
        pending,
        winwincode_storage::DeviceSchedulerReserveOutcome::Reserved(_)
    ));
    // The stale reservation holds the slot: a fresh schedule is exhausted.
    let request = scheduling_request(seed + 40, &fixture);
    let error = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect_err("the crashed scheduler still holds the slot");
    assert_eq!(
        error.kind(),
        DeviceSchedulerServiceErrorKind::CapacityExhausted
    );
    // The boundary sweeps: the stale reservation is reclaimed as expired.
    let reclaimed = DeviceSchedulerService::new(&mut storage)
        .expire_stale_reservations(&instant(T1))
        .expect("sweep");
    assert_eq!(reclaimed, vec![id("dsr", seed + 15)]);
    let stale = stored_reservation(&mut storage, &id("req", seed + 20)).expect("reservation");
    assert_eq!(stale.state, DeviceSchedulerReservationState::Released);
    assert_eq!(
        stale.release_reason,
        Some(DeviceSchedulerReleaseReason::Expired)
    );
    // The freed slot schedules, and the dead request identity stays closed.
    let request = scheduling_request(seed + 40, &fixture);
    DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&request, &instant(T2))
        .expect("the reclaimed slot is reusable");
    let replay = DeviceSchedulerService::new(&mut storage)
        .select_and_request_worker(&scheduling_request(seed + 20, &fixture), &instant(T2))
        .expect_err("the dead request identity must not resurrect");
    assert_eq!(
        replay.kind(),
        DeviceSchedulerServiceErrorKind::ReservationNotOpen
    );
}
