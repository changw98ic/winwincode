// SPDX-License-Identifier: Apache-2.0

//! Narrow application port for the enterprise management contract family.
//!
//! Community does not implement enterprise management. Those product surfaces
//! live in the Enterprise repository; this port always fails closed.

use winwincode_api::generated::{CommandRequest, QueryRequest, QueryResultResponse};

use crate::{ApiError, CommandDispatchResponse};

pub trait EnterpriseManagementApplicationPort: Send + Sync {
    /// # Errors
    ///
    /// Returns a canonical availability error without secret material.
    fn command(&self, request: CommandRequest) -> Result<CommandDispatchResponse, ApiError>;

    /// # Errors
    ///
    /// Returns a canonical availability error without exposing another tenant's snapshot.
    fn query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError>;
}

/// Fail-closed Community placeholder for enterprise management.
pub struct UnavailableEnterpriseManagementApplication;

impl EnterpriseManagementApplicationPort for UnavailableEnterpriseManagementApplication {
    fn command(&self, _request: CommandRequest) -> Result<CommandDispatchResponse, ApiError> {
        Err(unavailable())
    }

    fn query(&self, _request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        Err(unavailable())
    }
}

fn unavailable() -> ApiError {
    ApiError::new(
        503,
        "SERVICE_UNAVAILABLE",
        "enterprise management application is not configured",
    )
}
