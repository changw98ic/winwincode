// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_audit::AuditScope;
use winwincode_backup::{
    BackupComponentKind, BackupComponentSnapshot, BackupDependency, BackupId, BackupManifest,
    RestoreActivation, RestoreEvidence, RestoreId, RestorePreparation, RestoreTarget,
    RestoreTargetError, VerifiedRestore,
};
use winwincode_domain::{OrganizationId, ProjectId, RepositoryId, Sha256Digest, WorkspaceId};
use winwincode_drill::{
    ControlPlaneLifecycleSnapshot, DeploymentDrainAuthority, DeploymentRelease, DisasterFaultPort,
    DisasterRecoveryEvidence, DisasterRecoveryPlan, DisasterRecoveryRunner, DisasterScenario,
    DrainAuthorityError, DrillErrorKind, DrillId, FailureObservation, FaultPortError,
    LifecycleState, RecoveryObservation, RecoveryObservationPort, RecoveryObservationPortError,
    RollingUpgradeBackup, RollingUpgradeBackupPort, RollingUpgradeBackupPortError,
    RollingUpgradeEvidence, RollingUpgradePlan, RollingUpgradePort, RollingUpgradePortError,
    RollingUpgradeResult, RollingUpgradeRunner,
};
use winwincode_storage::{ControlPlaneInstanceIdentity, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-drill-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn digest(tail: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", tail.to_string().repeat(64)))
}

#[test]
fn canonical_instance_health_maps_without_a_second_lifecycle_authority() {
    let root = temporary_directory("instance-health");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let identity = ControlPlaneInstanceIdentity::try_new(
        "cpi_00000000000000000000000000000001",
        "cpb_00000000000000000000000000000001",
    )
    .expect("instance identity");
    let authority = storage
        .control_plane_instance_ledger()
        .expect("instance ledger")
        .register(&identity, 10, 100)
        .expect("register instance");
    let active = storage
        .control_plane_instance_ledger()
        .expect("instance ledger")
        .preflight(&authority, 20)
        .expect("active health");
    let mapped = ControlPlaneLifecycleSnapshot::from_instance_health(
        scope('1', '4'),
        digest('a'),
        20,
        &active,
    )
    .expect("mapped health");
    assert_eq!(mapped.state(), LifecycleState::Healthy);
    assert!(mapped.accepting_new_work());
    assert_eq!(mapped.confirmed_sequence(), 0);

    let drained = storage
        .control_plane_instance_ledger()
        .expect("instance ledger")
        .request_drain(&authority, 30, 90)
        .expect("drain instance");
    let mapped = ControlPlaneLifecycleSnapshot::from_instance_health(
        scope('1', '4'),
        digest('a'),
        30,
        &drained,
    )
    .expect("mapped drain");
    assert_eq!(mapped.state(), LifecycleState::Drained);
    assert!(!mapped.accepting_new_work());

    let expired = storage
        .control_plane_instance_ledger()
        .expect("instance ledger")
        .preflight(&authority, 101)
        .expect("expired health");
    let mapped = ControlPlaneLifecycleSnapshot::from_instance_health(
        scope('1', '4'),
        digest('a'),
        101,
        &expired,
    )
    .expect("mapped expired health");
    assert_eq!(mapped.state(), LifecycleState::Unhealthy);

    let mut inconsistent = active;
    inconsistent.accepting_new_work = false;
    assert_eq!(
        ControlPlaneLifecycleSnapshot::from_instance_health(
            scope('1', '4'),
            digest('a'),
            20,
            &inconsistent,
        )
        .expect_err("changed admission projection must fail")
        .kind(),
        DrillErrorKind::Invalid
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

fn scope(organization_tail: char, repository_tail: char) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", organization_tail)),
        WorkspaceId(id("wsp", '2')),
        ProjectId(id("prj", '3')),
        RepositoryId(id("rep", repository_tail)),
    )
    .expect("canonical drill scope")
}

fn manifest(
    scope: &AuditScope,
    captured_at_millis: u64,
    delivery_state_digest: Sha256Digest,
) -> BackupManifest {
    let cut = digest('f');
    let contents = [
        delivery_state_digest,
        digest('b'),
        digest('c'),
        digest('d'),
        digest('e'),
        digest('1'),
        digest('2'),
    ];
    let components = BackupComponentKind::REQUIRED
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            BackupComponentSnapshot::try_new(
                *kind,
                scope.clone(),
                cut.clone(),
                digest(char::from(
                    b'3' + u8::try_from(index).expect("bounded index"),
                )),
                contents[index].clone(),
                u64::try_from(index).expect("bounded count") + 1,
                u64::try_from(index).expect("bounded bytes") + 10,
            )
            .expect("component snapshot")
        })
        .collect::<Vec<_>>();
    let indexed = components
        .iter()
        .map(|component| (component.kind(), component.content_digest().clone()))
        .collect::<BTreeMap<_, _>>();
    let dependencies = [
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
            .expect("component dependency")
    })
    .collect::<Vec<_>>();
    BackupManifest::try_new(
        BackupId::try_new(id("bkp", 'A')).expect("backup id"),
        scope.clone(),
        "cn-north-1",
        captured_at_millis,
        components,
        dependencies,
    )
    .expect("complete drill manifest")
}

