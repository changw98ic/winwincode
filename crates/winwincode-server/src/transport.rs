// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use serde_json::Value;
use tokio::sync::mpsc;
use winwincode_api::generated::{Actor, Scope};
use winwincode_domain::UserId;

/// Credentials extracted from HTTP headers. Debug/serialization are omitted so
/// transports cannot accidentally log or return them.
pub struct TransportCredentials {
    bearer: Option<String>,
    session_cookie: Option<String>,
}

impl TransportCredentials {
    pub(crate) fn new(bearer: Option<String>, session_cookie: Option<String>) -> Self {
        Self {
            bearer,
            session_cookie,
        }
    }

    #[must_use]
    pub fn bearer(&self) -> Option<&str> {
        self.bearer.as_deref()
    }

    #[must_use]
    pub fn session_cookie(&self) -> Option<&str> {
        self.session_cookie.as_deref()
    }
}

/// Secret-free identity supplied to application routing.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthenticatedPrincipal {
    actor: Actor,
    authorized_scopes: Vec<Scope>,
    subject: String,
}

impl AuthenticatedPrincipal {
    /// Build one bounded current Actor and its exact authorized Scope set.
    ///
    /// # Errors
    ///
    /// Rejects an empty, duplicate, or overlong Scope set.
    pub fn new(actor: Actor, authorized_scopes: Vec<Scope>) -> Result<Self, AuthError> {
        if !valid_actor(&actor)
            || authorized_scopes.is_empty()
            || authorized_scopes.len() > 100
            || authorized_scopes.iter().any(|scope| !valid_scope(scope))
        {
            return Err(AuthError::new("authenticated scopes are invalid"));
        }
        for (index, scope) in authorized_scopes.iter().enumerate() {
            if authorized_scopes[..index].contains(scope) {
                return Err(AuthError::new("authenticated scopes are invalid"));
            }
        }
        let subject = actor_id(&actor).to_owned();
        Ok(Self {
            actor,
            authorized_scopes,
            subject,
        })
    }

    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    #[must_use]
    pub const fn actor(&self) -> &Actor {
        &self.actor
    }

    #[must_use]
    pub fn authorized_scopes(&self) -> &[Scope] {
        &self.authorized_scopes
    }

    /// Returns the durable `UserAccount` userId when this principal is a user
    /// actor, or `None` for service and system actors.
    #[must_use]
    pub fn actor_user_id(&self) -> Option<UserId> {
        match &self.actor {
            Actor::UserActor(user) => Some(user.id.clone()),
            Actor::ServiceAccountActor(_) | Actor::SystemActor(_) => None,
        }
    }

    #[must_use]
    pub fn authorizes(&self, scope: &Scope) -> bool {
        self.authorized_scopes.contains(scope)
    }
}

fn valid_actor(actor: &Actor) -> bool {
    match actor {
        Actor::UserActor(actor) => valid_id(&actor.id.0, "usr"),
        Actor::ServiceAccountActor(actor) => valid_id(&actor.id.0, "svc"),
        Actor::SystemActor(actor) => valid_id(&actor.id.0, "sys"),
    }
}

fn valid_scope(scope: &Scope) -> bool {
    match scope {
        Scope::OrganizationScope(scope) => valid_id(&scope.organization_id.0, "org"),
        Scope::WorkspaceScope(scope) => {
            valid_id(&scope.organization_id.0, "org") && valid_id(&scope.workspace_id.0, "wsp")
        }
        Scope::ProjectScope(scope) => {
            valid_id(&scope.organization_id.0, "org")
                && valid_id(&scope.workspace_id.0, "wsp")
                && valid_id(&scope.project_id.0, "prj")
        }
        Scope::RepositoryScope(scope) => {
            valid_id(&scope.organization_id.0, "org")
                && valid_id(&scope.workspace_id.0, "wsp")
                && valid_id(&scope.project_id.0, "prj")
                && valid_id(&scope.repository_id.0, "rep")
        }
    }
}

fn valid_id(value: &str, prefix: &str) -> bool {
    let Some(suffix) = value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('_'))
    else {
        return false;
    };
    suffix.len() == 26
        && suffix.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'H'
                    | b'J'..=b'N'
                    | b'P'..=b'T'
                    | b'V'..=b'Z'
            )
        })
}

fn actor_id(actor: &Actor) -> &str {
    match actor {
        Actor::UserActor(actor) => &actor.id.0,
        Actor::ServiceAccountActor(actor) => &actor.id.0,
        Actor::SystemActor(actor) => &actor.id.0,
    }
}

/// Authentication boundary.
pub trait RequestAuthenticator: Send + Sync {
    /// # Errors
    ///
    /// Returns a stable unauthenticated result without exposing credentials.
    fn authenticate(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<AuthenticatedPrincipal, AuthError>;
}

/// Stable authentication failure with redacted text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthError {
    message: String,
}

impl AuthError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuthError {}

/// Application-facing transport failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    status: u16,
    code: String,
    message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// Authorized event channel created from the first WebSocket subscribe frame.
pub struct EventSubscription {
    pub initial_frames: Vec<Value>,
    pub events: mpsc::Receiver<Value>,
}

/// The only application port used by the public server. Business rules and
/// state mutation remain inside the Control Plane implementation.
pub trait ControlPlaneApiPort: Send + Sync {
    /// Reports whether the application and its supervised execution runtime are healthy.
    ///
    /// The default keeps existing application ports source-compatible; a
    /// production composition overrides it to fail closed after a supervised
    /// runtime task faults.
    ///
    /// # Errors
    ///
    /// Returns an error when the application or supervised runtime is faulted.
    fn health(&self) -> Result<(), ApiError> {
        Ok(())
    }

    /// # Errors
    ///
    /// Returns a public, secret-free application error.
    fn command(
        &self,
        principal: &AuthenticatedPrincipal,
        request: Value,
    ) -> Result<Value, ApiError>;

    /// # Errors
    ///
    /// Returns a public, secret-free application error.
    fn query(&self, principal: &AuthenticatedPrincipal, request: Value) -> Result<Value, ApiError>;

    /// # Errors
    ///
    /// Rejects malformed or unauthorized subscriptions before an event stream
    /// is returned.
    fn subscribe(
        &self,
        principal: &AuthenticatedPrincipal,
        first_frame: Value,
    ) -> Result<EventSubscription, ApiError>;

    /// Handle acknowledgement, resume, ping, and other client control frames.
    ///
    /// # Errors
    ///
    /// Rejects invalid or unauthorized frames.
    fn event_control(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: Value,
    ) -> Result<Vec<Value>, ApiError>;

    /// Close the embedded application lifecycle after the listener drains.
    ///
    /// # Errors
    ///
    /// Returns a redacted shutdown failure.
    fn shutdown(&self) -> Result<(), ApiError>;
}
