// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_audit::{
    AuditActor, AuditOrigin, AuditScope, AuditStore, ClassificationRule, DataClassification,
    DataGovernanceAuthority, DeletionPermit, DeletionPortOutcome, GovernanceAuditContext,
    GovernedDataFact, LegalHold, LegalHoldId, RedactionStrategy, ResidencyRegion,
    RetentionRequirement,
};
use winwincode_backup::{
    BackupCaptureCoordinator, BackupComponentKind, BackupComponentSnapshot, BackupDeletionReceipt,
    BackupDeletionResult, BackupDeletionStore, BackupDeletionStoreError, BackupDependency,
    BackupErrorKind, BackupId, BackupManifest, BackupRetentionCoordinator, BackupSnapshotRequest,
    BackupSnapshotSource, BackupSnapshotSourceError, RestoreActivation, RestoreCoordinator,
    RestoreEvidence, RestoreId, RestorePreparation, RestoreTarget, RestoreTargetError,
    VerifiedRestore,
};
use winwincode_domain::{
    OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, SystemActorId, WorkspaceId,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-backup-{name}-{}-{serial}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("create backup fixture directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn digest(tail: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", tail.to_string().repeat(64)))
}

fn scope(organization_tail: char, repository_tail: char) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", organization_tail)),
        WorkspaceId(id("wsp", '2')),
        ProjectId(id("prj", '3')),
        RepositoryId(id("rep", repository_tail)),
    )
    .expect("canonical backup scope")
}

fn components(scope: &AuditScope, cut: char) -> Vec<BackupComponentSnapshot> {
    BackupComponentKind::REQUIRED
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            let checkpoint = char::from(b'1' + u8::try_from(index).expect("bounded index"));
            let content = ['a', 'b', 'c', 'd', 'e', 'f', '0'][index];
            BackupComponentSnapshot::try_new(
                *kind,
                scope.clone(),
                digest(cut),
                digest(checkpoint),
                digest(content),
                u64::try_from(index).expect("bounded count") + 1,
                u64::try_from(index).expect("bounded bytes") + 10,
            )
            .expect("valid component snapshot")
        })
        .collect()
}

fn dependencies(components: &[BackupComponentSnapshot]) -> Vec<BackupDependency> {
    let indexed = components
        .iter()
        .map(|component| (component.kind(), component.content_digest().clone()))
        .collect::<BTreeMap<_, _>>();
    [
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::AuditLedger,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::LeaseRegistry,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::UsageLedger,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::ArtifactObjects,
        ),
        (
            BackupComponentKind::ReferenceCatalog,
            BackupComponentKind::SecretReferences,
        ),
    ]
    .into_iter()
    .map(|(source, target)| {
        BackupDependency::try_new(source, target, indexed[&target].clone())
            .expect("valid backup dependency")
    })
    .collect()
}

fn manifest(scope: &AuditScope) -> BackupManifest {
    let components = components(scope, 'f');
    let dependencies = dependencies(&components);
    BackupManifest::try_new(
        BackupId::try_new(id("bkp", 'A')).expect("canonical backup id"),
        scope.clone(),
        "cn-north-1",
        1_000,
        components,
        dependencies,
    )
    .expect("complete backup manifest")
}

struct SnapshotSource {
    snapshot: BackupComponentSnapshot,
    calls: u64,
}

impl BackupSnapshotSource for SnapshotSource {
    fn kind(&self) -> BackupComponentKind {
        self.snapshot.kind()
    }

    fn snapshot(
        &mut self,
        request: &BackupSnapshotRequest,
    ) -> Result<BackupComponentSnapshot, BackupSnapshotSourceError> {
        assert_eq!(request.scope(), self.snapshot.scope());
        assert_eq!(
            request.consistency_cut_digest(),
            self.snapshot.consistency_cut_digest()
        );
        assert_eq!(request.captured_at_millis(), 1_000);
        self.calls += 1;
        Ok(self.snapshot.clone())
    }
}

