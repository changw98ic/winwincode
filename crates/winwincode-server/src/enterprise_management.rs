// SPDX-License-Identifier: Apache-2.0

//! Narrow application port for the canonical enterprise management contract family.

use std::sync::Arc;

use winwincode_api::generated::{
    CommandCompletedResponse, CommandRequest, QueryRequest, QueryResultResponse,
};
use winwincode_control_plane::{
    EnterpriseIdentityError, EnterpriseIdentityErrorKind, EnterpriseIdentityService,
    EnterpriseRbacError, EnterpriseRbacErrorKind, EnterpriseRbacService,
};

use crate::{ApiError, CommandDispatchResponse};

/// Enterprise domain applications behind the one generated HTTP dispatcher.
///
/// Implementations own authorization below the authenticated organization scope,
/// persistence, idempotency receipts, revisions, and public invalidation events.
/// The Server owns no enterprise resource state and does not translate wire DTOs.
pub trait EnterpriseManagementApplicationPort: Send + Sync {
    /// # Errors
    ///
    /// Returns a canonical permission, revision, idempotency, validation, or
    /// availability error without secret material.
    fn command(&self, request: CommandRequest) -> Result<CommandDispatchResponse, ApiError>;

    /// # Errors
    ///
    /// Returns a canonical permission, cursor, validation, or availability
    /// error without exposing another tenant's snapshot.
    fn query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError>;
}

/// Fail-closed placeholder used until the enterprise domain composition is supplied.
pub struct UnavailableEnterpriseManagementApplication;

impl EnterpriseManagementApplicationPort for UnavailableEnterpriseManagementApplication {
    fn command(&self, _request: CommandRequest) -> Result<CommandDispatchResponse, ApiError> {
        Err(unavailable())
    }

    fn query(&self, _request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        Err(unavailable())
    }
}

/// Canonical enterprise management adapter that owns the Identity family and
/// delegates other enterprise domains to their existing application port.
pub struct EnterpriseIdentityManagementApplication {
    identity: Arc<EnterpriseIdentityService>,
    remaining: Arc<dyn EnterpriseManagementApplicationPort>,
}

impl EnterpriseIdentityManagementApplication {
    #[must_use]
    pub fn new(
        identity: Arc<EnterpriseIdentityService>,
        remaining: Arc<dyn EnterpriseManagementApplicationPort>,
    ) -> Self {
        Self {
            identity,
            remaining,
        }
    }
}

impl EnterpriseManagementApplicationPort for EnterpriseIdentityManagementApplication {
    fn command(&self, request: CommandRequest) -> Result<CommandDispatchResponse, ApiError> {
        let command = match request {
            CommandRequest::EnterpriseIdentityUpdateCommand(command) => command,
            other => return self.remaining.command(other),
        };
        self.identity
            .update(&command)
            .map(|response| {
                CommandDispatchResponse::Completed(Box::new(
                    CommandCompletedResponse::EnterpriseIdentityUpdateCompletedResponse(response),
                ))
            })
            .map_err(|error| identity_error(&error))
    }

    fn query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let query = match request {
            QueryRequest::EnterpriseIdentityListQuery(query) => query,
            other => return self.remaining.query(other),
        };
        self.identity
            .list(&query)
            .map(QueryResultResponse::EnterpriseIdentityListResultResponse)
            .map_err(|error| identity_error(&error))
    }
}

/// Canonical enterprise management adapter for Organization, Membership,
/// Team, and versioned Role authority.
pub struct EnterpriseRbacManagementApplication {
    rbac: Arc<EnterpriseRbacService>,
    remaining: Arc<dyn EnterpriseManagementApplicationPort>,
}

impl EnterpriseRbacManagementApplication {
    #[must_use]
    pub fn new(
        rbac: Arc<EnterpriseRbacService>,
        remaining: Arc<dyn EnterpriseManagementApplicationPort>,
    ) -> Self {
        Self { rbac, remaining }
    }
}

