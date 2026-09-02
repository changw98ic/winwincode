// SPDX-License-Identifier: Apache-2.0

//! Browser session bootstrap, persistence, authentication, and revocation.

use std::fmt;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_api::generated::{Actor, AuthSessionResponse, Scope};
use winwincode_control_plane::{
    BrowserSessionLifecycleError, BrowserSessionLifecyclePort, ExternalAuthenticationOutcome,
};
use winwincode_domain::{Instant, SchemaVersion};

use crate::transport::{
    AuthError, AuthenticatedPrincipal, RequestAuthenticator, TransportCredentials,
};

const SESSION_COOKIE_NAME: &str = "wwc_session";
const SESSION_TOKEN_BYTES: usize = 32;
const MAX_BOOTSTRAP_WINDOW_SECONDS: u64 = 24 * 60 * 60;
const MAX_SESSION_TTL_SECONDS: u64 = 365 * 24 * 60 * 60;

/// Explicit lifetime limits for bootstrap proofs and browser sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthSessionConfig {
    bootstrap_window: Duration,
    session_ttl: Duration,
}

impl AuthSessionConfig {
    /// # Errors
    ///
    /// Rejects zero or unrepresentable durations.
    pub fn new(
        bootstrap_window: Duration,
        session_ttl: Duration,
    ) -> Result<Self, AuthSessionError> {
        duration_millis(bootstrap_window)?;
        duration_millis(session_ttl)?;
        if bootstrap_window.is_zero()
            || session_ttl.is_zero()
            || bootstrap_window > Duration::from_secs(MAX_BOOTSTRAP_WINDOW_SECONDS)
            || session_ttl > Duration::from_secs(MAX_SESSION_TTL_SECONDS)
        {
            return Err(AuthSessionError::configuration());
        }
        Ok(Self {
            bootstrap_window,
            session_ttl,
        })
    }

    #[must_use]
    pub const fn bootstrap_window(self) -> Duration {
        self.bootstrap_window
    }

    #[must_use]
    pub const fn session_ttl(self) -> Duration {
        self.session_ttl
    }
}

impl Default for AuthSessionConfig {
    fn default() -> Self {
        Self {
            bootstrap_window: Duration::from_mins(10),
            session_ttl: Duration::from_hours(8),
        }
    }
}

trait AuthSessionClock: Send + Sync {
    fn unix_millis(&self) -> Result<i64, AuthSessionError>;
}

struct SystemAuthSessionClock;

impl AuthSessionClock for SystemAuthSessionClock {
    fn unix_millis(&self) -> Result<i64, AuthSessionError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthSessionError::clock())?;
        i64::try_from(elapsed.as_millis()).map_err(|_| AuthSessionError::clock())
    }
}

trait AuthSessionTokenGenerator: Send + Sync {
    fn generate(&self) -> Result<[u8; SESSION_TOKEN_BYTES], AuthSessionError>;
}

struct SystemAuthSessionTokenGenerator;

impl AuthSessionTokenGenerator for SystemAuthSessionTokenGenerator {
    fn generate(&self) -> Result<[u8; SESSION_TOKEN_BYTES], AuthSessionError> {
        let mut bytes = [0_u8; SESSION_TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| AuthSessionError::entropy())?;
        Ok(bytes)
    }
}

/// One short-window bootstrap proof bound to an exact Actor and Scope set.
///
/// The proof deliberately has no `Debug`, serialization, or getter surface.
pub struct AuthSessionBootstrap {
    proof: String,
    principal: AuthenticatedPrincipal,
}

impl AuthSessionBootstrap {
    /// # Errors
    ///
    /// Rejects malformed proof material or an invalid authorization context.
    pub fn new(
        proof: impl Into<String>,
        actor: Actor,
        authorized_scopes: Vec<Scope>,
    ) -> Result<Self, AuthSessionError> {
        let proof = proof.into();
        if proof.is_empty()
            || proof.len() > 4096
            || proof.chars().any(char::is_whitespace)
            || proof.chars().any(char::is_control)
        {
            return Err(AuthSessionError::configuration());
        }
        let scopes = canonical_scopes(authorized_scopes)?;
        let principal = AuthenticatedPrincipal::new(actor, scopes)
            .map_err(|_| AuthSessionError::configuration())?;
        Ok(Self { proof, principal })
    }
}

/// The canonical standalone browser-session implementation.
pub struct SqliteAuthSessionManager {
    connection: Mutex<Connection>,
    bootstraps: Vec<AuthSessionBootstrap>,
    opened_at_millis: i64,
    config: AuthSessionConfig,
    clock: Arc<dyn AuthSessionClock>,
    token_generator: Arc<dyn AuthSessionTokenGenerator>,
}

