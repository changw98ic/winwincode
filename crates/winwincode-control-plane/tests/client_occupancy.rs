// SPDX-License-Identifier: Apache-2.0

//! `ClientOccupancyService` vertical tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    ClientOccupancyService, ClientOccupancyServiceErrorKind, OccupancyLeaseState,
};
use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, OccupancyClaim, OccupancyLeaseRecord, OccupancyReconcileTarget,
    OccupancyReleaseReason, SqliteStorage,
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
        "winwincode-client-occupancy-service-{name}-{}-{suffix}-{nanos}",
        std::process::id()
    ))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn node_id(seed: u64) -> String {
    format!("cnd_{seed:026}")
}

fn instance_id(seed: u64) -> String {
    format!("cix_{seed:026}")
}

fn lease_id(seed: u64) -> String {
    format!("ocl_{seed:026}")
}

fn grant_id(seed: u64) -> String {
    format!("cag_{seed:026}")
}

fn user_id(seed: u64) -> String {
    format!("usr_{seed:026}")
}

fn request_id(seed: u64) -> String {
    format!("req_{seed:026}")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T4: &str = "2026-01-01T00:04:00.000Z";

/// Seeds one registered, `online` client node with a `use` grant for `holder`.
fn seed_client_with_holder(storage: &mut SqliteStorage, seed: u64, holder: &str) -> String {
    let registration = ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:012}"),
        format!("Device {seed}"),
        "aarch64-unknown-linux-gnu",
        "aarch64",
        "1.0.0",
        None,
        Some(instance_id(seed)),
        2,
    )
    .expect("registration");
    {
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &instant(T0))
            .expect("register");
        registry
            .update_presence(node_id(seed).as_str(), ClientPresenceState::Online, 1)
            .expect("presence online");
    }
    grant_use_to_holder(storage, seed, node_id(seed).as_str(), holder);
    node_id(seed)
}

/// Adds one administrator `use` grant for `holder` on `client`.
fn grant_use_to_holder(storage: &mut SqliteStorage, grant_seed: u64, client: &str, holder: &str) {
    let issuance = AccessGrantIssuance::try_new(
        grant_id(grant_seed),
        client,
        holder,
        holder,
        GrantTrustMode::Trusted,
        None,
    )
    .expect("issuance");
    storage
        .client_connect_ledger()
        .expect("connect ledger")
        .create_grant(
            &issuance,
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        )
        .expect("grant");
}

fn set_presence(storage: &mut SqliteStorage, client: &str, target: ClientPresenceState) {
    let mut registry = storage.client_node_registry().expect("registry");
    let revision = registry
        .snapshot(client)
        .expect("snapshot")
        .expect("client node")
        .revision;
    registry
        .update_presence(client, target, revision)
        .expect("presence update");
}

fn claim(seed: u64, client: &str, holder: &str) -> OccupancyClaim {
    OccupancyClaim::try_new(lease_id(seed), client, holder, request_id(seed)).expect("claim")
}

fn expect_kind(
    error: &winwincode_control_plane::ClientOccupancyServiceError,
    kind: ClientOccupancyServiceErrorKind,
) {
    assert_eq!(error.kind(), kind);
}

/// One online client whose `use` grants cover both `holder` and `other`: the
/// world every recovery scenario starts from.
struct RecoveryWorld {
    storage: SqliteStorage,
    client: String,
    holder: String,
    other: String,
}

fn seed_recovery_world(name: &str) -> RecoveryWorld {
    let mut storage = SqliteStorage::open(temporary_directory(name)).expect("storage");
    let holder = user_id(2);
    let other = user_id(3);
    let client = seed_client_with_holder(&mut storage, 1, &holder);
    grant_use_to_holder(&mut storage, 5, client.as_str(), other.as_str());
    RecoveryWorld {
        storage,
        client,
        holder,
        other,
    }
}