impl EnterpriseManagementApplicationPort for EnterpriseRbacManagementApplication {
    fn command(&self, request: CommandRequest) -> Result<CommandDispatchResponse, ApiError> {
        let response = match request {
            CommandRequest::EnterpriseOrganizationUpdateCommand(command) => self
                .rbac
                .update_organization(&command)
                .map(CommandCompletedResponse::EnterpriseOrganizationUpdateCompletedResponse),
            CommandRequest::EnterpriseMembershipUpdateCommand(command) => self
                .rbac
                .update_membership(&command)
                .map(CommandCompletedResponse::EnterpriseMembershipUpdateCompletedResponse),
            CommandRequest::EnterpriseTeamUpdateCommand(command) => self
                .rbac
                .update_team(&command)
                .map(CommandCompletedResponse::EnterpriseTeamUpdateCompletedResponse),
            CommandRequest::EnterpriseRoleUpdateCommand(command) => self
                .rbac
                .update_role(&command)
                .map(CommandCompletedResponse::EnterpriseRoleUpdateCompletedResponse),
            other => return self.remaining.command(other),
        };
        response
            .map(|response| CommandDispatchResponse::Completed(Box::new(response)))
            .map_err(|error| rbac_error(&error))
    }

    fn query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        match request {
            QueryRequest::EnterpriseOrganizationListQuery(query) => self
                .rbac
                .list_organizations(&query)
                .map(QueryResultResponse::EnterpriseOrganizationListResultResponse),
            QueryRequest::EnterpriseMembershipListQuery(query) => self
                .rbac
                .list_memberships(&query)
                .map(QueryResultResponse::EnterpriseMembershipListResultResponse),
            QueryRequest::EnterpriseTeamListQuery(query) => self
                .rbac
                .list_teams(&query)
                .map(QueryResultResponse::EnterpriseTeamListResultResponse),
            QueryRequest::EnterpriseRoleListQuery(query) => self
                .rbac
                .list_roles(&query)
                .map(QueryResultResponse::EnterpriseRoleListResultResponse),
            other => return self.remaining.query(other),
        }
        .map_err(|error| rbac_error(&error))
    }
}

fn identity_error(error: &EnterpriseIdentityError) -> ApiError {
    match error.kind() {
        EnterpriseIdentityErrorKind::InvalidRequest => ApiError::new(
            400,
            "INVALID_REQUEST",
            "enterprise identity request is invalid",
        ),
        EnterpriseIdentityErrorKind::ScopeDenied => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "enterprise identity scope is denied",
        ),
        EnterpriseIdentityErrorKind::NotFound => ApiError::new(
            404,
            "RESOURCE_NOT_FOUND",
            "enterprise identity was not found",
        ),
        EnterpriseIdentityErrorKind::WrongState => ApiError::new(
            409,
            "WRONG_STATE",
            "enterprise identity state rejects this operation",
        ),
        EnterpriseIdentityErrorKind::RevisionConflict => ApiError::new(
            409,
            "REVISION_CONFLICT",
            "enterprise identity revision does not match",
        ),
        EnterpriseIdentityErrorKind::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "enterprise identity request conflicts with a durable receipt",
        ),
        EnterpriseIdentityErrorKind::Authentication => {
            ApiError::new(401, "AUTHENTICATION_REQUIRED", "authentication failed")
        }
        EnterpriseIdentityErrorKind::Storage
        | EnterpriseIdentityErrorKind::ClockUnavailable
        | EnterpriseIdentityErrorKind::EntropyUnavailable => ApiError::new(
            503,
            "SERVICE_UNAVAILABLE",
            "enterprise identity service is unavailable",
        ),
    }
}

fn rbac_error(error: &EnterpriseRbacError) -> ApiError {
    match error.kind() {
        EnterpriseRbacErrorKind::InvalidRequest => {
            ApiError::new(400, "INVALID_REQUEST", "enterprise RBAC request is invalid")
        }
        EnterpriseRbacErrorKind::ScopeDenied => {
            ApiError::new(403, "PERMISSION_DENIED", "enterprise RBAC scope is denied")
        }
        EnterpriseRbacErrorKind::NotFound => ApiError::new(
            404,
            "RESOURCE_NOT_FOUND",
            "enterprise RBAC resource was not found",
        ),
        EnterpriseRbacErrorKind::WrongState => ApiError::new(
            409,
            "WRONG_STATE",
            "enterprise RBAC state rejects this operation",
        ),
        EnterpriseRbacErrorKind::RevisionConflict => ApiError::new(
            409,
            "REVISION_CONFLICT",
            "enterprise RBAC revision does not match",
        ),
        EnterpriseRbacErrorKind::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "enterprise RBAC request conflicts with a durable receipt",
        ),
        EnterpriseRbacErrorKind::Storage | EnterpriseRbacErrorKind::ClockUnavailable => {
            ApiError::new(
                503,
                "SERVICE_UNAVAILABLE",
                "enterprise RBAC service is unavailable",
            )
        }
    }
}

fn unavailable() -> ApiError {
    ApiError::new(
        503,
        "SERVICE_UNAVAILABLE",
        "enterprise management application is not configured",
    )
}