/// Maps one verified external authentication outcome into the canonical
/// browser-session authority.
pub struct ExternalIdentitySessionIssuer {
    sessions: Arc<SqliteAuthSessionManager>,
}

impl ExternalIdentitySessionIssuer {
    #[must_use]
    pub const fn new(sessions: Arc<SqliteAuthSessionManager>) -> Self {
        Self { sessions }
    }

    /// Issues at most one cookie for a newly committed external assertion.
    ///
    /// # Errors
    ///
    /// Returns the canonical session authority failure without exposing token,
    /// assertion, or cookie material.
    pub fn issue(
        &self,
        outcome: ExternalAuthenticationOutcome,
    ) -> Result<ExternalIdentitySessionResult, AuthSessionError> {
        if outcome.idempotent_replay {
            return Ok(ExternalIdentitySessionResult { issued: None });
        }
        let issued = self
            .sessions
            .issue_authenticated(outcome.principal.actor, outcome.principal.authorized_scopes)?;
        Ok(ExternalIdentitySessionResult {
            issued: Some(issued),
        })
    }
}

/// Secret-safe external login result. Cookie material remains transport-only.
pub struct ExternalIdentitySessionResult {
    issued: Option<IssuedBrowserSession>,
}

impl ExternalIdentitySessionResult {
    #[must_use]
    pub const fn is_idempotent_replay(&self) -> bool {
        self.issued.is_none()
    }

    /// Returns the one transport-ready cookie header for a newly issued
    /// session, or `None` for an exact assertion replay.
    ///
    /// # Errors
    ///
    /// Returns a clock conversion failure without exposing cookie material in
    /// the error.
    pub fn set_cookie_header(&self) -> Result<Option<String>, AuthSessionError> {
        self.issued
            .as_ref()
            .map(IssuedBrowserSession::set_cookie_header)
            .transpose()
    }

    /// Returns the secret-free canonical session response for a newly issued
    /// session, or `None` for an exact assertion replay.
    ///
    /// # Errors
    ///
    /// Returns a clock/encoding failure without exposing assertion or cookie
    /// material.
    pub fn response(&self) -> Result<Option<AuthSessionResponse>, AuthSessionError> {
        self.issued
            .as_ref()
            .map(IssuedBrowserSession::response)
            .transpose()
    }
}

impl SqliteAuthSessionManager {
    /// Open or create the digest-only browser session database.
    ///
    /// # Errors
    ///
    /// Rejects invalid configuration or unavailable durable storage.
    pub fn open(
        directory: impl AsRef<Path>,
        bootstraps: Vec<AuthSessionBootstrap>,
        config: AuthSessionConfig,
    ) -> Result<Self, AuthSessionError> {
        Self::open_with_dependencies(
            directory.as_ref(),
            bootstraps,
            config,
            Arc::new(SystemAuthSessionClock),
            Arc::new(SystemAuthSessionTokenGenerator),
        )
    }

