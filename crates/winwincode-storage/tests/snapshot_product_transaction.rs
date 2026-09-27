// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    Candidate, CandidateDigest, CandidateId, CanonicalSnapshot, CodexThreadId, ExecutionJobId,
    GitObjectId, OrganizationId, ProductSessionId, RepositoryId, RequestId, Revision,
    SchemaVersion, Sha256Digest, Snapshot, SnapshotId, WorkContractId, WorkItemId, WorkRunId,
    WorkerSessionId, seal_snapshot, verify_snapshot_seal,
};
use winwincode_storage::{
    ProductStateStorage, PublicEventScope, ReceiptActorKey, ReceiptIdentity, SnapshotBindingCheck,
    SnapshotProductCommit, SnapshotProductStaged, SnapshotVerificationBinding, SqliteStorage,
    StorageErrorKind, commit_snapshot_product, receipt_scope_key, validate_snapshot_binding,
};

fn assert_product_result_eq(left: &SnapshotProductCommit, right: &SnapshotProductCommit) {
    assert_eq!(left.candidate, right.candidate);
    assert_eq!(left.snapshot, right.snapshot);
    assert_eq!(left.binding, right.binding);
    assert_eq!(left.dispatch, right.dispatch);
}

fn prefixed(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn digest(value: u8) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:064x}", u128::from(value)))
}

fn candidate(content: u8) -> Candidate {
    Candidate {
        schema_version: SchemaVersion::WinwincodeV1,
        id: CandidateId(prefixed("cnd", 1)),
        work_contract_id: WorkContractId(prefixed("wct", 1)),
        contract_revision: Revision(1),
        work_item_id: WorkItemId(prefixed("wit", 1)),
        work_run_id: WorkRunId(prefixed("wrn", 1)),
        attempt: 1,
        producer_worker_session_id: WorkerSessionId(prefixed("wsn", 1)),
        candidate_ref: "refs/winwincode/candidates/3333333333333333333333333333333333333333"
            .to_owned(),
        candidate_digest: CandidateDigest(digest(content.saturating_add(20)).0),
        base_commit: "1111111111111111111111111111111111111111".to_owned(),
        candidate_commit: "3333333333333333333333333333333333333333".to_owned(),
        candidate_tree: "4444444444444444444444444444444444444444".to_owned(),
        diff_digest: digest(content),
    }
}

fn snapshot(content: u8) -> Snapshot {
    let candidate = candidate(content);
    let mut snapshot = Snapshot {
        schema_version: SchemaVersion::WinwincodeV1,
        snapshot_id: SnapshotId(prefixed("snap", 1)),
        candidate_id: candidate.id,
        work_run_id: candidate.work_run_id,
        repository_id: RepositoryId(prefixed("rep", 1)),
        base_commit_id: GitObjectId(candidate.base_commit),
        base_tree_id: GitObjectId("2222222222222222222222222222222222222222".to_owned()),
        candidate_commit_id: GitObjectId(candidate.candidate_commit),
        candidate_tree_id: GitObjectId(candidate.candidate_tree),
        diff_sha256: candidate.diff_digest,
        content_digest: digest(content.saturating_add(10)),
        validation_seal: digest(0),
        created_at_millis: 1_800_000_000_000,
        immutable: true,
    };
    snapshot.validation_seal = seal_snapshot(&snapshot);
    snapshot
}

fn binding() -> SnapshotVerificationBinding {
    SnapshotVerificationBinding {
        verification_plan: winwincode_domain::VerificationPlan {
            schema_version: SchemaVersion::WinwincodeV1,
            id: winwincode_domain::VerificationPlanId(prefixed("vpl", 2)),
            candidate_digest: candidate(1).candidate_digest,
            work_contract_id: candidate(1).work_contract_id,
            contract_revision: Revision(1),
            work_item_id: candidate(1).work_item_id,
            work_item_revision: Revision(1),
            work_run_id: WorkRunId(prefixed("wrn", 2)),
            plan_revision: Revision(1),
            criterion_ids: vec![winwincode_domain::CriterionId(prefixed("crt", 1))],
            commands: vec!["cargo test".into()],
            permission_profile: "candidate-read-only".into(),
            required_roles: vec!["verifier".into()],
        },
        verification_session: winwincode_domain::VerificationSession {
            schema_version: SchemaVersion::WinwincodeV1,
            id: winwincode_domain::VerificationSessionId(prefixed("vsn", 2)),
            verification_session_id: winwincode_domain::VerificationSessionId(prefixed("vsn", 2)),
            verification_plan_id: winwincode_domain::VerificationPlanId(prefixed("vpl", 2)),
            snapshot_id: SnapshotId(prefixed("snap", 1)),
            candidate_id: candidate(1).id,
            work_run_id: WorkRunId(prefixed("wrn", 2)),
            attempt: 1,
            created_at: winwincode_domain::Instant("2027-01-15T08:00:00.000Z".into()),
            session_identity: winwincode_domain::SessionIdentity {
                work_run_id: Some(WorkRunId(prefixed("wrn", 2))),
                product_session_id: ProductSessionId(prefixed("psn", 2)),
                worker_session_id: WorkerSessionId(prefixed("wsn", 2)),
                codex_thread_id: CodexThreadId(prefixed("cdx", 2)),
            },
        },
        session_binding_id: prefixed("sbn", 2),
        work_run_id: WorkRunId(prefixed("wrn", 2)),
        execution_job_id: ExecutionJobId(prefixed("job", 2)),
        product_session_id: ProductSessionId(prefixed("psn", 2)),
        worker_session_id: Some(WorkerSessionId(prefixed("wsn", 2))),
        codex_thread_id: Some(CodexThreadId(prefixed("cdx", 2))),
        verification_role: "verifier".to_owned(),
        attempt: 1,
    }
}

