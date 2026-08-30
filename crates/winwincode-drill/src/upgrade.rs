// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_backup::{
    BackupComponentKind, BackupManifest, RestoreCoordinator, RestoreEvidence, RestoreId,
    RestoreTarget, VerifiedRestore,
};
use winwincode_domain::Sha256Digest;

use crate::{
    ControlPlaneLifecycleSnapshot, DeploymentDrainAuthority, DrillError, DrillId, LifecycleState,
    MAX_SAFE_INTEGER,
    lifecycle::{validate_digest, validate_scope},
};

const EVIDENCE_FORMAT: &str = "winwincode.rolling-upgrade-evidence.v1";
const EVIDENCE_DOMAIN: &[u8] = b"winwincode.rolling-upgrade-evidence.v1";

/// One approved immutable application source and its canonical boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentRelease {
    source_digest: Sha256Digest,
    contract_digest: Sha256Digest,
    data_boundary_digest: Sha256Digest,
    approved_at_millis: u64,
}

impl DeploymentRelease {
    /// Builds one approved release fact.
    ///
    /// # Errors
    ///
    /// Rejects malformed digests or approval time.
    pub fn try_new(
        source_digest: Sha256Digest,
        contract_digest: Sha256Digest,
        data_boundary_digest: Sha256Digest,
        approved_at_millis: u64,
    ) -> Result<Self, DrillError> {
        validate_digest(&source_digest)?;
        validate_digest(&contract_digest)?;
        validate_digest(&data_boundary_digest)?;
        validate_time(approved_at_millis)?;
        Ok(Self {
            source_digest,
            contract_digest,
            data_boundary_digest,
            approved_at_millis,
        })
    }

    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
    }
    #[must_use]
    pub const fn contract_digest(&self) -> &Sha256Digest {
        &self.contract_digest
    }
    #[must_use]
    pub const fn data_boundary_digest(&self) -> &Sha256Digest {
        &self.data_boundary_digest
    }
}

/// Fixed rollout plan. Current and target code must use the same one canonical
/// public contract and data boundary, so rollback never activates a legacy
/// contract or a second data model.
#[derive(Clone, Debug)]
pub struct RollingUpgradePlan {
    drill_id: DrillId,
    scope: AuditScope,
    current_release: DeploymentRelease,
    target_release: DeploymentRelease,
    initiated_at_millis: u64,
    maximum_rto_millis: u64,
}

impl RollingUpgradePlan {
    /// Builds a single-contract rolling upgrade.
    ///
    /// # Errors
    ///
    /// Requires distinct approved sources with identical contract/data
    /// boundaries and bounded timing facts.
    pub fn try_new(
        drill_id: DrillId,
        scope: AuditScope,
        current_release: DeploymentRelease,
        target_release: DeploymentRelease,
        initiated_at_millis: u64,
        maximum_rto_millis: u64,
    ) -> Result<Self, DrillError> {
        drill_id.validate()?;
        validate_scope(&scope)?;
        validate_time(initiated_at_millis)?;
        if maximum_rto_millis == 0 || maximum_rto_millis > MAX_SAFE_INTEGER {
            return Err(DrillError::invalid());
        }
        if current_release.source_digest == target_release.source_digest
            || current_release.contract_digest != target_release.contract_digest
            || current_release.data_boundary_digest != target_release.data_boundary_digest
            || current_release.approved_at_millis > initiated_at_millis
            || target_release.approved_at_millis > initiated_at_millis
        {
            return Err(DrillError::conflict());
        }
        Ok(Self {
            drill_id,
            scope,
            current_release,
            target_release,
            initiated_at_millis,
            maximum_rto_millis,
        })
    }
}

/// Captured post-drain backup plus exact staged read-back evidence for rollback.
pub struct RollingUpgradeBackup {
    manifest: BackupManifest,
    restore_evidence: Vec<RestoreEvidence>,
}

