// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_backup::{
    BackupManifest, RestoreCoordinator, RestoreEvidence, RestoreId, RestoreTarget,
};
use winwincode_domain::Sha256Digest;

use crate::{
    DrillError, DrillId, MAX_SAFE_INTEGER,
    lifecycle::{validate_digest, validate_scope},
};

const EVIDENCE_FORMAT: &str = "winwincode.disaster-recovery-evidence.v1";
const EVIDENCE_DOMAIN: &[u8] = b"winwincode.disaster-recovery-evidence.v1";

/// Four canonical production failure families exercised by the runner.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisasterScenario {
    ControlPlaneInstanceLoss,
    DatabaseUnavailable,
    ObjectStoreCorruption,
    SecretStoreUnavailable,
}

/// Fixed tenant and service objectives for one deterministic drill.
#[derive(Clone, Debug)]
pub struct DisasterRecoveryPlan {
    drill_id: DrillId,
    scope: AuditScope,
    maximum_rpo_millis: u64,
    maximum_rto_millis: u64,
}

impl DisasterRecoveryPlan {
    /// Builds one bounded recovery objective.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities or out-of-range objectives.
    pub fn try_new(
        drill_id: DrillId,
        scope: AuditScope,
        rpo_objective_millis: u64,
        recovery_time_objective_millis: u64,
    ) -> Result<Self, DrillError> {
        drill_id.validate()?;
        validate_scope(&scope)?;
        if rpo_objective_millis > MAX_SAFE_INTEGER
            || recovery_time_objective_millis == 0
            || recovery_time_objective_millis > MAX_SAFE_INTEGER
        {
            return Err(DrillError::invalid());
        }
        Ok(Self {
            drill_id,
            scope,
            maximum_rpo_millis: rpo_objective_millis,
            maximum_rto_millis: recovery_time_objective_millis,
        })
    }
}

/// Durable failure boundary observed after fault injection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureObservation {
    scope: AuditScope,
    failed_at_millis: u64,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
}

impl FailureObservation {
    /// Builds exact last-confirmed state at failure time.
    ///
    /// # Errors
    ///
    /// Rejects malformed digest, time, or sequence facts.
    pub fn try_new(
        scope: AuditScope,
        failed_at_millis: u64,
        confirmed_sequence: u64,
        confirmed_state_digest: Sha256Digest,
    ) -> Result<Self, DrillError> {
        validate_digest(&confirmed_state_digest)?;
        validate_scope(&scope)?;
        validate_time(failed_at_millis)?;
        validate_count(confirmed_sequence)?;
        Ok(Self {
            scope,
            failed_at_millis,
            confirmed_sequence,
            confirmed_state_digest,
        })
    }
}

/// Read-back facts after restore and health recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryObservation {
    scope: AuditScope,
    recovered_at_millis: u64,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
    active_manifest_digest: Sha256Digest,
    healthy: bool,
}

impl RecoveryObservation {
    /// Builds one recovered-state read-back.
    ///
    /// # Errors
    ///
    /// Rejects malformed digests, time, or sequence facts.
    pub fn try_new(
        scope: AuditScope,
        recovered_at_millis: u64,
        confirmed_sequence: u64,
        confirmed_state_digest: Sha256Digest,
        active_manifest_digest: Sha256Digest,
        healthy: bool,
    ) -> Result<Self, DrillError> {
        validate_digest(&confirmed_state_digest)?;
        validate_digest(&active_manifest_digest)?;
        validate_scope(&scope)?;
        validate_time(recovered_at_millis)?;
        validate_count(confirmed_sequence)?;
        Ok(Self {
            scope,
            recovered_at_millis,
            confirmed_sequence,
            confirmed_state_digest,
            active_manifest_digest,
            healthy,
        })
    }
}

/// Secret-safe fault-injection failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultPortError;

impl FaultPortError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for FaultPortError {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic infrastructure fault boundary. It does not mutate business
/// state directly.
pub trait DisasterFaultPort {
    /// Activates one deterministic fault and reads the last confirmed boundary.
    ///
    /// # Errors
    ///
    /// Returns when the fixture or infrastructure fault cannot be activated.
    fn inject(
        &mut self,
        scenario: DisasterScenario,
        scope: &AuditScope,
    ) -> Result<FailureObservation, FaultPortError>;

