// SPDX-License-Identifier: Apache-2.0

//! Tamper-evident, secret-safe audit records for the `WinWinCode` Control Plane.
//!
//! The crate accepts only structured identities, digests, stable action/result
//! codes, and a closed local-or-network origin. It has no raw payload, prompt,
//! credential, or diagnostic field. [`AuditStore`] persists one independently
//! ordered hash chain per organization and keeps immutable chain headers after
//! retained payloads expire.

mod event;
mod store;

pub use event::{
    AuditAccess, AuditAction, AuditActionKind, AuditActor, AuditBindingPhase, AuditBindingSource,
    AuditEvent, AuditEventId, AuditExecutionIdentity, AuditExecutionSubjectKind,
    AuditModelInvocation, AuditOrigin, AuditOutcome, AuditRetention, AuditScope, AuditState,
    AuditSubject, AuditSubjectKind,
};
pub use store::{AuditError, AuditErrorKind, AuditPage, AuditRecord, AuditStore};
