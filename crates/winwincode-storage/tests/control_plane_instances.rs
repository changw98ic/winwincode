use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use winwincode_domain::{RequestId, Sha256Digest};
use winwincode_storage::{
    ControlPlaneCommandAdmission, ControlPlaneInstanceErrorKind, ControlPlaneInstanceIdentity,
    ControlPlaneInstanceState, NewOutboxEvent, ProductStateStorage, ReceiptActorKey,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, StorageErrorKind,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-instances-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn identity(seed: u64) -> ControlPlaneInstanceIdentity {
    ControlPlaneInstanceIdentity::try_new(format!("cpi_{seed:032x}"), format!("cpb_{seed:032x}"))
        .expect("instance identity")
}

fn receipt(seed: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(format!("actor-{seed}").into_bytes()).expect("actor"),
        ReceiptScopeKey::from_encoded(format!("scope-{seed}").into_bytes()).expect("scope"),
        RequestId(format!("req_{seed:026}")),
    )
    .expect("receipt identity")
}

fn digest(seed: u64) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"control-plane-instance-test\0");
    digest.update(seed.to_be_bytes());
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn commit(seed: u64, identity: ReceiptIdentity, command_digest: Sha256Digest) -> StateCommit {
    StateCommit::new(
        identity,
        command_digest,
        format!("instance-test:{seed}"),
        0,
        format!("state-{seed}").into_bytes(),
        vec![NewOutboxEvent::internal(
            format!("evt_control_plane_instance_{seed:016x}"),
            "control-plane-instance.test.v1",
            format!("event-{seed}").into_bytes(),
        )],
    )
}

fn claimed(
    admission: ControlPlaneCommandAdmission,
) -> winwincode_storage::ControlPlaneCommandClaim {
    match admission {
        ControlPlaneCommandAdmission::Claimed(claim) => claim,
        ControlPlaneCommandAdmission::Committed(_) => panic!("expected command claim"),
    }
}