    /// Clears only the exact active scenario.
    ///
    /// # Errors
    ///
    /// Returns when the injected fault cannot be safely cleared.
    fn clear(
        &mut self,
        scenario: DisasterScenario,
        scope: &AuditScope,
    ) -> Result<(), FaultPortError>;
}

/// Stable recovered-state observer failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryObservationPortError;

impl RecoveryObservationPortError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for RecoveryObservationPortError {
    fn default() -> Self {
        Self::new()
    }
}

/// Authoritative read-back after the restored generation becomes active.
pub trait RecoveryObservationPort {
    /// Reads authoritative state after restore and health recovery.
    ///
    /// # Errors
    ///
    /// Returns when the restored generation cannot be read back.
    fn observe(
        &mut self,
        scenario: DisasterScenario,
        scope: &AuditScope,
        manifest_digest: &Sha256Digest,
    ) -> Result<RecoveryObservation, RecoveryObservationPortError>;
}

/// Canonical, portable RPO/RTO and state-preservation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisasterRecoveryEvidence {
    drill_id: DrillId,
    scope: AuditScope,
    scenario: DisasterScenario,
    manifest_digest: Sha256Digest,
    consistency_cut_digest: Sha256Digest,
    recovery_point_at_millis: u64,
    failed_at_millis: u64,
    recovered_at_millis: u64,
    rpo_millis: u64,
    rto_millis: u64,
    maximum_rpo_millis: u64,
    maximum_rto_millis: u64,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
    passed: bool,
    evidence_digest: Sha256Digest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceWire {
    format: String,
    drill_id: DrillId,
    scope: AuditScope,
    scenario: DisasterScenario,
    manifest_digest: Sha256Digest,
    consistency_cut_digest: Sha256Digest,
    recovery_point_at_millis: u64,
    failed_at_millis: u64,
    recovered_at_millis: u64,
    rpo_millis: u64,
    rto_millis: u64,
    maximum_rpo_millis: u64,
    maximum_rto_millis: u64,
    confirmed_sequence: u64,
    confirmed_state_digest: Sha256Digest,
    passed: bool,
    evidence_digest: Sha256Digest,
}

#[derive(Serialize)]
struct EvidenceDigestWire<'a> {
    format: &'static str,
    drill_id: &'a DrillId,
    scope: &'a AuditScope,
    scenario: DisasterScenario,
    manifest_digest: &'a Sha256Digest,
    consistency_cut_digest: &'a Sha256Digest,
    recovery_point_at_millis: u64,
    failed_at_millis: u64,
    recovered_at_millis: u64,
    rpo_millis: u64,
    rto_millis: u64,
    maximum_rpo_millis: u64,
    maximum_rto_millis: u64,
    confirmed_sequence: u64,
    confirmed_state_digest: &'a Sha256Digest,
    passed: bool,
}