impl RollingUpgradeBackup {
    #[must_use]
    pub fn new(manifest: BackupManifest, restore_evidence: Vec<RestoreEvidence>) -> Self {
        Self {
            manifest,
            restore_evidence,
        }
    }
}

/// Stable post-drain backup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RollingUpgradeBackupPortError;

impl RollingUpgradeBackupPortError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for RollingUpgradeBackupPortError {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapter that captures the single canonical backup only after the authority
/// proves the instance is drained.
pub trait RollingUpgradeBackupPort {
    /// Captures the confirmed post-drain generation.
    ///
    /// # Errors
    ///
    /// Returns when any required backup source cannot be captured.
    fn capture_after_drain(
        &mut self,
        drained: &ControlPlaneLifecycleSnapshot,
    ) -> Result<RollingUpgradeBackup, RollingUpgradeBackupPortError>;
}

/// Stable deployment adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RollingUpgradePortError;

impl RollingUpgradePortError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for RollingUpgradePortError {
    fn default() -> Self {
        Self::new()
    }
}

/// Deployment operations. Rollback receives only the plan's approved current
/// source and the exact verified backup generation.
pub trait RollingUpgradePort {
    /// Installs the approved target without activating it.
    ///
    /// # Errors
    ///
    /// Returns when installation fails or the source is unavailable.
    fn install(&mut self, target: &DeploymentRelease) -> Result<(), RollingUpgradePortError>;

    /// Atomically activates the healthy target.
    ///
    /// # Errors
    ///
    /// Returns when activation cannot be completed.
    fn activate(&mut self, target: &DeploymentRelease) -> Result<(), RollingUpgradePortError>;

    /// Restores only the approved current source over the verified generation.
    ///
    /// # Errors
    ///
    /// Returns when rollback cannot be completed.
    fn rollback(
        &mut self,
        approved_current: &DeploymentRelease,
        manifest_digest: &Sha256Digest,
    ) -> Result<(), RollingUpgradePortError>;
}

/// Final rollout outcome. A verified rollback is evidence of a successful
/// failure drill, not a successful target deployment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingUpgradeResult {
    Activated,
    RolledBack,
}

/// Canonical rollout/rollback evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollingUpgradeEvidence {
    drill_id: DrillId,
    scope: AuditScope,
    current_source_digest: Sha256Digest,
    target_source_digest: Sha256Digest,
    contract_digest: Sha256Digest,
    data_boundary_digest: Sha256Digest,
    manifest_digest: Sha256Digest,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
    initiated_at_millis: u64,
    completed_at_millis: u64,
    rto_millis: u64,
    maximum_rto_millis: u64,
    result: RollingUpgradeResult,
    passed: bool,
    evidence_digest: Sha256Digest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceWire {
    format: String,
    drill_id: DrillId,
    scope: AuditScope,
    current_source_digest: Sha256Digest,
    target_source_digest: Sha256Digest,
    contract_digest: Sha256Digest,
    data_boundary_digest: Sha256Digest,
    manifest_digest: Sha256Digest,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
    initiated_at_millis: u64,
    completed_at_millis: u64,
    rto_millis: u64,
    maximum_rto_millis: u64,
    result: RollingUpgradeResult,
    passed: bool,
    evidence_digest: Sha256Digest,
}

#[derive(Serialize)]
struct EvidenceDigestWire<'a> {
    format: &'static str,
    drill_id: &'a DrillId,
    scope: &'a AuditScope,
    current_source_digest: &'a Sha256Digest,
    target_source_digest: &'a Sha256Digest,
    contract_digest: &'a Sha256Digest,
    data_boundary_digest: &'a Sha256Digest,
    manifest_digest: &'a Sha256Digest,
    confirmed_sequence: u64,
    confirmed_state_digest: &'a Sha256Digest,
    initiated_at_millis: u64,
    completed_at_millis: u64,
    rto_millis: u64,
    maximum_rto_millis: u64,
    result: RollingUpgradeResult,
    passed: bool,
}

