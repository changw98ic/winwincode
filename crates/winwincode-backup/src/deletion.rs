// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::{
    AuditStore, DataGovernanceAuthority, DeletionPermit, DeletionPortError, DeletionPortOutcome,
    GovernanceAuditContext, GovernanceDenial, GovernedDataFact, GovernedDeletionPort,
    GovernedDeletionResult,
};
use winwincode_domain::Sha256Digest;

use crate::{
    BackupComponentKind, BackupError, BackupManifest, MAX_SAFE_INTEGER, manifest::validate_digest,
};

const PROOF_DOMAIN: &[u8] = b"winwincode.backup-deletion-proof.v1";
const PROOF_FORMAT: &str = "winwincode.backup-deletion-proof.v1";

/// Backend receipt proving every required generation component was removed or
/// was already absent for the same policy decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupDeletionReceipt {
    manifest_digest: Sha256Digest,
    decision_digest: Sha256Digest,
    deleted_at_millis: u64,
    backend_receipt_digest: Sha256Digest,
    deleted_components: Vec<BackupComponentKind>,
    outcome: DeletionPortOutcome,
}

impl BackupDeletionReceipt {
    /// Builds one complete backend deletion receipt.
    ///
    /// # Errors
    ///
    /// Requires every component exactly once and canonical digests/time.
    pub fn try_new(
        manifest_digest: Sha256Digest,
        decision_digest: Sha256Digest,
        deleted_at_millis: u64,
        backend_receipt_digest: Sha256Digest,
        mut deleted_components: Vec<BackupComponentKind>,
        outcome: DeletionPortOutcome,
    ) -> Result<Self, BackupError> {
        validate_digest(&manifest_digest)?;
        validate_digest(&decision_digest)?;
        validate_digest(&backend_receipt_digest)?;
        if deleted_at_millis == 0 || deleted_at_millis > MAX_SAFE_INTEGER {
            return Err(BackupError::invalid());
        }
        deleted_components.sort_unstable();
        deleted_components.dedup();
        if deleted_components != BackupComponentKind::REQUIRED {
            return Err(BackupError::incomplete());
        }
        Ok(Self {
            manifest_digest,
            decision_digest,
            deleted_at_millis,
            backend_receipt_digest,
            deleted_components,
            outcome,
        })
    }

    #[must_use]
    pub const fn manifest_digest(&self) -> &Sha256Digest {
        &self.manifest_digest
    }

    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        &self.decision_digest
    }

    #[must_use]
    pub const fn deleted_at_millis(&self) -> u64 {
        self.deleted_at_millis
    }

    #[must_use]
    pub const fn backend_receipt_digest(&self) -> &Sha256Digest {
        &self.backend_receipt_digest
    }

    #[must_use]
    pub fn deleted_components(&self) -> &[BackupComponentKind] {
        &self.deleted_components
    }

    #[must_use]
    pub const fn outcome(&self) -> DeletionPortOutcome {
        self.outcome
    }
}

/// Storage boundary that atomically tombstones a complete backup generation.
pub trait BackupDeletionStore {
    /// Deletes or replays deletion of the exact immutable manifest generation.
    ///
    /// # Errors
    ///
    /// Returns a stable error without backend details.
    fn delete_generation(
        &mut self,
        manifest: &BackupManifest,
        permit: &DeletionPermit,
    ) -> Result<BackupDeletionReceipt, BackupDeletionStoreError>;
}

/// Secret-safe backend deletion failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupDeletionStoreError;

impl BackupDeletionStoreError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for BackupDeletionStoreError {
    fn default() -> Self {
        Self::new()
    }
}

/// Sealed proof binding the immutable manifest, policy decision, and backend
/// tombstone receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupDeletionProof {
    manifest_digest: Sha256Digest,
    decision_digest: Sha256Digest,
    rule_version: u64,
    rule_digest: Sha256Digest,
    deleted_at_millis: u64,
    backend_receipt_digest: Sha256Digest,
    proof_digest: Sha256Digest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeletionProofWire {
    format: String,
    manifest_digest: Sha256Digest,
    decision_digest: Sha256Digest,
    rule_version: u64,
    rule_digest: Sha256Digest,
    deleted_at_millis: u64,
    backend_receipt_digest: Sha256Digest,
    proof_digest: Sha256Digest,
}

