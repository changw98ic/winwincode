// SPDX-License-Identifier: Apache-2.0

//! Shared, durable enforcement mechanics for enterprise Policy boundaries.
//!
//! Boundary modules construct facts only from their existing durable authority.
//! This module derives a separate idempotency identity for every Policy family,
//! evaluates through the sole enterprise Policy ledger, and reduces the audited
//! decision to a closed permit or rejection.

use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, RequestId, Sha256Digest};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyEvaluationError, EnterprisePolicyEvaluationOutcome,
    EnterprisePolicyEvaluationReceipt, EnterprisePolicyKind, EnterprisePolicyMode,
    EnterprisePolicyScope, SqliteStorage,
};

use crate::{
    EnterprisePolicyDecisionClock, EnterprisePolicyEvaluationService,
    EnterprisePolicyEvaluationTarget,
};

const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const POLICY_REQUEST_NAMESPACE: &[u8] = b"winwincode.enterprise-policy-enforcement-request.v1";
const POLICY_SUBJECT_NAMESPACE: &[u8] = b"winwincode.enterprise-policy-enforcement-subject.v1";

/// One immutable boundary evaluation request assembled from durable authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyEnforcementRequest {
    pub actor: EnterprisePolicyActor,
    pub base_request_id: RequestId,
    pub scope: EnterprisePolicyScope,
    pub policy_kind: EnterprisePolicyKind,
    pub resource: String,
    pub subject_sha256: Sha256Digest,
    pub matched_condition_sha256: Vec<Sha256Digest>,
    pub evaluated_at: Instant,
    pub exception_id: Option<winwincode_storage::EnterprisePolicyExceptionId>,
}

/// Audited outcome returned to one execution boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnterprisePolicyEnforcement {
    /// The action is permitted. An audit-mode negative decision remains visible
    /// in the receipt but does not become an execution denial.
    Permit(EnterprisePolicyEvaluationReceipt),
    /// Enforce-mode denial, approval, escalation, or any explicit hard deny.
    Reject(EnterprisePolicyEvaluationReceipt),
}

/// Stable failure returned by an enterprise Policy enforcement boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterprisePolicyEnforcementErrorKind {
    InvalidFacts,
    Evaluation,
}

/// Secret-free enforcement failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyEnforcementError {
    kind: EnterprisePolicyEnforcementErrorKind,
}

impl EnterprisePolicyEnforcementError {
    const fn invalid_facts() -> Self {
        Self {
            kind: EnterprisePolicyEnforcementErrorKind::InvalidFacts,
        }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> EnterprisePolicyEnforcementErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterprisePolicyEnforcementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("enterprise Policy enforcement failed")
    }
}

impl std::error::Error for EnterprisePolicyEnforcementError {}

impl From<EnterprisePolicyEvaluationError> for EnterprisePolicyEnforcementError {
    fn from(_error: EnterprisePolicyEvaluationError) -> Self {
        Self {
            kind: EnterprisePolicyEnforcementErrorKind::Evaluation,
        }
    }
}

impl EnterprisePolicyEnforcement {
    /// Returns the immutable Policy evaluation receipt used by the boundary.
    #[must_use]
    pub const fn receipt(&self) -> &EnterprisePolicyEvaluationReceipt {
        match self {
            Self::Permit(receipt) | Self::Reject(receipt) => receipt,
        }
    }

    /// Returns whether the guarded effect may proceed.
    #[must_use]
    pub const fn is_permitted(&self) -> bool {
        matches!(self, Self::Permit(_))
    }
}

struct FrozenPolicyClock(Instant);

impl EnterprisePolicyDecisionClock for FrozenPolicyClock {
    fn now(&mut self) -> Instant {
        self.0.clone()
    }
}

/// Evaluates and audits one boundary request through the canonical ledger.
///
/// # Errors
///
/// Returns the canonical evaluator error for invalid authority, changed replay,
/// corrupt Policy state, or unavailable durable storage.
pub fn enforce_enterprise_policy(
    storage: &mut SqliteStorage,
    request: &EnterprisePolicyEnforcementRequest,
) -> Result<EnterprisePolicyEnforcement, EnterprisePolicyEnforcementError> {
    let request_id = enforcement_request_id(&request.base_request_id, request.policy_kind);
    let mut conditions = request.matched_condition_sha256.clone();
    conditions.push(enterprise_policy_condition_sha256("all"));
    conditions.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    conditions.dedup();
    let target = EnterprisePolicyEvaluationTarget {
        scope: request.scope.clone(),
        policy_kind: request.policy_kind,
        resource: request.resource.clone(),
        subject_sha256: request.subject_sha256.clone(),
        matched_condition_sha256: conditions,
    };
    let mut clock = FrozenPolicyClock(request.evaluated_at.clone());
    let receipt = EnterprisePolicyEvaluationService::new(storage, &mut clock).evaluate(
        request.actor.clone(),
        request_id,
        &target,
        request.exception_id.clone(),
    )?;
    let decision = &receipt.audit.decision;
    let permitted = decision.outcome == EnterprisePolicyEvaluationOutcome::Allow
        || (decision.policy_mode == Some(EnterprisePolicyMode::Audit) && !decision.hard_invariant);
    Ok(if permitted {
        EnterprisePolicyEnforcement::Permit(receipt)
    } else {
        EnterprisePolicyEnforcement::Reject(receipt)
    })
}

/// Computes the stable condition digest understood by every enforcement adapter.
#[must_use]
pub fn enterprise_policy_condition_sha256(condition: &str) -> Sha256Digest {
    winwincode_execution_port::action_enforcement::policy_condition_sha256(condition)
}

/// Computes a canonical subject digest from secret-free durable facts.
///
/// # Errors
///
/// Returns an error when the facts cannot be represented as canonical JSON.
pub fn enterprise_policy_subject_sha256(
    facts: &impl Serialize,
) -> Result<Sha256Digest, EnterprisePolicyEnforcementError> {
    let bytes =
        serde_json::to_vec(facts).map_err(|_| EnterprisePolicyEnforcementError::invalid_facts())?;
    Ok(namespaced_digest(POLICY_SUBJECT_NAMESPACE, &bytes))
}

fn enforcement_request_id(
    base_request_id: &RequestId,
    policy_kind: EnterprisePolicyKind,
) -> RequestId {
    let encoded = serde_json::to_vec(&(base_request_id, policy_kind))
        .expect("Policy enforcement request identity is always serializable");
    let digest = Sha256::new()
        .chain_update(POLICY_REQUEST_NAMESPACE)
        .chain_update(encoded)
        .finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    RequestId(format!("req_{suffix}"))
}

fn namespaced_digest(namespace: &[u8], value: &[u8]) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::new()
            .chain_update(namespace)
            .chain_update(value)
            .finalize()
    ))
}