#[test]
fn concurrent_instances_share_one_claim_and_one_canonical_receipt() {
    let root = temporary_directory("concurrent");
    let mut first_storage = SqliteStorage::open(&root).expect("first storage");
    let mut second_storage = SqliteStorage::open(&root).expect("second storage");
    let first_authority = first_storage
        .control_plane_instance_ledger()
        .expect("first ledger")
        .register(&identity(1), 10, 100)
        .expect("first register");
    let second_authority = second_storage
        .control_plane_instance_ledger()
        .expect("second ledger")
        .register(&identity(2), 10, 100)
        .expect("second register");
    drop(first_storage);
    drop(second_storage);

    let barrier = Arc::new(Barrier::new(2));
    let command_identity = receipt(1);
    let command_digest = digest(1);
    let mut threads = Vec::new();
    for authority in [first_authority.clone(), second_authority.clone()] {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let command_identity = command_identity.clone();
        let command_digest = command_digest.clone();
        threads.push(thread::spawn(move || {
            let mut storage = SqliteStorage::open(root).expect("thread storage");
            barrier.wait();
            storage
                .control_plane_instance_ledger()
                .expect("thread ledger")
                .admit_command(&authority, 20, &command_identity, &command_digest)
        }));
    }
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(ControlPlaneCommandAdmission::Claimed(_))))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| {
                matches!(
                    result,
                    Err(error) if error.kind() == ControlPlaneInstanceErrorKind::CommandInFlight
                )
            })
            .count(),
        1
    );
    let claim = results
        .into_iter()
        .find_map(|result| match result {
            Ok(ControlPlaneCommandAdmission::Claimed(claim)) => Some(claim),
            Ok(ControlPlaneCommandAdmission::Committed(_)) | Err(_) => None,
        })
        .expect("winning claim");
    let mut storage = SqliteStorage::open(&root).expect("commit storage");
    let receipt = storage
        .control_plane_instance_ledger()
        .expect("commit ledger")
        .commit_claimed(
            &claim,
            30,
            &commit(1, command_identity.clone(), command_digest.clone()),
        )
        .expect("fenced commit");
    assert!(!receipt.idempotent_replay);
    let replay = storage
        .control_plane_instance_ledger()
        .expect("replay ledger")
        .admit_command(&second_authority, 31, &command_identity, &command_digest)
        .expect("receipt replay");
    let ControlPlaneCommandAdmission::Committed(replay) = replay else {
        panic!("committed receipt must win over operational ownership");
    };
    assert_eq!(replay.stream_id, "instance-test:1");
    assert_eq!(replay.revision, 1);
    assert_eq!(
        storage
            .load_state("instance-test:1")
            .expect("load state")
            .expect("stored state")
            .revision,
        1
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn expired_owner_is_taken_over_and_stale_result_is_fenced_without_writes() {
    let root = temporary_directory("takeover");
    let command_identity = receipt(2);
    let command_digest = digest(2);
    let mut old_storage = SqliteStorage::open(&root).expect("old storage");
    let old_authority = old_storage
        .control_plane_instance_ledger()
        .expect("old ledger")
        .register(&identity(3), 10, 20)
        .expect("old authority");
    let old_claim = claimed(
        old_storage
            .control_plane_instance_ledger()
            .expect("old claim ledger")
            .admit_command(&old_authority, 11, &command_identity, &command_digest)
            .expect("old claim"),
    );
    let mut new_storage = SqliteStorage::open(&root).expect("new storage");
    let new_authority = new_storage
        .control_plane_instance_ledger()
        .expect("new ledger")
        .register(&identity(4), 20, 100)
        .expect("new authority");
    new_storage
        .control_plane_instance_ledger()
        .expect("fence ledger")
        .fence_expired(old_authority.identity().instance_id(), 20)
        .expect("fence expired owner");
    let new_claim = claimed(
        new_storage
            .control_plane_instance_ledger()
            .expect("takeover ledger")
            .admit_command(&new_authority, 21, &command_identity, &command_digest)
            .expect("takeover claim"),
    );
    assert!(new_claim.claim_fence() > old_claim.claim_fence());
    let bypass = old_storage
        .commit(&commit(2, command_identity.clone(), command_digest.clone()))
        .expect_err("a claimed command cannot bypass the instance fence");
    assert_eq!(bypass.kind(), StorageErrorKind::InvalidInput);
    let stale = old_storage
        .control_plane_instance_ledger()
        .expect("stale ledger")
        .commit_claimed(
            &old_claim,
            21,
            &commit(2, command_identity.clone(), command_digest.clone()),
        )
        .expect_err("stale result must be fenced");
    assert_eq!(stale.kind(), ControlPlaneInstanceErrorKind::OwnershipLost);
    assert!(
        new_storage
            .load_state("instance-test:2")
            .expect("load state")
            .is_none()
    );
    new_storage
        .control_plane_instance_ledger()
        .expect("winner ledger")
        .commit_claimed(
            &new_claim,
            22,
            &commit(2, command_identity.clone(), command_digest.clone()),
        )
        .expect("winner commit");
    drop(new_storage);
    drop(old_storage);

    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    let replay = restarted
        .control_plane_instance_ledger()
        .expect("restart ledger")
        .admit_command(&new_authority, 23, &command_identity, &command_digest)
        .expect("restart replay");
    assert!(matches!(replay, ControlPlaneCommandAdmission::Committed(_)));
    assert_eq!(
        restarted
            .load_state("instance-test:2")
            .expect("restart state")
            .expect("stored state")
            .revision,
        1
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn drain_rejects_new_work_completes_inflight_and_can_release_or_resume() {
    let root = temporary_directory("drain");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = storage
        .control_plane_instance_ledger()
        .expect("ledger")
        .register(&identity(5), 10, 100)
        .expect("register");
    let first_identity = receipt(3);
    let first_digest = digest(3);
    let claim = claimed(
        storage
            .control_plane_instance_ledger()
            .expect("claim ledger")
            .admit_command(&authority, 11, &first_identity, &first_digest)
            .expect("claim"),
    );
    let draining = storage
        .control_plane_instance_ledger()
        .expect("drain ledger")
        .request_drain(&authority, 12, 80)
        .expect("begin drain");
    assert_eq!(draining.state, ControlPlaneInstanceState::Draining);
    assert!(!draining.accepting_new_work);
    assert_eq!(draining.in_flight, 1);
    let drain_replay = storage
        .control_plane_instance_ledger()
        .expect("drain replay ledger")
        .request_drain(&authority, 13, 90)
        .expect("drain replay");
    assert_eq!(drain_replay.drain_deadline_at, Some(80));
    let denied = storage
        .control_plane_instance_ledger()
        .expect("denied ledger")
        .admit_command(&authority, 14, &receipt(4), &digest(4))
        .expect_err("drain rejects new work");
    assert_eq!(denied.kind(), ControlPlaneInstanceErrorKind::Draining);
    storage
        .control_plane_instance_ledger()
        .expect("complete ledger")
        .commit_claimed(&claim, 15, &commit(3, first_identity, first_digest))
        .expect("complete in-flight");
    let drained = storage
        .control_plane_instance_ledger()
        .expect("status ledger")
        .preflight(&authority, 16)
        .expect("drained status");
    assert!(drained.drained());
    assert!(drained.confirmed_state_sequence > 0);
    let resumed = storage
        .control_plane_instance_ledger()
        .expect("resume ledger")
        .resume(&authority, 17)
        .expect("resume");
    assert!(resumed.accepting_new_work);
    let resume_replay = storage
        .control_plane_instance_ledger()
        .expect("resume replay ledger")
        .resume(&authority, 18)
        .expect("resume replay");
    assert!(resume_replay.accepting_new_work);
    let draining = storage
        .control_plane_instance_ledger()
        .expect("second drain ledger")
        .request_drain(&authority, 19, 80)
        .expect("second drain");
    assert!(draining.drained());
    let released = storage
        .control_plane_instance_ledger()
        .expect("release ledger")
        .release(&authority, 20)
        .expect("release");
    assert_eq!(released.state, ControlPlaneInstanceState::Closed);
    assert!(!released.lease_valid);
    let release_replay = storage
        .control_plane_instance_ledger()
        .expect("release replay ledger")
        .release(&authority, 21)
        .expect("release replay");
    assert_eq!(release_replay, released);
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn command_commit_failure_rolls_back_product_state_and_preserves_claim_for_restart() {
    let root = temporary_directory("rollback");
    let command_identity = receipt(5);
    let command_digest = digest(5);
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = storage
        .control_plane_instance_ledger()
        .expect("ledger")
        .register(&identity(6), 10, 100)
        .expect("register");
    let claim = claimed(
        storage
            .control_plane_instance_ledger()
            .expect("claim ledger")
            .admit_command(&authority, 11, &command_identity, &command_digest)
            .expect("claim"),
    );
    let raw = Connection::open(storage.database_path()).expect("raw connection");
    raw.execute_batch(
        "CREATE TRIGGER fail_control_plane_receipt
         BEFORE INSERT ON command_receipts BEGIN
           SELECT RAISE(ABORT, 'injected receipt failure');
         END;",
    )
    .expect("install trigger");
    let failure = storage
        .control_plane_instance_ledger()
        .expect("commit ledger")
        .commit_claimed(
            &claim,
            12,
            &commit(5, command_identity.clone(), command_digest.clone()),
        )
        .expect_err("injected commit must fail");
    assert_eq!(failure.kind(), ControlPlaneInstanceErrorKind::Storage);
    assert!(
        storage
            .load_state("instance-test:5")
            .expect("load state")
            .is_none()
    );
    raw.execute_batch("DROP TRIGGER fail_control_plane_receipt;")
        .expect("remove trigger");
    drop(raw);
    drop(storage);

    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    let replayed_claim = claimed(
        restarted
            .control_plane_instance_ledger()
            .expect("restart ledger")
            .admit_command(&authority, 13, &command_identity, &command_digest)
            .expect("claim replay"),
    );
    assert!(replayed_claim.idempotent_replay());
    assert_eq!(replayed_claim.claim_fence(), claim.claim_fence());
    restarted
        .control_plane_instance_ledger()
        .expect("retry ledger")
        .commit_claimed(
            &replayed_claim,
            14,
            &commit(5, command_identity, command_digest),
        )
        .expect("retry commit");
    assert_eq!(
        restarted
            .load_state("instance-test:5")
            .expect("load retried state")
            .expect("retried state")
            .revision,
        1
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn late_renewal_changed_reuse_and_live_boot_collision_fail_closed() {
    let root = temporary_directory("negative");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let instance = identity(7);
    let authority = storage
        .control_plane_instance_ledger()
        .expect("ledger")
        .register(&instance, 10, 20)
        .expect("register");
    let collision = ControlPlaneInstanceIdentity::try_new(
        instance.instance_id(),
        "cpb_ffffffffffffffffffffffffffffffff",
    )
    .expect("collision identity");
    let conflict = storage
        .control_plane_instance_ledger()
        .expect("collision ledger")
        .register(&collision, 11, 30)
        .expect_err("live boot collision");
    assert_eq!(
        conflict.kind(),
        ControlPlaneInstanceErrorKind::LeaseConflict
    );
    let late = storage
        .control_plane_instance_ledger()
        .expect("renew ledger")
        .renew(&authority, 20, 40)
        .expect_err("expired lease cannot renew");
    assert_eq!(late.kind(), ControlPlaneInstanceErrorKind::OwnershipLost);

    let replacement = storage
        .control_plane_instance_ledger()
        .expect("replacement ledger")
        .register(&collision, 20, 50)
        .expect("replacement boot");
    assert!(replacement.generation() > authority.generation());
    let command_identity = receipt(6);
    storage
        .control_plane_instance_ledger()
        .expect("claim ledger")
        .admit_command(&replacement, 21, &command_identity, &digest(6))
        .expect("claim");
    let changed = storage
        .control_plane_instance_ledger()
        .expect("changed ledger")
        .admit_command(&replacement, 22, &command_identity, &digest(7))
        .expect_err("changed request reuse");
    assert_eq!(
        changed.kind(),
        ControlPlaneInstanceErrorKind::RequestConflict
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}
