// SPDX-License-Identifier: Apache-2.0

//! Durable occupancy lease and fencing-token contract tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, OccupancyClaim, OccupancyLeaseRecord, OccupancyLeaseState,
    OccupancyLedger, OccupancyReconcileTarget, OccupancyReleaseReason, OccupancyStoreError,
    OccupancyStoreErrorKind, SqliteStorage,
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
        "winwincode-client-occupancy-{name}-{}-{suffix}-{nanos}",
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
const T3: &str = "2026-01-01T00:03:00.000Z";
const T4: &str = "2026-01-01T00:04:00.000Z";

fn open(path: PathBuf) -> SqliteStorage {
    SqliteStorage::open(path).expect("storage")
}

/// Seeds one registered, `online` client node with a `use` grant for `holder`.
fn seed_client_with_holder(
    storage: &mut SqliteStorage,
    seed: u64,
    max_slots: u32,
    holder: &str,
) -> String {
    let registration = ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:012}"),
        format!("Device {seed}"),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        Some(instance_id(seed)),
        max_slots,
    )
    .expect("registration");
    {
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &instant(T0))
            .expect("register");
        let online = registry
            .update_presence(node_id(seed).as_str(), ClientPresenceState::Online, 1)
            .expect("presence online");
        assert_eq!(online.presence_state, ClientPresenceState::Online);
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

