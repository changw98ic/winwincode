// SPDX-License-Identifier: Apache-2.0

use winwincode_delivery::domain::{
    AcceptanceCriterionId, CriterionResult, CriterionResultId, CriterionVerdict, DeliveryId,
    DeliverySpecId, DeliveryVerdict, DeliveryVerdictId, EvidenceId, EvidenceRef, EvidenceRefType,
    StageRunId,
};
use winwincode_test_assets::manifest::{
    TEST_ASSET_MANIFEST_SCHEMA_VERSION, TestAsset, TestAssetAuthority, TestAssetGate,
    TestAssetLifecycle, TestAssetManifest, TestAssetManifestActor, TestAssetManifestErrorCode,
    TestAssetMutability, TestAssetVerdictBinding, TestAssetVerdictInvalidationReason,
    detect_verdict_invalidation, validate_manifest_transition,
};

const CANDIDATE_REF: &str =
    "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn canonical_asset() -> TestAsset {
    TestAsset {
        id: "asset-api-contract".into(),
        owner: "quality-platform".into(),
        scope: vec!["packages/contracts".into()],
        purpose: "prove the public API contract".into(),
        authority: TestAssetAuthority::Canonical,
        mutability: TestAssetMutability::Protected,
        lifecycle: TestAssetLifecycle::Active,
        gate: TestAssetGate::RequirementBlocking,
        requirement_refs: vec!["REQ-API-001".into()],
        source_path: "tests/api-contract.test.ts".into(),
        content_sha256: "b".repeat(64),
    }
}

fn manifest() -> TestAssetManifest {
    TestAssetManifest {
        schema_version: TEST_ASSET_MANIFEST_SCHEMA_VERSION,
        id: "test-manifest-main".into(),
        revision: 1,
        candidate_ref: CANDIDATE_REF.into(),
        source_commit: "a".repeat(40),
        assets: vec![canonical_asset()],
    }
}