#[test]
fn capture_calls_each_authoritative_backend_once_and_builds_dependencies() {
    let tenant = scope('1', '4');
    let mut sources = components(&tenant, 'f')
        .into_iter()
        .rev()
        .map(|snapshot| SnapshotSource { snapshot, calls: 0 })
        .collect::<Vec<_>>();
    let mut ports = sources
        .iter_mut()
        .map(|source| source as &mut dyn BackupSnapshotSource)
        .collect::<Vec<_>>();
    let manifest = BackupCaptureCoordinator::capture(
        BackupId::try_new(id("bkp", 'Z')).expect("backup id"),
        tenant,
        "cn-north-1",
        1_000,
        digest('f'),
        &mut ports,
    )
    .expect("capture complete backend cut");
    drop(ports);
    assert!(sources.iter().all(|source| source.calls == 1));
    assert_eq!(
        manifest.components().len(),
        BackupComponentKind::REQUIRED.len()
    );
    assert_eq!(manifest.dependencies().len(), 5);
}

#[test]
fn manifest_is_canonical_complete_and_tamper_evident() {
    let manifest = manifest(&scope('1', '4'));
    let bytes = manifest.encode_canonical().expect("encode manifest");
    let decoded = BackupManifest::decode_canonical(&bytes).expect("decode canonical manifest");
    assert_eq!(decoded, manifest);

    let mut tampered = bytes.clone();
    let position = String::from_utf8_lossy(&tampered)
        .find("cn-north-1")
        .expect("region in manifest");
    tampered[position + 9] = b'2';
    assert_eq!(
        BackupManifest::decode_canonical(&tampered)
            .expect_err("changed manifest is rejected")
            .kind(),
        BackupErrorKind::Integrity
    );

    let noncanonical = [b" ".as_slice(), bytes.as_slice()].concat();
    assert_eq!(
        BackupManifest::decode_canonical(&noncanonical)
            .expect_err("whitespace representation is not canonical")
            .kind(),
        BackupErrorKind::Integrity
    );
}

#[test]
fn manifest_rejects_missing_mixed_cut_and_cross_tenant_components() {
    let tenant = scope('1', '4');
    let mut missing = components(&tenant, 'f');
    missing.pop();
    assert_eq!(
        BackupManifest::try_new(
            BackupId::try_new(id("bkp", 'B')).expect("backup id"),
            tenant.clone(),
            "cn-north-1",
            1_000,
            missing.clone(),
            dependencies(&components(&tenant, 'f')),
        )
        .expect_err("missing component is rejected")
        .kind(),
        BackupErrorKind::Incomplete
    );

    let mut mixed = components(&tenant, 'f');
    mixed[0] = components(&tenant, 'e')[0].clone();
    assert_eq!(
        BackupManifest::try_new(
            BackupId::try_new(id("bkp", 'C')).expect("backup id"),
            tenant.clone(),
            "cn-north-1",
            1_000,
            mixed.clone(),
            dependencies(&mixed),
        )
        .expect_err("mixed consistency cut is rejected")
        .kind(),
        BackupErrorKind::Integrity
    );

    let mut cross_tenant = components(&tenant, 'f');
    cross_tenant[2] = components(&scope('9', '8'), 'f')[2].clone();
    assert_eq!(
        BackupManifest::try_new(
            BackupId::try_new(id("bkp", 'D')).expect("backup id"),
            tenant,
            "cn-north-1",
            1_000,
            cross_tenant.clone(),
            dependencies(&cross_tenant),
        )
        .expect_err("cross-tenant component is rejected")
        .kind(),
        BackupErrorKind::TenantMismatch
    );

    let complete = components(&scope('1', '4'), 'f');
    let mut broken_dependencies = dependencies(&complete);
    broken_dependencies[0] = BackupDependency::try_new(
        broken_dependencies[0].source(),
        broken_dependencies[0].target(),
        digest('9'),
    )
    .expect("well-formed changed dependency");
    assert_eq!(
        BackupManifest::try_new(
            BackupId::try_new(id("bkp", 'E')).expect("backup id"),
            scope('1', '4'),
            "cn-north-1",
            1_000,
            complete,
            broken_dependencies,
        )
        .expect_err("dependency on another restored content digest is rejected")
        .kind(),
        BackupErrorKind::Integrity
    );
}

#[derive(Default)]
struct CrashSafeTarget {
    prepared: Option<(String, Sha256Digest)>,
    active: Option<Sha256Digest>,
    fail_next_activation: bool,
    prepare_calls: u64,
    activation_calls: u64,
}