impl DisasterRecoveryEvidence {
    #[must_use]
    pub const fn scenario(&self) -> DisasterScenario {
        self.scenario
    }
    #[must_use]
    pub const fn rpo_millis(&self) -> u64 {
        self.rpo_millis
    }
    #[must_use]
    pub const fn rto_millis(&self) -> u64 {
        self.rto_millis
    }
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }
    #[must_use]
    pub const fn evidence_digest(&self) -> &Sha256Digest {
        &self.evidence_digest
    }

    /// Verifies all evidence facts and the canonical digest.
    ///
    /// # Errors
    ///
    /// Rejects altered, inconsistent, or out-of-range evidence.
    pub fn verify(&self) -> Result<(), DrillError> {
        self.drill_id.validate()?;
        validate_scope(&self.scope)?;
        validate_digest(&self.manifest_digest)?;
        validate_digest(&self.consistency_cut_digest)?;
        validate_digest(&self.confirmed_state_digest)?;
        validate_digest(&self.evidence_digest)?;
        validate_time(self.failed_at_millis)?;
        validate_time(self.recovered_at_millis)?;
        validate_time(self.recovery_point_at_millis)?;
        if self.maximum_rpo_millis > MAX_SAFE_INTEGER
            || self.maximum_rto_millis == 0
            || self.maximum_rto_millis > MAX_SAFE_INTEGER
            || self.confirmed_sequence > MAX_SAFE_INTEGER
            || self.recovery_point_at_millis > self.failed_at_millis
            || self.rpo_millis != self.failed_at_millis - self.recovery_point_at_millis
            || self.recovered_at_millis < self.failed_at_millis
            || self.rto_millis != self.recovered_at_millis - self.failed_at_millis
            || self.passed
                != (self.rpo_millis <= self.maximum_rpo_millis
                    && self.rto_millis <= self.maximum_rto_millis)
            || evidence_digest(self)? != self.evidence_digest
        {
            return Err(DrillError::integrity());
        }
        Ok(())
    }

    /// Encodes one canonical evidence document.
    ///
    /// # Errors
    ///
    /// Rejects invalid evidence or an encoding failure.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, DrillError> {
        self.verify()?;
        serde_json::to_vec(&self.wire()).map_err(|_| DrillError::integrity())
    }

    /// Decodes only the canonical evidence representation.
    ///
    /// # Errors
    ///
    /// Rejects unknown versions, non-canonical bytes, or altered fields.
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
            scenario: self.scenario,
            manifest_digest: self.manifest_digest.clone(),
            consistency_cut_digest: self.consistency_cut_digest.clone(),
            recovery_point_at_millis: self.recovery_point_at_millis,
            failed_at_millis: self.failed_at_millis,
            recovered_at_millis: self.recovered_at_millis,
            rpo_millis: self.rpo_millis,
            rto_millis: self.rto_millis,
            maximum_rpo_millis: self.maximum_rpo_millis,
            maximum_rto_millis: self.maximum_rto_millis,
            confirmed_sequence: self.confirmed_sequence,
            confirmed_state_digest: self.confirmed_state_digest.clone(),
            passed: self.passed,
            evidence_digest: self.evidence_digest.clone(),
        }
    }

    fn from_wire(wire: EvidenceWire) -> Self {
        Self {
            drill_id: wire.drill_id,
            scope: wire.scope,
            scenario: wire.scenario,
            manifest_digest: wire.manifest_digest,
            consistency_cut_digest: wire.consistency_cut_digest,
            recovery_point_at_millis: wire.recovery_point_at_millis,
            failed_at_millis: wire.failed_at_millis,
            recovered_at_millis: wire.recovered_at_millis,
            rpo_millis: wire.rpo_millis,
            rto_millis: wire.rto_millis,
            maximum_rpo_millis: wire.maximum_rpo_millis,
            maximum_rto_millis: wire.maximum_rto_millis,
            confirmed_sequence: wire.confirmed_sequence,
            confirmed_state_digest: wire.confirmed_state_digest,
            passed: wire.passed,
            evidence_digest: wire.evidence_digest,
        }
    }
}

/// Runs deterministic fault, restore, read-back, and evidence verification.
pub struct DisasterRecoveryRunner;

impl DisasterRecoveryRunner {
    /// Executes one canonical scenario using the sole backup restore verifier.
    ///
    /// # Errors
    ///
    /// Fails closed for cross-tenant facts, altered restored state, fault-port
    /// errors, restore failure, or unhealthy read-back.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        plan: &DisasterRecoveryPlan,
        scenario: DisasterScenario,
        manifest: &BackupManifest,
        restore_id: RestoreId,
        restore_evidence: impl IntoIterator<Item = RestoreEvidence>,
        restore_target: &mut dyn RestoreTarget,
        fault: &mut dyn DisasterFaultPort,
        observer: &mut dyn RecoveryObservationPort,
    ) -> Result<DisasterRecoveryEvidence, DrillError> {
        if manifest.scope() != &plan.scope {
            return Err(DrillError::tenant());
        }
        let failed = fault
            .inject(scenario, &plan.scope)
            .map_err(|_| DrillError::unavailable())?;
        fault
            .clear(scenario, &plan.scope)
            .map_err(|_| DrillError::unavailable())?;
        if failed.scope != plan.scope {
            return Err(DrillError::tenant());
        }
        if failed.failed_at_millis < manifest.captured_at_millis()
            || delivery_state_digest(manifest)? != &failed.confirmed_state_digest
        {
            return Err(DrillError::integrity());
        }
        RestoreCoordinator::restore(restore_id, manifest, restore_evidence, restore_target)
            .map_err(|error| match error.kind() {
                winwincode_backup::BackupErrorKind::TenantMismatch => DrillError::tenant(),
                winwincode_backup::BackupErrorKind::Integrity => DrillError::integrity(),
                _ => DrillError::unavailable(),
            })?;
        let recovered = observer
            .observe(scenario, &plan.scope, manifest.manifest_digest())
            .map_err(|_| DrillError::unavailable())?;
        validate_recovered(plan, manifest, &failed, &recovered)?;
        build_evidence(plan, scenario, manifest, &failed, &recovered)
    }
}

