// SPDX-License-Identifier: Apache-2.0

//! Production model-policy snapshot boundary.
//!
//! Policy lookup is deliberately keyed only by organization and the exact
//! already-resolved Provider route. User and session preferences cannot enter
//! this boundary, so they cannot widen the configured policy.

use std::fmt;

use winwincode_domain::OrganizationId;

use crate::{
    FrozenModelAdmissionPolicy, FrozenModelRouteAuthority, ModelAdmissionError,
    ModelAdmissionPolicyLayer,
};

/// Secret-free key sent to the external policy authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPolicyRouteKey {
    organization_id: OrganizationId,
    provider_id: String,
    model_id: String,
    catalog_version: u64,
    provider_version: u64,
    credential_rotation_version: u64,
    route_authority_fingerprint: String,
}

impl ModelPolicyRouteKey {
    /// Builds the only policy lookup key from a verified route authority.
    ///
    /// # Errors
    ///
    /// Rejects incomplete route/version authority.
    pub fn try_from_authority(
        authority: &FrozenModelRouteAuthority,
    ) -> Result<Self, ModelPolicyResolutionError> {
        if authority.catalog_version() == 0
            || authority.provider_version() == 0
            || authority.credential_rotation_version() == 0
            || authority.fingerprint().is_empty()
        {
            return Err(ModelPolicyResolutionError::invalid());
        }
        Ok(Self {
            organization_id: authority.route_key().organization_id().clone(),
            provider_id: authority.route().provider_id.clone(),
            model_id: authority.route().model_id.clone(),
            catalog_version: authority.catalog_version(),
            provider_version: authority.provider_version(),
            credential_rotation_version: authority.credential_rotation_version(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
        })
    }

    #[must_use]
    pub const fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    #[must_use]
    pub const fn catalog_version(&self) -> u64 {
        self.catalog_version
    }

    #[must_use]
    pub const fn provider_version(&self) -> u64 {
        self.provider_version
    }

    #[must_use]
    pub const fn credential_rotation_version(&self) -> u64 {
        self.credential_rotation_version
    }

    #[must_use]
    pub fn route_authority_fingerprint(&self) -> &str {
        &self.route_authority_fingerprint
    }
}

/// One atomic Community policy authority result.
/// Construction freezes the layer immediately, retaining its revision and
/// decision as auditable sources.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelPolicyAuthoritySnapshot {
    key: ModelPolicyRouteKey,
    policy: FrozenModelAdmissionPolicy,
}

impl ModelPolicyAuthoritySnapshot {
    /// Freezes one policy authority response for the exact requested route.
    ///
    /// # Errors
    ///
    /// Rejects invalid policy layers or a mismatched budget period.
    pub fn freeze(
        key: ModelPolicyRouteKey,
        base: ModelAdmissionPolicyLayer,
    ) -> Result<Self, ModelPolicyResolutionError> {
        let policy = FrozenModelAdmissionPolicy::freeze(base)
            .map_err(|error| map_admission_error(&error))?;
        Ok(Self { key, policy })
    }

    #[must_use]
    pub const fn key(&self) -> &ModelPolicyRouteKey {
        &self.key
    }

    #[must_use]
    pub const fn policy(&self) -> &FrozenModelAdmissionPolicy {
        &self.policy
    }
}

/// Stable failure returned by the external policy authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelPolicyAuthorityError;

impl ModelPolicyAuthorityError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self
    }
}

impl fmt::Display for ModelPolicyAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("model policy authority is unavailable")
    }
}

impl std::error::Error for ModelPolicyAuthorityError {}

/// External organization policy authority. There is no personal or session
/// policy input.
pub trait ModelPolicyAuthorityPort: Send + Sync {
    /// Resolves the exact organization/route/version key.
    ///
    /// # Errors
    ///
    /// Returns a stable dependency error without copying authority diagnostics.
    fn snapshot(
        &self,
        key: &ModelPolicyRouteKey,
    ) -> Result<ModelPolicyAuthoritySnapshot, ModelPolicyAuthorityError>;
}

/// Immutable local production configuration.
#[derive(Clone, Debug)]
pub struct LocalModelPolicyAuthorityConfig {
    pub base: ModelAdmissionPolicyLayer,
}