fn evidence(manifest: &BackupManifest) -> Vec<RestoreEvidence> {
    manifest
        .components()
        .iter()
        .cloned()
        .map(RestoreEvidence::new)
        .collect()
}

#[derive(Default)]
struct RestoreFixture {
    prepared: BTreeMap<String, Sha256Digest>,
    active: Option<Sha256Digest>,
}

impl RestoreTarget for RestoreFixture {
    fn prepare(
        &mut self,
        restore: &VerifiedRestore,
    ) -> Result<RestorePreparation, RestoreTargetError> {
        match self.prepared.get(restore.restore_id().as_str()) {
            Some(digest) if digest == restore.manifest_digest() => {
                Ok(RestorePreparation::AlreadyPrepared)
            }
            Some(_) => Err(RestoreTargetError::new()),
            None => {
                self.prepared.insert(
                    restore.restore_id().as_str().to_owned(),
                    restore.manifest_digest().clone(),
                );
                Ok(RestorePreparation::Prepared)
            }
        }
    }

    fn activate(
        &mut self,
        restore: &VerifiedRestore,
    ) -> Result<RestoreActivation, RestoreTargetError> {
        if self.active.as_ref() == Some(restore.manifest_digest()) {
            Ok(RestoreActivation::AlreadyActivated)
        } else {
            self.active = Some(restore.manifest_digest().clone());
            Ok(RestoreActivation::Activated)
        }
    }
}

struct FaultFixture {
    failed_at_millis: u64,
    confirmed_state_digest: Sha256Digest,
    injected: Vec<DisasterScenario>,
    cleared: Vec<DisasterScenario>,
}

impl DisasterFaultPort for FaultFixture {
    fn inject(
        &mut self,
        scenario: DisasterScenario,
        scope: &AuditScope,
    ) -> Result<FailureObservation, FaultPortError> {
        self.injected.push(scenario);
        FailureObservation::try_new(
            scope.clone(),
            self.failed_at_millis,
            10,
            self.confirmed_state_digest.clone(),
        )
        .map_err(|_| FaultPortError::new())
    }

    fn clear(
        &mut self,
        scenario: DisasterScenario,
        _scope: &AuditScope,
    ) -> Result<(), FaultPortError> {
        self.cleared.push(scenario);
        Ok(())
    }
}

struct RecoveryFixture {
    recovered_at_millis: u64,
    confirmed_state_digest: Sha256Digest,
}

impl RecoveryObservationPort for RecoveryFixture {
    fn observe(
        &mut self,
        _scenario: DisasterScenario,
        scope: &AuditScope,
        manifest_digest: &Sha256Digest,
    ) -> Result<RecoveryObservation, RecoveryObservationPortError> {
        RecoveryObservation::try_new(
            scope.clone(),
            self.recovered_at_millis,
            10,
            self.confirmed_state_digest.clone(),
            manifest_digest.clone(),
            true,
        )
        .map_err(|_| RecoveryObservationPortError::new())
    }
}