impl BackupDeletionProof {
    #[must_use]
    pub const fn manifest_digest(&self) -> &Sha256Digest {
        &self.manifest_digest
    }

    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        &self.decision_digest
    }

    #[must_use]
    pub const fn deleted_at_millis(&self) -> u64 {
        self.deleted_at_millis
    }

    #[must_use]
    pub const fn proof_digest(&self) -> &Sha256Digest {
        &self.proof_digest
    }

    /// Encodes the one canonical portable proof representation.
    ///
    /// # Errors
    ///
    /// Returns an integrity error if the proof cannot be encoded.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, BackupError> {
        self.verify()?;
        serde_json::to_vec(&DeletionProofWire {
            format: PROOF_FORMAT.to_owned(),
            manifest_digest: self.manifest_digest.clone(),
            decision_digest: self.decision_digest.clone(),
            rule_version: self.rule_version,
            rule_digest: self.rule_digest.clone(),
            deleted_at_millis: self.deleted_at_millis,
            backend_receipt_digest: self.backend_receipt_digest.clone(),
            proof_digest: self.proof_digest.clone(),
        })
        .map_err(|_| BackupError::integrity())
    }

    /// Decodes and verifies a canonical portable proof.
    ///
    /// # Errors
    ///
    /// Rejects another version, non-canonical bytes, malformed digests, or
    /// changed proof fields.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, BackupError> {
        let wire = serde_json::from_slice::<DeletionProofWire>(bytes)
            .map_err(|_| BackupError::invalid())?;
        if wire.format != PROOF_FORMAT {
            return Err(BackupError::new(crate::BackupErrorKind::UnsupportedVersion));
        }
        validate_digest(&wire.manifest_digest)?;
        validate_digest(&wire.decision_digest)?;
        validate_digest(&wire.rule_digest)?;
        validate_digest(&wire.backend_receipt_digest)?;
        validate_digest(&wire.proof_digest)?;
        if wire.rule_version == 0
            || wire.rule_version > MAX_SAFE_INTEGER
            || wire.deleted_at_millis == 0
            || wire.deleted_at_millis > MAX_SAFE_INTEGER
        {
            return Err(BackupError::invalid());
        }
        let proof = Self {
            manifest_digest: wire.manifest_digest,
            decision_digest: wire.decision_digest,
            rule_version: wire.rule_version,
            rule_digest: wire.rule_digest,
            deleted_at_millis: wire.deleted_at_millis,
            backend_receipt_digest: wire.backend_receipt_digest,
            proof_digest: wire.proof_digest,
        };
        proof.verify()?;
        if proof.encode_canonical()? != bytes {
            return Err(BackupError::integrity());
        }
        Ok(proof)
    }

    /// Recomputes the sealed proof digest.
    ///
    /// # Errors
    ///
    /// Rejects altered proof fields.
    pub fn verify(&self) -> Result<(), BackupError> {
        validate_digest(&self.manifest_digest)?;
        validate_digest(&self.decision_digest)?;
        validate_digest(&self.rule_digest)?;
        validate_digest(&self.backend_receipt_digest)?;
        validate_digest(&self.proof_digest)?;
        if self.rule_version == 0
            || self.rule_version > MAX_SAFE_INTEGER
            || self.deleted_at_millis == 0
            || self.deleted_at_millis > MAX_SAFE_INTEGER
        {
            return Err(BackupError::integrity());
        }
        let expected = proof_digest(
            &self.manifest_digest,
            &self.decision_digest,
            self.rule_version,
            &self.rule_digest,
            self.deleted_at_millis,
            &self.backend_receipt_digest,
        );
        if expected == self.proof_digest {
            Ok(())
        } else {
            Err(BackupError::integrity())
        }
    }
}

/// Outcome from the audited retention/deletion coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupDeletionResult {
    Denied(GovernanceDenial),
    Applied(BackupDeletionProof),
}

/// Coordinates canonical Audit governance with complete-generation deletion.
pub struct BackupRetentionCoordinator;

