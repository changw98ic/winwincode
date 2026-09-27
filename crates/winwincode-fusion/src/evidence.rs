// SPDX-License-Identifier: Apache-2.0

//! Evidence provenance gates for model-led Fusion investigations.
//!
//! Model prose is retained as a hypothesis, while a source receipt records a
//! machine-locatable observation and a claim verification records an
//! independent conclusion about one proposition, scope and version. The
//! ledger deliberately keeps those three layers separate.

use std::collections::{BTreeMap, BTreeSet};

/// The machine source classes accepted by the provenance contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceReceiptKind {
    Command,
    Test,
    Diff,
    File,
    RunEvent,
    IndependentReview,
}

/// The direction of an independently checked claim verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationConclusion {
    Support,
    Counter,
}

impl VerificationConclusion {
    /// Compatibility spelling used by callers that speak in claim-position terms.
    #[allow(non_upper_case_globals)]
    pub const Supports: Self = Self::Support;
    /// Compatibility spelling for a checked counter conclusion.
    #[allow(non_upper_case_globals)]
    pub const Refuted: Self = Self::Counter;
}

/// A model-authored assertion kept separate from machine-produced evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelClaim {
    pub claim_id: String,
    pub proposition: String,
    pub scope: String,
    pub version: String,
    pub explanation: String,
    pub source_receipt_ids: Vec<String>,
}

/// The exact target checked by a claim verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimTarget {
    pub proposition: String,
    pub scope: String,
    pub version: String,
}

/// A machine source receipt. It proves a source exists, not a proposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceReceipt {
    pub id: String,
    pub kind: SourceReceiptKind,
    pub locator: String,
    pub version: String,
    pub execution_owner: String,
    pub content_digest: String,
}

/// An independent verification result for one exact claim target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimVerification {
    pub id: String,
    pub proposition: String,
    pub scope: String,
    pub version: String,
    pub conclusion: VerificationConclusion,
    pub source_receipt_ids: Vec<String>,
    pub verifier: String,
}

/// The result of checking a model claim against registered source receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelClaimAdmission {
    pub claim_id: String,
    pub model_explanation: String,
    pub produced_new_evidence: bool,
    pub resolved_source_receipt_ids: Vec<String>,
    pub rejected_source_receipt_ids: Vec<String>,
    pub validated_verification_ids: Vec<String>,
}

/// Errors are explicit so an invalid or unbound receipt can never become evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceError {
    InvalidReceipt(String),
    DuplicateReceipt(String),
    UnknownReceipt(String),
    InvalidVerification(String),
    DuplicateVerification(String),
    ReceiptVersionMismatch(String),
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidReceipt(value)
            | Self::DuplicateReceipt(value)
            | Self::UnknownReceipt(value)
            | Self::InvalidVerification(value)
            | Self::DuplicateVerification(value)
            | Self::ReceiptVersionMismatch(value) => f.write_str(value),
        }
    }
}

impl std::error::Error for EvidenceError {}

/// A target can be supplied as a model claim or as a plain verification target.
pub trait ClaimTargetLike {
    fn target(&self) -> ClaimTarget;
}

impl ClaimTargetLike for ModelClaim {
    fn target(&self) -> ClaimTarget {
        ClaimTarget {
            proposition: self.proposition.clone(),
            scope: self.scope.clone(),
            version: self.version.clone(),
        }
    }
}

impl ClaimTargetLike for ClaimTarget {
    fn target(&self) -> ClaimTarget {
        self.clone()
    }
}

/// Source receipts, independent verifications and increment accounting.
#[derive(Debug, Default)]
pub struct EvidenceLedger {
    receipts: BTreeMap<String, SourceReceipt>,
    invalid_receipts: BTreeSet<String>,
    verifications: BTreeMap<String, ClaimVerification>,
    consumed_content_digests: BTreeSet<String>,
}

impl EvidenceLedger {
    /// Register a machine receipt after checking its immutable identity bindings.
    ///
    /// # Errors
    /// Rejects malformed receipts and duplicate receipt identities.
    pub fn register_source_receipt(
        &mut self,
        receipt: SourceReceipt,
    ) -> Result<SourceReceipt, EvidenceError> {
        validate_receipt(&receipt)?;
        if self.receipts.contains_key(&receipt.id) {
            return Err(EvidenceError::DuplicateReceipt(receipt.id));
        }
        let registered = receipt.clone();
        self.receipts.insert(registered.id.clone(), receipt);
        Ok(registered)
    }

    /// Mark a receipt unavailable. Existing derived support/counter status is
    /// recomputed from live receipts and therefore becomes invalid automatically.
    pub fn invalidate_source_receipt(&mut self, receipt_id: &str) -> bool {
        if self.receipts.contains_key(receipt_id) {
            self.invalid_receipts.insert(receipt_id.to_owned())
        } else {
            false
        }
    }

    /// Record an independent verification. A model claim cannot call this with
    /// a prose-only explanation: all cited receipts must already be machine facts.
    ///
    /// # Errors
    /// Rejects invalid or duplicate verifications and missing or mismatched receipts.
    pub fn record_claim_verification(
        &mut self,
        verification: ClaimVerification,
    ) -> Result<ClaimVerification, EvidenceError> {
        validate_verification(&verification)?;
        if self.verifications.contains_key(&verification.id) {
            return Err(EvidenceError::DuplicateVerification(verification.id));
        }
        for receipt_id in &verification.source_receipt_ids {
            let receipt = self
                .receipts
                .get(receipt_id)
                .filter(|receipt| !self.invalid_receipts.contains(&receipt.id))
                .ok_or_else(|| EvidenceError::UnknownReceipt(receipt_id.clone()))?;
            if receipt.version != verification.version {
                return Err(EvidenceError::ReceiptVersionMismatch(receipt_id.clone()));
            }
        }
        let registered = verification.clone();
        self.verifications
            .insert(registered.id.clone(), verification);
        Ok(registered)
    }