fn request(request: u8) -> (ReceiptIdentity, Sha256Digest) {
    (
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(b"test-actor".to_vec()).expect("actor key"),
            receipt_scope_key(&PublicEventScope::Organization {
                organization_id: OrganizationId(prefixed("org", 1)),
            })
            .expect("scope key"),
            RequestId(prefixed("req", u64::from(request))),
        )
        .expect("receipt identity"),
        digest(request),
    )
}

#[test]
fn snapshot_candidate_binding_and_dispatch_commit_and_replay_together() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-snapshot-product-{}-1",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let staged = SnapshotProductStaged::new(
        candidate(1),
        snapshot(1).try_into().expect("canonical snapshot"),
        binding(),
    )
    .expect("stage snapshot product");
    let (identity, command_digest) = request(1);

    let first = commit_snapshot_product(
        &mut storage,
        identity.clone(),
        command_digest.clone(),
        &staged,
    )
    .expect("first snapshot commit");
    assert!(!first.receipt.idempotent_replay);
    assert_eq!(first.candidate.id, CandidateId(prefixed("cnd", 1)));
    assert_eq!(
        first.snapshot.snapshot_id(),
        &SnapshotId(prefixed("snap", 1))
    );
    assert_eq!(first.dispatch.topic, "verification.snapshot.dispatch");

    // Reopen after the commit, before the dispatch is consumed, as after a crash.
    drop(storage);
    let mut storage = SqliteStorage::open(&root).expect("reopen committed snapshot");
    let durable = storage
        .load_state(&format!("snapshot-product:v1:{}", prefixed("snap", 1)))
        .expect("load snapshot")
        .expect("persisted snapshot");
    let decoded: Snapshot =
        serde_json::from_slice(&durable.payload).expect("generated schema shape");
    assert_eq!(decoded, snapshot(1));
    let canonical: CanonicalSnapshot =
        serde_json::from_slice(&durable.payload).expect("strict snapshot");
    assert!(verify_snapshot_seal(&canonical));
    assert_eq!(canonical, first.snapshot);

    let replay = commit_snapshot_product(&mut storage, identity, command_digest, &staged)
        .expect("snapshot replay");
    assert!(replay.receipt.idempotent_replay);
    assert_product_result_eq(&replay, &first);

    let bound = validate_snapshot_binding(
        &storage,
        &SnapshotBindingCheck::new(
            SnapshotId(prefixed("snap", 1)),
            ExecutionJobId(prefixed("job", 2)),
            WorkRunId(prefixed("wrn", 2)),
            ProductSessionId(prefixed("psn", 2)),
            1,
        ),
    )
    .expect("exact snapshot binding");
    assert_eq!(bound.snapshot_id, SnapshotId(prefixed("snap", 1)));

    for (snapshot, job, run, session, attempt) in [
        (9, 2, 2, 2, 1),
        (1, 9, 2, 2, 1),
        (1, 2, 9, 2, 1),
        (1, 2, 2, 9, 1),
        (1, 2, 2, 2, 2),
    ] {
        let check = SnapshotBindingCheck::new(
            SnapshotId(prefixed("snap", snapshot)),
            ExecutionJobId(prefixed("job", job)),
            WorkRunId(prefixed("wrn", run)),
            ProductSessionId(prefixed("psn", session)),
            attempt,
        );
        assert!(
            validate_snapshot_binding(&storage, &check).is_err(),
            "foreign binding: {check:?}"
        );
    }

    drop(storage);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn snapshot_replay_rejects_changed_product_result_under_the_same_request() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-snapshot-product-{}-2",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let staged = SnapshotProductStaged::new(
        candidate(1),
        snapshot(1).try_into().expect("canonical snapshot"),
        binding(),
    )
    .expect("stage snapshot product");
    let (identity, command_digest) = request(2);

    commit_snapshot_product(
        &mut storage,
        identity.clone(),
        command_digest.clone(),
        &staged,
    )
    .expect("first snapshot commit");

    let changed = SnapshotProductStaged::new(
        candidate(2),
        snapshot(2).try_into().expect("canonical snapshot"),
        SnapshotVerificationBinding {
            verification_plan: winwincode_domain::VerificationPlan {
                candidate_digest: candidate(2).candidate_digest,
                ..binding().verification_plan
            },
            ..binding()
        },
    )
    .expect("stage changed snapshot product");
    let error = commit_snapshot_product(&mut storage, identity, command_digest, &changed)
        .expect_err("changed product result must not replay as exact");
    assert_eq!(error.kind(), StorageErrorKind::RequestConflict);

    drop(storage);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn staging_rejects_a_sealed_snapshot_from_another_candidate() {
    for field in [
        "candidateId",
        "workRunId",
        "baseCommitId",
        "candidateCommitId",
        "candidateTreeId",
        "diffSha256",
    ] {
        let mut json = serde_json::to_value(snapshot(1)).unwrap();
        json[field] = serde_json::Value::String(match field {
            "candidateId" => prefixed("cnd", 9),
            "workRunId" => prefixed("wrn", 9),
            "diffSha256" => digest(99).0,
            _ => "a".repeat(40),
        });
        let mut changed: Snapshot = serde_json::from_value(json).unwrap();
        changed.validation_seal = seal_snapshot(&changed);
        let error =
            SnapshotProductStaged::new(candidate(1), changed.try_into().unwrap(), binding())
                .expect_err("foreign code or product identity must fail staging");
        assert_eq!(error.kind(), StorageErrorKind::InvalidInput, "{field}");
    }
}