/// Claims and ACKs an occupied lease for the holder, loses the heartbeat so
/// the registry projects the node offline, then marks the lease
/// `RecoveryPending` at `T4`.
fn seed_recovery_pending(world: &mut RecoveryWorld) -> OccupancyLeaseRecord {
    {
        let mut service = ClientOccupancyService::new(&mut world.storage);
        let reserved = service
            .atomic_claim(&claim(21, &world.client, &world.holder), &instant(T0))
            .expect("claim");
        service
            .record_acknowledgement(
                reserved.occupancy_lease_id.as_str(),
                reserved.fencing_token,
                None,
                &instant(T1),
            )
            .expect("ack");
    }
    // The heartbeat is lost: the registry projects the node offline first.
    set_presence(
        &mut world.storage,
        &world.client,
        ClientPresenceState::Offline,
    );
    let mut service = ClientOccupancyService::new(&mut world.storage);
    service
        .mark_recovery_pending(lease_id(21).as_str(), &instant(T4))
        .expect("mark recovery")
}

#[test]
fn service_claims_acknowledges_and_expires_leases_across_fencing_tokens() {
    let mut storage = SqliteStorage::open(temporary_directory("vertical")).expect("storage");
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, &holder);
    let mut service = ClientOccupancyService::new(&mut storage);

    // Claim -> reserving with the first fencing token of the database.
    let reserved = service
        .atomic_claim(&claim(10, &client, &holder), &instant(T0))
        .expect("claim");
    assert_eq!(reserved.state, OccupancyLeaseState::Reserving);
    assert_eq!(reserved.fencing_token, 1);
    assert!(
        service
            .active_lease_for_node(&client)
            .expect("active lease")
            .is_some()
    );

    // Only the matching token ACK promotes the lease to occupied.
    expect_kind(
        &service
            .record_acknowledgement(
                reserved.occupancy_lease_id.as_str(),
                reserved.fencing_token + 7,
                Some(&instant(T2)),
                &instant(T1),
            )
            .expect_err("mismatched ACK must fail"),
        ClientOccupancyServiceErrorKind::FencingTokenMismatch,
    );
    let occupied = service
        .record_acknowledgement(
            reserved.occupancy_lease_id.as_str(),
            reserved.fencing_token,
            Some(&instant(T2)),
            &instant(T1),
        )
        .expect("ack");
    assert_eq!(occupied.state, OccupancyLeaseState::Occupied);

    // Idle expiry releases an occupied lease without tasks.
    let expired = service
        .expire_idle(&instant(T2), |_| 0)
        .expect("idle sweep");
    assert_eq!(expired, vec![occupied.occupancy_lease_id.clone()]);

    // The second occupancy mints a strictly higher token.
    let second = service
        .atomic_claim(&claim(11, &client, &holder), &instant(T0))
        .expect("second claim");
    assert!(second.fencing_token > occupied.fencing_token);
    service
        .record_acknowledgement(
            second.occupancy_lease_id.as_str(),
            second.fencing_token,
            None,
            &instant(T1),
        )
        .expect("ack");
    assert_eq!(
        service.current_fencing_token().expect("current"),
        second.fencing_token
    );
}

#[test]
fn service_releases_and_drains_into_the_snapshot_read_model() {
    let mut storage = SqliteStorage::open(temporary_directory("release-drain")).expect("storage");
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, &holder);
    let mut service = ClientOccupancyService::new(&mut storage);
    let reserved = service
        .atomic_claim(&claim(10, &client, &holder), &instant(T0))
        .expect("claim");
    let occupied = service
        .record_acknowledgement(
            reserved.occupancy_lease_id.as_str(),
            reserved.fencing_token,
            None,
            &instant(T1),
        )
        .expect("ack");

    // Release with active tasks drains, then completes automatically.
    let draining = service
        .request_release(
            occupied.occupancy_lease_id.as_str(),
            occupied.fencing_token,
            2,
            &instant(T2),
        )
        .expect("release request");
    assert_eq!(draining.state, OccupancyLeaseState::Draining);
    let released = service
        .drain_complete(draining.occupancy_lease_id.as_str())
        .expect("drain complete");
    assert_eq!(
        released.release_reason,
        Some(OccupancyReleaseReason::DrainCompleted)
    );

    // The full chain is visible through the snapshot read model.
    let history = service
        .snapshot(released.occupancy_lease_id.as_str())
        .expect("snapshot")
        .expect("history");
    assert_eq!(history, released);
    assert!(
        service
            .active_lease_for_node(&client)
            .expect("active lease")
            .is_none()
    );
    assert_eq!(
        service.current_fencing_token().expect("current"),
        released.fencing_token
    );
}

