// SPDX-License-Identifier: Apache-2.0

//! Typed, bounded `StrongFlow` application reads.
//!
//! The transport-facing seam accepts only generated query envelopes. All
//! source selection and projection composition remains inside the Control
//! Plane.

mod application;
mod mapping;
mod sources;

use std::fmt;

use winwincode_api::generated::{
    DeliveryGetQuery, ErrorCode, PageInfo, QueryName, QueryResult, QueryResultResponse,
    RuntimeProjectionGetParameters, RuntimeProjectionGetQuery, Scope,
};

use crate::ControlPlane;

pub use application::PublicationAuthorizationSnapshot;
pub use sources::{
    DeliveryRuntimeReadRequest, PublicationFactBinding, PublicationResourceFact,
    PublicationResourceKind, PublicationResultFact, RuntimeCutExpectation,
    StrongFlowProjectionSources, TrustedProjectionReadError, TrustedPublicationProjectionAdapter,
    TrustedPublicationProjectionRead, TrustedRuntimeProjectionAdapter,
    TrustedRuntimeProjectionRead,
};

/// Stable failure classes for typed `StrongFlow` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrongFlowProjectionError {
    InvalidRequest(String),
    PermissionDenied(String),
    ResourceNotFound(String),
    RevisionConflict(String),
    ReadCursorExpired(String),
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
            Self::ReadCursorExpired(_) => ErrorCode::ReadCursorExpired,
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
            | Self::ReadCursorExpired(message)
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
    /// Returns one current bounded `StrongFlow` detail read.
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
        if query.page.cursor.is_some() {
            return Err(StrongFlowProjectionError::invalid_request(
                "StrongFlow exact reads use atCursor rather than page cursor",
            ));
        }
        let read = match &query.parameters.at_cursor {
            Some(cursor) => application::replay_delivery_read(
                self,
                &query.actor,
                scope,
                &query.parameters.delivery_id,
                cursor,
                query.page.limit,
            )?,
            None => application::establish_delivery_read(
                self,
                &query.actor,
                scope,
                &query.parameters.delivery_id,
                query.page.limit,
            )?,
        };
        Ok(response(
            QueryName::DeliveryGet,
            query.request_id.clone(),
            QueryResult::DeliveryDetailProjection(mapping::delivery_detail(&read)?),
        ))
    }

    fn runtime_projection_get(
        &self,
        query: &RuntimeProjectionGetQuery,
    ) -> Result<QueryResultResponse, StrongFlowProjectionError> {
        let Scope::RepositoryScope(scope) = &query.scope else {
            return Err(StrongFlowProjectionError::PermissionDenied(
                "runtime projection requires repository scope".to_owned(),
            ));
        };
        if query.page.cursor.is_some() {
            return Err(StrongFlowProjectionError::invalid_request(
                "StrongFlow exact reads do not accept a page cursor",
            ));
        }
        application::validate_scope(scope)?;
        let limit = application::validate_limit(query.page.limit)?;
        let sources = self.strongflow_sources.as_ref().ok_or_else(|| {
            StrongFlowProjectionError::trusted_facts_unavailable(
                "trusted runtime and publication facts are unavailable",
            )
        })?;
        let snapshot = match &query.parameters {
            RuntimeProjectionGetParameters::DeliveryStageRuntimeProjectionGetParameters(
                parameters,
            ) => {
                let read = application::replay_delivery_read(
                    self,
                    &query.actor,
                    scope,
                    &parameters.delivery_id,
                    &parameters.at_cursor,
                    query.page.limit,
                )?;
                if parameters.at_cursor != mapping::cursor(&read)? {
                    return Err(StrongFlowProjectionError::RevisionConflict(
                        "delivery and runtime reads do not share the same bounded cursor"
                            .to_owned(),
                    ));
                }
                mapping::runtime_snapshot_for_delivery(
                    &read,
                    &parameters.stage_run_id,
                    &parameters.product_session_id,
                )?
            }
            RuntimeProjectionGetParameters::ProductSessionRuntimeProjectionGetParameters(
                parameters,
            ) => {
                let read = sources
                    .runtime
                    .read_product_session(scope, &parameters.product_session_id, limit)
                    .map_err(application::current_source_error)?;
                mapping::runtime_snapshot_for_product_session(
                    &read,
                    &parameters.product_session_id,
                )?
            }
        };
        Ok(response(
            QueryName::RuntimeProjectionGet,
            query.request_id.clone(),
            QueryResult::RuntimeProjectionSnapshot(snapshot),
        ))
    }
}

fn response(
    query: QueryName,
    request_id: winwincode_domain::RequestId,
    result: QueryResult,
) -> QueryResultResponse {
    QueryResultResponse {
        schema_version: winwincode_api::generated::SchemaVersion::WinwincodeV1,
        request_id,
        query,
        result,
        page: PageInfo {
            has_more: false,
            next_cursor: None,
        },
    }
}
