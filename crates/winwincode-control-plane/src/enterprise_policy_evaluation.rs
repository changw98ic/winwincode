// SPDX-License-Identifier: Apache-2.0

//! Trusted-clock application boundary for enterprise Policy decisions and exceptions.

use serde::{Deserialize, Serialize};
use winwincode_domain::{Instant, RequestId, Sha256Digest};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyEvaluation, EnterprisePolicyEvaluationCommand,
    EnterprisePolicyEvaluationError, EnterprisePolicyEvaluationInput,
    EnterprisePolicyEvaluationReceipt, EnterprisePolicyEvaluationRequest,
    EnterprisePolicyExceptionDecision, EnterprisePolicyExceptionDecisionCommand,
    EnterprisePolicyExceptionId, EnterprisePolicyExceptionReceipt,
    EnterprisePolicyExceptionRequest, EnterprisePolicyKind, EnterprisePolicyScope, SqliteStorage,
};

/// Trusted clock used to freeze one deterministic Policy decision cut.
pub trait EnterprisePolicyDecisionClock {
    fn now(&mut self) -> Instant;
}

/// Caller facts that exclude the trusted evaluation time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyEvaluationTarget {
    pub scope: EnterprisePolicyScope,
    pub policy_kind: EnterprisePolicyKind,
    pub resource: String,
    pub subject_sha256: Sha256Digest,
    pub matched_condition_sha256: Vec<Sha256Digest>,
}

impl EnterprisePolicyEvaluationTarget {
    fn at(&self, evaluated_at: Instant) -> EnterprisePolicyEvaluationInput {
        EnterprisePolicyEvaluationInput {
            scope: self.scope.clone(),
            policy_kind: self.policy_kind,
            resource: self.resource.clone(),
            subject_sha256: self.subject_sha256.clone(),
            matched_condition_sha256: self.matched_condition_sha256.clone(),
            evaluated_at,
        }
    }
}

/// Trusted application service backed by the sole durable Policy ledger.
pub struct EnterprisePolicyEvaluationService<'storage, 'clock> {
    storage: &'storage mut SqliteStorage,
    clock: &'clock mut dyn EnterprisePolicyDecisionClock,
}

impl<'storage, 'clock> EnterprisePolicyEvaluationService<'storage, 'clock> {
    #[must_use]
    pub fn new(
        storage: &'storage mut SqliteStorage,
        clock: &'clock mut dyn EnterprisePolicyDecisionClock,
    ) -> Self {
        Self { storage, clock }
    }

    /// Evaluates a Policy without writing an audit or exception record.
    ///
    /// # Errors
    ///
    /// Returns an error when the target, Policy chain, exception seal, or
    /// durable state is invalid or unavailable.
    pub fn dry_run(
        &mut self,
        target: &EnterprisePolicyEvaluationTarget,
        exception_id: Option<EnterprisePolicyExceptionId>,
    ) -> Result<EnterprisePolicyEvaluation, EnterprisePolicyEvaluationError> {
        let request = EnterprisePolicyEvaluationRequest {
            input: target.at(self.clock.now()),
            exception_id,
        };
        self.storage
            .enterprise_policy_evaluation_ledger()?
            .dry_run(&request)
    }

    /// Evaluates a Policy and atomically records one immutable audit receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when the request conflicts, authority is stale, or
    /// durable evaluation cannot complete.
    pub fn evaluate(
        &mut self,
        actor: EnterprisePolicyActor,
        request_id: RequestId,
        target: &EnterprisePolicyEvaluationTarget,
        exception_id: Option<EnterprisePolicyExceptionId>,
    ) -> Result<EnterprisePolicyEvaluationReceipt, EnterprisePolicyEvaluationError> {
        let command = EnterprisePolicyEvaluationCommand {
            request: EnterprisePolicyEvaluationRequest {
                input: target.at(self.clock.now()),
                exception_id,
            },
            actor,
            request_id,
        };
        self.storage
            .enterprise_policy_evaluation_ledger()?
            .evaluate(&command)
    }

    /// Opens one Policy-bound exception request at the trusted current time.
    ///
    /// # Errors
    ///
    /// Returns an error for a hard invariant, invalid expiry, conflicting
    /// request reuse, stale Policy authority, or durable storage failure.
    pub fn request_exception(
        &mut self,
        request: EnterprisePolicyExceptionOpenRequest,
    ) -> Result<EnterprisePolicyExceptionReceipt, EnterprisePolicyEvaluationError> {
        let requested_at = self.clock.now();
        self.storage
            .enterprise_policy_evaluation_ledger()?
            .request_exception(&EnterprisePolicyExceptionRequest {
                exception_id: request.exception_id,
                input: request.target.at(requested_at),
                justification_sha256: request.justification_sha256,
                expires_at: request.expires_at,
                actor: request.actor,
                request_id: request.request_id,
            })
    }

    /// Applies one user decision to an exact exception revision.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-user actor, expired or stale authority,
    /// invalid transition, conflicting replay, or durable storage failure.
    pub fn decide_exception(
        &mut self,
        command: EnterprisePolicyExceptionDecisionRequest,
    ) -> Result<EnterprisePolicyExceptionReceipt, EnterprisePolicyEvaluationError> {
        self.storage
            .enterprise_policy_evaluation_ledger()?
            .decide_exception(&EnterprisePolicyExceptionDecisionCommand {
                exception_id: command.exception_id,
                scope: command.scope,
                expected_revision: command.expected_revision,
                decision: command.decision,
                actor: command.actor,
                request_id: command.request_id,
                decided_at: self.clock.now(),
            })
    }
}

/// Trusted application input for opening an exception.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyExceptionOpenRequest {
    pub exception_id: EnterprisePolicyExceptionId,
    pub target: EnterprisePolicyEvaluationTarget,
    pub justification_sha256: Sha256Digest,
    pub expires_at: Instant,
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
}

/// Trusted application input for deciding an exception revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyExceptionDecisionRequest {
    pub exception_id: EnterprisePolicyExceptionId,
    pub scope: EnterprisePolicyScope,
    pub expected_revision: u64,
    pub decision: EnterprisePolicyExceptionDecision,
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
}
