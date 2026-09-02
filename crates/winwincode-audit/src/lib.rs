// SPDX-License-Identifier: Apache-2.0

//! Tamper-evident, secret-safe audit records for the `WinWinCode` Control Plane.
//!
//! The crate accepts only structured identities, digests, stable action/result
//! codes, and a closed local-or-network origin. It has no raw payload, prompt,
//! credential, or diagnostic field. [`AuditStore`] persists one independently
//! ordered hash chain per organization and keeps immutable chain headers after
//! retained payloads expire.

mod event;
mod export;
mod governance;
mod store;

pub use event::{
    AuditAccess, AuditAction, AuditActionKind, AuditActor, AuditBindingPhase, AuditBindingSource,
    AuditEvent, AuditEventId, AuditExecutionIdentity, AuditExecutionSubjectKind,
    AuditModelInvocation, AuditOrigin, AuditOutcome, AuditRetention, AuditScope, AuditState,
    AuditSubject, AuditSubjectKind,
};
pub use export::{
    AuditArtifactDigestKind, AuditArtifactDigestReference, AuditDeletionProof, AuditExportContent,
    AuditExportCursor, AuditExportError, AuditExportErrorKind, AuditExportHeader,
    AuditExportLimits, AuditExportManifest, AuditExportPage, AuditExportPolicyProof,
    AuditExportQuery, AuditExportRecord, AuditExportTimeRange, AuditExportVerificationState,
    AuditExportVerifier, AuditSubjectFilter,
};
pub use governance::{
    ClassificationRule, DataClassification, DataGovernanceAuthority, DeletionDecision,
    DeletionPermit, DeletionPortError, DeletionPortOutcome, GovernanceAuditContext,
    GovernanceDenial, GovernanceError, GovernanceErrorKind, GovernedDataFact, GovernedDeletionPort,
    GovernedDeletionResult, LegalHold, LegalHoldId, PlacementDecision, PlacementPermit,
    RedactionPlan, RedactionStrategy, ResidencyRegion, RetentionPlan, RetentionRequirement,
};
pub use store::{
    AuditChainCheckpoint, AuditError, AuditErrorKind, AuditPage, AuditRecord, AuditStore,
};
