// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::WorkspaceRevision;

use winwincode_domain::{
    ChangeBatchId, CodexThreadId, ExecutionJobId, FencingToken, LeaseId, ProductSessionId,
    RepositoryId, SessionIdentity, Sha256Digest, WorkerSessionId,
};
use winwincode_execution_port::{
    change_batch_identity::{
        ChangeBatchIdentityDerivationError, derive_change_batch_id,
        validate_change_batch_identity_derivation,
    },
    generated::ChangeBatchIdentity,
};

const PATCH_DIGEST: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn identity(call_id: Option<&str>) -> ChangeBatchIdentity {
    let patch_digest = Sha256Digest(PATCH_DIGEST.to_owned());
    let batch_id = derive_change_batch_id("run-key-1", "turn-1", call_id, &patch_digest)
        .expect("derive fixture batch ID");
    ChangeBatchIdentity {
        attempt: 1,
        batch_id,
        call_id: call_id.map(str::to_owned),
        fencing_token: FencingToken("1".to_owned()),
        job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
        lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
        patch_digest,
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        run_key: "run-key-1".to_owned(),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
            stage_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        turn_id: "turn-1".to_owned(),
        workspace_revision: WorkspaceRevision(
            "git-tree:0000000000000000000000000000000000000000".to_owned(),
        ),
    }
}

#[test]
fn fixed_vector_pins_the_cross_runtime_hash_contract() {
    let derived = derive_change_batch_id(
        "run-key-1",
        "turn-1",
        Some("call-1"),
        &Sha256Digest(PATCH_DIGEST.to_owned()),
    )
    .expect("derive fixed vector");
    assert_eq!(
        derived,
        ChangeBatchId(
            "sha256:ad2e6992cbb0386d043f7a01aea789ba906bec616c993b2bb820d1681173ec8a".to_owned()
        )
    );
}

#[test]
fn length_framing_prevents_adjacent_field_collisions() {
    let patch_digest = Sha256Digest(PATCH_DIGEST.to_owned());
    let left = derive_change_batch_id("ab", "c", None, &patch_digest).expect("left ID");
    let right = derive_change_batch_id("a", "bc", None, &patch_digest).expect("right ID");
    assert_ne!(left, right);
}

#[test]
fn optional_call_id_changes_identity_and_empty_some_is_rejected() {
    let patch_digest = Sha256Digest(PATCH_DIGEST.to_owned());
    let absent =
        derive_change_batch_id("run-key-1", "turn-1", None, &patch_digest).expect("absent call ID");
    let present = derive_change_batch_id("run-key-1", "turn-1", Some("call-1"), &patch_digest)
        .expect("present call ID");
    assert_ne!(absent, present);
    assert_eq!(
        derive_change_batch_id("run-key-1", "turn-1", Some(""), &patch_digest),
        Err(ChangeBatchIdentityDerivationError::InvalidCallId)
    );
}

#[test]
fn identity_validation_rejects_every_derived_field_tamper() {
    let valid = identity(Some("call-1"));
    validate_change_batch_identity_derivation(&valid).expect("valid identity");

    let mut cases = Vec::new();
    let mut changed = valid.clone();
    changed.batch_id = ChangeBatchId(format!("sha256:{}", "f".repeat(64)));
    cases.push(changed);
    let mut changed = valid.clone();
    changed.run_key = "run-key-2".to_owned();
    cases.push(changed);
    let mut changed = valid.clone();
    changed.turn_id = "turn-2".to_owned();
    cases.push(changed);
    let mut changed = valid.clone();
    changed.call_id = None;
    cases.push(changed);
    let mut changed = valid;
    changed.patch_digest = Sha256Digest(format!("sha256:{}", "2".repeat(64)));
    cases.push(changed);

    for changed in cases {
        assert_eq!(
            validate_change_batch_identity_derivation(&changed),
            Err(ChangeBatchIdentityDerivationError::BatchIdMismatch)
        );
    }
}

#[test]
fn delivery_authority_changes_do_not_change_content_derivation() {
    let mut replayed = identity(None);
    replayed.attempt = 2;
    replayed.job_id = ExecutionJobId("job_00000000000000000000000001".to_owned());
    replayed.lease_id = LeaseId("lse_00000000000000000000000001".to_owned());
    replayed.fencing_token = FencingToken("2".to_owned());
    replayed.workspace_revision =
        WorkspaceRevision("git-tree:ffffffffffffffffffffffffffffffffffffffff".to_owned());
    validate_change_batch_identity_derivation(&replayed)
        .expect("delivery authority is outside the content-derived ID");
}

#[test]
fn derivation_rejects_noncanonical_inputs_before_hashing() {
    let valid_digest = Sha256Digest(PATCH_DIGEST.to_owned());
    assert_eq!(
        derive_change_batch_id("", "turn-1", None, &valid_digest),
        Err(ChangeBatchIdentityDerivationError::InvalidRunKey)
    );
    assert_eq!(
        derive_change_batch_id("run-key-1", "turn 1", None, &valid_digest),
        Err(ChangeBatchIdentityDerivationError::InvalidTurnId)
    );
    assert_eq!(
        derive_change_batch_id(
            "run-key-1",
            "turn-1",
            None,
            &Sha256Digest(format!("sha256:{}", "F".repeat(64))),
        ),
        Err(ChangeBatchIdentityDerivationError::InvalidPatchDigest)
    );
}