    fn open_with_dependencies(
        directory: &Path,
        bootstraps: Vec<AuthSessionBootstrap>,
        config: AuthSessionConfig,
        clock: Arc<dyn AuthSessionClock>,
        token_generator: Arc<dyn AuthSessionTokenGenerator>,
    ) -> Result<Self, AuthSessionError> {
        if bootstraps.is_empty() {
            return Err(AuthSessionError::configuration());
        }
        AuthSessionConfig::new(config.bootstrap_window, config.session_ttl)?;
        for (index, bootstrap) in bootstraps.iter().enumerate() {
            if bootstraps[..index].iter().any(|candidate| {
                constant_time_equal(candidate.proof.as_bytes(), bootstrap.proof.as_bytes())
            }) {
                return Err(AuthSessionError::configuration());
            }
        }
        std::fs::create_dir_all(directory).map_err(|_| AuthSessionError::storage())?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| AuthSessionError::storage())?;
        let database_path = directory.join("auth-sessions.sqlite3");
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&database_path)
            .map_err(|_| AuthSessionError::storage())?;
        std::fs::set_permissions(&database_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| AuthSessionError::storage())?;
        let connection =
            Connection::open(&database_path).map_err(|_| AuthSessionError::storage())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| AuthSessionError::storage())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
                 PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS auth_sessions (
                   session_digest TEXT PRIMARY KEY NOT NULL
                     CHECK(length(session_digest) = 64),
                   subject TEXT NOT NULL CHECK(length(subject) BETWEEN 1 AND 200),
                   actor_json TEXT NOT NULL CHECK(length(actor_json) BETWEEN 2 AND 4096),
                   authorized_scopes_json TEXT NOT NULL
                     CHECK(length(authorized_scopes_json) BETWEEN 2 AND 65536),
                   created_at_millis INTEGER NOT NULL CHECK(created_at_millis >= 0),
                   expires_at_millis INTEGER NOT NULL
                     CHECK(expires_at_millis > created_at_millis),
                   revoked_at_millis INTEGER
                     CHECK(revoked_at_millis IS NULL OR revoked_at_millis >= created_at_millis)
                 );
                 CREATE INDEX IF NOT EXISTS auth_sessions_expiry
                   ON auth_sessions(expires_at_millis);",
            )
            .map_err(|_| AuthSessionError::storage())?;
        ensure_session_context_columns(&connection)?;
        let opened_at_millis = clock.unix_millis()?;
        Ok(Self {
            connection: Mutex::new(connection),
            bootstraps,
            opened_at_millis,
            config,
            clock,
            token_generator,
        })
    }

    /// Exchange an in-memory bootstrap proof for an independent random cookie.
    ///
    /// # Errors
    ///
    /// Rejects non-bearer credentials, bad or late proofs, entropy failure, and
    /// unavailable durable storage.
    pub(crate) fn bootstrap(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<IssuedBrowserSession, AuthSessionError> {
        if credentials.session_cookie().is_some() {
            return Err(AuthSessionError::authentication());
        }
        let proof = credentials
            .bearer()
            .ok_or_else(AuthSessionError::authentication)?;
        let now = self.clock.unix_millis()?;
        let window = duration_millis(self.config.bootstrap_window)?;
        if now > self.opened_at_millis.saturating_add(window) {
            return Err(AuthSessionError::authentication());
        }
        let mut matched = None;
        for bootstrap in &self.bootstraps {
            if constant_time_equal(proof.as_bytes(), bootstrap.proof.as_bytes()) {
                matched = Some(bootstrap.principal.clone());
            }
        }
        let principal = matched.ok_or_else(AuthSessionError::authentication)?;
        self.issue_principal(principal)
    }

    fn issue_authenticated(
        &self,
        actor: Actor,
        authorized_scopes: Vec<Scope>,
    ) -> Result<IssuedBrowserSession, AuthSessionError> {
        let scopes = canonical_scopes(authorized_scopes)?;
        let principal = AuthenticatedPrincipal::new(actor, scopes)
            .map_err(|_| AuthSessionError::configuration())?;
        self.issue_principal(principal)
    }

    fn issue_principal(
        &self,
        principal: AuthenticatedPrincipal,
    ) -> Result<IssuedBrowserSession, AuthSessionError> {
        let now = self.clock.unix_millis()?;
        let expires_at_millis = now
            .checked_add(duration_millis(self.config.session_ttl)?)
            .ok_or_else(AuthSessionError::clock)?;
        let raw = self.token_generator.generate()?;
        let cookie_value = URL_SAFE_NO_PAD.encode(raw);
        let digest = session_digest(&cookie_value);
        let actor_json = encode_json(principal.actor())?;
        let authorized_scopes_json = encode_json(principal.authorized_scopes())?;
        self.connection
            .lock()
            .map_err(|_| AuthSessionError::storage())?
            .execute(
                "INSERT INTO auth_sessions (
                   session_digest, subject, actor_json, authorized_scopes_json,
                   created_at_millis, expires_at_millis, revoked_at_millis
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    digest,
                    principal.subject(),
                    actor_json,
                    authorized_scopes_json,
                    now,
                    expires_at_millis
                ],
            )
            .map_err(|_| AuthSessionError::storage())?;
        Ok(IssuedBrowserSession {
            cookie_value,
            expires_at_millis,
            max_age_seconds: self.config.session_ttl.as_secs(),
            principal,
        })
    }

    /// Revoke the exact current cookie digest.
    ///
    /// # Errors
    ///
    /// Rejects bearer credentials, missing/expired/revoked cookies, and storage
    /// failures.
    pub(crate) fn revoke(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<(), AuthSessionError> {
        if credentials.bearer().is_some() {
            return Err(AuthSessionError::authentication());
        }
        let cookie = credentials
            .session_cookie()
            .ok_or_else(AuthSessionError::authentication)?;
        let now = self.clock.unix_millis()?;
        let changed = self
            .connection
            .lock()
            .map_err(|_| AuthSessionError::storage())?
            .execute(
                "UPDATE auth_sessions
                 SET revoked_at_millis = ?2
                 WHERE session_digest = ?1
                   AND revoked_at_millis IS NULL
                   AND expires_at_millis > ?2",
                params![session_digest(cookie), now],
            )
            .map_err(|_| AuthSessionError::storage())?;
        if changed == 1 {
            Ok(())
        } else {
            Err(AuthSessionError::authentication())
        }
    }

    fn read_session(&self, cookie: &str) -> Result<CurrentBrowserSession, AuthSessionError> {
        let now = self.clock.unix_millis()?;
        let stored = self
            .connection
            .lock()
            .map_err(|_| AuthSessionError::storage())?
            .query_row(
                "SELECT subject, actor_json, authorized_scopes_json, expires_at_millis
                 FROM auth_sessions
                 WHERE session_digest = ?1
                   AND actor_json IS NOT NULL
                   AND authorized_scopes_json IS NOT NULL
                   AND revoked_at_millis IS NULL
                   AND expires_at_millis > ?2",
                params![session_digest(cookie), now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| AuthSessionError::storage())?
            .ok_or_else(AuthSessionError::authentication)?;
        let actor: Actor = decode_json(&stored.1)?;
        let scopes: Vec<Scope> = decode_json(&stored.2)?;
        let scopes = canonical_scopes(scopes).map_err(|_| AuthSessionError::storage())?;
        let principal =
            AuthenticatedPrincipal::new(actor, scopes).map_err(|_| AuthSessionError::storage())?;
        if principal.subject() != stored.0 {
            return Err(AuthSessionError::storage());
        }
        Ok(CurrentBrowserSession {
            principal,
            expires_at_millis: stored.3,
        })
    }

    /// Return the current secret-free cookie session context.
    ///
    /// # Errors
    ///
    /// Rejects bearer credentials, missing/expired/revoked cookies, and invalid durable context.
    pub fn current(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<AuthSessionResponse, AuthSessionError> {
        if credentials.bearer().is_some() {
            return Err(AuthSessionError::authentication());
        }
        let cookie = credentials
            .session_cookie()
            .ok_or_else(AuthSessionError::authentication)?;
        self.read_session(cookie)?.response()
    }

    /// Replace every live session Scope set for one exact Actor.
    ///
    /// # Errors
    ///
    /// Rejects invalid scopes or unavailable durable storage.
    pub fn replace_authorized_scopes(
        &self,
        actor: &Actor,
        authorized_scopes: Vec<Scope>,
    ) -> Result<usize, AuthSessionError> {
        if authorized_scopes.is_empty() {
            return self.revoke_actor_sessions(actor);
        }
        let scopes = canonical_scopes(authorized_scopes)?;
        let principal = AuthenticatedPrincipal::new(actor.clone(), scopes)
            .map_err(|_| AuthSessionError::configuration())?;
        let now = self.clock.unix_millis()?;
        let actor_json = encode_json(principal.actor())?;
        let scopes_json = encode_json(principal.authorized_scopes())?;
        self.connection
            .lock()
            .map_err(|_| AuthSessionError::storage())?
            .execute(
                "UPDATE auth_sessions
                 SET actor_json = ?2, authorized_scopes_json = ?3
                 WHERE subject = ?1
                   AND revoked_at_millis IS NULL
                   AND expires_at_millis > ?4",
                params![principal.subject(), actor_json, scopes_json, now],
            )
            .map_err(|_| AuthSessionError::storage())
    }

    /// Revoke every live session for one exact Actor.
    ///
    /// # Errors
    ///
    /// Returns a storage failure without exposing Actor or cookie data.
    pub fn revoke_actor_sessions(&self, actor: &Actor) -> Result<usize, AuthSessionError> {
        let subject = actor_subject(actor);
        let now = self.clock.unix_millis()?;
        self.connection
            .lock()
            .map_err(|_| AuthSessionError::storage())?
            .execute(
                "UPDATE auth_sessions
                 SET revoked_at_millis = ?2
                 WHERE subject = ?1
                   AND revoked_at_millis IS NULL
                   AND expires_at_millis > ?2",
                params![subject, now],
            )
            .map_err(|_| AuthSessionError::storage())
    }
}

impl BrowserSessionLifecyclePort for SqliteAuthSessionManager {
    fn replace_authorized_scopes(
        &self,
        actor: &Actor,
        authorized_scopes: Vec<Scope>,
    ) -> Result<usize, BrowserSessionLifecycleError> {
        Self::replace_authorized_scopes(self, actor, authorized_scopes)
            .map_err(|_| BrowserSessionLifecycleError)
    }

    fn revoke_actor_sessions(&self, actor: &Actor) -> Result<usize, BrowserSessionLifecycleError> {
        Self::revoke_actor_sessions(self, actor).map_err(|_| BrowserSessionLifecycleError)
    }
}

impl RequestAuthenticator for SqliteAuthSessionManager {
    fn authenticate(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<AuthenticatedPrincipal, AuthError> {
        if credentials.bearer().is_some() {
            return Err(AuthError::new("browser session authentication is required"));
        }
        let cookie = credentials
            .session_cookie()
            .ok_or_else(|| AuthError::new("browser session authentication is required"))?;
        self.read_session(cookie)
            .map(|session| session.principal)
            .map_err(|_| AuthError::new("browser session authentication failed"))
    }
}

/// Raw cookie material exists only long enough to build one `Set-Cookie` header.
pub(crate) struct IssuedBrowserSession {
    cookie_value: String,
    expires_at_millis: i64,
    max_age_seconds: u64,
    principal: AuthenticatedPrincipal,
}

impl IssuedBrowserSession {
    pub(crate) fn set_cookie_header(&self) -> Result<String, AuthSessionError> {
        let expires = httpdate::fmt_http_date(self.expiry_system_time()?);
        Ok(format!(
            "{SESSION_COOKIE_NAME}={}; Path=/; HttpOnly; Secure; SameSite=None; Max-Age={}; Expires={expires}",
            self.cookie_value, self.max_age_seconds
        ))
    }

    pub(crate) fn response(&self) -> Result<AuthSessionResponse, AuthSessionError> {
        session_response(&self.principal, self.expires_at_millis)
    }

    fn expiry_system_time(&self) -> Result<SystemTime, AuthSessionError> {
        let millis =
            u64::try_from(self.expires_at_millis).map_err(|_| AuthSessionError::clock())?;
        UNIX_EPOCH
            .checked_add(Duration::from_millis(millis))
            .ok_or_else(AuthSessionError::clock)
    }
}

struct CurrentBrowserSession {
    principal: AuthenticatedPrincipal,
    expires_at_millis: i64,
}

impl CurrentBrowserSession {
    fn response(&self) -> Result<AuthSessionResponse, AuthSessionError> {
        session_response(&self.principal, self.expires_at_millis)
    }
}

#[must_use]
pub(crate) fn cleared_session_cookie_header() -> &'static str {
    "wwc_session=; Path=/; HttpOnly; Secure; SameSite=None; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
}

/// Secret-free auth-session failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthSessionError {
    kind: AuthSessionErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthSessionErrorKind {
    Authentication,
    Clock,
    Configuration,
    Entropy,
    Storage,
}

impl AuthSessionError {
    const fn authentication() -> Self {
        Self {
            kind: AuthSessionErrorKind::Authentication,
        }
    }

    const fn clock() -> Self {
        Self {
            kind: AuthSessionErrorKind::Clock,
        }
    }

    const fn configuration() -> Self {
        Self {
            kind: AuthSessionErrorKind::Configuration,
        }
    }

    const fn entropy() -> Self {
        Self {
            kind: AuthSessionErrorKind::Entropy,
        }
    }

    const fn storage() -> Self {
        Self {
            kind: AuthSessionErrorKind::Storage,
        }
    }

    pub(crate) const fn response_encoding() -> Self {
        Self::storage()
    }

    #[must_use]
    pub(crate) const fn is_authentication(self) -> bool {
        matches!(self.kind, AuthSessionErrorKind::Authentication)
    }
}

impl fmt::Display for AuthSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            AuthSessionErrorKind::Authentication => "browser session authentication failed",
            AuthSessionErrorKind::Clock => "browser session clock is unavailable",
            AuthSessionErrorKind::Configuration => "browser session configuration is invalid",
            AuthSessionErrorKind::Entropy => "browser session entropy is unavailable",
            AuthSessionErrorKind::Storage => "browser session storage is unavailable",
        })
    }
}

