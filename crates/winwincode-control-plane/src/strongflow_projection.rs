// SPDX-License-Identifier: Apache-2.0

//! Typed, bounded StrongFlow application reads.
//!
//! The transport-facing seam accepts only generated query envelopes. All
//! source selection and projection composition remains inside the Control
//! Plane.

mod application;
mod sources;

use std::fmt;

use winwincode_api::generated::{
    DeliveryGetQuery, ErrorCode, QueryResultResponse, RuntimeProjectionGetQuery, Scope,
};

use crate::ControlPlane;

pub use application::PublicationAuthorizationSnapshot;
pub use sources::{
    DeliveryRuntimeReadRequest, PublicationFactBinding, PublicationResultFact,
    RuntimeCutExpectation, StrongFlowProjectionSources, TrustedProjectionReadError,
    TrustedPublicationProjectionAdapter, TrustedPublicationProjectionRead,
    TrustedRuntimeProjectionAdapter, TrustedRuntimeProjectionRead,
};

/// Stable failure classes for typed StrongFlow reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrongFlowProjectionError {
    InvalidRequest(String),
    PermissionDenied(String),
    ResourceNotFound(String),
    RevisionConflict(String),
    TrustedFactsUnavailable(String),
    ServiceUnavailable(String),
    Internal(String),
}

impl StrongFlowProjectionError {
    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    pub(crate) fn trusted_facts_unavailable(message: impl Into<String>) -> Self {
        Self::TrustedFactsUnavailable(message.into())
    }

    /// Returns the generated public error code without exposing source details.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::PermissionDenied(_) => ErrorCode::PermissionDenied,
            Self::ResourceNotFound(_) => ErrorCode::ResourceNotFound,
            Self::RevisionConflict(_) => ErrorCode::RevisionConflict,
            Self::TrustedFactsUnavailable(_) => ErrorCode::TrustedFactsUnavailable,
            Self::ServiceUnavailable(_) => ErrorCode::ServiceUnavailable,
            Self::Internal(_) => ErrorCode::InternalError,
        }
    }

    /// Returns a redacted, stable explanation suitable for an API error.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::PermissionDenied(message)
            | Self::ResourceNotFound(message)
            | Self::RevisionConflict(message)
            | Self::TrustedFactsUnavailable(message)
            | Self::ServiceUnavailable(message)
            | Self::Internal(message) => message,
        }
    }
}

impl fmt::Display for StrongFlowProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for StrongFlowProjectionError {}

impl From<winwincode_delivery::projection::ProjectionError> for StrongFlowProjectionError {
    fn from(_source: winwincode_delivery::projection::ProjectionError) -> Self {
        Self::TrustedFactsUnavailable(
            "current projection facts are incomplete or no longer exact".to_owned(),
        )
    }
}

/// Generated-query application port used by future HTTP and WebSocket adapters.
pub trait StrongFlowProjectionQueryPort {
    /// Returns one current bounded StrongFlow detail read.
    ///
    /// # Errors
    ///
    /// Fails closed when scope, revision, cursor, or trusted sources are not exact.
    fn delivery_get(
        &self,
        query: &DeliveryGetQuery,
    ) -> Result<QueryResultResponse, StrongFlowProjectionError>;

    /// Returns one runtime projection at the exact bounded cut named by the query.
    ///
    /// # Errors
    ///
    /// Fails closed on a foreign, stale, gapped, or unavailable source cut.
    fn runtime_projection_get(
        &self,
        query: &RuntimeProjectionGetQuery,
    ) -> Result<QueryResultResponse, StrongFlowProjectionError>;
}

impl StrongFlowProjectionQueryPort for ControlPlane {
    fn delivery_get(
        &self,
        query: &DeliveryGetQuery,
    ) -> Result<QueryResultResponse, StrongFlowProjectionError> {
        let Scope::RepositoryScope(scope) = &query.scope else {
            return Err(StrongFlowProjectionError::PermissionDenied(
                "delivery detail requires repository scope".to_owned(),
            ));
        };
        let _read = application::establish_delivery_read(
            self,
            &query.actor,
            scope,
            &query.parameters.delivery_id,
            query.page.limit,
        )?;
        Err(StrongFlowProjectionError::trusted_facts_unavailable(
            "typed projection mapping is not installed",
        ))
    }

    fn runtime_projection_get(
        &self,
        _query: &RuntimeProjectionGetQuery,
    ) -> Result<QueryResultResponse, StrongFlowProjectionError> {
        let _sources = self.strongflow_sources.as_ref().ok_or_else(|| {
            StrongFlowProjectionError::trusted_facts_unavailable(
                "trusted runtime and publication facts are unavailable",
            )
        })?;
        Err(StrongFlowProjectionError::trusted_facts_unavailable(
            "typed projection mapping is not installed",
        ))
    }
}