    /// Resolve a model claim. Prose and unknown receipt IDs remain auditable but
    /// never satisfy `produced_new_evidence`.
    pub fn admit_model_claim(&mut self, claim: ModelClaim) -> ModelClaimAdmission {
        let mut resolved = Vec::new();
        let mut rejected = Vec::new();
        for receipt_id in &claim.source_receipt_ids {
            if self.receipts.get(receipt_id).is_some_and(|receipt| {
                !self.invalid_receipts.contains(&receipt.id) && receipt.version == claim.version
            }) {
                resolved.push(receipt_id.clone());
            } else {
                rejected.push(receipt_id.clone());
            }
        }

        let mut validated_verification_ids = Vec::new();
        let mut produced_new_evidence = false;
        for verification in self.verifications.values() {
            if !same_target(
                &verification.proposition,
                &verification.scope,
                &verification.version,
                &claim.proposition,
                &claim.scope,
                &claim.version,
            ) {
                continue;
            }
            if verification.source_receipt_ids.is_empty()
                || !verification
                    .source_receipt_ids
                    .iter()
                    .all(|id| resolved.contains(id))
            {
                continue;
            }
            validated_verification_ids.push(verification.id.clone());
            for receipt_id in &verification.source_receipt_ids {
                let Some(receipt) = self.receipts.get(receipt_id) else {
                    continue;
                };
                // Content identity is global: changing a receipt ID, source
                // label, claim wording, or verification wrapper does not make
                // the same output a new observation.
                let content_identity = receipt.content_digest.to_ascii_lowercase();
                if self.consumed_content_digests.insert(content_identity) {
                    produced_new_evidence = true;
                }
            }
        }

        ModelClaimAdmission {
            claim_id: claim.claim_id,
            model_explanation: claim.explanation,
            produced_new_evidence,
            resolved_source_receipt_ids: resolved,
            rejected_source_receipt_ids: rejected,
            validated_verification_ids,
        }
    }

    /// A valid counter must match the target exactly and cite live machine facts.
    pub fn has_valid_counter<T: ClaimTargetLike>(&self, target: &T) -> bool {
        let target = target.target();
        self.verifications.values().any(|verification| {
            verification.conclusion == VerificationConclusion::Counter
                && same_target(
                    &verification.proposition,
                    &verification.scope,
                    &verification.version,
                    &target.proposition,
                    &target.scope,
                    &target.version,
                )
                && !verification.source_receipt_ids.is_empty()
                && verification.source_receipt_ids.iter().all(|id| {
                    self.receipts
                        .get(id)
                        .filter(|receipt| !self.invalid_receipts.contains(&receipt.id))
                        .is_some_and(|receipt| receipt.version == verification.version)
                })
        })
    }

    /// Alias used by state transitions; a model opinion alone never passes it.
    pub fn can_refute<T: ClaimTargetLike>(&self, target: &T) -> bool {
        self.has_valid_counter(target)
    }

    /// Exact support lookup used by confirmation gates. Scope and version are
    /// part of the target and are never inferred from a majority or model text.
    pub fn has_verified_support<T: ClaimTargetLike>(&self, target: &T) -> bool {
        let target = target.target();
        self.verifications.values().any(|verification| {
            verification.conclusion == VerificationConclusion::Support
                && same_target(
                    &verification.proposition,
                    &verification.scope,
                    &verification.version,
                    &target.proposition,
                    &target.scope,
                    &target.version,
                )
                && self.verification_has_live_receipts(verification)
        })
    }

    fn verification_has_live_receipts(&self, verification: &ClaimVerification) -> bool {
        !verification.source_receipt_ids.is_empty()
            && verification.source_receipt_ids.iter().all(|id| {
                self.receipts
                    .get(id)
                    .filter(|receipt| !self.invalid_receipts.contains(&receipt.id))
                    .is_some_and(|receipt| receipt.version == verification.version)
            })
    }
}

fn validate_receipt(receipt: &SourceReceipt) -> Result<(), EvidenceError> {
    let invalid =
        |field: &str| EvidenceError::InvalidReceipt(format!("invalid source receipt {field}"));
    if receipt.id.trim().is_empty() {
        return Err(invalid("id"));
    }
    if receipt.locator.trim().is_empty() {
        return Err(invalid("locator"));
    }
    if receipt.version.trim().is_empty() {
        return Err(invalid("version"));
    }
    if receipt.execution_owner.trim().is_empty() {
        return Err(invalid("execution_owner"));
    }
    if !is_sha256_digest(&receipt.content_digest) {
        return Err(invalid("content_digest"));
    }
    Ok(())
}

fn validate_verification(verification: &ClaimVerification) -> Result<(), EvidenceError> {
    let invalid = |field: &str| {
        EvidenceError::InvalidVerification(format!("invalid claim verification {field}"))
    };
    if verification.id.trim().is_empty() {
        return Err(invalid("id"));
    }
    if verification.proposition.trim().is_empty()
        || verification.scope.trim().is_empty()
        || verification.version.trim().is_empty()
    {
        return Err(invalid("target"));
    }
    if verification.verifier.trim().is_empty() {
        return Err(invalid("verifier"));
    }
    if verification.source_receipt_ids.is_empty() {
        return Err(invalid("source_receipt_ids"));
    }
    Ok(())
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn same_target(
    proposition: &str,
    scope: &str,
    version: &str,
    other_proposition: &str,
    other_scope: &str,
    other_version: &str,
) -> bool {
    normalize(proposition) == normalize(other_proposition)
        && normalize(scope) == normalize(other_scope)
        && normalize(version) == normalize(other_version)
}