impl RestoreTarget for CrashSafeTarget {
    fn prepare(
        &mut self,
        restore: &VerifiedRestore,
    ) -> Result<RestorePreparation, RestoreTargetError> {
        self.prepare_calls += 1;
        let key = (
            restore.restore_id().as_str().to_owned(),
            restore.manifest_digest().clone(),
        );
        match &self.prepared {
            Some(existing) if existing == &key => Ok(RestorePreparation::AlreadyPrepared),
            Some(_) => Err(RestoreTargetError::new()),
            None => {
                self.prepared = Some(key);
                Ok(RestorePreparation::Prepared)
            }
        }
    }

    fn activate(
        &mut self,
        restore: &VerifiedRestore,
    ) -> Result<RestoreActivation, RestoreTargetError> {
        self.activation_calls += 1;
        if self.fail_next_activation {
            self.fail_next_activation = false;
            return Err(RestoreTargetError::new());
        }
        if self.active.as_ref() == Some(restore.manifest_digest()) {
            return Ok(RestoreActivation::AlreadyActivated);
        }
        self.active = Some(restore.manifest_digest().clone());
        Ok(RestoreActivation::Activated)
    }
}

fn evidence(manifest: &BackupManifest) -> Vec<RestoreEvidence> {
    manifest
        .components()
        .iter()
        .cloned()
        .map(RestoreEvidence::new)
        .collect()
}

#[test]
fn restore_survives_activation_crash_and_exact_restart_replay() {
    let manifest = manifest(&scope('1', '4'));
    let restore_id = RestoreId::try_new(id("rst", 'A')).expect("restore id");
    let previous = digest('0');
    let mut target = CrashSafeTarget {
        active: Some(previous.clone()),
        fail_next_activation: true,
        ..CrashSafeTarget::default()
    };

    let error = RestoreCoordinator::restore(
        restore_id.clone(),
        &manifest,
        evidence(&manifest),
        &mut target,
    )
    .expect_err("simulated activation crash");
    assert_eq!(error.kind(), BackupErrorKind::Unavailable);
    assert_eq!(target.active, Some(previous));
    assert!(target.prepared.is_some());

    assert_eq!(
        RestoreCoordinator::restore(
            restore_id.clone(),
            &manifest,
            evidence(&manifest),
            &mut target,
        )
        .expect("restart activates staged generation"),
        RestoreActivation::Activated
    );
    assert_eq!(target.active.as_ref(), Some(manifest.manifest_digest()));
    assert_eq!(
        RestoreCoordinator::restore(restore_id, &manifest, evidence(&manifest), &mut target)
            .expect("exact restore replay"),
        RestoreActivation::AlreadyActivated
    );
    assert_eq!(target.prepare_calls, 3);
    assert_eq!(target.activation_calls, 3);
}

#[test]
fn restore_rejects_tamper_and_cross_tenant_evidence_before_target() {
    let manifest = manifest(&scope('1', '4'));
    let mut tampered = evidence(&manifest);
    let component = tampered[0].snapshot();
    tampered[0] = RestoreEvidence::new(
        BackupComponentSnapshot::try_new(
            component.kind(),
            component.scope().clone(),
            component.consistency_cut_digest().clone(),
            component.checkpoint_digest().clone(),
            digest('0'),
            component.record_count(),
            component.byte_count(),
        )
        .expect("well-formed altered evidence"),
    );
    let mut target = CrashSafeTarget::default();
    assert_eq!(
        RestoreCoordinator::restore(
            RestoreId::try_new(id("rst", 'B')).expect("restore id"),
            &manifest,
            tampered,
            &mut target,
        )
        .expect_err("tampered evidence is rejected")
        .kind(),
        BackupErrorKind::Integrity
    );
    assert_eq!(target.prepare_calls, 0);

    let mut foreign = evidence(&manifest);
    let component = foreign[0].snapshot();
    foreign[0] = RestoreEvidence::new(
        BackupComponentSnapshot::try_new(
            component.kind(),
            scope('9', '8'),
            component.consistency_cut_digest().clone(),
            component.checkpoint_digest().clone(),
            component.content_digest().clone(),
            component.record_count(),
            component.byte_count(),
        )
        .expect("foreign evidence"),
    );
    assert_eq!(
        RestoreCoordinator::restore(
            RestoreId::try_new(id("rst", 'C')).expect("restore id"),
            &manifest,
            foreign,
            &mut target,
        )
        .expect_err("cross-tenant evidence is rejected")
        .kind(),
        BackupErrorKind::TenantMismatch
    );
    assert_eq!(target.prepare_calls, 0);
}