/// Local production adapter for deployments whose audited policy snapshots
/// are supplied in immutable host configuration. The adapter owns no billing
/// ledger and never accepts user or session overrides.
pub struct LocalModelPolicyAuthority {
    base: ModelAdmissionPolicyLayer,
}

impl LocalModelPolicyAuthority {
    /// Validates the configured policy at startup.
    ///
    /// # Errors
    ///
    /// Rejects duplicate organizations or invalid layer intersections.
    pub fn try_new(
        config: LocalModelPolicyAuthorityConfig,
    ) -> Result<Self, ModelPolicyResolutionError> {
        FrozenModelAdmissionPolicy::freeze(config.base.clone())
            .map_err(|error| map_admission_error(&error))?;
        Ok(Self { base: config.base })
    }
}

impl ModelPolicyAuthorityPort for LocalModelPolicyAuthority {
    fn snapshot(
        &self,
        key: &ModelPolicyRouteKey,
    ) -> Result<ModelPolicyAuthoritySnapshot, ModelPolicyAuthorityError> {
        ModelPolicyAuthoritySnapshot::freeze(key.clone(), self.base.clone())
            .map_err(|_| ModelPolicyAuthorityError::unavailable())
    }
}

impl fmt::Debug for LocalModelPolicyAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalModelPolicyAuthority")
            .field("base", &"<validated-policy-layer>")
            .finish()
    }
}

/// Verified production policy resolution used by durable admission.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelPolicyResolution {
    key: ModelPolicyRouteKey,
    policy: FrozenModelAdmissionPolicy,
}

impl ModelPolicyResolution {
    #[must_use]
    pub const fn key(&self) -> &ModelPolicyRouteKey {
        &self.key
    }

    #[must_use]
    pub const fn policy(&self) -> &FrozenModelAdmissionPolicy {
        &self.policy
    }
}

/// Stable production policy-resolution failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelPolicyResolutionErrorKind {
    InvalidAuthority,
    SnapshotMismatch,
    AuthorityUnavailable,
}

/// Bounded policy-source error containing no route payload or upstream text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPolicyResolutionError {
    kind: ModelPolicyResolutionErrorKind,
    message: &'static str,
}

impl ModelPolicyResolutionError {
    const fn new(kind: ModelPolicyResolutionErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ModelPolicyResolutionErrorKind::InvalidAuthority,
            "model policy route authority is invalid",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> ModelPolicyResolutionErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelPolicyResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ModelPolicyResolutionError {}

/// Production freezer joining the exact resolved route with one atomic policy
/// authority snapshot.
pub struct ProductionModelPolicySource<'authority> {
    authority: &'authority dyn ModelPolicyAuthorityPort,
}

impl<'authority> ProductionModelPolicySource<'authority> {
    #[must_use]
    pub const fn new(authority: &'authority dyn ModelPolicyAuthorityPort) -> Self {
        Self { authority }
    }

    /// Resolves and verifies the immutable policy used by one reservation.
    ///
    /// # Errors
    ///
    /// Fails closed when the authority is unavailable or returns another
    /// organization, route, catalog, Provider, Credential, or route revision.
    pub fn resolve(
        &self,
        route_authority: &FrozenModelRouteAuthority,
    ) -> Result<ModelPolicyResolution, ModelPolicyResolutionError> {
        let key = ModelPolicyRouteKey::try_from_authority(route_authority)?;
        let snapshot = self.authority.snapshot(&key).map_err(|_| {
            ModelPolicyResolutionError::new(
                ModelPolicyResolutionErrorKind::AuthorityUnavailable,
                "model policy authority is unavailable",
            )
        })?;
        if snapshot.key != key {
            return Err(ModelPolicyResolutionError::new(
                ModelPolicyResolutionErrorKind::SnapshotMismatch,
                "model policy authority returned another route snapshot",
            ));
        }
        Ok(ModelPolicyResolution {
            key,
            policy: snapshot.policy,
        })
    }
}

impl fmt::Debug for ProductionModelPolicySource<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionModelPolicySource")
            .finish_non_exhaustive()
    }
}

fn map_admission_error(_error: &ModelAdmissionError) -> ModelPolicyResolutionError {
    ModelPolicyResolutionError::invalid()
}