#[test]
fn service_denies_claims_from_holders_without_a_use_grant() {
    let mut world = seed_recovery_world("stranger-denial");
    let mut service = ClientOccupancyService::new(&mut world.storage);
    // A user without any grant is denied before the registry gate.
    let stranger = user_id(9);
    expect_kind(
        &service
            .atomic_claim(
                &claim(20, world.client.as_str(), stranger.as_str()),
                &instant(T0),
            )
            .expect_err("stranger claim must fail"),
        ClientOccupancyServiceErrorKind::AccessDenied,
    );
}

#[test]
fn service_marks_recovery_pending_after_a_lost_heartbeat() {
    let mut world = seed_recovery_world("recovery-pending");
    let pending = seed_recovery_pending(&mut world);
    assert_eq!(pending.state, OccupancyLeaseState::RecoveryPending);
}

#[test]
fn service_blocks_preemption_and_cleanup_during_the_recovery_window() {
    let mut world = seed_recovery_world("recovery-window");
    let pending = seed_recovery_pending(&mut world);
    // The device reconnects while reconciliation is still pending: the node
    // is reachable again, yet the recovery lease must not be preemptable.
    set_presence(
        &mut world.storage,
        &world.client,
        ClientPresenceState::Online,
    );
    let mut service = ClientOccupancyService::new(&mut world.storage);
    expect_kind(
        &service
            .atomic_claim(&claim(22, &world.client, &world.other), &instant(T1))
            .expect_err("recovery must block preemption"),
        ClientOccupancyServiceErrorKind::ActiveLeaseConflict,
    );
    // Safe cleanup refuses to run while the recovery window is open.
    expect_kind(
        &service
            .force_release(pending.occupancy_lease_id.as_str(), &instant(T1))
            .expect_err("early cleanup must fail"),
        ClientOccupancyServiceErrorKind::IllegalStateTransition,
    );
}

#[test]
fn service_reconcile_resume_restores_the_fencing_token_without_minting() {
    let mut world = seed_recovery_world("reconcile-resume");
    let pending = seed_recovery_pending(&mut world);
    let mut service = ClientOccupancyService::new(&mut world.storage);
    // Reconciliation resumes the lease under the original fencing token.
    let resumed = service
        .reconcile_resume(
            pending.occupancy_lease_id.as_str(),
            OccupancyReconcileTarget::ResumeOccupied,
            None,
            &instant(T2),
        )
        .expect("resume");
    assert_eq!(resumed.fencing_token, pending.fencing_token);
    assert_eq!(
        service.mint_fencing_token().expect("mint"),
        pending.fencing_token + 1,
        "recovery must not mint; the next new occupancy does"
    );
}

#[test]
fn service_rejects_ack_and_drain_after_a_terminal_release() {
    let mut world = seed_recovery_world("terminal-release");
    let pending = seed_recovery_pending(&mut world);
    let mut service = ClientOccupancyService::new(&mut world.storage);
    let resumed = service
        .reconcile_resume(
            pending.occupancy_lease_id.as_str(),
            OccupancyReconcileTarget::ResumeOccupied,
            None,
            &instant(T2),
        )
        .expect("resume");
    // An ACK against a terminal lease is not a legal transition.
    let released = service
        .request_release(
            resumed.occupancy_lease_id.as_str(),
            resumed.fencing_token,
            0,
            &instant(T2),
        )
        .expect("release");
    expect_kind(
        &service
            .record_acknowledgement(
                released.occupancy_lease_id.as_str(),
                released.fencing_token,
                None,
                &instant(T2),
            )
            .expect_err("terminal ACK must fail"),
        ClientOccupancyServiceErrorKind::IllegalStateTransition,
    );
    expect_kind(
        &service
            .drain_complete(resumed.occupancy_lease_id.as_str())
            .expect_err("released drain must fail"),
        ClientOccupancyServiceErrorKind::IllegalStateTransition,
    );
}