impl std::error::Error for AuthSessionError {}

fn actor_subject(actor: &Actor) -> &str {
    match actor {
        Actor::UserActor(actor) => &actor.id.0,
        Actor::ServiceAccountActor(actor) => &actor.id.0,
        Actor::SystemActor(actor) => &actor.id.0,
    }
}

fn canonical_scopes(scopes: Vec<Scope>) -> Result<Vec<Scope>, AuthSessionError> {
    if scopes.is_empty() || scopes.len() > 100 {
        return Err(AuthSessionError::configuration());
    }
    let mut keyed = scopes
        .into_iter()
        .map(|scope| encode_json(&scope).map(|key| (key, scope)))
        .collect::<Result<Vec<_>, _>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    if keyed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(AuthSessionError::configuration());
    }
    Ok(keyed.into_iter().map(|(_, scope)| scope).collect())
}

fn encode_json<T: Serialize + ?Sized>(value: &T) -> Result<String, AuthSessionError> {
    serde_json::to_string(value).map_err(|_| AuthSessionError::storage())
}

fn decode_json<T: DeserializeOwned>(value: &str) -> Result<T, AuthSessionError> {
    serde_json::from_str(value).map_err(|_| AuthSessionError::storage())
}

fn ensure_session_context_columns(connection: &Connection) -> Result<(), AuthSessionError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(auth_sessions)")
        .map_err(|_| AuthSessionError::storage())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| AuthSessionError::storage())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AuthSessionError::storage())?;
    drop(statement);
    if !columns.iter().any(|column| column == "actor_json") {
        connection
            .execute("ALTER TABLE auth_sessions ADD COLUMN actor_json TEXT", [])
            .map_err(|_| AuthSessionError::storage())?;
    }
    if !columns
        .iter()
        .any(|column| column == "authorized_scopes_json")
    {
        connection
            .execute(
                "ALTER TABLE auth_sessions ADD COLUMN authorized_scopes_json TEXT",
                [],
            )
            .map_err(|_| AuthSessionError::storage())?;
    }
    Ok(())
}