#[test]
fn four_disaster_scenarios_restore_confirmed_state_with_reproducible_rpo_rto() {
    let tenant = scope('1', '4');
    let confirmed = digest('a');
    let manifest = manifest(&tenant, 1_000, confirmed.clone());
    let scenarios = [
        DisasterScenario::ControlPlaneInstanceLoss,
        DisasterScenario::DatabaseUnavailable,
        DisasterScenario::ObjectStoreCorruption,
        DisasterScenario::SecretStoreUnavailable,
    ];
    for (index, scenario) in scenarios.into_iter().enumerate() {
        let tail = char::from(b'A' + u8::try_from(index).expect("bounded index"));
        let plan = DisasterRecoveryPlan::try_new(
            DrillId::try_new(id("drl", tail)).expect("drill id"),
            tenant.clone(),
            250,
            150,
        )
        .expect("recovery plan");
        let mut fault = FaultFixture {
            failed_at_millis: 1_200,
            confirmed_state_digest: confirmed.clone(),
            injected: Vec::new(),
            cleared: Vec::new(),
        };
        let mut observer = RecoveryFixture {
            recovered_at_millis: 1_300,
            confirmed_state_digest: confirmed.clone(),
        };
        let mut target = RestoreFixture::default();
        let evidence = DisasterRecoveryRunner::run(
            &plan,
            scenario,
            &manifest,
            RestoreId::try_new(id("rst", tail)).expect("restore id"),
            evidence(&manifest),
            &mut target,
            &mut fault,
            &mut observer,
        )
        .expect("run disaster scenario");
        assert_eq!(evidence.scenario(), scenario);
        assert_eq!(evidence.rpo_millis(), 200);
        assert_eq!(evidence.rto_millis(), 100);
        assert!(evidence.passed());
        evidence.verify().expect("verify recovery evidence");
        let encoded = evidence.encode_canonical().expect("encode evidence");
        assert_eq!(
            DisasterRecoveryEvidence::decode_canonical(&encoded).expect("decode recovery evidence"),
            evidence
        );
        assert_eq!(fault.injected, vec![scenario]);
        assert_eq!(fault.cleared, vec![scenario]);
    }
}

#[test]
fn disaster_evidence_records_objective_breach_and_rejects_noncanonical_bytes() {
    let tenant = scope('1', '4');
    let confirmed = digest('a');
    let manifest = manifest(&tenant, 1_000, confirmed.clone());
    let plan = DisasterRecoveryPlan::try_new(
        DrillId::try_new(id("drl", 'E')).expect("drill id"),
        tenant,
        100,
        50,
    )
    .expect("strict recovery plan");
    let mut fault = FaultFixture {
        failed_at_millis: 1_200,
        confirmed_state_digest: confirmed.clone(),
        injected: Vec::new(),
        cleared: Vec::new(),
    };
    let mut observer = RecoveryFixture {
        recovered_at_millis: 1_300,
        confirmed_state_digest: confirmed,
    };
    let mut target = RestoreFixture::default();
    let result = DisasterRecoveryRunner::run(
        &plan,
        DisasterScenario::DatabaseUnavailable,
        &manifest,
        RestoreId::try_new(id("rst", 'E')).expect("restore id"),
        evidence(&manifest),
        &mut target,
        &mut fault,
        &mut observer,
    )
    .expect("restore succeeds while objective fails");
    assert!(!result.passed());
    let canonical = result.encode_canonical().expect("encode evidence");
    let mut changed = String::from_utf8(canonical.clone()).expect("UTF-8 evidence");
    changed = changed.replacen("\"rto_millis\":100", "\"rto_millis\":101", 1);
    assert_eq!(
        DisasterRecoveryEvidence::decode_canonical(changed.as_bytes())
            .expect_err("changed RTO evidence is rejected")
            .kind(),
        DrillErrorKind::Integrity
    );
    let mut noncanonical = vec![b' '];
    noncanonical.extend(canonical);
    assert_eq!(
        DisasterRecoveryEvidence::decode_canonical(&noncanonical)
            .expect_err("noncanonical evidence is rejected")
            .kind(),
        DrillErrorKind::Integrity
    );
}