#[test]
fn staging_rejects_tampered_seal_and_legacy_snapshot_shape() {
    let mut changed = snapshot(1);
    changed.content_digest = digest(99);
    assert!(
        SnapshotProductStaged::new(candidate(1), changed.try_into().unwrap(), binding()).is_err()
    );
    let mut legacy = serde_json::to_value(snapshot(1)).unwrap();
    legacy.as_object_mut().unwrap().remove("schemaVersion");
    assert!(serde_json::from_value::<CanonicalSnapshot>(legacy).is_err());
}

#[test]
fn staging_rejects_legacy_or_foreign_candidate_refs() {
    for reference in [
        "git-candidate:sha256:".to_owned() + &"a".repeat(64),
        "refs/winwincode/candidates/".to_owned() + &"a".repeat(40),
        "refs/heads/main".to_owned(),
        String::new(),
    ] {
        let mut candidate = candidate(1);
        candidate.candidate_ref = reference.clone();
        assert!(
            SnapshotProductStaged::new(candidate, snapshot(1).try_into().unwrap(), binding(),)
                .is_err(),
            "{reference}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn verification_retry_reuses_immutable_product_and_changed_input_gets_new_identity() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-snapshot-product-{}-retry",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let mut storage = SqliteStorage::open(&root).unwrap();
    let first =
        SnapshotProductStaged::new(candidate(1), snapshot(1).try_into().unwrap(), binding())
            .unwrap();
    let (identity, digest) = request(3);
    commit_snapshot_product(&mut storage, identity, digest, &first).unwrap();
    let snapshot_stream = format!("snapshot-product:v1:{}", prefixed("snap", 1));
    let candidate_stream = format!("candidate-product:v1:{}", prefixed("cnd", 1));
    let original_snapshot = storage.load_state(&snapshot_stream).unwrap().unwrap();
    let original_candidate = storage.load_state(&candidate_stream).unwrap().unwrap();

    let mut retry_binding = binding();
    retry_binding.attempt = 2;
    retry_binding.execution_job_id = binding().execution_job_id;
    retry_binding.work_run_id = WorkRunId(prefixed("wrn", 3));
    retry_binding.session_binding_id = prefixed("sbn", 3);
    retry_binding.product_session_id = ProductSessionId(prefixed("psn", 3));
    retry_binding.worker_session_id = Some(WorkerSessionId(prefixed("wsn", 3)));
    retry_binding.codex_thread_id = Some(CodexThreadId(prefixed("cdx", 3)));
    retry_binding.verification_plan.work_run_id = retry_binding.work_run_id.clone();
    retry_binding.verification_session.work_run_id = retry_binding.work_run_id.clone();
    retry_binding.verification_session.attempt = retry_binding.attempt;
    retry_binding.verification_session.session_identity = winwincode_domain::SessionIdentity {
        work_run_id: Some(retry_binding.work_run_id.clone()),
        product_session_id: retry_binding.product_session_id.clone(),
        worker_session_id: retry_binding.worker_session_id.clone().unwrap(),
        codex_thread_id: retry_binding.codex_thread_id.clone().unwrap(),
    };
    let retry =
        SnapshotProductStaged::new(candidate(1), snapshot(1).try_into().unwrap(), retry_binding)
            .unwrap();
    let (identity, digest) = request(4);
    let committed =
        commit_snapshot_product(&mut storage, identity.clone(), digest.clone(), &retry).unwrap();
    drop(storage);
    let mut storage = SqliteStorage::open(&root).unwrap();
    let replay = commit_snapshot_product(&mut storage, identity, digest, &retry).unwrap();
    assert!(replay.receipt.idempotent_replay);
    assert_product_result_eq(&committed, &replay);
    assert_eq!(
        storage.load_state(&snapshot_stream).unwrap().unwrap(),
        original_snapshot
    );
    assert_eq!(
        storage.load_state(&candidate_stream).unwrap().unwrap(),
        original_candidate
    );
    validate_snapshot_binding(
        &storage,
        &SnapshotBindingCheck::new(
            SnapshotId(prefixed("snap", 1)),
            binding().execution_job_id,
            WorkRunId(prefixed("wrn", 3)),
            ProductSessionId(prefixed("psn", 3)),
            2,
        ),
    )
    .unwrap();

    let mut next_candidate = candidate(2);
    next_candidate.id = CandidateId(prefixed("cnd", 2));
    next_candidate.candidate_commit = "e".repeat(40);
    next_candidate.candidate_tree = "f".repeat(40);
    next_candidate.candidate_ref = format!(
        "refs/winwincode/candidates/{}",
        next_candidate.candidate_commit
    );
    let mut next_snapshot = snapshot(2);
    next_snapshot.snapshot_id = SnapshotId(prefixed("snap", 2));
    next_snapshot.candidate_id = next_candidate.id.clone();
    next_snapshot.candidate_commit_id = GitObjectId(next_candidate.candidate_commit.clone());
    next_snapshot.candidate_tree_id = GitObjectId(next_candidate.candidate_tree.clone());
    next_snapshot.validation_seal = seal_snapshot(&next_snapshot);
    let mut next_binding = binding();
    next_binding.execution_job_id = ExecutionJobId(prefixed("job", 4));
    next_binding.verification_plan.candidate_digest = next_candidate.candidate_digest.clone();
    next_binding.verification_session.snapshot_id = next_snapshot.snapshot_id.clone();
    next_binding.verification_session.candidate_id = next_candidate.id.clone();
    let next = SnapshotProductStaged::new(
        next_candidate,
        next_snapshot.try_into().unwrap(),
        next_binding,
    )
    .unwrap();
    let (identity, digest) = request(5);
    let changed = commit_snapshot_product(&mut storage, identity, digest, &next).unwrap();
    assert_ne!(
        changed.snapshot.snapshot_id(),
        committed.snapshot.snapshot_id()
    );
    assert_eq!(
        storage.load_state(&snapshot_stream).unwrap().unwrap(),
        original_snapshot
    );
    assert_eq!(
        storage.load_state(&candidate_stream).unwrap().unwrap(),
        original_candidate
    );
    drop(storage);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_delivery_guard_prevents_snapshot_binding_and_dispatch_commit() {
    let root = std::env::temp_dir().join(format!("snapshot-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut storage = SqliteStorage::open(&root).unwrap();
    let staged =
        SnapshotProductStaged::new(candidate(1), snapshot(1).try_into().unwrap(), binding())
            .unwrap()
            .with_state_guard(
                winwincode_storage::StateRevisionGuard::new("delivery-authority", 1).unwrap(),
            );
    let (identity, digest) = request(8);
    assert!(commit_snapshot_product(&mut storage, identity, digest, &staged).is_err());
    assert!(
        storage
            .load_state(&format!("snapshot-product:v1:{}", prefixed("snap", 1)))
            .unwrap()
            .is_none()
    );
    assert!(storage.pending_events().unwrap().is_empty());
    drop(storage);
    std::fs::remove_dir_all(root).unwrap();
}
