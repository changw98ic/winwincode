// SPDX-License-Identifier: Apache-2.0

//! Enterprise Publication Policy enforcement before Credential resolution.

use std::{fmt, path::Path};

use winwincode_api::generated::PublicationPublishCommand;
use winwincode_publication::{PublicationAuthorization, PublicationEnterpriseAttribution};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyKind, EnterprisePolicyScope, ProductStateStorage,
    SqliteStorage,
};

use crate::{
    EnterprisePolicyEnforcement, EnterprisePolicyEnforcementRequest, enforce_enterprise_policy,
    enterprise_policy_condition_sha256, enterprise_policy_subject_sha256,
    publication_enterprise_quota::publication_quota_requested_at,
};

/// Stable Publication Policy enforcement failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationEnterprisePolicyErrorKind {
    Rejected,
    Unavailable,
}

/// Secret-free Publication Policy failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationEnterprisePolicyError {
    kind: PublicationEnterprisePolicyErrorKind,
}

impl PublicationEnterprisePolicyError {
    const fn new(kind: PublicationEnterprisePolicyErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> PublicationEnterprisePolicyErrorKind {
        self.kind
    }
}

impl fmt::Display for PublicationEnterprisePolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Publication enterprise Policy enforcement failed")
    }
}

impl std::error::Error for PublicationEnterprisePolicyError {}

/// Production adapter backed by the canonical product database.
pub struct DurablePublicationPolicyEnforcement {
    storage: SqliteStorage,
}

impl DurablePublicationPolicyEnforcement {
    /// Opens the sole enterprise Policy ledger in the product database.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the database cannot be opened.
    pub fn open(
        data_directory: impl AsRef<Path>,
    ) -> Result<Self, PublicationEnterprisePolicyError> {
        let mut storage = SqliteStorage::open(data_directory).map_err(|_| unavailable())?;
        storage
            .enterprise_policy_ledger()
            .map_err(|_| unavailable())?;
        storage
            .enterprise_policy_evaluation_ledger()
            .map_err(|_| unavailable())?;
        Ok(Self { storage })
    }

    /// Enforces Publication Policy from resolved, immutable candidate and
    /// approval authority. This method performs no Credential or provider read.
    ///
    /// # Errors
    ///
    /// Returns `Rejected` for an enforced negative decision and `Unavailable`
    /// for invalid, conflicting, corrupt, or unreadable Policy authority.
    pub fn enforce(
        &mut self,
        command: &PublicationPublishCommand,
        authorization: &PublicationAuthorization,
        attribution: &PublicationEnterpriseAttribution,
    ) -> Result<EnterprisePolicyEnforcement, PublicationEnterprisePolicyError> {
        let evaluated_at = publication_quota_requested_at(authorization.approved_at_millis())
            .map_err(|_| unavailable())?;
        let subject_sha256 = enterprise_policy_subject_sha256(&(
            &command.payload.publication_id,
            &command.payload.delivery_id,
            &command.payload.candidate_digest,
            &command.payload.target,
            authorization.candidate_digest(),
            authorization.artifact_digest(),
            authorization.repository_scope_sha256(),
            attribution,
        ))
        .map_err(|_| unavailable())?;
        let decision = enforce_enterprise_policy(
            &mut self.storage,
            &EnterprisePolicyEnforcementRequest {
                actor: EnterprisePolicyActor::User {
                    id: attribution.user_id().clone(),
                },
                base_request_id: command.request_id.clone(),
                scope: EnterprisePolicyScope::Repository {
                    organization_id: attribution.organization_id().clone(),
                    workspace_id: attribution.workspace_id().clone(),
                    project_id: attribution.project_id().clone(),
                    repository_id: attribution.repository_id().clone(),
                },
                policy_kind: EnterprisePolicyKind::Publication,
                resource: format!("publication:github:{}", command.payload.target.repository.0),
                subject_sha256,
                matched_condition_sha256: vec![enterprise_policy_condition_sha256(
                    "approved-publication-authority",
                )],
                evaluated_at,
                exception_id: None,
            },
        )
        .map_err(|_| unavailable())?;
        if decision.is_permitted() {
            Ok(decision)
        } else {
            Err(PublicationEnterprisePolicyError::new(
                PublicationEnterprisePolicyErrorKind::Rejected,
            ))
        }
    }

    /// Closes the owned Policy database connection.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the connection cannot be closed cleanly.
    pub fn close(self) -> Result<(), PublicationEnterprisePolicyError> {
        Box::new(self.storage).close().map_err(|_| unavailable())
    }
}

const fn unavailable() -> PublicationEnterprisePolicyError {
    PublicationEnterprisePolicyError::new(PublicationEnterprisePolicyErrorKind::Unavailable)
}