impl RollingUpgradeEvidence {
    #[must_use]
    pub const fn result(&self) -> RollingUpgradeResult {
        self.result
    }
    #[must_use]
    pub const fn rto_millis(&self) -> u64 {
        self.rto_millis
    }
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }

    /// Verifies source/boundary/timing facts and the evidence digest.
    ///
    /// # Errors
    ///
    /// Rejects changed or inconsistent evidence.
    pub fn verify(&self) -> Result<(), DrillError> {
        self.drill_id.validate()?;
        validate_scope(&self.scope)?;
        for digest in [
            &self.current_source_digest,
            &self.target_source_digest,
            &self.contract_digest,
            &self.data_boundary_digest,
            &self.manifest_digest,
            &self.confirmed_state_digest,
            &self.evidence_digest,
        ] {
            validate_digest(digest)?;
        }
        validate_time(self.initiated_at_millis)?;
        validate_time(self.completed_at_millis)?;
        if self.current_source_digest == self.target_source_digest
            || self.confirmed_sequence > MAX_SAFE_INTEGER
            || self.maximum_rto_millis == 0
            || self.maximum_rto_millis > MAX_SAFE_INTEGER
            || self.completed_at_millis < self.initiated_at_millis
            || self.rto_millis != self.completed_at_millis - self.initiated_at_millis
            || self.passed != (self.rto_millis <= self.maximum_rto_millis)
            || evidence_digest(self)? != self.evidence_digest
        {
            return Err(DrillError::integrity());
        }
        Ok(())
    }

    /// Encodes the canonical portable rollout evidence.
    ///
    /// # Errors
    ///
    /// Rejects invalid evidence or an encoding failure.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, DrillError> {
        self.verify()?;
        serde_json::to_vec(&self.wire()).map_err(|_| DrillError::integrity())
    }

    /// Decodes only the canonical rollout evidence representation.
    ///
    /// # Errors
    ///
    /// Rejects unknown versions, altered fields, or non-canonical bytes.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, DrillError> {
        let wire =
            serde_json::from_slice::<EvidenceWire>(bytes).map_err(|_| DrillError::invalid())?;
        if wire.format != EVIDENCE_FORMAT {
            return Err(DrillError::conflict());
        }
        let evidence = Self::from_wire(wire);
        evidence.verify()?;
        if evidence.encode_canonical()? != bytes {
            return Err(DrillError::integrity());
        }
        Ok(evidence)
    }

    fn wire(&self) -> EvidenceWire {
        EvidenceWire {
            format: EVIDENCE_FORMAT.to_owned(),
            drill_id: self.drill_id.clone(),
            scope: self.scope.clone(),
            current_source_digest: self.current_source_digest.clone(),
            target_source_digest: self.target_source_digest.clone(),
            contract_digest: self.contract_digest.clone(),
            data_boundary_digest: self.data_boundary_digest.clone(),
            manifest_digest: self.manifest_digest.clone(),
            confirmed_sequence: self.confirmed_sequence,
            confirmed_state_digest: self.confirmed_state_digest.clone(),
            initiated_at_millis: self.initiated_at_millis,
            completed_at_millis: self.completed_at_millis,
            rto_millis: self.rto_millis,
            maximum_rto_millis: self.maximum_rto_millis,
            result: self.result,
            passed: self.passed,
            evidence_digest: self.evidence_digest.clone(),
        }
    }

    fn from_wire(wire: EvidenceWire) -> Self {
        Self {
            drill_id: wire.drill_id,
            scope: wire.scope,
            current_source_digest: wire.current_source_digest,
            target_source_digest: wire.target_source_digest,
            contract_digest: wire.contract_digest,
            data_boundary_digest: wire.data_boundary_digest,
            manifest_digest: wire.manifest_digest,
            confirmed_sequence: wire.confirmed_sequence,
            confirmed_state_digest: wire.confirmed_state_digest,
            initiated_at_millis: wire.initiated_at_millis,
            completed_at_millis: wire.completed_at_millis,
            rto_millis: wire.rto_millis,
            maximum_rto_millis: wire.maximum_rto_millis,
            result: wire.result,
            passed: wire.passed,
            evidence_digest: wire.evidence_digest,
        }
    }
}

