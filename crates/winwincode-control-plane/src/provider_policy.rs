// SPDX-License-Identifier: Apache-2.0

//! Enterprise Model and Provider Policy enforcement over durable retry facts.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyKind, EnterprisePolicyScope, ProductStateStorage,
    SqliteStorage,
};

use crate::{
    EnterprisePolicyEnforcement, EnterprisePolicyEnforcementError,
    EnterprisePolicyEnforcementRequest, ModelRetrySettlementContext, enforce_enterprise_policy,
    enterprise_policy_condition_sha256, enterprise_policy_subject_sha256,
};

/// Stable Provider Policy enforcement failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderPolicyErrorKind {
    Rejected,
    Unavailable,
}

/// Bounded Provider Policy error which retains no model input or Credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPolicyError {
    kind: ProviderPolicyErrorKind,
}

impl ProviderPolicyError {
    const fn new(kind: ProviderPolicyErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> ProviderPolicyErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider enterprise Policy enforcement failed")
    }
}

impl std::error::Error for ProviderPolicyError {}

/// The two version-bound receipts required before one Provider side effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPolicyReceipt {
    model: EnterprisePolicyEnforcement,
    provider: EnterprisePolicyEnforcement,
}

impl ProviderPolicyReceipt {
    /// Returns the audited Model Policy decision.
    #[must_use]
    pub const fn model(&self) -> &EnterprisePolicyEnforcement {
        &self.model
    }

    /// Returns the audited Provider Policy decision.
    #[must_use]
    pub const fn provider(&self) -> &EnterprisePolicyEnforcement {
        &self.provider
    }
}

/// Production adapter backed by the canonical product database.
pub struct DurableProviderPolicyEnforcement {
    storage: SqliteStorage,
    database_path: PathBuf,
}

impl DurableProviderPolicyEnforcement {
    /// Opens the sole enterprise Policy ledger in the product database.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the database cannot be opened.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, ProviderPolicyError> {
        let mut storage = SqliteStorage::open(data_directory)
            .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?;
        storage
            .enterprise_policy_ledger()
            .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?;
        storage
            .enterprise_policy_evaluation_ledger()
            .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))?;
        let database_path = storage.database_path().to_path_buf();
        Ok(Self {
            storage,
            database_path,
        })
    }

    /// Returns the exact canonical database path for composition checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Enforces Model then Provider Policy using only the frozen retry context.
    ///
    /// # Errors
    ///
    /// Returns `Rejected` for an enforced negative decision and `Unavailable`
    /// for invalid, conflicting, corrupt, or unreadable Policy authority.
    pub fn enforce(
        &mut self,
        context: &ModelRetrySettlementContext,
    ) -> Result<ProviderPolicyReceipt, ProviderPolicyError> {
        let quota = context.enterprise_quota_request();
        let attribution = &quota.attribution;
        let scope = EnterprisePolicyScope::Repository {
            organization_id: attribution.organization_id.clone(),
            workspace_id: attribution.workspace_id.clone(),
            project_id: attribution.project_id.clone(),
            repository_id: attribution.repository_id.clone(),
        };
        let actor = EnterprisePolicyActor::User {
            id: attribution.user_id.clone(),
        };
        let start = context.start_receipt();
        let subject_sha256 = enterprise_policy_subject_sha256(&(
            context.context_fingerprint(),
            &start.model_exchange_id,
            start.attempt,
            &start.route_fingerprint,
        ))
        .map_err(unavailable)?;
        let base = EnterprisePolicyEnforcementRequest {
            actor,
            base_request_id: context.request().request_id.clone(),
            scope,
            policy_kind: EnterprisePolicyKind::Model,
            resource: format!("model:{}/{}", start.provider_id, start.model_id),
            subject_sha256,
            matched_condition_sha256: vec![enterprise_policy_condition_sha256(
                "provider-route-authority",
            )],
            evaluated_at: quota.requested_at.clone(),
            exception_id: None,
        };
        let model = self.evaluate(&base)?;
        let provider = self.evaluate(&EnterprisePolicyEnforcementRequest {
            policy_kind: EnterprisePolicyKind::Provider,
            resource: format!("provider:{}", start.provider_id),
            ..base
        })?;
        Ok(ProviderPolicyReceipt { model, provider })
    }

    /// Closes the owned Policy database connection.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the database cannot be closed cleanly.
    pub fn close(self) -> Result<(), ProviderPolicyError> {
        Box::new(self.storage)
            .close()
            .map_err(|_| ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable))
    }

    fn evaluate(
        &mut self,
        request: &EnterprisePolicyEnforcementRequest,
    ) -> Result<EnterprisePolicyEnforcement, ProviderPolicyError> {
        let decision =
            enforce_enterprise_policy(&mut self.storage, request).map_err(unavailable)?;
        if decision.is_permitted() {
            Ok(decision)
        } else {
            Err(ProviderPolicyError::new(ProviderPolicyErrorKind::Rejected))
        }
    }
}

fn unavailable(_error: EnterprisePolicyEnforcementError) -> ProviderPolicyError {
    ProviderPolicyError::new(ProviderPolicyErrorKind::Unavailable)
}