fn validate_recovered(
    plan: &DisasterRecoveryPlan,
    manifest: &BackupManifest,
    failed: &FailureObservation,
    recovered: &RecoveryObservation,
) -> Result<(), DrillError> {
    if recovered.scope != plan.scope {
        return Err(DrillError::tenant());
    }
    if !recovered.healthy
        || recovered.active_manifest_digest != *manifest.manifest_digest()
        || recovered.recovered_at_millis < failed.failed_at_millis
        || recovered.confirmed_sequence < failed.confirmed_sequence
        || recovered.confirmed_state_digest != failed.confirmed_state_digest
    {
        return Err(DrillError::integrity());
    }
    Ok(())
}

fn delivery_state_digest(manifest: &BackupManifest) -> Result<&Sha256Digest, DrillError> {
    manifest
        .components()
        .iter()
        .find(|component| component.kind() == winwincode_backup::BackupComponentKind::DeliveryState)
        .map(winwincode_backup::BackupComponentSnapshot::content_digest)
        .ok_or_else(DrillError::integrity)
}

fn build_evidence(
    plan: &DisasterRecoveryPlan,
    scenario: DisasterScenario,
    manifest: &BackupManifest,
    failed: &FailureObservation,
    recovered: &RecoveryObservation,
) -> Result<DisasterRecoveryEvidence, DrillError> {
    let recovery_point_lag = failed.failed_at_millis - manifest.captured_at_millis();
    let recovery_duration = recovered.recovered_at_millis - failed.failed_at_millis;
    let passed = recovery_point_lag <= plan.maximum_rpo_millis
        && recovery_duration <= plan.maximum_rto_millis;
    let mut evidence = DisasterRecoveryEvidence {
        drill_id: plan.drill_id.clone(),
        scope: plan.scope.clone(),
        scenario,
        manifest_digest: manifest.manifest_digest().clone(),
        consistency_cut_digest: manifest.consistency_cut_digest().clone(),
        recovery_point_at_millis: manifest.captured_at_millis(),
        failed_at_millis: failed.failed_at_millis,
        recovered_at_millis: recovered.recovered_at_millis,
        rpo_millis: recovery_point_lag,
        rto_millis: recovery_duration,
        maximum_rpo_millis: plan.maximum_rpo_millis,
        maximum_rto_millis: plan.maximum_rto_millis,
        confirmed_sequence: recovered.confirmed_sequence,
        confirmed_state_digest: recovered.confirmed_state_digest.clone(),
        passed,
        evidence_digest: Sha256Digest(String::new()),
    };
    evidence.evidence_digest = evidence_digest(&evidence)?;
    evidence.verify()?;
    Ok(evidence)
}

fn evidence_digest(evidence: &DisasterRecoveryEvidence) -> Result<Sha256Digest, DrillError> {
    let bytes = serde_json::to_vec(&EvidenceDigestWire {
        format: EVIDENCE_FORMAT,
        drill_id: &evidence.drill_id,
        scope: &evidence.scope,
        scenario: evidence.scenario,
        manifest_digest: &evidence.manifest_digest,
        consistency_cut_digest: &evidence.consistency_cut_digest,
        recovery_point_at_millis: evidence.recovery_point_at_millis,
        failed_at_millis: evidence.failed_at_millis,
        recovered_at_millis: evidence.recovered_at_millis,
        rpo_millis: evidence.rpo_millis,
        rto_millis: evidence.rto_millis,
        maximum_rpo_millis: evidence.maximum_rpo_millis,
        maximum_rto_millis: evidence.maximum_rto_millis,
        confirmed_sequence: evidence.confirmed_sequence,
        confirmed_state_digest: &evidence.confirmed_state_digest,
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

fn validate_count(value: u64) -> Result<(), DrillError> {
    if value > MAX_SAFE_INTEGER {
        Err(DrillError::invalid())
    } else {
        Ok(())
    }
}