/// Canonical rolling-upgrade state machine.
pub struct RollingUpgradeRunner;

impl RollingUpgradeRunner {
    /// Drains, captures, installs, health-checks, and activates; any failure
    /// after capture restores the exact generation and rolls back only to the
    /// approved current source.
    ///
    /// # Errors
    ///
    /// Fails closed on incomplete drain, cross-tenant facts, changed confirmed
    /// state, failed restore/rollback, or unavailable lifecycle authority.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        plan: &RollingUpgradePlan,
        restore_id: RestoreId,
        drain: &mut dyn DeploymentDrainAuthority,
        backup: &mut dyn RollingUpgradeBackupPort,
        restore_target: &mut dyn RestoreTarget,
        deployment: &mut dyn RollingUpgradePort,
    ) -> Result<RollingUpgradeEvidence, DrillError> {
        let preflight = drain
            .preflight(&plan.scope)
            .map_err(|_| DrillError::unavailable())?;
        validate_preflight(plan, &preflight)?;
        let draining = drain
            .begin_drain(&plan.scope)
            .map_err(|_| DrillError::unavailable())?;
        validate_draining(plan, &preflight, &draining)?;
        let drained = drain
            .await_drained(&plan.scope)
            .map_err(|_| DrillError::unavailable())?;
        validate_drained(plan, &draining, &drained)?;
        let captured = backup
            .capture_after_drain(&drained)
            .map_err(|_| DrillError::unavailable())?;
        validate_backup(plan, &drained, &captured.manifest)?;
        let verified =
            RestoreCoordinator::verify(restore_id, &captured.manifest, captured.restore_evidence)
                .map_err(|_| DrillError::integrity())?;

        if deployment.install(&plan.target_release).is_err() {
            return rollback(
                plan,
                drain,
                restore_target,
                deployment,
                &captured.manifest,
                &verified,
                &drained,
            );
        }
        let Ok(target_health) =
            drain.target_health(&plan.scope, plan.target_release.source_digest())
        else {
            return rollback(
                plan,
                drain,
                restore_target,
                deployment,
                &captured.manifest,
                &verified,
                &drained,
            );
        };
        if validate_target_health(plan, &drained, &target_health).is_err()
            || deployment.activate(&plan.target_release).is_err()
        {
            return rollback(
                plan,
                drain,
                restore_target,
                deployment,
                &captured.manifest,
                &verified,
                &drained,
            );
        }
        let Ok(resumed) = drain.resume(&plan.scope, plan.target_release.source_digest()) else {
            return rollback(
                plan,
                drain,
                restore_target,
                deployment,
                &captured.manifest,
                &verified,
                &drained,
            );
        };
        validate_resumed(
            plan,
            &drained,
            &resumed,
            plan.target_release.source_digest(),
        )?;
        build_evidence(
            plan,
            &captured.manifest,
            &resumed,
            RollingUpgradeResult::Activated,
        )
    }
}

