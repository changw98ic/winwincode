// SPDX-License-Identifier: Apache-2.0

//! Narrow collaboration port for the generated HTTP dispatcher.
//!
//! Community does not ship enterprise collaboration (Activity / notifications /
//! Presence). Those product surfaces live in the Enterprise repository; this
//! port always fails closed.

use winwincode_api::generated::{CommandRequest, QueryRequest, QueryResultResponse, Scope};

use crate::{ApiError, CommandDispatchResponse};

pub trait CollaborationApplicationPort: Send + Sync {
    /// # Errors
    ///
    /// Returns a canonical availability error without secret material.
    fn command(
        &self,
        _scopes: &[Scope],
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError>;

    /// # Errors
    ///
    /// Returns a canonical availability error without secret material.
    fn query(
        &self,
        _scopes: &[Scope],
        request: QueryRequest,
    ) -> Result<QueryResultResponse, ApiError>;
}

/// Fail-closed Community placeholder for enterprise collaboration.
pub struct UnavailableCollaborationApplication;

impl CollaborationApplicationPort for UnavailableCollaborationApplication {
    fn command(
        &self,
        _scopes: &[Scope],
        _request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        Err(unavailable())
    }

    fn query(
        &self,
        _scopes: &[Scope],
        _request: QueryRequest,
    ) -> Result<QueryResultResponse, ApiError> {
        Err(unavailable())
    }
}

fn unavailable() -> ApiError {
    ApiError::new(
        503,
        "SERVICE_UNAVAILABLE",
        "collaboration application is not configured",
    )
}
