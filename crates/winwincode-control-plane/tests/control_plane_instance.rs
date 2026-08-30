use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    ControlPlaneInstanceRuntime, ControlPlaneInstanceRuntimeConfig,
    ControlPlaneInstanceRuntimeErrorKind,
};
use winwincode_domain::{RequestId, Sha256Digest};
use winwincode_storage::{
    ControlPlaneCommandAdmission, ControlPlaneInstanceErrorKind, ControlPlaneInstanceIdentity,
    ControlPlaneInstanceState, NewOutboxEvent, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-instance-runtime-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn instance(seed: u64) -> ControlPlaneInstanceIdentity {
    ControlPlaneInstanceIdentity::try_new(format!("cpi_{seed:032x}"), format!("cpb_{seed:032x}"))
        .expect("instance identity")
}

fn receipt(seed: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(format!("actor-{seed}").into_bytes()).expect("actor"),
        ReceiptScopeKey::from_encoded(format!("scope-{seed}").into_bytes()).expect("scope"),
        RequestId(format!("req_{seed:026}")),
    )
    .expect("receipt")
}

fn digest(seed: u64) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"control-plane-instance-runtime-test\0");
    digest.update(seed.to_be_bytes());
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn commit(seed: u64, identity: ReceiptIdentity, digest: Sha256Digest) -> StateCommit {
    StateCommit::new(
        identity,
        digest,
        format!("runtime-instance:{seed}"),
        0,
        format!("state-{seed}").into_bytes(),
        vec![NewOutboxEvent::internal(
            format!("evt_runtime_instance_{seed:016x}"),
            "runtime-instance.test.v1",
            b"{}".to_vec(),
        )],
    )
}

#[test]
fn runtime_restart_observes_the_original_receipt_and_drain_projection() {
    let root = temporary_directory("restart");
    let config = ControlPlaneInstanceRuntimeConfig::try_new(100, 50).expect("config");
    let command_identity = receipt(1);
    let command_digest = digest(1);
    let mut first =
        ControlPlaneInstanceRuntime::start_with_identity(&root, &instance(1), 10, config)
            .expect("first runtime");
    let admission = first
        .admit_command(11, &command_identity, &command_digest)
        .expect("admit");
    let ControlPlaneCommandAdmission::Claimed(claim) = admission else {
        panic!("first attempt must own the claim");
    };
    first
        .commit_claimed(
            12,
            &claim,
            &commit(1, command_identity.clone(), command_digest.clone()),
        )
        .expect("commit");
    let first_health = first.preflight(13).expect("health");
    assert!(first_health.accepting_new_work);
    assert_eq!(first_health.confirmed_state_sequence, 1);
    drop(first);

    let mut restarted =
        ControlPlaneInstanceRuntime::start_with_identity(&root, &instance(2), 20, config)
            .expect("restart runtime");
    let replay = restarted
        .admit_command(21, &command_identity, &command_digest)
        .expect("replay");
    let ControlPlaneCommandAdmission::Committed(replay) = replay else {
        panic!("restart must resolve the durable receipt first");
    };
    assert_eq!(replay.revision, 1);
    let draining = restarted.begin_drain(22).expect("drain");
    assert_eq!(draining.state, ControlPlaneInstanceState::Draining);
    assert!(draining.drained());
    let released = restarted.release(23).expect("release");
    assert_eq!(released.state, ControlPlaneInstanceState::Closed);
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn runtime_renews_before_expiry_and_fails_closed_after_expiry() {
    let root = temporary_directory("renew");
    let config = ControlPlaneInstanceRuntimeConfig::try_new(10, 20).expect("config");
    let mut runtime =
        ControlPlaneInstanceRuntime::start_with_identity(&root, &instance(3), 100, config)
            .expect("runtime");
    let renewed = runtime.renew(105).expect("renew");
    assert_eq!(renewed.lease_expires_at, 115);
    let error = runtime.renew(115).expect_err("late renewal");
    assert_eq!(
        error.kind(),
        ControlPlaneInstanceRuntimeErrorKind::Instance(
            ControlPlaneInstanceErrorKind::OwnershipLost
        )
    );
    drop(runtime);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn production_start_generates_bounded_distinct_instance_and_boot_identities() {
    let first_root = temporary_directory("entropy-first");
    let second_root = temporary_directory("entropy-second");
    let config = ControlPlaneInstanceRuntimeConfig::default();
    let first = ControlPlaneInstanceRuntime::start(&first_root, 1, config).expect("first runtime");
    let second =
        ControlPlaneInstanceRuntime::start(&second_root, 1, config).expect("second runtime");
    assert_ne!(first.authority().identity(), second.authority().identity());
    assert!(
        first
            .authority()
            .identity()
            .instance_id()
            .starts_with("cpi_")
    );
    assert!(first.authority().identity().boot_id().starts_with("cpb_"));
    drop(first);
    drop(second);
    fs::remove_dir_all(first_root).expect("first cleanup");
    fs::remove_dir_all(second_root).expect("second cleanup");
}
