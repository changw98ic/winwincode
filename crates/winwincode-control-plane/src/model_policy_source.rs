use std::fmt;

use winwincode_domain::OrganizationId;

use crate::{
    FrozenModelAdmissionPolicy, FrozenModelRouteAuthority, ModelAdmissionError,
    ModelAdmissionPolicyLayer,
};

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

#[derive(Clone, Debug, PartialEq)]
pub struct ModelPolicyAuthoritySnapshot {
    key: ModelPolicyRouteKey,
    policy: FrozenModelAdmissionPolicy,
}

impl ModelPolicyAuthoritySnapshot {
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

pub trait ModelPolicyAuthorityPort: Send + Sync {
    fn snapshot(
        &self,
        key: &ModelPolicyRouteKey,
    ) -> Result<ModelPolicyAuthoritySnapshot, ModelPolicyAuthorityError>;
}

#[derive(Clone, Debug)]
pub struct LocalModelPolicyAuthorityConfig {
    pub base: ModelAdmissionPolicyLayer,
}

pub struct LocalModelPolicyAuthority {
    base: ModelAdmissionPolicyLayer,
}

impl LocalModelPolicyAuthority {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelPolicyResolutionErrorKind {
    InvalidAuthority,
    SnapshotMismatch,
    AuthorityUnavailable,
}

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

pub struct ProductionModelPolicySource<'authority> {
    authority: &'authority dyn ModelPolicyAuthorityPort,
}

impl<'authority> ProductionModelPolicySource<'authority> {
    #[must_use]
    pub const fn new(authority: &'authority dyn ModelPolicyAuthorityPort) -> Self {
        Self { authority }
    }

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