fn evidence(evidence_type: EvidenceRefType) -> EvidenceRef {
    EvidenceRef {
        schema_version: 3,
        id: EvidenceId("evidence-api-contract".into()),
        delivery_id: DeliveryId("dlv_01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
        delivery_spec_id: DeliverySpecId("spec-1".into()),
        delivery_spec_revision: 1,
        stage_run_id: StageRunId("stage-run-1".into()),
        session_binding_id: winwincode_delivery::domain::SessionBindingId("binding-1".into()),
        candidate_ref: CANDIDATE_REF.into(),
        evidence_type,
        source_ref: "runtime-event-42".into(),
        created_at_millis: 10,
    }
}

fn verdict() -> DeliveryVerdict {
    DeliveryVerdict {
        schema_version: 3,
        id: DeliveryVerdictId("verdict-1".into()),
        delivery_id: DeliveryId("dlv_01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
        delivery_spec_id: DeliverySpecId("spec-1".into()),
        candidate_ref: CANDIDATE_REF.into(),
        status: CriterionVerdict::Pass,
        criteria: vec![CriterionResult {
            schema_version: 3,
            id: CriterionResultId("criterion-result-1".into()),
            delivery_id: DeliveryId("dlv_01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
            delivery_spec_id: DeliverySpecId("spec-1".into()),
            criterion_id: AcceptanceCriterionId("criterion-1".into()),
            candidate_ref: CANDIDATE_REF.into(),
            verdict: CriterionVerdict::Pass,
            evidence_refs: vec![EvidenceId("evidence-api-contract".into())],
            explanation: "the API contract passed".into(),
            evaluated_at_millis: 9,
        }],
        unresolved_findings: Vec::new(),
        produced_at_millis: 10,
    }
}

#[test]
fn candidate_constructor_is_advisory_by_default() {
    let asset = TestAsset::candidate(
        "asset-candidate",
        "executor",
        vec!["crates/example".into()],
        "exercise a proposed edge case",
        "crates/example/tests/candidate.rs",
        "c".repeat(64),
    );

    assert_eq!(asset.authority, TestAssetAuthority::Candidate);
    assert_eq!(asset.gate, TestAssetGate::Advisory);
    assert!(!asset.blocks_delivery());

    let mut invalid = asset;
    invalid.gate = TestAssetGate::RequirementBlocking;
    invalid.requirement_refs = vec!["REQ-NEW-001".into()];
    let manifest = TestAssetManifest {
        assets: vec![invalid],
        ..manifest()
    };
    assert_eq!(
        manifest
            .validate()
            .expect_err("candidate cannot self-promote")
            .code(),
        TestAssetManifestErrorCode::InvalidValue
    );
}

#[test]
fn executor_cannot_modify_delete_or_create_protected_assets() {
    let previous = manifest();
    let mut modified = previous.clone();
    modified.revision = 2;
    modified.assets[0].content_sha256 = "c".repeat(64);
    assert_eq!(
        validate_manifest_transition(&previous, &modified, TestAssetManifestActor::Executor)
            .expect_err("Executor cannot rewrite canonical content")
            .code(),
        TestAssetManifestErrorCode::ExecutorModificationDenied
    );

    let mut deleted = previous.clone();
    deleted.revision = 2;
    deleted.assets.clear();
    assert_eq!(
        validate_manifest_transition(&previous, &deleted, TestAssetManifestActor::Executor)
            .expect_err("Executor cannot delete a canonical asset")
            .code(),
        TestAssetManifestErrorCode::ExecutorModificationDenied
    );

    let mut created = previous.clone();
    created.revision = 2;
    let mut protected_candidate = TestAsset::candidate(
        "asset-protected-candidate",
        "quality-platform",
        vec!["tests".into()],
        "protected proposed test",
        "tests/protected.test.ts",
        "d".repeat(64),
    );
    protected_candidate.mutability = TestAssetMutability::Protected;
    created.assets.push(protected_candidate);
    assert_eq!(
        validate_manifest_transition(&previous, &created, TestAssetManifestActor::Executor)
            .expect_err("Executor cannot create a protected asset")
            .code(),
        TestAssetManifestErrorCode::ExecutorModificationDenied
    );

    assert!(
        validate_manifest_transition(&previous, &modified, TestAssetManifestActor::ControlPlane)
            .is_ok()
    );
}

#[test]
fn evidence_binding_uses_existing_test_evidence_and_exact_artifact() {
    let manifest = manifest();
    let binding = manifest
        .bind_evidence(&evidence(EvidenceRefType::Test), "asset-api-contract")
        .expect("test Evidence should bind");

    assert_eq!(binding.evidence_ref_id().0, "evidence-api-contract");
    assert_eq!(binding.asset_content_sha256(), "b".repeat(64));
    assert_eq!(binding.manifest_ref().revision, 1);
    assert_eq!(binding.manifest_ref().digest_sha256.len(), 64);
    assert!(binding.blocks_delivery());

    let error = manifest
        .bind_evidence(&evidence(EvidenceRefType::Command), "asset-api-contract")
        .expect_err("command Evidence must not bind as test Evidence");
    assert_eq!(error.code(), TestAssetManifestErrorCode::EvidenceMismatch);
}

#[test]
fn candidate_or_manifest_change_invalidates_related_verdict() {
    let manifest = manifest();
    let evidence_binding = manifest
        .bind_evidence(&evidence(EvidenceRefType::Test), "asset-api-contract")
        .expect("Evidence should bind");
    let verdict_binding = TestAssetVerdictBinding::new(
        &verdict(),
        &manifest,
        std::slice::from_ref(&evidence_binding),
    )
    .expect("verdict should bind cited Evidence");

    assert_eq!(
        detect_verdict_invalidation(&verdict_binding, CANDIDATE_REF, &manifest, 11)
            .expect("freshness check should succeed"),
        None
    );

    let candidate_change = detect_verdict_invalidation(
        &verdict_binding,
        "git-candidate:sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        &manifest,
        12,
    )
    .expect("freshness check should succeed")
    .expect("candidate change must invalidate");
    assert_eq!(
        candidate_change.reason(),
        TestAssetVerdictInvalidationReason::CandidateChanged
    );

    let mut changed_manifest = manifest.clone();
    changed_manifest.revision = 2;
    changed_manifest.source_commit = "e".repeat(40);
    let manifest_change =
        detect_verdict_invalidation(&verdict_binding, CANDIDATE_REF, &changed_manifest, 13)
            .expect("freshness check should succeed")
            .expect("manifest change must invalidate");
    assert_eq!(
        manifest_change.reason(),
        TestAssetVerdictInvalidationReason::ManifestChanged
    );
}

#[test]
fn verdict_binding_rejects_uncited_or_stale_test_evidence() {
    let manifest = manifest();
    assert_eq!(
        TestAssetVerdictBinding::new(&verdict(), &manifest, &[])
            .expect_err("a verdict without test Evidence is unrelated")
            .code(),
        TestAssetManifestErrorCode::VerdictMismatch
    );

    let mut evidence = evidence(EvidenceRefType::Test);
    evidence.id = EvidenceId("evidence-not-cited".into());
    let binding = manifest
        .bind_evidence(&evidence, "asset-api-contract")
        .expect("Evidence should bind");
    assert_eq!(
        TestAssetVerdictBinding::new(&verdict(), &manifest, &[binding])
            .expect_err("uncited Evidence must be rejected")
            .code(),
        TestAssetManifestErrorCode::VerdictMismatch
    );
}
