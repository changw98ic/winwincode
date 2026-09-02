// SPDX-License-Identifier: Apache-2.0

//! Enterprise Policy enforcement for authenticated Worker placement and verifier facts.

use std::{fmt, path::Path};

use serde::Serialize;
use winwincode_domain::{
    Instant, OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyKind, EnterprisePolicyScope, ProductStateStorage,
    SqliteStorage,
};

use crate::{
    EnterprisePolicyEnforcement, EnterprisePolicyEnforcementError,
    EnterprisePolicyEnforcementRequest, WorkerEnterpriseQuotaAuthority, enforce_enterprise_policy,
    enterprise_policy_condition_sha256, enterprise_policy_subject_sha256,
};

/// Stable Worker/Verifier Policy failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPolicyErrorKind {
    Rejected,
    Unavailable,
}

/// Secret-free Policy failure at a Worker execution boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPolicyError {
    kind: WorkerPolicyErrorKind,
}

impl WorkerPolicyError {
    const fn new(kind: WorkerPolicyErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> WorkerPolicyErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Worker enterprise Policy enforcement failed")
    }
}

impl std::error::Error for WorkerPolicyError {}

/// Trusted, immutable verifier facts assembled from the durable execution Job
/// and the validated Worker terminal message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct VerifierPolicyAuthority {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub user_id: UserId,
    pub request_id: RequestId,
    pub evaluated_at: Instant,
    pub verifier_resource: String,
    pub subject_sha256: Sha256Digest,
}

/// Production adapter backed by the canonical product database.
pub struct DurableWorkerPolicyEnforcement {
    storage: SqliteStorage,
}

impl DurableWorkerPolicyEnforcement {
    /// Opens the sole enterprise Policy ledger in the product database.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the product database cannot be opened.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, WorkerPolicyError> {
        let mut storage = SqliteStorage::open(data_directory)
            .map_err(|_| WorkerPolicyError::new(WorkerPolicyErrorKind::Unavailable))?;
        storage
            .enterprise_policy_ledger()
            .map_err(|_| WorkerPolicyError::new(WorkerPolicyErrorKind::Unavailable))?;
        storage
            .enterprise_policy_evaluation_ledger()
            .map_err(|_| WorkerPolicyError::new(WorkerPolicyErrorKind::Unavailable))?;
        Ok(Self { storage })
    }

    /// Enforces Worker Placement Policy from the exact durable claim join.
    ///
    /// # Errors
    ///
    /// Returns `Rejected` for an enforced negative decision and `Unavailable`
    /// for invalid, conflicting, corrupt, or unreadable Policy authority.
    pub fn enforce_placement(
        &mut self,
        authority: &WorkerEnterpriseQuotaAuthority,
    ) -> Result<EnterprisePolicyEnforcement, WorkerPolicyError> {
        let admission = authority.admission();
        let subject_sha256 = enterprise_policy_subject_sha256(&(
            authority.job(),
            authority.placement(),
            authority.claim(),
        ))
        .map_err(unavailable)?;
        self.evaluate(&EnterprisePolicyEnforcementRequest {
            actor: EnterprisePolicyActor::User {
                id: admission.user_id.clone(),
            },
            base_request_id: authority.claim().request_id.clone(),
            scope: EnterprisePolicyScope::Repository {
                organization_id: admission.scope.organization_id.clone(),
                workspace_id: admission.scope.workspace_id.clone(),
                project_id: admission.scope.project_id.clone(),
                repository_id: admission.scope.repository_id.clone(),
            },
            policy_kind: EnterprisePolicyKind::WorkerPlacement,
            resource: format!(
                "worker-placement:{}",
                authority.placement().worker_pool_id.0
            ),
            subject_sha256,
            matched_condition_sha256: vec![enterprise_policy_condition_sha256(
                "authenticated-placement",
            )],
            evaluated_at: authority.claim().issued_at.clone(),
            exception_id: None,
        })
    }

    /// Enforces Verifier Policy from an exact terminal authority assembled by
    /// the terminal transaction boundary.
    ///
    /// # Errors
    ///
    /// Returns `Rejected` for an enforced negative decision and `Unavailable`
    /// for invalid, conflicting, corrupt, or unreadable Policy authority.
    pub(crate) fn enforce_verifier(
        &mut self,
        authority: &VerifierPolicyAuthority,
    ) -> Result<EnterprisePolicyEnforcement, WorkerPolicyError> {
        self.evaluate(&EnterprisePolicyEnforcementRequest {
            actor: EnterprisePolicyActor::User {
                id: authority.user_id.clone(),
            },
            base_request_id: authority.request_id.clone(),
            scope: EnterprisePolicyScope::Repository {
                organization_id: authority.organization_id.clone(),
                workspace_id: authority.workspace_id.clone(),
                project_id: authority.project_id.clone(),
                repository_id: authority.repository_id.clone(),
            },
            policy_kind: EnterprisePolicyKind::Verifier,
            resource: authority.verifier_resource.clone(),
            subject_sha256: authority.subject_sha256.clone(),
            matched_condition_sha256: vec![enterprise_policy_condition_sha256(
                "verified-terminal-authority",
            )],
            evaluated_at: authority.evaluated_at.clone(),
            exception_id: None,
        })
    }

    /// Closes the owned Policy database connection.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the connection cannot be closed cleanly.
    pub fn close(self) -> Result<(), WorkerPolicyError> {
        Box::new(self.storage)
            .close()
            .map_err(|_| WorkerPolicyError::new(WorkerPolicyErrorKind::Unavailable))
    }

    fn evaluate(
        &mut self,
        request: &EnterprisePolicyEnforcementRequest,
    ) -> Result<EnterprisePolicyEnforcement, WorkerPolicyError> {
        let decision =
            enforce_enterprise_policy(&mut self.storage, request).map_err(unavailable)?;
        if decision.is_permitted() {
            Ok(decision)
        } else {
            Err(WorkerPolicyError::new(WorkerPolicyErrorKind::Rejected))
        }
    }
}

fn unavailable(_error: EnterprisePolicyEnforcementError) -> WorkerPolicyError {
    WorkerPolicyError::new(WorkerPolicyErrorKind::Unavailable)
}
