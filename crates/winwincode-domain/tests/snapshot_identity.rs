// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    CandidateId, CanonicalSnapshot, Snapshot, SnapshotBindingError, SnapshotId,
    SnapshotVerificationBinding, VerificationSessionId, WorkRunId, seal_snapshot,
    verify_snapshot_seal,
};

const SNAPSHOT_JSON: &str = r#"{
  "schemaVersion":"winwincode/v1",
  "snapshotId":"snap_01J00000000000000000000000",
  "candidateId":"cnd_01J00000000000000000000000",
  "workRunId":"wrn_01J00000000000000000000000",
  "repositoryId":"rep_01J00000000000000000000000",
  "baseCommitId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "baseTreeId":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "candidateCommitId":"cccccccccccccccccccccccccccccccccccccccc",
  "candidateTreeId":"dddddddddddddddddddddddddddddddddddddddd",
  "diffSha256":"sha256:1111111111111111111111111111111111111111111111111111111111111111",
  "contentDigest":"sha256:2222222222222222222222222222222222222222222222222222222222222222",
  "validationSeal":"sha256:c681ae94290fd5a7edb72dd302423b71dd15a493119d65cbb5d69856da031089",
  "createdAtMillis":1769150400000,
  "immutable":true
}"#;

#[test]
fn canonical_snapshot_strictly_round_trips_generated_contract() {
    let contract: Snapshot = serde_json::from_str(SNAPSHOT_JSON).expect("generated Snapshot JSON");
    let snapshot = CanonicalSnapshot::try_from(contract.clone()).expect("canonical Snapshot");
    let serialized = serde_json::to_string(&snapshot).expect("serialize CanonicalSnapshot");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serialized).expect("serialized JSON"),
        serde_json::from_str::<serde_json::Value>(SNAPSHOT_JSON).expect("fixture JSON"),
    );

    let decoded: CanonicalSnapshot = serde_json::from_str(&serialized).expect("strict decode");
    assert_eq!(decoded, snapshot);
    assert!(verify_snapshot_seal(&snapshot));
    assert_eq!(
        seal_snapshot(&contract),
        winwincode_domain::Sha256Digest(
            "sha256:c681ae94290fd5a7edb72dd302423b71dd15a493119d65cbb5d69856da031089".to_owned(),
        ),
    );

    let mut tampered = contract;
    tampered.content_digest = winwincode_domain::Sha256Digest(format!("sha256:{}", "f".repeat(64)));
    assert_ne!(seal_snapshot(&tampered), tampered.validation_seal);
}

#[test]
fn canonical_snapshot_rejects_malformed_and_legacy_identity() {
    for (needle, replacement, expected) in [
        (
            r#""snapshotId":"snap_01J00000000000000000000000""#,
            r#""snapshotId":"candidate_1""#,
            "Snapshot identity is not canonical",
        ),
        (
            r#""candidateId":"cnd_01J00000000000000000000000""#,
            r#""candidateId":"candidate_1""#,
            "Snapshot identity is not canonical",
        ),
        (
            r#""baseCommitId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa""#,
            r#""baseCommitId":"HEAD""#,
            "Snapshot code identity is malformed",
        ),
        (
            r#""candidateCommitId":"cccccccccccccccccccccccccccccccccccccccc""#,
            r#""candidateCommitId":"""#,
            "Snapshot code identity is malformed",
        ),
        (
            r#""immutable":true"#,
            r#""immutable":false"#,
            "Snapshot must be immutable",
        ),
        (
            r#""contentDigest":"sha256:2222222222222222222222222222222222222222222222222222222222222222""#,
            r#""contentDigest":"digest""#,
            "Snapshot code identity is malformed",
        ),
        (
            r#""immutable":true"#,
            r#""immutable":true,"candidateRef":"git-candidate:legacy""#,
            "unknown field",
        ),
    ] {
        let malformed = SNAPSHOT_JSON.replace(needle, replacement);
        let error = serde_json::from_str::<CanonicalSnapshot>(&malformed)
            .expect_err("malformed Snapshot must fail");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn verification_binding_rejects_foreign_snapshot_candidate_run_and_session() {
    let contract: Snapshot = serde_json::from_str(SNAPSHOT_JSON).expect("generated Snapshot JSON");
    let snapshot = CanonicalSnapshot::try_from(contract).expect("canonical Snapshot");
    let session_id = VerificationSessionId("vsn_01J00000000000000000000000".to_owned());
    let binding = snapshot.verification_binding(session_id.clone());

    binding
        .accept(
            snapshot.snapshot_id(),
            snapshot.candidate_id(),
            snapshot.work_run_id(),
            &session_id,
        )
        .expect("exact binding");

    assert!(matches!(
        binding.accept(
            &SnapshotId("snap_01J00000000000000000000001".to_owned()),
            snapshot.candidate_id(),
            snapshot.work_run_id(),
            &session_id,
        ),
        Err(SnapshotBindingError::SnapshotMismatch { .. })
    ));
    assert!(matches!(
        binding.accept(
            snapshot.snapshot_id(),
            &CandidateId("cnd_01J00000000000000000000001".to_owned()),
            snapshot.work_run_id(),
            &session_id,
        ),
        Err(SnapshotBindingError::CandidateMismatch { .. })
    ));
    assert!(matches!(
        binding.accept(
            snapshot.snapshot_id(),
            snapshot.candidate_id(),
            &WorkRunId("wrn_01J00000000000000000000001".to_owned()),
            &session_id,
        ),
        Err(SnapshotBindingError::WorkRunMismatch { .. })
    ));
    assert!(matches!(
        binding.accept(
            snapshot.snapshot_id(),
            snapshot.candidate_id(),
            snapshot.work_run_id(),
            &VerificationSessionId("vsn_01J00000000000000000000001".to_owned()),
        ),
        Err(SnapshotBindingError::VerificationSessionMismatch { .. })
    ));

    assert_eq!(
        serde_json::to_value(&binding).expect("binding JSON"),
        serde_json::json!({
            "snapshotId": snapshot.snapshot_id().0,
            "candidateId": snapshot.candidate_id().0,
            "workRunId": snapshot.work_run_id().0,
            "verificationSessionId": session_id.0,
        }),
    );
    assert_eq!(
        SnapshotVerificationBinding::from_json(
            &serde_json::to_value(&binding).expect("binding JSON")
        )
        .expect("binding decode"),
        binding,
    );
    assert!(
        SnapshotVerificationBinding::from_json(&serde_json::json!({
            "snapshotId": snapshot.snapshot_id().0,
            "candidateRef": "git-candidate:legacy",
        }))
        .is_err()
    );
}