impl BackupRetentionCoordinator {
    /// Evaluates legal holds and retention, records the immutable Audit
    /// decision, applies complete deletion, then seals a verifiable proof.
    ///
    /// # Errors
    ///
    /// Rejects a fact for another tenant or manifest before Audit/storage.
    /// Audit or storage failure does not produce a proof; exact replay can
    /// continue from the same deterministic policy decision.
    #[allow(clippy::too_many_arguments)]
    pub fn delete(
        authority: &DataGovernanceAuthority,
        manifest: &BackupManifest,
        data: &GovernedDataFact,
        requested_at_millis: u64,
        audit_context: &GovernanceAuditContext,
        audit_store: &mut AuditStore,
        storage: &mut dyn BackupDeletionStore,
    ) -> Result<BackupDeletionResult, BackupError> {
        if data.scope() != manifest.scope() || data.source_digest() != manifest.manifest_digest() {
            return Err(BackupError::tenant());
        }
        let mut port = GovernedBackupPort {
            manifest,
            storage,
            receipt: None,
        };
        let result = authority
            .execute_deletion(
                data,
                requested_at_millis,
                audit_context,
                audit_store,
                &mut port,
            )
            .map_err(|_| BackupError::governance())?;
        match result {
            GovernedDeletionResult::Denied(denial) => Ok(BackupDeletionResult::Denied(denial)),
            GovernedDeletionResult::Applied { permit, outcome } => {
                let receipt = port.receipt.ok_or_else(BackupError::integrity)?;
                validate_receipt(manifest, &permit, outcome, &receipt)?;
                let proof_digest = proof_digest(
                    manifest.manifest_digest(),
                    permit.decision_digest(),
                    permit.rule_version(),
                    permit.rule_digest(),
                    receipt.deleted_at_millis(),
                    receipt.backend_receipt_digest(),
                );
                Ok(BackupDeletionResult::Applied(BackupDeletionProof {
                    manifest_digest: manifest.manifest_digest().clone(),
                    decision_digest: permit.decision_digest().clone(),
                    rule_version: permit.rule_version(),
                    rule_digest: permit.rule_digest().clone(),
                    deleted_at_millis: receipt.deleted_at_millis(),
                    backend_receipt_digest: receipt.backend_receipt_digest().clone(),
                    proof_digest,
                }))
            }
        }
    }
}

struct GovernedBackupPort<'a> {
    manifest: &'a BackupManifest,
    storage: &'a mut dyn BackupDeletionStore,
    receipt: Option<BackupDeletionReceipt>,
}

impl GovernedDeletionPort for GovernedBackupPort<'_> {
    fn delete(
        &mut self,
        permit: &DeletionPermit,
    ) -> Result<DeletionPortOutcome, DeletionPortError> {
        if permit.scope() != self.manifest.scope()
            || permit.source_digest() != self.manifest.manifest_digest()
        {
            return Err(DeletionPortError::new());
        }
        let receipt = self
            .storage
            .delete_generation(self.manifest, permit)
            .map_err(|_| DeletionPortError::new())?;
        let outcome = receipt.outcome();
        self.receipt = Some(receipt);
        Ok(outcome)
    }
}

fn validate_receipt(
    manifest: &BackupManifest,
    permit: &DeletionPermit,
    outcome: DeletionPortOutcome,
    receipt: &BackupDeletionReceipt,
) -> Result<(), BackupError> {
    if receipt.manifest_digest() != manifest.manifest_digest()
        || receipt.decision_digest() != permit.decision_digest()
        || receipt.deleted_at_millis() != permit.requested_at_millis()
        || receipt.outcome() != outcome
        || receipt.deleted_components() != BackupComponentKind::REQUIRED
    {
        return Err(BackupError::integrity());
    }
    Ok(())
}

fn proof_digest(
    manifest_digest: &Sha256Digest,
    decision_digest: &Sha256Digest,
    rule_version: u64,
    rule_digest: &Sha256Digest,
    deleted_at_millis: u64,
    backend_receipt_digest: &Sha256Digest,
) -> Sha256Digest {
    let mut hash = Sha256::new();
    hash.update(PROOF_DOMAIN);
    for value in [
        manifest_digest.0.as_bytes(),
        decision_digest.0.as_bytes(),
        rule_digest.0.as_bytes(),
        backend_receipt_digest.0.as_bytes(),
    ] {
        hash.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hash.update(value);
    }
    hash.update(rule_version.to_be_bytes());
    hash.update(deleted_at_millis.to_be_bytes());
    Sha256Digest(format!("sha256:{:x}", hash.finalize()))
}
