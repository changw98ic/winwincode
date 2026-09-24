// SPDX-License-Identifier: Apache-2.0

use winwincode_delivery::domain::snapshot::{SnapshotBuilder, verify_seal};

fn builder() -> SnapshotBuilder {
    SnapshotBuilder::new(
        "cnd_00000000000000000000000001",
        "wrn_00000000000000000000000001",
        "rep_00000000000000000000000001",
    )
    .with_base("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .with_candidate(
        "cccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddd",
    )
    .with_diff_sha256("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    .with_content_digest("sha256:1111111111111111111111111111111111111111111111111111111111111111")
    .with_created_at_millis(1_800_000_000_000)
}

#[test]
fn snapshot_carries_the_exact_code_identity_and_a_seal() {
    let snapshot = builder().build().expect("snapshot");
    assert!(snapshot.snapshot_id().as_str().starts_with("snap_"));
    assert_eq!(
        snapshot.candidate_commit_id(),
        "cccccccccccccccccccccccccccccccccccccccc"
    );
    assert!(verify_seal(&snapshot), "the seal must cover the code identity");
}

#[test]
fn snapshot_seal_breaks_when_the_code_identity_changes() {
    let good = builder().build().expect("snapshot");
    let mut tampered = good.clone();
    // Simulate a rewritten candidate commit under the same snapshot id.
    tampered.force_candidate_commit_for_test("ffffffffffffffffffffffffffffffffffffffff");
    assert!(!verify_seal(&tampered), "a rewritten identity must fail its own seal");
}

#[test]
fn snapshot_requires_a_candidate_commit() {
    let incomplete = SnapshotBuilder::new(
        "cnd_00000000000000000000000002",
        "wrn_00000000000000000000000002",
        "rep_00000000000000000000000002",
    );
    assert!(incomplete.build().is_err(), "a snapshot without a candidate commit is not buildable");
}

#[test]
fn snapshot_accepts_the_job_candidate_ref_verbatim() {
    // The job carries `work_input.candidate_ref` as a plain string. The
    // snapshot must not demand a `cnd_` newtype it has no source for.
    let snapshot = SnapshotBuilder::new(
        "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "wrn_00000000000000000000000004",
        "rep_00000000000000000000000004",
    )
    .with_base(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .with_candidate(
        "cccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddd",
    )
    .with_diff_sha256("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    .with_content_digest("sha256:1111111111111111111111111111111111111111111111111111111111111111")
    .with_created_at_millis(1_800_000_000_000)
    .build()
    .expect("snapshot from a plain candidate ref");
    assert!(verify_seal(&snapshot));
}