fn region() -> ResidencyRegion {
    ResidencyRegion::try_new("cn-north-1").expect("canonical region")
}

fn rules(restricted_retention_millis: u64) -> Vec<ClassificationRule> {
    [
        (DataClassification::Public, RedactionStrategy::Reveal),
        (DataClassification::Internal, RedactionStrategy::Mask),
        (DataClassification::Confidential, RedactionStrategy::Hash),
        (DataClassification::Restricted, RedactionStrategy::Mask),
        (DataClassification::Secret, RedactionStrategy::Remove),
    ]
    .into_iter()
    .map(|(classification, redaction)| {
        let retention = if classification == DataClassification::Restricted {
            restricted_retention_millis
        } else {
            0
        };
        ClassificationRule::try_new(
            classification,
            [region()],
            RetentionRequirement::MinimumDuration(retention),
            redaction,
        )
        .expect("complete backup governance rule")
    })
    .collect()
}

fn governance(restricted_retention_millis: u64, holds: Vec<LegalHold>) -> DataGovernanceAuthority {
    DataGovernanceAuthority::try_new(
        "enterprise.backup-retention",
        1,
        rules(restricted_retention_millis),
        holds,
    )
    .expect("complete backup governance authority")
}

fn governed_manifest(manifest: &BackupManifest) -> GovernedDataFact {
    GovernedDataFact::try_new(
        manifest.scope().clone(),
        manifest.manifest_digest().clone(),
        DataClassification::Restricted,
        region(),
        manifest.captured_at_millis(),
    )
    .expect("governed backup fact")
}

fn audit_context() -> GovernanceAuditContext {
    GovernanceAuditContext::new(
        AuditActor::System(SystemActorId(id("sys", '4'))),
        RequestId(id("req", '5')),
        AuditOrigin::local("backup-retention").expect("canonical audit origin"),
    )
}

#[derive(Default)]
struct DeletionStore {
    calls: u64,
    fail_next: bool,
    receipt: Option<BackupDeletionReceipt>,
}

impl BackupDeletionStore for DeletionStore {
    fn delete_generation(
        &mut self,
        manifest: &BackupManifest,
        permit: &DeletionPermit,
    ) -> Result<BackupDeletionReceipt, BackupDeletionStoreError> {
        self.calls += 1;
        if self.fail_next {
            self.fail_next = false;
            return Err(BackupDeletionStoreError::new());
        }
        if let Some(receipt) = &self.receipt {
            return BackupDeletionReceipt::try_new(
                receipt.manifest_digest().clone(),
                receipt.decision_digest().clone(),
                receipt.deleted_at_millis(),
                receipt.backend_receipt_digest().clone(),
                receipt.deleted_components().to_vec(),
                DeletionPortOutcome::AlreadyDeleted,
            )
            .map_err(|_| BackupDeletionStoreError::new());
        }
        let receipt = BackupDeletionReceipt::try_new(
            manifest.manifest_digest().clone(),
            permit.decision_digest().clone(),
            permit.requested_at_millis(),
            digest('d'),
            BackupComponentKind::REQUIRED.to_vec(),
            DeletionPortOutcome::Deleted,
        )
        .map_err(|_| BackupDeletionStoreError::new())?;
        self.receipt = Some(receipt.clone());
        Ok(receipt)
    }
}

#[test]
fn legal_hold_precedes_retention_and_never_reaches_deletion_store() {
    let directory = TestDirectory::new("legal-hold");
    let mut audit = AuditStore::open(directory.path()).expect("open audit store");
    let manifest = manifest(&scope('1', '4'));
    let hold = LegalHold::try_new(
        LegalHoldId::try_new(id("lgh", '6')).expect("legal hold id"),
        manifest.scope().clone(),
        Some(manifest.manifest_digest().clone()),
        1_001,
        None,
    )
    .expect("active legal hold");
    let authority = governance(0, vec![hold]);
    let data = governed_manifest(&manifest);
    let mut storage = DeletionStore::default();
    let result = BackupRetentionCoordinator::delete(
        &authority,
        &manifest,
        &data,
        2_000,
        &audit_context(),
        &mut audit,
        &mut storage,
    )
    .expect("legal hold is a policy result");
    assert!(matches!(result, BackupDeletionResult::Denied(_)));
    assert_eq!(storage.calls, 0);
    let checkpoint = audit
        .verify_organization(manifest.scope().organization_id())
        .expect("verify legal-hold audit chain");
    assert_eq!(checkpoint.last_sequence(), 1);
}

