// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};
use winwincode_change_batch::{
    AppliedDeltaError, ChangeBatchPlanError, ChangeBatchPolicy, MAX_FILES, MAX_HUNKS,
    MAX_PATCH_BYTES, PlannedFileChange, PreflightPathState, canonical_applied_file_summaries,
    derive_delta_digest, prepare_change_batch,
};
use winwincode_domain::{
    ChangeBatchId, CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId,
    RepositoryId, SessionIdentity, Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    change_batch_identity::derive_change_batch_id,
    generated::{
        AppliedFileOperation, AppliedFileSummary, ChangeBatchIdentity, ChangeBatchProposal,
        ChangeBatchProposalDisposition, ChangeBatchProposalEvent, ValidationProfileName,
    },
};

fn patch_digest(patch: &str) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(patch.as_bytes())))
}

fn event(patch: String) -> ChangeBatchProposalEvent {
    let digest = patch_digest(&patch);
    let run_key = "run-key-1";
    let turn_id = "turn-1";
    let batch_id = derive_change_batch_id(run_key, turn_id, None, &digest).expect("batch ID");
    ChangeBatchProposalEvent {
        identity: ChangeBatchIdentity {
            attempt: 1,
            batch_id,
            call_id: None,
            fencing_token: FencingToken("1".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
            lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
            patch_digest: digest,
            repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
            run_key: run_key.to_owned(),
            session_identity: SessionIdentity {
                product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
                stage_run_id: None,
                worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
                codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            },
            turn_id: turn_id.to_owned(),
            workspace_revision: WorkspaceRevision(
                "git-tree:0123456789abcdef0123456789abcdef01234567".to_owned(),
            ),
        },
        occurred_at: Instant("2026-09-01T00:00:00.000Z".to_owned()),
        proposal: ChangeBatchProposal {
            acceptance_criteria_ids: vec!["criterion-1".to_owned()],
            disposition: ChangeBatchProposalDisposition::Final,
            patch,
            schema_version: 1,
            validation_profile: ValidationProfileName::Fast,
        },
    }
}

fn add_files(count: usize) -> String {
    let mut patch = String::from("*** Begin Patch\n");
    for index in 0..count {
        write!(
            &mut patch,
            "*** Add File: src/file-{index:03}.txt\n+content-{index}\n"
        )
        .expect("write patch fixture");
    }
    patch.push_str("*** End Patch");
    patch
}

fn update_chunks(count: usize) -> String {
    let mut patch = String::from("*** Begin Patch\n*** Update File: src/file.txt\n");
    for index in 0..count {
        write!(&mut patch, "@@\n-old-{index}\n+new-{index}\n").expect("write patch fixture");
    }
    patch.push_str("*** End Patch");
    patch
}

fn exact_size_add_patch(size: usize, character: char) -> String {
    let prefix = "*** Begin Patch\n*** Add File: src/file.txt\n+";
    let suffix = "\n*** End Patch";
    let fixed = prefix.len() + suffix.len();
    assert!(size >= fixed);
    let unit = character.len_utf8();
    assert_eq!((size - fixed) % unit, 0);
    format!(
        "{prefix}{}{suffix}",
        character.to_string().repeat((size - fixed) / unit)
    )
}

#[test]
fn accepts_twenty_paths_and_rejects_twenty_one() {
    let plan = prepare_change_batch(&event(add_files(MAX_FILES)), ChangeBatchPolicy::default())
        .expect("20 files");
    assert_eq!(plan.touched_paths().len(), MAX_FILES);
    assert_eq!(plan.operations().len(), MAX_FILES);
    assert_eq!(plan.hunk_count(), MAX_FILES);

    assert!(matches!(
        prepare_change_batch(
            &event(add_files(MAX_FILES + 1)),
            ChangeBatchPolicy::default()
        ),
        Err(ChangeBatchPlanError::TooManyFiles {
            actual: 21,
            maximum: 20
        })
    ));
}

#[test]
fn counts_real_update_chunks_and_rejects_single_update_with_101_chunks() {
    let plan = prepare_change_batch(
        &event(update_chunks(MAX_HUNKS)),
        ChangeBatchPolicy::default(),
    )
    .expect("100 chunks");
    assert_eq!(plan.hunk_count(), MAX_HUNKS);
    assert!(matches!(
        &plan.operations()[0],
        PlannedFileChange::Update {
            chunk_count: 100,
            ..
        }
    ));

    assert!(matches!(
        prepare_change_batch(
            &event(update_chunks(MAX_HUNKS + 1)),
            ChangeBatchPolicy::default()
        ),
        Err(ChangeBatchPlanError::TooManyHunks {
            actual: 101,
            maximum: 100
        })
    ));
}

#[test]
fn enforces_utf8_byte_limit_before_parser_or_schema_character_limits() {
    let exact = exact_size_add_patch(MAX_PATCH_BYTES, 'a');
    let plan = prepare_change_batch(&event(exact), ChangeBatchPolicy::default())
        .expect("exact byte limit");
    assert_eq!(plan.patch_bytes(), MAX_PATCH_BYTES);

    let oversized = exact_size_add_patch(MAX_PATCH_BYTES + 1, 'a');
    assert!(matches!(
        prepare_change_batch(&event(oversized), ChangeBatchPolicy::default()),
        Err(ChangeBatchPlanError::PatchTooLarge {
            actual: 524_289,
            maximum: 524_288
        })
    ));

    let multibyte_size = MAX_PATCH_BYTES + 2;
    let multibyte = exact_size_add_patch(multibyte_size, 'é');
    assert!(multibyte.chars().count() < MAX_PATCH_BYTES);
    assert!(matches!(
        prepare_change_batch(&event(multibyte), ChangeBatchPolicy::default()),
        Err(ChangeBatchPlanError::PatchTooLarge { actual, .. }) if actual == multibyte_size
    ));
}

#[test]
fn rejects_conflicting_add_and_move_graphs() {
    let add_existing = concat!(
        "*** Begin Patch\n",
        "*** Add File: src/file.txt\n+created\n",
        "*** Delete File: src/file.txt\n",
        "*** End Patch"
    );
    assert!(matches!(
        prepare_change_batch(&event(add_existing.to_owned()), ChangeBatchPolicy::default()),
        Err(ChangeBatchPlanError::ConflictingPath(path)) if path == "src/file.txt"
    ));

    let move_collision = concat!(
        "*** Begin Patch\n",
        "*** Add File: src/destination.txt\n+created\n",
        "*** Update File: src/source.txt\n",
        "*** Move to: src/destination.txt\n",
        "@@\n-old\n+new\n",
        "*** End Patch"
    );
    assert!(matches!(
        prepare_change_batch(&event(move_collision.to_owned()), ChangeBatchPolicy::default()),
        Err(ChangeBatchPlanError::ConflictingPath(path)) if path == "src/destination.txt"
    ));
}

#[test]
fn emits_sorted_operations_paths_and_no_follow_preflight_requirements() {
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Delete File: z/delete.txt\n",
        "*** Update File: m/source.txt\n",
        "*** Move to: a/destination.txt\n",
        "@@\n-old\n+new\n",
        "*** Add File: b/add.txt\n+created\n",
        "*** Update File: c/update.txt\n",
        "@@\n-old\n+new\n",
        "*** End Patch"
    );
    let plan = prepare_change_batch(&event(patch.to_owned()), ChangeBatchPolicy::default())
        .expect("canonical plan");
    assert_eq!(
        plan.operations()
            .iter()
            .map(PlannedFileChange::path)
            .collect::<Vec<_>>(),
        ["b/add.txt", "c/update.txt", "m/source.txt", "z/delete.txt"]
    );
    assert_eq!(
        plan.touched_paths(),
        [
            "a/destination.txt",
            "b/add.txt",
            "c/update.txt",
            "m/source.txt",
            "z/delete.txt"
        ]
    );
    assert_eq!(
        plan.preflight_requirements()
            .iter()
            .map(|requirement| (requirement.path(), requirement.state()))
            .collect::<Vec<_>>(),
        [
            ("a/destination.txt", PreflightPathState::Absent),
            ("b/add.txt", PreflightPathState::Absent),
            ("c/update.txt", PreflightPathState::RegularUtf8File),
            ("m/source.txt", PreflightPathState::RegularUtf8File),
            ("z/delete.txt", PreflightPathState::RegularUtf8File),
        ]
    );
}

