// SPDX-License-Identifier: Apache-2.0

//! Typed, bounded `StrongFlow` application reads.
//!
//! The transport-facing seam accepts only generated query envelopes. All
//! source selection and projection composition remains inside the Control
//! Plane.

mod application;
mod mapping;
mod production_sources;
mod sources;

use std::fmt;

use winwincode_api::generated::{
    DeliveryGetQuery, DeliveryGetResultResponse, DeliveryGetResultResponseQuery, ErrorCode,
    PageInfo, QueryResultResponse, RuntimeProjectionGetParameters, RuntimeProjectionGetQuery,
    RuntimeProjectionGetResultResponse, RuntimeProjectionGetResultResponseQuery,
};
use winwincode_domain::{SchemaVersion, is_canonical_delivery_id};

use crate::ControlPlane;

pub use application::PublicationAuthorizationSnapshot;
pub(crate) use application::{
    current_publication_approval, derive_publication_binding, load_current,
    load_current_publication_read,
};
pub use production_sources::SqliteTrustedPublicationProjectionAdapter;
pub(crate) use production_sources::production_sources;
pub use sources::{
    DeliveryRuntimeReadRequest, ProductSessionRuntimeReadRequest, PublicationFactBinding,
    PublicationResourceFact, PublicationResourceKind, PublicationResultFact, RuntimeCutExpectation,
    SqliteStorageRuntimeProjectionReadCutReader, SqliteTrustedRuntimeProjectionAdapter,
    StrongFlowProjectionSources, TrustedProjectionReadError, TrustedPublicationProjectionAdapter,
    TrustedPublicationProjectionRead, TrustedRuntimeFoldSnapshot, TrustedRuntimeProjectionAdapter,
    TrustedRuntimeProjectionRead, TrustedRuntimeProjectionReadCut,
    TrustedRuntimeProjectionReadCutReader,
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
        let scope = &query.scope;
        if !is_canonical_delivery_id(&query.parameters.delivery_id.0) {
            return Err(StrongFlowProjectionError::invalid_request(
                "delivery identity is not canonical",
            ));
        }
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
        Ok(QueryResultResponse::DeliveryGetResultResponse(
            DeliveryGetResultResponse {
                schema_version: SchemaVersion::WinwincodeV1,
                request_id: query.request_id.clone(),
                query: DeliveryGetResultResponseQuery::DeliveryGet,
                result: mapping::delivery_detail(&read)?,
                page: page(),
            },
        ))
    }

    fn runtime_projection_get(
        &self,
        query: &RuntimeProjectionGetQuery,
    ) -> Result<QueryResultResponse, StrongFlowProjectionError> {
        let scope = &query.scope;
        if query.page.cursor.is_some() {
            return Err(StrongFlowProjectionError::invalid_request(
                "StrongFlow exact reads do not accept a page cursor",
            ));
        }
        application::validate_scope(scope)?;
        let limit = application::validate_limit(query.page.limit)?;
        let snapshot = match &query.parameters {
            RuntimeProjectionGetParameters::DeliveryStageRuntimeProjectionGetParameters(
                parameters,
            ) => {
                if !is_canonical_delivery_id(&parameters.delivery_id.0) {
                    return Err(StrongFlowProjectionError::invalid_request(
                        "delivery identity is not canonical",
                    ));
                }
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
                let read = application::establish_product_session_read(
                    self,
                    scope,
                    &parameters.product_session_id,
                    limit,
                )?;
                mapping::runtime_snapshot_for_product_session(
                    &read,
                    &parameters.product_session_id,
                )?
            }
        };
        Ok(QueryResultResponse::RuntimeProjectionGetResultResponse(
            RuntimeProjectionGetResultResponse {
                schema_version: SchemaVersion::WinwincodeV1,
                request_id: query.request_id.clone(),
                query: RuntimeProjectionGetResultResponseQuery::RuntimeProjectionGet,
                result: snapshot,
                page: page(),
            },
        ))
    }
}

fn page() -> PageInfo {
    PageInfo {
        has_more: false,
        next_cursor: None,
    }
}