#[test]
fn minimum_retention_denies_early_deletion_without_storage_side_effects() {
    let directory = TestDirectory::new("minimum-retention");
    let mut audit = AuditStore::open(directory.path()).expect("open audit store");
    let manifest = manifest(&scope('1', '4'));
    let mut storage = DeletionStore::default();
    let result = BackupRetentionCoordinator::delete(
        &governance(1_500, Vec::new()),
        &manifest,
        &governed_manifest(&manifest),
        2_000,
        &audit_context(),
        &mut audit,
        &mut storage,
    )
    .expect("active retention is a policy result");
    assert!(matches!(result, BackupDeletionResult::Denied(_)));
    assert_eq!(storage.calls, 0);
}

#[test]
fn deletion_failure_restarts_from_audit_and_returns_stable_verifiable_proof() {
    let directory = TestDirectory::new("deletion-restart");
    let mut audit = AuditStore::open(directory.path()).expect("open audit store");
    let manifest = manifest(&scope('1', '4'));
    let authority = governance(0, Vec::new());
    let data = governed_manifest(&manifest);
    let mut storage = DeletionStore {
        fail_next: true,
        ..DeletionStore::default()
    };
    let first = BackupRetentionCoordinator::delete(
        &authority,
        &manifest,
        &data,
        2_000,
        &audit_context(),
        &mut audit,
        &mut storage,
    )
    .expect_err("backend fails after durable policy audit");
    assert_eq!(first.kind(), BackupErrorKind::Governance);
    assert_eq!(storage.calls, 1);
    let checkpoint = audit
        .verify_organization(manifest.scope().organization_id())
        .expect("audit remains valid after backend failure");
    assert_eq!(checkpoint.last_sequence(), 1);

    let proof = match BackupRetentionCoordinator::delete(
        &authority,
        &manifest,
        &data,
        2_000,
        &audit_context(),
        &mut audit,
        &mut storage,
    )
    .expect("restart completes deletion")
    {
        BackupDeletionResult::Applied(proof) => proof,
        BackupDeletionResult::Denied(_) => panic!("deletion must be allowed"),
    };
    proof.verify().expect("verify sealed deletion proof");
    let proof_bytes = proof.encode_canonical().expect("encode deletion proof");
    assert_eq!(
        winwincode_backup::BackupDeletionProof::decode_canonical(&proof_bytes)
            .expect("decode deletion proof"),
        proof
    );
    assert_eq!(proof.manifest_digest(), manifest.manifest_digest());
    assert_eq!(storage.calls, 2);
    assert_eq!(
        audit
            .verify_organization(manifest.scope().organization_id())
            .expect("verify exact audit replay")
            .last_sequence(),
        1
    );

    let replay = match BackupRetentionCoordinator::delete(
        &authority,
        &manifest,
        &data,
        2_000,
        &audit_context(),
        &mut audit,
        &mut storage,
    )
    .expect("exact deletion replay")
    {
        BackupDeletionResult::Applied(proof) => proof,
        BackupDeletionResult::Denied(_) => panic!("deletion replay must stay allowed"),
    };
    assert_eq!(replay, proof);
    assert_eq!(storage.calls, 3);
}

#[test]
fn deletion_rejects_cross_tenant_fact_before_audit_or_storage() {
    let directory = TestDirectory::new("cross-tenant-delete");
    let mut audit = AuditStore::open(directory.path()).expect("open audit store");
    let manifest = manifest(&scope('1', '4'));
    let foreign = GovernedDataFact::try_new(
        scope('9', '8'),
        manifest.manifest_digest().clone(),
        DataClassification::Restricted,
        region(),
        1_000,
    )
    .expect("foreign governed fact");
    let mut storage = DeletionStore::default();
    let error = BackupRetentionCoordinator::delete(
        &governance(0, Vec::new()),
        &manifest,
        &foreign,
        2_000,
        &audit_context(),
        &mut audit,
        &mut storage,
    )
    .expect_err("cross-tenant fact is rejected");
    assert_eq!(error.kind(), BackupErrorKind::TenantMismatch);
    assert_eq!(storage.calls, 0);
    assert_eq!(
        audit
            .verify_organization(manifest.scope().organization_id())
            .expect("empty organization chain")
            .last_sequence(),
        0
    );
}