#[test]
fn rejects_traversal_and_nonportable_paths() {
    for path in [
        "../outside.txt",
        "/absolute.txt",
        "src\\windows.txt",
        "src//double.txt",
        "C:/windows.txt",
        "src/file:stream.txt",
        "src/bad?.txt",
        "src/trailing. ",
        "src/CON.txt",
        "src/com1.log",
    ] {
        let patch = format!("*** Begin Patch\n*** Add File: {path}\n+content\n*** End Patch");
        assert!(
            prepare_change_batch(&event(patch), ChangeBatchPolicy::default()).is_err(),
            "path: {path}"
        );
    }
}

#[test]
fn rejects_identity_and_patch_tampering() {
    let patch = add_files(1);
    let mut changed_batch = event(patch.clone());
    changed_batch.identity.batch_id = ChangeBatchId(format!("sha256:{}", "f".repeat(64)));
    assert_eq!(
        prepare_change_batch(&changed_batch, ChangeBatchPolicy::default()),
        Err(ChangeBatchPlanError::InvalidIdentity)
    );

    let mut changed_content = event(patch);
    changed_content.proposal.patch = add_files(2);
    assert_eq!(
        prepare_change_batch(&changed_content, ChangeBatchPolicy::default()),
        Err(ChangeBatchPlanError::PatchDigestMismatch)
    );
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn update_summary(path: &str, before: char, after: char) -> AppliedFileSummary {
    AppliedFileSummary {
        after_sha256: Some(digest(after)),
        before_sha256: Some(digest(before)),
        bytes_after: 7,
        bytes_before: 6,
        mode_after: Some("0644".to_owned()),
        mode_before: Some("0644".to_owned()),
        move_path: None,
        operation: AppliedFileOperation::Update,
        path: path.to_owned(),
    }
}

#[test]
fn applied_summaries_sort_and_digest_with_a_fixed_vector() {
    let summaries = vec![
        update_summary("src/z.txt", '2', '3'),
        update_summary("src/a.txt", '0', '1'),
    ];
    let sorted = canonical_applied_file_summaries(&summaries).expect("exact summaries");
    assert_eq!(
        sorted
            .iter()
            .map(|summary| summary.path.as_str())
            .collect::<Vec<_>>(),
        ["src/a.txt", "src/z.txt"]
    );
    assert_eq!(
        derive_delta_digest(&summaries).expect("delta digest").0,
        "sha256:b12663b20c92e74b6f0cb515a5c9bf6f6680b0e02b24579f14932f7056e306bf"
    );
}

#[test]
fn plan_digest_has_a_fixed_vector_and_binds_preimage_policy() {
    let event = event(add_files(1));
    let default = prepare_change_batch(&event, ChangeBatchPolicy::default()).expect("default plan");
    assert_eq!(
        default.plan_digest().0,
        "sha256:7e096ec7e3e5adb0728c93b6388e81b5177974f53b4081f045b8d85b5c8a80ad"
    );

    let stricter = prepare_change_batch(
        &event,
        ChangeBatchPolicy::with_max_preimage_bytes(1_024).expect("stricter policy"),
    )
    .expect("stricter plan");
    assert_ne!(default.plan_digest(), stricter.plan_digest());
    assert_eq!(stricter.max_preimage_bytes(), 1_024);
}

#[test]
fn exact_delta_rejects_conflicting_or_incomplete_summaries() {
    let duplicate = update_summary("src/file.txt", '0', '1');
    assert_eq!(
        derive_delta_digest(&[duplicate.clone(), duplicate]),
        Err(AppliedDeltaError::ConflictingPath)
    );

    let mut missing_after = update_summary("src/file.txt", '0', '1');
    missing_after.after_sha256 = None;
    assert_eq!(
        derive_delta_digest(&[missing_after]),
        Err(AppliedDeltaError::InvalidSummary)
    );
}