fn session_response(
    principal: &AuthenticatedPrincipal,
    expires_at_millis: i64,
) -> Result<AuthSessionResponse, AuthSessionError> {
    let unix_nanos = i128::from(expires_at_millis)
        .checked_mul(1_000_000)
        .ok_or_else(AuthSessionError::clock)?;
    let expires_at = OffsetDateTime::from_unix_timestamp_nanos(unix_nanos)
        .map_err(|_| AuthSessionError::clock())?
        .format(&Rfc3339)
        .map_err(|_| AuthSessionError::clock())?;
    Ok(AuthSessionResponse {
        schema_version: SchemaVersion::WinwincodeV1,
        expires_at: Instant(expires_at),
        actor: principal.actor().clone(),
        authorized_scopes: principal.authorized_scopes().to_vec(),
    })
}

fn duration_millis(duration: Duration) -> Result<i64, AuthSessionError> {
    i64::try_from(duration.as_millis()).map_err(|_| AuthSessionError::configuration())
}

fn session_digest(cookie: &str) -> String {
    let digest = Sha256::digest(cookie.as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left = left.get(index).copied().unwrap_or(0);
        let right = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};

    use winwincode_api::generated::{
        Actor, OrganizationScope, OrganizationScopeKind, Scope, UserActor, UserActorKind,
    };
    use winwincode_control_plane::ExternalIdentityPrincipal;
    use winwincode_domain::{ExternalIdentityId, OrganizationId, UserId};

    use super::*;

    struct ManualClock(AtomicI64);

    impl ManualClock {
        const fn new(now: i64) -> Self {
            Self(AtomicI64::new(now))
        }

        fn set(&self, now: i64) {
            self.0.store(now, Ordering::Relaxed);
        }
    }

    impl AuthSessionClock for ManualClock {
        fn unix_millis(&self) -> Result<i64, AuthSessionError> {
            Ok(self.0.load(Ordering::Relaxed))
        }
    }

    struct SequenceToken(AtomicU8);

    impl SequenceToken {
        const fn new(first: u8) -> Self {
            Self(AtomicU8::new(first))
        }
    }

    impl AuthSessionTokenGenerator for SequenceToken {
        fn generate(&self) -> Result<[u8; SESSION_TOKEN_BYTES], AuthSessionError> {
            Ok([self.0.fetch_add(1, Ordering::Relaxed); SESSION_TOKEN_BYTES])
        }
    }

    fn test_directory(label: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "winwincode-auth-session-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    fn actor(value: u8) -> Actor {
        Actor::UserActor(UserActor {
            kind: UserActorKind::User,
            id: UserId(format!("usr_{value:026}")),
        })
    }

    fn scope(value: u8) -> Scope {
        Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: OrganizationId(format!("org_{value:026}")),
        })
    }

    fn bootstrap(proof: &str, actor_value: u8, scopes: Vec<Scope>) -> AuthSessionBootstrap {
        AuthSessionBootstrap::new(proof, actor(actor_value), scopes).expect("bootstrap context")
    }

    fn manager(
        directory: &Path,
        clock: Arc<ManualClock>,
        bootstraps: Vec<AuthSessionBootstrap>,
    ) -> Result<SqliteAuthSessionManager, AuthSessionError> {
        SqliteAuthSessionManager::open_with_dependencies(
            directory,
            bootstraps,
            AuthSessionConfig::new(Duration::from_secs(10), Duration::from_mins(1))?,
            clock,
            Arc::new(SequenceToken::new(7)),
        )
    }

    fn issue(manager: &SqliteAuthSessionManager, proof: &str) -> IssuedBrowserSession {
        manager
            .bootstrap(&TransportCredentials::new(Some(proof.to_owned()), None))
            .expect("bootstrap")
    }

    #[test]
    fn bootstrap_persists_digest_and_secret_free_context_then_revokes() {
        let directory = test_directory("lifecycle");
        let clock = Arc::new(ManualClock::new(1_000));
        let manager = manager(
            &directory,
            Arc::clone(&clock),
            vec![bootstrap("bootstrap-proof", 1, vec![scope(2), scope(1)])],
        )
        .expect("manager");
        let issued = issue(&manager, "bootstrap-proof");
        let raw = issued.cookie_value.clone();
        let response = issued.response().expect("response");

        assert_ne!(raw, "bootstrap-proof");
        assert!(raw.len() >= 43);
        assert_eq!(response.expires_at.0, "1970-01-01T00:01:01Z");
        assert_eq!(response.actor, actor(1));
        assert_eq!(response.authorized_scopes, vec![scope(1), scope(2)]);
        assert!(
            issued
                .set_cookie_header()
                .expect("Set-Cookie")
                .contains("Expires=Thu, 01 Jan 1970 00:01:01 GMT")
        );

        let database_path = directory.join("auth-sessions.sqlite3");
        assert_eq!(
            std::fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&database_path)
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let connection = Connection::open(&database_path).expect("inspect database");
        let row = connection
            .query_row(
                "SELECT session_digest, subject, actor_json, authorized_scopes_json,
                        created_at_millis, expires_at_millis, revoked_at_millis
                 FROM auth_sessions",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .expect("session row");
        assert_eq!(row.0, session_digest(&raw));
        assert_eq!(row.1, "usr_00000000000000000000000001");
        assert_eq!(
            decode_json::<Actor>(&row.2).expect("stored Actor"),
            actor(1)
        );
        assert_eq!(
            decode_json::<Vec<Scope>>(&row.3).expect("stored Scopes"),
            vec![scope(1), scope(2)]
        );
        assert_eq!((row.4, row.5, row.6), (1_000, 61_000, None));
        let dump = std::fs::read(&database_path).expect("database bytes");
        assert!(
            !dump
                .windows(raw.len())
                .any(|window| window == raw.as_bytes())
        );
        assert!(
            !dump
                .windows("bootstrap-proof".len())
                .any(|window| window == b"bootstrap-proof")
        );

        let credentials = TransportCredentials::new(None, Some(raw));
        assert_eq!(manager.current(&credentials).expect("current"), response);
        assert_eq!(
            manager
                .authenticate(&credentials)
                .expect("cookie authenticates")
                .authorized_scopes(),
            [scope(1), scope(2)]
        );
        manager.revoke(&credentials).expect("revoke");
        assert!(manager.current(&credentials).is_err());
        assert!(manager.authenticate(&credentials).is_err());
    }

    #[test]
    fn multi_user_scopes_shrink_and_actor_revocation_are_exact() {
        let directory = test_directory("multi-user");
        let clock = Arc::new(ManualClock::new(2_000));
        let manager = manager(
            &directory,
            clock,
            vec![
                bootstrap("proof-user-one", 1, vec![scope(1), scope(2)]),
                bootstrap("proof-user-two", 2, vec![scope(2)]),
            ],
        )
        .expect("manager");
        let first =
            TransportCredentials::new(None, Some(issue(&manager, "proof-user-one").cookie_value));
        let second =
            TransportCredentials::new(None, Some(issue(&manager, "proof-user-two").cookie_value));

        assert_eq!(manager.current(&first).expect("first").actor, actor(1));
        assert_eq!(manager.current(&second).expect("second").actor, actor(2));
        assert!(
            manager
                .bootstrap(&TransportCredentials::new(
                    Some("proof-user-one".to_owned()),
                    second.session_cookie().map(str::to_owned),
                ))
                .is_err()
        );
        assert_eq!(
            manager
                .replace_authorized_scopes(&actor(1), vec![scope(2)])
                .expect("shrink"),
            1
        );
        assert_eq!(
            manager.current(&first).expect("shrunk").authorized_scopes,
            vec![scope(2)]
        );
        assert_eq!(
            manager
                .current(&second)
                .expect("other unchanged")
                .authorized_scopes,
            vec![scope(2)]
        );
        assert_eq!(
            manager
                .replace_authorized_scopes(&actor(1), Vec::new())
                .expect("empty authorization revokes Actor"),
            1
        );
        assert!(manager.current(&first).is_err());
        assert_eq!(
            manager.current(&second).expect("other active").actor,
            actor(2)
        );
    }

    #[test]
    fn external_identity_principal_uses_canonical_session_store_and_lifecycle_port() {
        let directory = test_directory("external-identity");
        let clock = Arc::new(ManualClock::new(3_000));
        let session_manager = Arc::new(
            manager(
                &directory,
                Arc::clone(&clock),
                vec![bootstrap("unused-bootstrap", 2, vec![scope(2)])],
            )
            .expect("manager"),
        );
        let session_issuer = ExternalIdentitySessionIssuer::new(Arc::clone(&session_manager));
        let outcome = ExternalAuthenticationOutcome {
            principal: ExternalIdentityPrincipal {
                actor: actor(1),
                authorized_scopes: vec![scope(2), scope(1)],
                organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
                external_identity_id: ExternalIdentityId(
                    "xid_00000000000000000000000001".to_owned(),
                ),
            },
            idempotent_replay: false,
        };
        let external_session = session_issuer
            .issue(outcome.clone())
            .expect("issue external session");
        assert!(!external_session.is_idempotent_replay());
        assert!(
            external_session
                .response()
                .expect("session response")
                .is_some()
        );
        assert!(
            external_session
                .set_cookie_header()
                .expect("cookie header")
                .is_some()
        );
        let credentials = TransportCredentials::new(
            None,
            Some(
                external_session
                    .issued
                    .as_ref()
                    .expect("issued session")
                    .cookie_value
                    .clone(),
            ),
        );
        let replay = session_issuer
            .issue(ExternalAuthenticationOutcome {
                idempotent_replay: true,
                ..outcome
            })
            .expect("exact assertion replay");
        assert!(replay.is_idempotent_replay());
        assert!(replay.response().expect("replay response").is_none());
        assert!(replay.set_cookie_header().expect("replay cookie").is_none());
        assert_eq!(
            session_manager
                .current(&credentials)
                .expect("external session")
                .authorized_scopes,
            vec![scope(1), scope(2)]
        );

        let lifecycle: &dyn BrowserSessionLifecyclePort = session_manager.as_ref();
        assert_eq!(
            lifecycle
                .replace_authorized_scopes(&actor(1), vec![scope(2)])
                .expect("shrink through lifecycle port"),
            1
        );
        assert_eq!(
            session_manager
                .current(&credentials)
                .expect("shrunk external session")
                .authorized_scopes,
            vec![scope(2)]
        );
        assert_eq!(
            lifecycle
                .revoke_actor_sessions(&actor(1))
                .expect("revoke through lifecycle port"),
            1
        );
        assert!(session_manager.current(&credentials).is_err());

        drop(session_issuer);
        drop(session_manager);
        let restarted = manager(
            &directory,
            clock,
            vec![bootstrap("after-restart", 2, vec![scope(2)])],
        )
        .expect("restart manager");
        assert!(restarted.current(&credentials).is_err());
    }

    #[test]
    fn expiry_and_restart_restore_only_persisted_current_context() {
        let directory = test_directory("restart");
        let first_clock = Arc::new(ManualClock::new(5_000));
        let first = manager(
            &directory,
            Arc::clone(&first_clock),
            vec![bootstrap("proof-before-restart", 1, vec![scope(1)])],
        )
        .expect("first manager");
        let raw = issue(&first, "proof-before-restart").cookie_value;
        drop(first);

        let restarted = manager(
            &directory,
            Arc::new(ManualClock::new(6_000)),
            vec![bootstrap("proof-after-restart", 2, vec![scope(2)])],
        )
        .expect("restarted manager");
        let credentials = TransportCredentials::new(None, Some(raw));
        let restored = restarted.current(&credentials).expect("persisted session");
        assert_eq!(restored.actor, actor(1));
        assert_eq!(restored.authorized_scopes, vec![scope(1)]);

        first_clock.set(65_000);
        let expiry_directory = test_directory("expiry");
        let expiry_clock = Arc::new(ManualClock::new(10_000));
        let expiry = manager(
            &expiry_directory,
            Arc::clone(&expiry_clock),
            vec![bootstrap("expiry-proof", 1, vec![scope(1)])],
        )
        .expect("expiry manager");
        let expiring =
            TransportCredentials::new(None, Some(issue(&expiry, "expiry-proof").cookie_value));
        expiry_clock.set(70_000);
        assert!(expiry.current(&expiring).is_err());
        expiry_clock.set(20_001);
        assert!(
            expiry
                .bootstrap(&TransportCredentials::new(
                    Some("expiry-proof".to_owned()),
                    None,
                ))
                .is_err()
        );
    }

    #[test]
    fn legacy_rows_without_context_fail_closed_after_schema_upgrade() {
        let directory = test_directory("legacy");
        std::fs::create_dir_all(&directory).expect("directory");
        let database_path = directory.join("auth-sessions.sqlite3");
        let connection = Connection::open(&database_path).expect("legacy database");
        connection
            .execute_batch(
                "CREATE TABLE auth_sessions (
                   session_digest TEXT PRIMARY KEY NOT NULL,
                   subject TEXT NOT NULL,
                   created_at_millis INTEGER NOT NULL,
                   expires_at_millis INTEGER NOT NULL,
                   revoked_at_millis INTEGER
                 );",
            )
            .expect("legacy schema");
        connection
            .execute(
                "INSERT INTO auth_sessions VALUES (?1, ?2, 0, 60000, NULL)",
                params![session_digest("legacy-cookie"), actor_subject(&actor(1))],
            )
            .expect("legacy row");
        drop(connection);

        let manager = manager(
            &directory,
            Arc::new(ManualClock::new(1_000)),
            vec![bootstrap("new-proof", 1, vec![scope(1)])],
        )
        .expect("upgraded manager");
        let legacy = TransportCredentials::new(None, Some("legacy-cookie".to_owned()));
        assert!(manager.current(&legacy).is_err());
        assert!(manager.authenticate(&legacy).is_err());
    }
}