fn rollback(
    plan: &RollingUpgradePlan,
    drain: &mut dyn DeploymentDrainAuthority,
    restore_target: &mut dyn RestoreTarget,
    deployment: &mut dyn RollingUpgradePort,
    manifest: &BackupManifest,
    verified: &VerifiedRestore,
    confirmed: &ControlPlaneLifecycleSnapshot,
) -> Result<RollingUpgradeEvidence, DrillError> {
    RestoreCoordinator::activate(verified, restore_target).map_err(|_| DrillError::integrity())?;
    deployment
        .rollback(&plan.current_release, manifest.manifest_digest())
        .map_err(|_| DrillError::unavailable())?;
    let resumed = drain
        .resume(&plan.scope, plan.current_release.source_digest())
        .map_err(|_| DrillError::unavailable())?;
    let expected = delivery_state_digest(manifest)?;
    if resumed.confirmed_state_digest() != expected {
        return Err(DrillError::integrity());
    }
    validate_resumed(
        plan,
        confirmed,
        &resumed,
        plan.current_release.source_digest(),
    )?;
    build_evidence(plan, manifest, &resumed, RollingUpgradeResult::RolledBack)
}

fn validate_preflight(
    plan: &RollingUpgradePlan,
    snapshot: &ControlPlaneLifecycleSnapshot,
) -> Result<(), DrillError> {
    validate_scope_release(plan, snapshot, plan.current_release.source_digest())?;
    if snapshot.state() != LifecycleState::Healthy || !snapshot.accepting_new_work() {
        return Err(DrillError::drain());
    }
    Ok(())
}

fn validate_draining(
    plan: &RollingUpgradePlan,
    preflight: &ControlPlaneLifecycleSnapshot,
    draining: &ControlPlaneLifecycleSnapshot,
) -> Result<(), DrillError> {
    validate_scope_release(plan, draining, plan.current_release.source_digest())?;
    if !matches!(
        draining.state(),
        LifecycleState::Draining | LifecycleState::Drained
    ) || draining.accepting_new_work()
        || draining.confirmed_sequence() < preflight.confirmed_sequence()
        || draining.observed_at_millis() < preflight.observed_at_millis()
    {
        return Err(DrillError::drain());
    }
    Ok(())
}

fn validate_drained(
    plan: &RollingUpgradePlan,
    draining: &ControlPlaneLifecycleSnapshot,
    drained: &ControlPlaneLifecycleSnapshot,
) -> Result<(), DrillError> {
    validate_scope_release(plan, drained, plan.current_release.source_digest())?;
    if drained.state() != LifecycleState::Drained
        || drained.accepting_new_work()
        || drained.in_flight_requests() != 0
        || drained.confirmed_sequence() < draining.confirmed_sequence()
        || drained.observed_at_millis() < draining.observed_at_millis()
    {
        return Err(DrillError::drain());
    }
    Ok(())
}

fn validate_backup(
    plan: &RollingUpgradePlan,
    drained: &ControlPlaneLifecycleSnapshot,
    manifest: &BackupManifest,
) -> Result<(), DrillError> {
    if manifest.scope() != &plan.scope {
        return Err(DrillError::tenant());
    }
    if manifest.captured_at_millis() < drained.observed_at_millis()
        || delivery_state_digest(manifest)? != drained.confirmed_state_digest()
    {
        return Err(DrillError::integrity());
    }
    Ok(())
}

fn validate_target_health(
    plan: &RollingUpgradePlan,
    drained: &ControlPlaneLifecycleSnapshot,
    health: &ControlPlaneLifecycleSnapshot,
) -> Result<(), DrillError> {
    validate_scope_release(plan, health, plan.target_release.source_digest())?;
    if health.state() != LifecycleState::Healthy
        || health.accepting_new_work()
        || health.confirmed_sequence() != drained.confirmed_sequence()
        || health.confirmed_state_digest() != drained.confirmed_state_digest()
        || health.observed_at_millis() < drained.observed_at_millis()
    {
        return Err(DrillError::integrity());
    }
    Ok(())
}

fn validate_resumed(
    plan: &RollingUpgradePlan,
    confirmed: &ControlPlaneLifecycleSnapshot,
    resumed: &ControlPlaneLifecycleSnapshot,
    expected_release: &Sha256Digest,
) -> Result<(), DrillError> {
    validate_scope_release(plan, resumed, expected_release)?;
    if resumed.state() != LifecycleState::Healthy
        || !resumed.accepting_new_work()
        || resumed.confirmed_sequence() < confirmed.confirmed_sequence()
        || resumed.confirmed_state_digest() != confirmed.confirmed_state_digest()
    {
        return Err(DrillError::integrity());
    }
    Ok(())
}

