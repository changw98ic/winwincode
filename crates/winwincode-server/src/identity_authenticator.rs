// SPDX-License-Identifier: Apache-2.0

//! One authentication boundary for browser sessions and enterprise API Tokens.

use std::sync::Arc;

use winwincode_control_plane::EnterpriseIdentityService;

use crate::{
    AuthError, AuthenticatedPrincipal, RequestAuthenticator, SqliteAuthSessionManager,
    TransportCredentials,
};

/// Resolves exactly one browser cookie or one API Token bearer.
pub struct EnterpriseRequestAuthenticator {
    sessions: Arc<SqliteAuthSessionManager>,
    identities: Arc<EnterpriseIdentityService>,
}

impl EnterpriseRequestAuthenticator {
    #[must_use]
    pub const fn new(
        sessions: Arc<SqliteAuthSessionManager>,
        identities: Arc<EnterpriseIdentityService>,
    ) -> Self {
        Self {
            sessions,
            identities,
        }
    }
}

impl RequestAuthenticator for EnterpriseRequestAuthenticator {
    fn authenticate(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<AuthenticatedPrincipal, AuthError> {
        match (credentials.bearer(), credentials.session_cookie()) {
            (Some(bearer), None) => self
                .identities
                .authenticate_bearer(bearer)
                .map_err(|_| AuthError::new("API Token authentication failed"))
                .and_then(|identity| {
                    AuthenticatedPrincipal::new(identity.actor, identity.authorized_scopes)
                }),
            (None, Some(_)) => self.sessions.authenticate(credentials),
            (None, None) | (Some(_), Some(_)) => Err(AuthError::new(
                "exactly one authentication method is required",
            )),
        }
    }
}