#[test]
fn disaster_cross_tenant_manifest_fails_before_fault_injection() {
    let plan = DisasterRecoveryPlan::try_new(
        DrillId::try_new(id("drl", 'F')).expect("drill id"),
        scope('1', '4'),
        100,
        100,
    )
    .expect("recovery plan");
    let manifest = manifest(&scope('9', '8'), 1_000, digest('a'));
    let mut fault = FaultFixture {
        failed_at_millis: 1_200,
        confirmed_state_digest: digest('a'),
        injected: Vec::new(),
        cleared: Vec::new(),
    };
    let mut observer = RecoveryFixture {
        recovered_at_millis: 1_300,
        confirmed_state_digest: digest('a'),
    };
    let error = DisasterRecoveryRunner::run(
        &plan,
        DisasterScenario::ControlPlaneInstanceLoss,
        &manifest,
        RestoreId::try_new(id("rst", 'F')).expect("restore id"),
        evidence(&manifest),
        &mut RestoreFixture::default(),
        &mut fault,
        &mut observer,
    )
    .expect_err("cross-tenant manifest is rejected");
    assert_eq!(error.kind(), DrillErrorKind::TenantMismatch);
    assert!(fault.injected.is_empty());
}

fn lifecycle(
    scope: &AuditScope,
    release: Sha256Digest,
    state: LifecycleState,
    accepting: bool,
    in_flight: u64,
    confirmed: (u64, Sha256Digest),
    at_millis: u64,
) -> ControlPlaneLifecycleSnapshot {
    ControlPlaneLifecycleSnapshot::try_new(
        scope.clone(),
        release,
        state,
        accepting,
        in_flight,
        confirmed.0,
        confirmed.1,
        at_millis,
    )
    .expect("lifecycle snapshot")
}

struct DrainFixture {
    preflight: ControlPlaneLifecycleSnapshot,
    draining: ControlPlaneLifecycleSnapshot,
    drained: ControlPlaneLifecycleSnapshot,
    target_health: ControlPlaneLifecycleSnapshot,
    resumed_target: ControlPlaneLifecycleSnapshot,
    resumed_current: ControlPlaneLifecycleSnapshot,
}