fn validate_scope_release(
    plan: &RollingUpgradePlan,
    snapshot: &ControlPlaneLifecycleSnapshot,
    expected_release: &Sha256Digest,
) -> Result<(), DrillError> {
    if snapshot.scope() != &plan.scope {
        return Err(DrillError::tenant());
    }
    if snapshot.release_source_digest() != expected_release {
        return Err(DrillError::conflict());
    }
    Ok(())
}

fn delivery_state_digest(manifest: &BackupManifest) -> Result<&Sha256Digest, DrillError> {
    manifest
        .components()
        .iter()
        .find(|component| component.kind() == BackupComponentKind::DeliveryState)
        .map(winwincode_backup::BackupComponentSnapshot::content_digest)
        .ok_or_else(DrillError::integrity)
}

fn build_evidence(
    plan: &RollingUpgradePlan,
    manifest: &BackupManifest,
    completed: &ControlPlaneLifecycleSnapshot,
    result: RollingUpgradeResult,
) -> Result<RollingUpgradeEvidence, DrillError> {
    if completed.observed_at_millis() < plan.initiated_at_millis {
        return Err(DrillError::integrity());
    }
    let rto_millis = completed.observed_at_millis() - plan.initiated_at_millis;
    let mut evidence = RollingUpgradeEvidence {
        drill_id: plan.drill_id.clone(),
        scope: plan.scope.clone(),
        current_source_digest: plan.current_release.source_digest.clone(),
        target_source_digest: plan.target_release.source_digest.clone(),
        contract_digest: plan.target_release.contract_digest.clone(),
        data_boundary_digest: plan.target_release.data_boundary_digest.clone(),
        manifest_digest: manifest.manifest_digest().clone(),
        confirmed_sequence: completed.confirmed_sequence(),
        confirmed_state_digest: completed.confirmed_state_digest().clone(),
        initiated_at_millis: plan.initiated_at_millis,
        completed_at_millis: completed.observed_at_millis(),
        rto_millis,
        maximum_rto_millis: plan.maximum_rto_millis,
        result,
        passed: rto_millis <= plan.maximum_rto_millis,
        evidence_digest: Sha256Digest(String::new()),
    };
    evidence.evidence_digest = evidence_digest(&evidence)?;
    evidence.verify()?;
    Ok(evidence)
}

fn evidence_digest(evidence: &RollingUpgradeEvidence) -> Result<Sha256Digest, DrillError> {
    let bytes = serde_json::to_vec(&EvidenceDigestWire {
        format: EVIDENCE_FORMAT,
        drill_id: &evidence.drill_id,
        scope: &evidence.scope,
        current_source_digest: &evidence.current_source_digest,
        target_source_digest: &evidence.target_source_digest,
        contract_digest: &evidence.contract_digest,
        data_boundary_digest: &evidence.data_boundary_digest,
        manifest_digest: &evidence.manifest_digest,
        confirmed_sequence: evidence.confirmed_sequence,
        confirmed_state_digest: &evidence.confirmed_state_digest,
        initiated_at_millis: evidence.initiated_at_millis,
        completed_at_millis: evidence.completed_at_millis,
        rto_millis: evidence.rto_millis,
        maximum_rto_millis: evidence.maximum_rto_millis,
        result: evidence.result,
        passed: evidence.passed,
    })
    .map_err(|_| DrillError::integrity())?;
    let mut hash = Sha256::new();
    hash.update(EVIDENCE_DOMAIN);
    hash.update([0]);
    hash.update(bytes);
    Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
}

fn validate_time(value: u64) -> Result<(), DrillError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(DrillError::invalid())
    } else {
        Ok(())
    }
}