/// Adds one extra `use` grant for another holder on an already seeded client.
fn grant_extra_holder(storage: &mut SqliteStorage, client: &str, holder: &str, seed: u64) {
    let issuance = AccessGrantIssuance::try_new(
        grant_id(seed),
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

fn claimed(
    ledger: &mut OccupancyLedger<'_>,
    seed: u64,
    client: &str,
    holder: &str,
) -> OccupancyLeaseRecord {
    ledger
        .atomic_claim(&claim(seed, client, holder), &instant(T0))
        .expect("claim")
}

fn acknowledged(
    ledger: &mut OccupancyLedger<'_>,
    seed: u64,
    client: &str,
    holder: &str,
) -> OccupancyLeaseRecord {
    let record = claimed(ledger, seed, client, holder);
    let idle = instant(T2);
    ledger
        .record_acknowledgement(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            Some(&idle),
            &instant(T1),
        )
        .expect("acknowledgement")
}

fn expect_kind<T>(result: Result<T, OccupancyStoreError>, kind: OccupancyStoreErrorKind)
where
    T: std::fmt::Debug,
{
    let error = result.expect_err("operation must fail");
    assert_eq!(error.kind(), kind);
}

#[test]
fn claim_gate_enforces_grant_presence_lock_and_capacity_conditions() {
    let mut storage = open(temporary_directory("claim-gate"));
    let holder = user_id(2);
    let other = user_id(3);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);

    // No grant: the claim is denied before any registry fact is consulted,
    // and an unknown client node reports precisely.
    {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        expect_kind(
            ledger.atomic_claim(&claim(10, &client, &other), &instant(T0)),
            OccupancyStoreErrorKind::AccessDenied,
        );
        expect_kind(
            ledger.atomic_claim(&claim(10, &node_id(99), &holder), &instant(T0)),
            OccupancyStoreErrorKind::UnknownClientNode,
        );

        // The happy-path claim passes every condition.
        let record = claimed(&mut ledger, 10, &client, &holder);
        assert_eq!(record.state, OccupancyLeaseState::Reserving);
        assert_eq!(record.fencing_token, 1);
        assert_eq!(record.claimed_at, Some(instant(T0)));
        assert_eq!(record.acknowledged_at, None);
        assert_eq!(record.release_reason, None);
        assert_eq!(record.revision, 1);
        assert_eq!(record.created_at, instant(T0));
        assert_eq!(record.claim_request_id, request_id(10));

        // A second lease id while the first is active loses on the active
        // check.
        expect_kind(
            ledger.atomic_claim(&claim(11, &client, &holder), &instant(T0)),
            OccupancyStoreErrorKind::ActiveLeaseConflict,
        );
    }

    // A node whose presence is `locked` is not `online`, so the presence
    // condition denies the claim. The independent `lock_state` switch is
    // exercised against an `online` node in the crate unit tests.
    set_presence(&mut storage, &client, ClientPresenceState::Locked);
    {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        expect_kind(
            ledger.atomic_claim(&claim(12, &client, &holder), &instant(T0)),
            OccupancyStoreErrorKind::PresenceNotOnline,
        );
    }
    set_presence(&mut storage, &client, ClientPresenceState::Online);

    // Offline node denies the claim.
    set_presence(&mut storage, &client, ClientPresenceState::Offline);
    {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        expect_kind(
            ledger.atomic_claim(&claim(13, &client, &holder), &instant(T0)),
            OccupancyStoreErrorKind::PresenceNotOnline,
        );
    }
    set_presence(&mut storage, &client, ClientPresenceState::Online);

    // A capacity-zero node denies the claim despite grant and presence.
    let tiny = seed_client_with_holder(&mut storage, 4, 0, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");
    expect_kind(
        ledger.atomic_claim(&claim(14, &tiny, &holder), &instant(T0)),
        OccupancyStoreErrorKind::CapacityExhausted,
    );
}

#[test]
fn acknowledgement_requires_matching_token_to_reach_occupied() {
    let mut storage = open(temporary_directory("ack"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");
    let record = claimed(&mut ledger, 20, &client, &holder);

    // A stale or foreign token never promotes the lease.
    expect_kind(
        ledger.record_acknowledgement(
            record.occupancy_lease_id.as_str(),
            record.fencing_token + 1,
            Some(&instant(T2)),
            &instant(T1),
        ),
        OccupancyStoreErrorKind::FencingTokenMismatch,
    );
    let unchanged = ledger
        .snapshot(record.occupancy_lease_id.as_str())
        .expect("snapshot")
        .expect("lease");
    assert_eq!(unchanged.state, OccupancyLeaseState::Reserving);
    assert_eq!(unchanged.acknowledged_at, None);

    // The exact lease and token ACK promotes reserving to occupied.
    let occupied = ledger
        .record_acknowledgement(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            Some(&instant(T2)),
            &instant(T1),
        )
        .expect("ack");
    assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
    assert_eq!(occupied.acknowledged_at, Some(instant(T1)));
    assert_eq!(occupied.last_renewed_at, Some(instant(T1)));
    assert_eq!(occupied.idle_expires_at, Some(instant(T2)));
    assert_eq!(occupied.revision, 2);

    // An ACK replay with the matching token is an idempotent no-op.
    let replay = ledger
        .record_acknowledgement(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            Some(&instant(T2)),
            &instant(T1),
        )
        .expect("ack replay");
    assert_eq!(replay.revision, 2);
    // ...and a mismatched token still fails against the occupied lease.
    expect_kind(
        ledger.record_acknowledgement(
            record.occupancy_lease_id.as_str(),
            99,
            Some(&instant(T2)),
            &instant(T1),
        ),
        OccupancyStoreErrorKind::FencingTokenMismatch,
    );

    // A rejected offer can never be acknowledged into occupied. Release the
    // occupied lease first: only one active lease may exist per client.
    ledger
        .request_release(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            0,
            &instant(T1),
        )
        .expect("release before next claim");
    let rejected = claimed(&mut ledger, 21, &client, &holder);
    ledger
        .reject_offer(
            rejected.occupancy_lease_id.as_str(),
            rejected.fencing_token,
            OccupancyReleaseReason::ClientRejected,
            &instant(T1),
        )
        .expect("reject");
    expect_kind(
        ledger.record_acknowledgement(
            rejected.occupancy_lease_id.as_str(),
            rejected.fencing_token,
            None,
            &instant(T1),
        ),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );
}

#[test]
fn reject_offer_terminals_reserving_with_distinguishing_reasons() {
    let mut storage = open(temporary_directory("reject"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");

    for (seed, reason) in [
        (30, OccupancyReleaseReason::AckTimeout),
        (31, OccupancyReleaseReason::ClientRejected),
        (32, OccupancyReleaseReason::ClaimWithdrawn),
    ] {
        let record = claimed(&mut ledger, seed, &client, &holder);
        let released = ledger
            .reject_offer(
                record.occupancy_lease_id.as_str(),
                record.fencing_token,
                reason,
                &instant(T1),
            )
            .expect("reject");
        assert_eq!(released.state, OccupancyLeaseState::Released);
        assert_eq!(released.release_reason, Some(reason));
        // Replay with the same reason is an idempotent no-op.
        let replay = ledger
            .reject_offer(
                record.occupancy_lease_id.as_str(),
                record.fencing_token,
                reason,
                &instant(T1),
            )
            .expect("reject replay");
        assert_eq!(replay.revision, released.revision);
        // Holder-release does not belong to the reserving endings: the reason
        // is validated before any state judgement.
        expect_kind(
            ledger.reject_offer(
                record.occupancy_lease_id.as_str(),
                record.fencing_token,
                OccupancyReleaseReason::HolderReleased,
                &instant(T1),
            ),
            OccupancyStoreErrorKind::InvalidInput,
        );
    }

    // Holder-release and drain reasons do not belong to reserving endings.
    let record = claimed(&mut ledger, 33, &client, &holder);
    expect_kind(
        ledger.reject_offer(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            OccupancyReleaseReason::HolderReleased,
            &instant(T1),
        ),
        OccupancyStoreErrorKind::InvalidInput,
    );
    // Free the lease so the next claim can proceed.
    ledger
        .reject_offer(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            OccupancyReleaseReason::ClaimWithdrawn,
            &instant(T1),
        )
        .expect("withdraw claim");

    // Rejecting an occupied lease is not a legal transition: the state is
    // judged before any token comparison.
    let occupied = acknowledged(&mut ledger, 34, &client, &holder);
    expect_kind(
        ledger.reject_offer(
            occupied.occupancy_lease_id.as_str(),
            occupied.fencing_token,
            OccupancyReleaseReason::ClientRejected,
            &instant(T1),
        ),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );
    expect_kind(
        ledger.reject_offer(
            occupied.occupancy_lease_id.as_str(),
            occupied.fencing_token + 5,
            OccupancyReleaseReason::ClientRejected,
            &instant(T1),
        ),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );
}

#[test]
fn release_request_splits_on_task_count_and_drain_completes_automatically() {
    let mut storage = open(temporary_directory("drain"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");

    // Active worker sessions move the lease to draining.
    let draining = acknowledged(&mut ledger, 40, &client, &holder);
    let draining = ledger
        .request_release(
            draining.occupancy_lease_id.as_str(),
            draining.fencing_token,
            2,
            &instant(T2),
        )
        .expect("release request");
    assert_eq!(draining.state, OccupancyLeaseState::Draining);
    assert_eq!(draining.release_reason, None);

    // A retried release request with tasks stays an idempotent no-op.
    let replay = ledger
        .request_release(
            draining.occupancy_lease_id.as_str(),
            draining.fencing_token,
            2,
            &instant(T2),
        )
        .expect("release replay");
    assert_eq!(replay.revision, draining.revision);

    // A zero-count release against draining is refused: only the automatic
    // drain completion may release it.
    expect_kind(
        ledger.request_release(
            draining.occupancy_lease_id.as_str(),
            draining.fencing_token,
            0,
            &instant(T2),
        ),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );

    // Drain completion releases automatically with drain_completed.
    let released = ledger
        .drain_complete(draining.occupancy_lease_id.as_str())
        .expect("drain complete");
    assert_eq!(released.state, OccupancyLeaseState::Released);
    assert_eq!(
        released.release_reason,
        Some(OccupancyReleaseReason::DrainCompleted)
    );
    let replay = ledger
        .drain_complete(draining.occupancy_lease_id.as_str())
        .expect("drain replay");
    assert_eq!(replay.revision, released.revision);

    // No active worker session releases the occupied lease immediately.
    let occupied = acknowledged(&mut ledger, 41, &client, &holder);
    let released = ledger
        .request_release(
            occupied.occupancy_lease_id.as_str(),
            occupied.fencing_token,
            0,
            &instant(T2),
        )
        .expect("release");
    assert_eq!(released.state, OccupancyLeaseState::Released);
    assert_eq!(
        released.release_reason,
        Some(OccupancyReleaseReason::HolderReleased)
    );

    // A terminal release replays with the matching token and reason, and a
    // drain on an occupied lease is not a legal transition.
    let replay = ledger
        .request_release(
            released.occupancy_lease_id.as_str(),
            released.fencing_token,
            0,
            &instant(T2),
        )
        .expect("release replay");
    assert_eq!(replay.revision, released.revision);
    expect_kind(
        ledger.drain_complete(occupied.occupancy_lease_id.as_str()),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );
    expect_kind(
        ledger.drain_complete("ocl_FFFFFFFFFFFFFFFFFFFFFFFFFF"),
        OccupancyStoreErrorKind::UnknownOccupancyLease,
    );
}

#[test]
fn fencing_tokens_strictly_increase_and_recovery_never_mints() {
    let mut storage = open(temporary_directory("fencing"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);

    // First occupancy mints token 1 and the recovery loop reuses it.
    let record = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        assert_eq!(ledger.current_fencing_token().expect("current"), 0);
        let record = claimed(&mut ledger, 50, &client, &holder);
        assert_eq!(record.fencing_token, 1);
        let idle = instant(T2);
        ledger
            .record_acknowledgement(
                record.occupancy_lease_id.as_str(),
                record.fencing_token,
                Some(&idle),
                &instant(T1),
            )
            .expect("ack")
    };
    set_presence(&mut storage, &client, ClientPresenceState::Offline);
    let resumed = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let deadline = instant(T4);
        let record = ledger
            .mark_recovery_pending(record.occupancy_lease_id.as_str(), &deadline)
            .expect("recovery");
        assert_eq!(record.fencing_token, 1);
        let resumed = ledger
            .reconcile_resume(
                record.occupancy_lease_id.as_str(),
                OccupancyReconcileTarget::ResumeOccupied,
                Some(&instant(T3)),
                &instant(T2),
            )
            .expect("resume");
        assert_eq!(resumed.fencing_token, 1, "recovery must not mint a token");
        assert_eq!(ledger.current_fencing_token().expect("current"), 1);
        resumed
    };

    // The device is reachable again; the next new occupancy mints a strictly
    // higher token.
    set_presence(&mut storage, &client, ClientPresenceState::Online);
    let second = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let released = ledger
            .request_release(
                resumed.occupancy_lease_id.as_str(),
                resumed.fencing_token,
                0,
                &instant(T3),
            )
            .expect("release");
        assert_eq!(released.fencing_token, 1);
        let second = claimed(&mut ledger, 51, &client, &holder);
        assert!(second.fencing_token > released.fencing_token);
        ledger
            .reject_offer(
                second.occupancy_lease_id.as_str(),
                second.fencing_token,
                OccupancyReleaseReason::AckTimeout,
                &instant(T1),
            )
            .expect("reject");
        second
    };

    // A direct mint moves the counter and the following claim is higher still.
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");
    let minted = ledger.mint_fencing_token().expect("mint");
    assert_eq!(minted, 3);
    let third = claimed(&mut ledger, 52, &client, &holder);
    assert!(third.fencing_token > second.fencing_token);
    assert_eq!(third.fencing_token, 4);
    assert_eq!(ledger.current_fencing_token().expect("current"), 4);
}

// The recovery walk covers every transition of the frozen recovery loop in
// one durable story, so its length is intentional.
#[allow(clippy::too_many_lines)]
#[test]
fn recovery_pending_blocks_preemption_until_reconciliation() {
    let mut storage = open(temporary_directory("recovery"));
    let holder = user_id(2);
    let other = user_id(3);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    grant_extra_holder(&mut storage, &client, &other, 5);

    // occupied -> recovery_pending after the heartbeat is lost.
    let record = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        acknowledged(&mut ledger, 60, &client, &holder)
    };
    set_presence(&mut storage, &client, ClientPresenceState::Offline);
    let pending = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let deadline = instant(T4);
        let pending = ledger
            .mark_recovery_pending(record.occupancy_lease_id.as_str(), &deadline)
            .expect("mark recovery");
        assert_eq!(pending.state, OccupancyLeaseState::RecoveryPending);
        assert_eq!(pending.recovery_deadline_at, Some(instant(T4)));
        assert_eq!(pending.fencing_token, record.fencing_token);
        // Replay marking is an idempotent no-op.
        let replay = ledger
            .mark_recovery_pending(record.occupancy_lease_id.as_str(), &instant(T3))
            .expect("mark replay");
        assert_eq!(replay.revision, pending.revision);
        assert_eq!(replay.recovery_deadline_at, Some(instant(T4)));
        pending
    };

    // The device reconnects with the same identity while reconciliation is
    // still pending: the node is reachable again but stays occupied.
    set_presence(&mut storage, &client, ClientPresenceState::Online);
    let resumed = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        // No user may preempt a recovery_pending lease: not another holder,
        // not the original one, even though the node is online again.
        expect_kind(
            ledger.atomic_claim(&claim(61, &client, &other), &instant(T1)),
            OccupancyStoreErrorKind::ActiveLeaseConflict,
        );
        expect_kind(
            ledger.atomic_claim(&claim(62, &client, &holder), &instant(T1)),
            OccupancyStoreErrorKind::ActiveLeaseConflict,
        );

        // An accepted reconcile with workers still running resumes occupied.
        let resumed = ledger
            .reconcile_resume(
                pending.occupancy_lease_id.as_str(),
                OccupancyReconcileTarget::ResumeOccupied,
                Some(&instant(T3)),
                &instant(T2),
            )
            .expect("resume occupied");
        assert_eq!(resumed.state, OccupancyLeaseState::Occupied);
        assert_eq!(resumed.fencing_token, record.fencing_token);
        assert_eq!(resumed.idle_expires_at, Some(instant(T3)));
        assert_eq!(resumed.recovery_deadline_at, None);
        assert_eq!(resumed.last_renewed_at, Some(instant(T2)));
        // A resume replay to the current state is an idempotent no-op.
        let replay = ledger
            .reconcile_resume(
                resumed.occupancy_lease_id.as_str(),
                OccupancyReconcileTarget::ResumeOccupied,
                Some(&instant(T3)),
                &instant(T2),
            )
            .expect("resume replay");
        assert_eq!(replay.revision, resumed.revision);
        resumed
    };

    // Drop again: occupied -> recovery_pending, then resume to draining.
    set_presence(&mut storage, &client, ClientPresenceState::Offline);
    {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let pending = ledger
            .mark_recovery_pending(resumed.occupancy_lease_id.as_str(), &instant(T4))
            .expect("mark recovery");
        expect_kind(
            ledger.reconcile_resume(
                pending.occupancy_lease_id.as_str(),
                OccupancyReconcileTarget::ResumeDraining,
                Some(&instant(T3)),
                &instant(T2),
            ),
            OccupancyStoreErrorKind::InvalidInput,
        );
        let draining = ledger
            .reconcile_resume(
                pending.occupancy_lease_id.as_str(),
                OccupancyReconcileTarget::ResumeDraining,
                None,
                &instant(T2),
            )
            .expect("resume draining");
        assert_eq!(draining.state, OccupancyLeaseState::Draining);
        assert_eq!(draining.idle_expires_at, None);
        assert_eq!(draining.fencing_token, record.fencing_token);
        let released = ledger
            .drain_complete(draining.occupancy_lease_id.as_str())
            .expect("drain complete");
        assert_eq!(released.state, OccupancyLeaseState::Released);
    }

    // Resuming a non-recovery lease is refused. The device is reachable and
    // the released node accepts a fresh claim.
    set_presence(&mut storage, &client, ClientPresenceState::Online);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");
    let fresh = claimed(&mut ledger, 63, &client, &holder);
    expect_kind(
        ledger.reconcile_resume(
            fresh.occupancy_lease_id.as_str(),
            OccupancyReconcileTarget::ResumeOccupied,
            None,
            &instant(T2),
        ),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );

    // A reserving lease never enters recovery: the state is judged before the
    // presence projection.
    expect_kind(
        ledger.mark_recovery_pending(fresh.occupancy_lease_id.as_str(), &instant(T4)),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );

    // Marking recovery against an online occupied node is refused: the
    // heartbeat has not been lost.
    let occupied = ledger
        .record_acknowledgement(
            fresh.occupancy_lease_id.as_str(),
            fresh.fencing_token,
            None,
            &instant(T1),
        )
        .expect("ack");
    expect_kind(
        ledger.mark_recovery_pending(occupied.occupancy_lease_id.as_str(), &instant(T4)),
        OccupancyStoreErrorKind::PresenceNotOnline,
    );
}

#[test]
fn force_release_requires_an_expired_recovery_window() {
    let mut storage = open(temporary_directory("force-release"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);

    let record = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        acknowledged(&mut ledger, 70, &client, &holder)
    };
    set_presence(&mut storage, &client, ClientPresenceState::Offline);
    let released = {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let pending = ledger
            .mark_recovery_pending(record.occupancy_lease_id.as_str(), &instant(T3))
            .expect("mark recovery");

        // Before the deadline the safe cleanup is refused.
        expect_kind(
            ledger.force_release(pending.occupancy_lease_id.as_str(), &instant(T2)),
            OccupancyStoreErrorKind::IllegalStateTransition,
        );
        // At or after the deadline the administrator cleanup releases.
        let released = ledger
            .force_release(pending.occupancy_lease_id.as_str(), &instant(T3))
            .expect("force release");
        assert_eq!(released.state, OccupancyLeaseState::Released);
        assert_eq!(
            released.release_reason,
            Some(OccupancyReleaseReason::ForceReleased)
        );
        assert_eq!(released.fencing_token, record.fencing_token);
        // Replay is idempotent.
        let replay = ledger
            .force_release(released.occupancy_lease_id.as_str(), &instant(T3))
            .expect("force release replay");
        assert_eq!(replay.revision, released.revision);
        released
    };

    // Force release against occupied fails, and the terminal cleanup replays
    // idempotently instead of re-releasing. The node is reachable again so
    // the second occupancy can be claimed.
    set_presence(&mut storage, &client, ClientPresenceState::Online);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");
    let occupied = acknowledged(&mut ledger, 71, &client, &holder);
    expect_kind(
        ledger.force_release(occupied.occupancy_lease_id.as_str(), &instant(T3)),
        OccupancyStoreErrorKind::IllegalStateTransition,
    );
    // The terminal cleanup replays idempotently instead of re-releasing.
    let replay_again = ledger
        .force_release(released.occupancy_lease_id.as_str(), &instant(T4))
        .expect("force release replay");
    assert_eq!(replay_again.revision, released.revision);
}

#[test]
fn idle_expiry_requires_due_deadline_and_zero_active_tasks() {
    let mut storage = open(temporary_directory("idle"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");

    let record = claimed(&mut ledger, 80, &client, &holder);
    let idle = instant(T2);
    let occupied = ledger
        .record_acknowledgement(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            Some(&idle),
            &instant(T1),
        )
        .expect("ack");

    // Not yet due: nothing expires.
    let expired = ledger.expire_idle(&instant(T1), |_| 0).expect("idle sweep");
    assert!(expired.is_empty());
    // Due but tasks are still active: nothing expires.
    let expired = ledger.expire_idle(&instant(T2), |_| 1).expect("idle sweep");
    assert!(expired.is_empty());
    // Due with no active task: the lease expires.
    let expired = ledger.expire_idle(&instant(T2), |_| 0).expect("idle sweep");
    assert_eq!(expired, vec![occupied.occupancy_lease_id.clone()]);
    let terminal = ledger
        .snapshot(occupied.occupancy_lease_id.as_str())
        .expect("snapshot")
        .expect("lease");
    assert_eq!(terminal.state, OccupancyLeaseState::Expired);
    assert_eq!(terminal.release_reason, None);
    // The terminal row is not swept again.
    let expired = ledger.expire_idle(&instant(T3), |_| 0).expect("idle sweep");
    assert!(expired.is_empty());

    // After expiry the node is available and the next claim mints higher.
    let next = claimed(&mut ledger, 81, &client, &holder);
    assert!(next.fencing_token > occupied.fencing_token);

    // A lease without an idle policy never expires.
    let no_idle = ledger
        .record_acknowledgement(
            next.occupancy_lease_id.as_str(),
            next.fencing_token,
            None,
            &instant(T1),
        )
        .expect("ack");
    let expired = ledger.expire_idle(&instant(T3), |_| 0).expect("idle sweep");
    assert!(expired.is_empty());
    assert_eq!(
        ledger
            .snapshot(no_idle.occupancy_lease_id.as_str())
            .expect("snapshot")
            .expect("lease")
            .state,
        OccupancyLeaseState::Occupied
    );
}

#[test]
fn concurrent_claims_produce_exactly_one_winner() {
    let directory = temporary_directory("concurrent");
    let holder = user_id(2);
    let other = user_id(3);
    let client = {
        let mut storage = open(directory.clone());
        seed_client_with_holder(&mut storage, 1, 2, &holder)
    };
    {
        let mut storage = open(directory.clone());
        grant_extra_holder(&mut storage, &client, &other, 5);
    }

    let claims: Vec<(String, String)> = vec![(lease_id(90), holder), (lease_id(91), other)];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(claims.len()));
    let handles = claims
        .into_iter()
        .map(|(id, user)| {
            let directory = directory.clone();
            let client = client.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut storage = open(directory);
                let mut ledger = storage.client_occupancy_ledger().expect("ledger");
                let attempt =
                    OccupancyClaim::try_new(id, client, user, request_id(92)).expect("claim");
                barrier.wait();
                ledger.atomic_claim(&attempt, &instant(T0))
            })
        })
        .collect::<Vec<_>>();
    let mut winners = 0;
    let mut losers = 0;
    for handle in handles {
        match handle.join().expect("claim thread") {
            Ok(record) => {
                assert_eq!(record.state, OccupancyLeaseState::Reserving);
                winners += 1;
            }
            Err(error) => {
                assert_eq!(error.kind(), OccupancyStoreErrorKind::ActiveLeaseConflict);
                losers += 1;
            }
        }
    }
    assert_eq!(winners, 1, "exactly one concurrent claim may win");
    assert_eq!(losers, 1, "the losing claim must fail closed");

    // The durable active set holds exactly the winner.
    let mut storage = open(directory);
    let ledger = storage.client_occupancy_ledger().expect("ledger");
    let active = ledger
        .active_lease_for_node(&client)
        .expect("active lease")
        .expect("winner");
    assert_eq!(active.state, OccupancyLeaseState::Reserving);
}

#[test]
fn snapshots_distinguish_active_leases_from_the_available_projection() {
    let mut storage = open(temporary_directory("snapshots"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");

    assert!(
        ledger
            .snapshot("ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1")
            .expect("snapshot")
            .is_none()
    );
    assert!(
        ledger
            .active_lease_for_node(&client)
            .expect("active lease")
            .is_none(),
        "a client without leases projects available"
    );

    let record = claimed(&mut ledger, 95, &client, &holder);
    let active = ledger
        .active_lease_for_node(&client)
        .expect("active lease")
        .expect("active lease");
    assert_eq!(active.occupancy_lease_id, record.occupancy_lease_id);

    let released = ledger
        .reject_offer(
            record.occupancy_lease_id.as_str(),
            record.fencing_token,
            OccupancyReleaseReason::ClaimWithdrawn,
            &instant(T1),
        )
        .expect("reject");
    assert!(
        ledger
            .active_lease_for_node(&client)
            .expect("active lease")
            .is_none(),
        "terminal leases return the projection to available"
    );
    let by_id = ledger
        .snapshot(released.occupancy_lease_id.as_str())
        .expect("snapshot")
        .expect("history");
    assert_eq!(by_id, released);

    // A valid idle expiry path can also release: resume this lease through a
    // zero-task idle sweep after its policy deadline.
    let next = claimed(&mut ledger, 96, &client, &holder);
    let idle = instant(T2);
    let occupied = ledger
        .record_acknowledgement(
            next.occupancy_lease_id.as_str(),
            next.fencing_token,
            Some(&idle),
            &instant(T1),
        )
        .expect("ack");
    assert!(
        ledger
            .active_lease_for_node(&client)
            .expect("active lease")
            .is_some()
    );
    let expired = ledger.expire_idle(&instant(T2), |_| 0).expect("idle sweep");
    assert_eq!(expired, vec![occupied.occupancy_lease_id.clone()]);
    assert!(
        ledger
            .active_lease_for_node(&client)
            .expect("active lease")
            .is_none()
    );
}

#[test]
fn malformed_commands_are_rejected_before_storage() {
    let mut storage = open(temporary_directory("malformed"));
    let holder = user_id(2);
    let client = seed_client_with_holder(&mut storage, 1, 2, &holder);
    let mut ledger = storage.client_occupancy_ledger().expect("ledger");

    assert!(OccupancyClaim::try_new("lease", &client, &holder, request_id(1)).is_err());
    assert!(OccupancyClaim::try_new(lease_id(1), &client, &holder, "request").is_err());

    let bad_instant = Instant("not-an-instant".to_owned());
    let attempt = claim(97, &client, &holder);
    expect_kind(
        ledger.atomic_claim(&attempt, &bad_instant),
        OccupancyStoreErrorKind::InvalidInput,
    );
    expect_kind(
        ledger.record_acknowledgement("ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1", 1, None, &bad_instant),
        OccupancyStoreErrorKind::InvalidInput,
    );
    expect_kind(
        ledger.record_acknowledgement("ocl_1", 1, None, &instant(T0)),
        OccupancyStoreErrorKind::InvalidInput,
    );
    expect_kind(
        ledger.expire_idle(&bad_instant, |_| 0),
        OccupancyStoreErrorKind::InvalidInput,
    );
    assert!(ledger.snapshot("ocl_short").is_err());
    assert!(ledger.active_lease_for_node("node").is_err());
}