impl DeploymentDrainAuthority for DrainFixture {
    fn preflight(
        &mut self,
        _scope: &AuditScope,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError> {
        Ok(self.preflight.clone())
    }

    fn begin_drain(
        &mut self,
        _scope: &AuditScope,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError> {
        Ok(self.draining.clone())
    }

    fn await_drained(
        &mut self,
        _scope: &AuditScope,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError> {
        Ok(self.drained.clone())
    }

    fn target_health(
        &mut self,
        _scope: &AuditScope,
        _release_source_digest: &Sha256Digest,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError> {
        Ok(self.target_health.clone())
    }

    fn resume(
        &mut self,
        _scope: &AuditScope,
        release_source_digest: &Sha256Digest,
    ) -> Result<ControlPlaneLifecycleSnapshot, DrainAuthorityError> {
        if release_source_digest == self.resumed_target.release_source_digest() {
            Ok(self.resumed_target.clone())
        } else {
            Ok(self.resumed_current.clone())
        }
    }
}

struct UpgradeBackupFixture {
    manifest: BackupManifest,
    calls: u64,
    corrupt_evidence: bool,
}

impl RollingUpgradeBackupPort for UpgradeBackupFixture {
    fn capture_after_drain(
        &mut self,
        _drained: &ControlPlaneLifecycleSnapshot,
    ) -> Result<RollingUpgradeBackup, RollingUpgradeBackupPortError> {
        self.calls += 1;
        let mut restore_evidence = evidence(&self.manifest);
        if self.corrupt_evidence {
            let snapshot = restore_evidence[0].snapshot();
            restore_evidence[0] = RestoreEvidence::new(
                BackupComponentSnapshot::try_new(
                    snapshot.kind(),
                    snapshot.scope().clone(),
                    snapshot.consistency_cut_digest().clone(),
                    snapshot.checkpoint_digest().clone(),
                    digest('7'),
                    snapshot.record_count(),
                    snapshot.byte_count(),
                )
                .expect("changed restore evidence"),
            );
        }
        Ok(RollingUpgradeBackup::new(
            self.manifest.clone(),
            restore_evidence,
        ))
    }
}

#[derive(Default)]
struct DeploymentFixture {
    fail_install: bool,
    installs: u64,
    activations: u64,
    rollbacks: Vec<Sha256Digest>,
}

impl RollingUpgradePort for DeploymentFixture {
    fn install(&mut self, _target: &DeploymentRelease) -> Result<(), RollingUpgradePortError> {
        self.installs += 1;
        if self.fail_install {
            Err(RollingUpgradePortError::new())
        } else {
            Ok(())
        }
    }

    fn activate(&mut self, _target: &DeploymentRelease) -> Result<(), RollingUpgradePortError> {
        self.activations += 1;
        Ok(())
    }

    fn rollback(
        &mut self,
        approved_current: &DeploymentRelease,
        _manifest_digest: &Sha256Digest,
    ) -> Result<(), RollingUpgradePortError> {
        self.rollbacks
            .push(approved_current.source_digest().clone());
        Ok(())
    }
}

fn releases() -> (DeploymentRelease, DeploymentRelease) {
    let current = DeploymentRelease::try_new(digest('1'), digest('c'), digest('d'), 800)
        .expect("current release");
    let target = DeploymentRelease::try_new(digest('2'), digest('c'), digest('d'), 900)
        .expect("target release");
    (current, target)
}

fn upgrade_fixtures(
    target_health_state: LifecycleState,
) -> (
    RollingUpgradePlan,
    DrainFixture,
    UpgradeBackupFixture,
    RestoreFixture,
    DeploymentFixture,
) {
    let tenant = scope('1', '4');
    let confirmed = digest('a');
    let (current, target) = releases();
    let plan = RollingUpgradePlan::try_new(
        DrillId::try_new(id("drl", 'G')).expect("drill id"),
        tenant.clone(),
        current,
        target,
        1_000,
        500,
    )
    .expect("upgrade plan");
    let drain = DrainFixture {
        preflight: lifecycle(
            &tenant,
            digest('1'),
            LifecycleState::Healthy,
            true,
            2,
            (9, digest('9')),
            1_000,
        ),
        draining: lifecycle(
            &tenant,
            digest('1'),
            LifecycleState::Draining,
            false,
            1,
            (10, confirmed.clone()),
            1_050,
        ),
        drained: lifecycle(
            &tenant,
            digest('1'),
            LifecycleState::Drained,
            false,
            0,
            (10, confirmed.clone()),
            1_100,
        ),
        target_health: lifecycle(
            &tenant,
            digest('2'),
            target_health_state,
            false,
            0,
            (10, confirmed.clone()),
            1_300,
        ),
        resumed_target: lifecycle(
            &tenant,
            digest('2'),
            LifecycleState::Healthy,
            true,
            0,
            (10, confirmed.clone()),
            1_400,
        ),
        resumed_current: lifecycle(
            &tenant,
            digest('1'),
            LifecycleState::Healthy,
            true,
            0,
            (10, confirmed.clone()),
            1_400,
        ),
    };
    let backup = UpgradeBackupFixture {
        manifest: manifest(&tenant, 1_150, confirmed),
        calls: 0,
        corrupt_evidence: false,
    };
    (
        plan,
        drain,
        backup,
        RestoreFixture::default(),
        DeploymentFixture::default(),
    )
}

#[test]
fn rolling_upgrade_drains_captures_and_activates_one_canonical_contract() {
    let (plan, mut drain, mut backup, mut restore, mut deployment) =
        upgrade_fixtures(LifecycleState::Healthy);
    let result = RollingUpgradeRunner::run(
        &plan,
        RestoreId::try_new(id("rst", 'G')).expect("restore id"),
        &mut drain,
        &mut backup,
        &mut restore,
        &mut deployment,
    )
    .expect("rolling upgrade");
    assert_eq!(result.result(), RollingUpgradeResult::Activated);
    assert_eq!(result.rto_millis(), 400);
    assert!(result.passed());
    assert_eq!(backup.calls, 1);
    assert_eq!(deployment.installs, 1);
    assert_eq!(deployment.activations, 1);
    assert!(deployment.rollbacks.is_empty());
    let encoded = result.encode_canonical().expect("encode upgrade evidence");
    assert_eq!(
        RollingUpgradeEvidence::decode_canonical(&encoded).expect("decode upgrade evidence"),
        result
    );
}

#[test]
fn unhealthy_target_restores_exact_backup_and_rolls_back_to_approved_source() {
    let (plan, mut drain, mut backup, mut restore, mut deployment) =
        upgrade_fixtures(LifecycleState::Unhealthy);
    let manifest_digest = backup.manifest.manifest_digest().clone();
    let result = RollingUpgradeRunner::run(
        &plan,
        RestoreId::try_new(id("rst", 'H')).expect("restore id"),
        &mut drain,
        &mut backup,
        &mut restore,
        &mut deployment,
    )
    .expect("failed health rolls back");
    assert_eq!(result.result(), RollingUpgradeResult::RolledBack);
    assert_eq!(restore.active, Some(manifest_digest));
    assert_eq!(deployment.rollbacks, vec![digest('1')]);
    assert_eq!(deployment.activations, 0);
}

#[test]
fn rollback_rejects_a_readback_that_loses_confirmed_sequence() {
    let (plan, mut drain, mut backup, mut restore, mut deployment) =
        upgrade_fixtures(LifecycleState::Unhealthy);
    drain.resumed_current = lifecycle(
        &scope('1', '4'),
        digest('1'),
        LifecycleState::Healthy,
        true,
        0,
        (9, digest('a')),
        1_400,
    );
    let error = RollingUpgradeRunner::run(
        &plan,
        RestoreId::try_new(id("rst", 'M')).expect("restore id"),
        &mut drain,
        &mut backup,
        &mut restore,
        &mut deployment,
    )
    .expect_err("rollback may not lose a confirmed sequence");
    assert_eq!(error.kind(), DrillErrorKind::Integrity);
    assert_eq!(deployment.rollbacks, vec![digest('1')]);
}

#[test]
fn changed_contract_or_data_boundary_is_rejected_before_drain() {
    let tenant = scope('1', '4');
    let current = DeploymentRelease::try_new(digest('1'), digest('c'), digest('d'), 800)
        .expect("current release");
    let changed_contract = DeploymentRelease::try_new(digest('2'), digest('e'), digest('d'), 900)
        .expect("changed contract release");
    assert_eq!(
        RollingUpgradePlan::try_new(
            DrillId::try_new(id("drl", 'H')).expect("drill id"),
            tenant,
            current,
            changed_contract,
            1_000,
            500,
        )
        .expect_err("dual contract rollout is rejected")
        .kind(),
        DrillErrorKind::Conflict
    );
}

#[test]
fn post_drain_backup_must_match_the_confirmed_state_digest() {
    let (plan, mut drain, mut backup, mut restore, mut deployment) =
        upgrade_fixtures(LifecycleState::Healthy);
    backup.manifest = manifest(&scope('1', '4'), 1_150, digest('8'));
    let error = RollingUpgradeRunner::run(
        &plan,
        RestoreId::try_new(id("rst", 'J')).expect("restore id"),
        &mut drain,
        &mut backup,
        &mut restore,
        &mut deployment,
    )
    .expect_err("changed confirmed state is rejected");
    assert_eq!(error.kind(), DrillErrorKind::Integrity);
    assert_eq!(deployment.installs, 0);
}

#[test]
fn rollback_evidence_is_verified_before_target_installation() {
    let (plan, mut drain, mut backup, mut restore, mut deployment) =
        upgrade_fixtures(LifecycleState::Healthy);
    backup.corrupt_evidence = true;
    let error = RollingUpgradeRunner::run(
        &plan,
        RestoreId::try_new(id("rst", 'K')).expect("restore id"),
        &mut drain,
        &mut backup,
        &mut restore,
        &mut deployment,
    )
    .expect_err("unrestorable backup stops before install");
    assert_eq!(error.kind(), DrillErrorKind::Integrity);
    assert_eq!(deployment.installs, 0);
    assert!(restore.active.is_none());
}
