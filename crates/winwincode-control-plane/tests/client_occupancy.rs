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
    GrantSource, GrantTrustMode, OccupancyClaim, OccupancyReconcileTarget, OccupancyReleaseReason,
    SqliteStorage,
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
    let issuance = AccessGrantIssuance::try_new(
        grant_id(seed),
        node_id(seed),
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
    node_id(seed)
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

#[test]
fn service_drives_claim_ack_release_and_reapproval_across_a_fencing_chain() {
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
    let occupied = service
        .record_acknowledgement(
            second.occupancy_lease_id.as_str(),
            second.fencing_token,
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
        second.fencing_token
    );
}

#[test]
// The recovery matrix asserts many denial kinds in one durable scenario, so
// the line-count lint is intentionally allowed for this single test.
#[allow(clippy::too_many_lines)]
fn service_maps_recovery_semantics_and_denial_kinds() {
    let mut storage = SqliteStorage::open(temporary_directory("recovery-errors")).expect("storage");
    let holder = user_id(2);
    let other = user_id(3);
    let client = seed_client_with_holder(&mut storage, 1, &holder);
    // A second holder with a `use` grant on the same client.
    let issuance = AccessGrantIssuance::try_new(
        grant_id(5),
        client.as_str(),
        other.as_str(),
        other.as_str(),
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

    // A user without any grant is denied before the registry gate.
    {
        let mut service = ClientOccupancyService::new(&mut storage);
        let stranger = user_id(9);
        expect_kind(
            &service
                .atomic_claim(&claim(20, &client, stranger.as_str()), &instant(T0))
                .expect_err("stranger claim must fail"),
            ClientOccupancyServiceErrorKind::AccessDenied,
        );

        // Recovery: claim and ACK an occupied lease.
        let reserved = service
            .atomic_claim(&claim(21, &client, &holder), &instant(T0))
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
    set_presence(&mut storage, &client, ClientPresenceState::Offline);
    let pending = {
        let mut service = ClientOccupancyService::new(&mut storage);
        let pending = service
            .mark_recovery_pending(lease_id(21).as_str(), &instant(T4))
            .expect("mark recovery");
        assert_eq!(pending.state, OccupancyLeaseState::RecoveryPending);
        pending
    };
    // The device reconnects while reconciliation is still pending: the node
    // is reachable again, yet the recovery lease must not be preemptable.
    set_presence(&mut storage, &client, ClientPresenceState::Online);
    let mut service = ClientOccupancyService::new(&mut storage);
    expect_kind(
        &service
            .atomic_claim(&claim(22, &client, other.as_str()), &instant(T1))
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
