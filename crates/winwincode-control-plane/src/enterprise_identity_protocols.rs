// SPDX-License-Identifier: Apache-2.0

//! OIDC, SAML, and SCIM protocol boundary for enterprise identity lifecycle.
//!
//! Cryptographic decoders return verified claims through narrow ports. This
//! module owns issuer, audience, time, replay, and SCIM ordering checks, then
//! delegates all principal and authorization changes to the canonical
//! Identity/RBAC lifecycle port.

use std::{
    fmt,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{Actor, OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditSubject,
};
use winwincode_domain::{EnterpriseTeamId, OrganizationId, RequestId, Sha256Digest, UserId};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, PendingAuditEvent, ProductStateStorage, StateCommit,
    StorageError, StorageErrorKind, StoredState,
};

use crate::{
    DeprovisionExternalUser, EnterpriseIdentityLifecycleError, EnterpriseIdentityLifecyclePort,
    ExternalIdentityLifecycleOutcome, ExternalIdentityPrincipal, ExternalIdentityProvider,
    ExternalIdentityReference, ProvisionExternalUser, UpsertExternalTeam, command_receipt_identity,
};

const STATE_SCHEMA: &str = "winwincode.enterprise-identity-protocol.v1";
const STREAM_PREFIX: &str = "enterprise-identity-protocol:";
const AUTH_TOPIC: &str = "enterprise.identity.protocol.authenticated.v1";
const SCIM_TOPIC: &str = "enterprise.identity.protocol.scim-applied.v1";
const INTENT_TOPIC: &str = "enterprise.identity.protocol.scim-intent.v1";
const AUDIT_ORIGIN: &str = "control-plane.enterprise-identity-protocol";
const MAX_CREDENTIAL_BYTES: usize = 1_048_576;
const MAX_IDENTIFIER_BYTES: usize = 2_048;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub struct OidcIdToken(String);

impl OidcIdToken {
    /// Builds one opaque compact OIDC ID Token.
    ///
    /// # Errors
    ///
    /// Rejects empty, whitespace-bearing, control-character, or oversized credentials.
    pub fn new(value: impl Into<String>) -> Result<Self, EnterpriseProtocolError> {
        let value = value.into();
        validate_credential(value.as_bytes())?;
        if value.chars().any(char::is_whitespace) {
            return Err(invalid());
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct SamlResponse(Vec<u8>);

impl SamlResponse {
    /// Builds one opaque signed SAML response.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized protocol messages.
    pub fn new(value: impl Into<Vec<u8>>) -> Result<Self, EnterpriseProtocolError> {
        let value = value.into();
        validate_credential(&value)?;
        Ok(Self(value))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub struct ScimBearerToken(String);

impl ScimBearerToken {
    /// Builds one opaque SCIM bearer credential.
    ///
    /// # Errors
    ///
    /// Rejects empty, whitespace-bearing, control-character, or oversized credentials.
    pub fn new(value: impl Into<String>) -> Result<Self, EnterpriseProtocolError> {
        let value = value.into();
        validate_credential(value.as_bytes())?;
        if value.chars().any(char::is_whitespace) {
            return Err(invalid());
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedOidcClaims {
    pub issuer: String,
    pub audiences: Vec<String>,
    pub subject: String,
    pub token_id: String,
    pub issued_at_millis: u64,
    pub not_before_millis: u64,
    pub expires_at_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSamlClaims {
    pub issuer: String,
    pub audiences: Vec<String>,
    pub subject: String,
    pub assertion_id: String,
    pub issued_at_millis: u64,
    pub not_before_millis: u64,
    pub expires_at_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedScimClient {
    pub issuer: String,
    pub audiences: Vec<String>,
    pub client_id: String,
    pub expires_at_millis: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolVerificationErrorKind {
    InvalidMessage,
    SignatureRejected,
    KeyUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolVerificationError {
    kind: ProtocolVerificationErrorKind,
}

impl ProtocolVerificationError {
    #[must_use]
    pub const fn invalid_message() -> Self {
        Self {
            kind: ProtocolVerificationErrorKind::InvalidMessage,
        }
    }

    #[must_use]
    pub const fn signature_rejected() -> Self {
        Self {
            kind: ProtocolVerificationErrorKind::SignatureRejected,
        }
    }

    #[must_use]
    pub const fn key_unavailable() -> Self {
        Self {
            kind: ProtocolVerificationErrorKind::KeyUnavailable,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ProtocolVerificationErrorKind {
        self.kind
    }
}

impl fmt::Display for ProtocolVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("enterprise identity protocol verification failed")
    }
}

impl std::error::Error for ProtocolVerificationError {}

pub trait OidcTokenVerifier: Send {
    /// Verifies the compact token signature and returns only signed claims.
    ///
    /// # Errors
    ///
    /// Separates malformed input, rejected signatures, and unavailable key material.
    fn verify(&mut self, token: &str) -> Result<VerifiedOidcClaims, ProtocolVerificationError>;
}

pub trait SamlResponseVerifier: Send {
    /// Verifies XML signature coverage and returns only signed assertion claims.
    ///
    /// # Errors
    ///
    /// Separates malformed input, rejected signatures, and unavailable key material.
    fn verify(&mut self, response: &[u8]) -> Result<VerifiedSamlClaims, ProtocolVerificationError>;
}

pub trait ScimBearerVerifier: Send {
    /// Verifies a SCIM bearer and returns its signed client claims.
    ///
    /// # Errors
    ///
    /// Separates malformed input, rejected signatures, and unavailable key material.
    fn verify(&mut self, bearer: &str) -> Result<VerifiedScimClient, ProtocolVerificationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnterpriseProtocolClockError;

pub trait EnterpriseProtocolClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when time is unavailable or out of range.
    fn now_millis(&mut self) -> Result<u64, EnterpriseProtocolClockError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnterpriseProtocolClock;

impl EnterpriseProtocolClock for SystemEnterpriseProtocolClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseProtocolClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EnterpriseProtocolClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| EnterpriseProtocolClockError)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedProtocolParty {
    pub issuer: String,
    pub audience: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnterpriseIdentityProtocolConfig {
    pub organization_id: OrganizationId,
    pub management_actor: Actor,
    pub oidc: TrustedProtocolParty,
    pub saml: TrustedProtocolParty,
    pub scim: TrustedProtocolParty,
    pub max_clock_skew_millis: u64,
    pub max_assertion_age_millis: u64,
}

impl EnterpriseIdentityProtocolConfig {
    /// Validates one tenant-bound protocol configuration.
    ///
    /// # Errors
    ///
    /// Rejects empty or unbounded trust facts and unsafe time limits.
    pub fn validate(&self) -> Result<(), EnterpriseProtocolError> {
        for party in [&self.oidc, &self.saml, &self.scim] {
            validate_identifier(&party.issuer)?;
            validate_identifier(&party.audience)?;
        }
        if self.max_clock_skew_millis > 3_600_000
            || self.max_assertion_age_millis == 0
            || self.max_assertion_age_millis > 86_400_000
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScimUserProvision {
    pub external_subject: String,
    pub user_id: UserId,
    pub display_name: String,
    pub authorized_scopes: Vec<Scope>,
    pub team_ids: Vec<EnterpriseTeamId>,
    pub role_assignments: Vec<winwincode_api::generated::EnterpriseRoleAssignment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScimUserDeprovision {
    pub external_subject: String,
    pub user_id: UserId,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScimTeamUpsert {
    pub team_id: EnterpriseTeamId,
    pub display_name: String,
    pub state: String,
    pub role_assignments: Vec<winwincode_api::generated::EnterpriseRoleAssignment>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScimOperation {
    ProvisionUser(ScimUserProvision),
    DeprovisionUser(ScimUserDeprovision),
    UpsertTeam(ScimTeamUpsert),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScimLifecycleEvent {
    pub event_id: String,
    pub sequence: u64,
    pub operation: ScimOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseProtocolErrorKind {
    InvalidRequest,
    SignatureRejected,
    VerificationUnavailable,
    IssuerMismatch,
    AudienceMismatch,
    Expired,
    NotYetValid,
    ReplayConflict,
    OutOfOrder,
    SubjectBusy,
    LifecycleRejected,
    StorageUnavailable,
    ClockUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseProtocolError {
    kind: EnterpriseProtocolErrorKind,
}

impl EnterpriseProtocolError {
    const fn new(kind: EnterpriseProtocolErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> EnterpriseProtocolErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            EnterpriseProtocolErrorKind::InvalidRequest => "identity protocol request is invalid",
            EnterpriseProtocolErrorKind::SignatureRejected => {
                "identity protocol signature was rejected"
            }
            EnterpriseProtocolErrorKind::VerificationUnavailable => {
                "identity protocol verification is unavailable"
            }
            EnterpriseProtocolErrorKind::IssuerMismatch => {
                "identity protocol issuer does not match"
            }
            EnterpriseProtocolErrorKind::AudienceMismatch => {
                "identity protocol audience does not match"
            }
            EnterpriseProtocolErrorKind::Expired => "identity protocol credential expired",
            EnterpriseProtocolErrorKind::NotYetValid => {
                "identity protocol credential is not yet valid"
            }
            EnterpriseProtocolErrorKind::ReplayConflict => {
                "identity protocol replay changed immutable input"
            }
            EnterpriseProtocolErrorKind::OutOfOrder => "SCIM lifecycle event arrived out of order",
            EnterpriseProtocolErrorKind::SubjectBusy => {
                "SCIM subject has another durable event in progress"
            }
            EnterpriseProtocolErrorKind::LifecycleRejected => {
                "canonical identity lifecycle rejected the protocol event"
            }
            EnterpriseProtocolErrorKind::StorageUnavailable => {
                "identity protocol durable replay storage is unavailable"
            }
            EnterpriseProtocolErrorKind::ClockUnavailable => {
                "identity protocol clock is unavailable"
            }
        })
    }
}

impl std::error::Error for EnterpriseProtocolError {}

pub struct EnterpriseIdentityProtocolAdapter {
    inner: Mutex<EnterpriseIdentityProtocolInner>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalAuthenticationOutcome {
    pub principal: ExternalIdentityPrincipal,
    pub idempotent_replay: bool,
}

struct EnterpriseIdentityProtocolInner {
    storage: Box<dyn ProductStateStorage>,
    lifecycle: Box<dyn EnterpriseIdentityLifecyclePort>,
    oidc_verifier: Box<dyn OidcTokenVerifier>,
    saml_verifier: Box<dyn SamlResponseVerifier>,
    scim_verifier: Box<dyn ScimBearerVerifier>,
    clock: Box<dyn EnterpriseProtocolClock>,
    config: EnterpriseIdentityProtocolConfig,
}

impl EnterpriseIdentityProtocolAdapter {
    /// Builds one tenant-bound protocol adapter over canonical lifecycle ports.
    ///
    /// # Errors
    ///
    /// Rejects malformed trust or time configuration.
    pub fn new(
        storage: Box<dyn ProductStateStorage>,
        lifecycle: Box<dyn EnterpriseIdentityLifecyclePort>,
        oidc_verifier: Box<dyn OidcTokenVerifier>,
        saml_verifier: Box<dyn SamlResponseVerifier>,
        scim_verifier: Box<dyn ScimBearerVerifier>,
        config: EnterpriseIdentityProtocolConfig,
    ) -> Result<Self, EnterpriseProtocolError> {
        Self::with_clock(
            storage,
            lifecycle,
            oidc_verifier,
            saml_verifier,
            scim_verifier,
            Box::new(SystemEnterpriseProtocolClock),
            config,
        )
    }

    /// Builds one adapter with an injected deterministic clock.
    ///
    /// # Errors
    ///
    /// Rejects malformed trust or time configuration.
    pub fn with_clock(
        storage: Box<dyn ProductStateStorage>,
        lifecycle: Box<dyn EnterpriseIdentityLifecyclePort>,
        oidc_verifier: Box<dyn OidcTokenVerifier>,
        saml_verifier: Box<dyn SamlResponseVerifier>,
        scim_verifier: Box<dyn ScimBearerVerifier>,
        clock: Box<dyn EnterpriseProtocolClock>,
        config: EnterpriseIdentityProtocolConfig,
    ) -> Result<Self, EnterpriseProtocolError> {
        config.validate()?;
        Ok(Self {
            inner: Mutex::new(EnterpriseIdentityProtocolInner {
                storage,
                lifecycle,
                oidc_verifier,
                saml_verifier,
                scim_verifier,
                clock,
                config,
            }),
        })
    }

    /// Authenticates one OIDC callback with durable exact replay.
    ///
    /// # Errors
    ///
    /// Rejects signature, issuer, audience, time, replay, lifecycle, and storage failures.
    pub fn authenticate_oidc(
        &self,
        token: &OidcIdToken,
    ) -> Result<ExternalAuthenticationOutcome, EnterpriseProtocolError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .authenticate_oidc(token)
    }

    /// Authenticates one SAML response with durable exact replay.
    ///
    /// # Errors
    ///
    /// Rejects signature, issuer, audience, time, replay, lifecycle, and storage failures.
    pub fn authenticate_saml(
        &self,
        response: &SamlResponse,
    ) -> Result<ExternalAuthenticationOutcome, EnterpriseProtocolError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .authenticate_saml(response)
    }

    /// Applies one authenticated and monotonically ordered SCIM lifecycle event.
    ///
    /// # Errors
    ///
    /// Rejects bearer, issuer, audience, time, replay, ordering, lifecycle, and storage failures.
    pub fn apply_scim(
        &self,
        bearer: &ScimBearerToken,
        event: &ScimLifecycleEvent,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseProtocolError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .apply_scim(bearer, event)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthenticationReceipt {
    response: ExternalIdentityPrincipalWire,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExternalIdentityPrincipalWire {
    actor: Actor,
    authorized_scopes: Vec<Scope>,
    organization_id: OrganizationId,
    external_identity_id: winwincode_domain::ExternalIdentityId,
}

impl From<ExternalIdentityPrincipal> for ExternalIdentityPrincipalWire {
    fn from(value: ExternalIdentityPrincipal) -> Self {
        Self {
            actor: value.actor,
            authorized_scopes: value.authorized_scopes,
            organization_id: value.organization_id,
            external_identity_id: value.external_identity_id,
        }
    }
}

impl From<ExternalIdentityPrincipalWire> for ExternalIdentityPrincipal {
    fn from(value: ExternalIdentityPrincipalWire) -> Self {
        Self {
            actor: value.actor,
            authorized_scopes: value.authorized_scopes,
            organization_id: value.organization_id,
            external_identity_id: value.external_identity_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScimReceipt {
    response: ScimOutcomeWire,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ScimOutcomeWire {
    User(ExternalIdentityPrincipalWire),
    Team(winwincode_api::generated::EnterpriseTeamProjection),
}

impl From<ExternalIdentityLifecycleOutcome> for ScimOutcomeWire {
    fn from(value: ExternalIdentityLifecycleOutcome) -> Self {
        match value {
            ExternalIdentityLifecycleOutcome::User(principal) => Self::User(principal.into()),
            ExternalIdentityLifecycleOutcome::Team(team) => Self::Team(team),
        }
    }
}

impl From<ScimOutcomeWire> for ExternalIdentityLifecycleOutcome {
    fn from(value: ScimOutcomeWire) -> Self {
        match value {
            ScimOutcomeWire::User(principal) => Self::User(principal.into()),
            ScimOutcomeWire::Team(team) => Self::Team(team),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScimCursorState {
    schema: String,
    last_sequence: u64,
    pending: Option<ScimPending>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScimPending {
    event_id: String,
    event_sha256: Sha256Digest,
    sequence: u64,
}

#[derive(Clone, Copy)]
struct SignedClaimValidation<'a> {
    issuer: &'a str,
    audiences: &'a [String],
    subject: &'a str,
    replay_id: &'a str,
    issued_at: u64,
    not_before: u64,
    expires_at: u64,
}

#[derive(Clone, Copy)]
struct ScimPhase<'a> {
    stream_id: &'a str,
    expected_revision: u64,
    cursor: &'a ScimCursorState,
    event_id: &'a str,
    phase: &'a [u8],
    event_digest: &'a Sha256Digest,
    topic: &'static str,
}

impl EnterpriseIdentityProtocolInner {
    fn authenticate_oidc(
        &mut self,
        token: &OidcIdToken,
    ) -> Result<ExternalAuthenticationOutcome, EnterpriseProtocolError> {
        let claims = self
            .oidc_verifier
            .verify(token.as_str())
            .map_err(|error| verification_error(&error))?;
        let now = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        validate_signed_claims(
            SignedClaimValidation {
                issuer: &claims.issuer,
                audiences: &claims.audiences,
                subject: &claims.subject,
                replay_id: &claims.token_id,
                issued_at: claims.issued_at_millis,
                not_before: claims.not_before_millis,
                expires_at: claims.expires_at_millis,
            },
            &self.config.oidc,
            now,
            &self.config,
        )?;
        let digest = digest_serializable(&(
            "oidc",
            &claims.issuer,
            &claims.audiences,
            &claims.subject,
            &claims.token_id,
            claims.issued_at_millis,
            claims.not_before_millis,
            claims.expires_at_millis,
        ))?;
        self.authenticate_verified(
            ExternalIdentityProvider::Oidc,
            &claims.issuer,
            &claims.subject,
            &claims.token_id,
            &digest,
        )
    }

    fn authenticate_saml(
        &mut self,
        response: &SamlResponse,
    ) -> Result<ExternalAuthenticationOutcome, EnterpriseProtocolError> {
        let claims = self
            .saml_verifier
            .verify(response.as_bytes())
            .map_err(|error| verification_error(&error))?;
        let now = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        validate_signed_claims(
            SignedClaimValidation {
                issuer: &claims.issuer,
                audiences: &claims.audiences,
                subject: &claims.subject,
                replay_id: &claims.assertion_id,
                issued_at: claims.issued_at_millis,
                not_before: claims.not_before_millis,
                expires_at: claims.expires_at_millis,
            },
            &self.config.saml,
            now,
            &self.config,
        )?;
        let digest = digest_serializable(&(
            "saml",
            &claims.issuer,
            &claims.audiences,
            &claims.subject,
            &claims.assertion_id,
            claims.issued_at_millis,
            claims.not_before_millis,
            claims.expires_at_millis,
        ))?;
        self.authenticate_verified(
            ExternalIdentityProvider::Saml,
            &claims.issuer,
            &claims.subject,
            &claims.assertion_id,
            &digest,
        )
    }

    fn authenticate_verified(
        &mut self,
        provider: ExternalIdentityProvider,
        issuer: &str,
        subject: &str,
        replay_id: &str,
        digest: &Sha256Digest,
    ) -> Result<ExternalAuthenticationOutcome, EnterpriseProtocolError> {
        let phase = provider.as_str().as_bytes();
        let request_id = derived_request_id(replay_id, phase);
        let receipt_identity = self.receipt_identity(request_id.clone())?;
        let reference = ExternalIdentityReference {
            organization_id: self.config.organization_id.clone(),
            provider,
            issuer_sha256: sha256(issuer.as_bytes()),
            subject_sha256: sha256(subject.as_bytes()),
        };
        if let Some(receipt) = self.storage.load_receipt(&receipt_identity, digest)? {
            return self.replay_current_authentication(&receipt, &reference);
        }
        let principal = self
            .lifecycle
            .authenticate_external(&reference)
            .map_err(lifecycle_error)?;
        let payload = serde_json::to_vec(&AuthenticationReceipt {
            response: principal.clone().into(),
        })
        .map_err(|_| invalid())?;
        let state_payload = payload.clone();
        let stream_id = replay_stream(provider.as_str(), replay_id);
        let before = self.storage.load_state(&stream_id)?;
        let expected = before.as_ref().map_or(0, |state| state.revision);
        if before.is_some() {
            return Err(replay_conflict());
        }
        let event_id = protocol_event_id(AUTH_TOPIC, digest);
        let audit = pending_audit(
            &self.config,
            request_id,
            "external_identity.authenticate",
            None,
            &state_payload,
            &event_id,
            self.clock.now_millis().map_err(|_| clock_unavailable())?,
        )?;
        let commit = StateCommit::new(
            receipt_identity.clone(),
            digest.clone(),
            stream_id,
            expected,
            state_payload,
            vec![NewOutboxEvent::internal(event_id, AUTH_TOPIC, payload)],
        )
        .with_pending_audit_event(audit);
        match self.storage.commit(&commit) {
            Ok(_) => Ok(ExternalAuthenticationOutcome {
                principal,
                idempotent_replay: false,
            }),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => self
                .storage
                .load_receipt(&receipt_identity, digest)?
                .map_or_else(
                    || Err(replay_conflict()),
                    |receipt| self.replay_current_authentication(&receipt, &reference),
                ),
            Err(error) => Err(error.into()),
        }
    }

    fn replay_current_authentication(
        &mut self,
        receipt: &CommitReceipt,
        reference: &ExternalIdentityReference,
    ) -> Result<ExternalAuthenticationOutcome, EnterpriseProtocolError> {
        let recorded = replay_authentication(receipt)?;
        let current = self
            .lifecycle
            .authenticate_external(reference)
            .map_err(lifecycle_error)?;
        if recorded.actor != current.actor
            || recorded.organization_id != current.organization_id
            || recorded.external_identity_id != current.external_identity_id
        {
            return Err(storage_unavailable());
        }
        Ok(ExternalAuthenticationOutcome {
            principal: current,
            idempotent_replay: true,
        })
    }

    fn apply_scim(
        &mut self,
        bearer: &ScimBearerToken,
        event: &ScimLifecycleEvent,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseProtocolError> {
        validate_scim_event(event)?;
        let now = self.verify_scim_bearer(bearer)?;
        let event_digest = digest_serializable(event)?;
        let final_request_id = derived_request_id(&event.event_id, b"scim-final");
        let final_identity = self.receipt_identity(final_request_id.clone())?;
        if let Some(receipt) = self.storage.load_receipt(&final_identity, &event_digest)? {
            return replay_scim(&receipt);
        }

        let stream_id = scim_subject_stream(event);
        let stored = self.storage.load_state(&stream_id)?;
        let mut cursor = decode_scim_cursor(stored.as_ref())?;
        let pending = ScimPending {
            event_id: event.event_id.clone(),
            event_sha256: event_digest.clone(),
            sequence: event.sequence,
        };
        let intent_revision = if let Some(current) = &cursor.pending {
            if current != &pending {
                return Err(EnterpriseProtocolError::new(
                    EnterpriseProtocolErrorKind::SubjectBusy,
                ));
            }
            stored.as_ref().map_or(0, |state| state.revision)
        } else {
            if event.sequence <= cursor.last_sequence {
                return Err(EnterpriseProtocolError::new(
                    EnterpriseProtocolErrorKind::OutOfOrder,
                ));
            }
            cursor.pending = Some(pending.clone());
            self.commit_scim_phase(ScimPhase {
                stream_id: &stream_id,
                expected_revision: stored.as_ref().map_or(0, |state| state.revision),
                cursor: &cursor,
                event_id: &event.event_id,
                phase: b"scim-intent",
                event_digest: &event_digest,
                topic: INTENT_TOPIC,
            })?
        };

        let outcome = match self.apply_scim_lifecycle(event) {
            Ok(outcome) => outcome,
            Err(error) => return Err(lifecycle_error(error)),
        };
        cursor.pending = None;
        cursor.last_sequence = event.sequence;
        let next_payload = serde_json::to_vec(&cursor).map_err(|_| invalid())?;
        let before_payload = stored.as_ref().map(|state| state.payload.as_slice());
        let receipt_payload = serde_json::to_vec(&ScimReceipt {
            response: outcome.clone().into(),
        })
        .map_err(|_| invalid())?;
        let event_id = protocol_event_id(SCIM_TOPIC, &event_digest);
        let audit = pending_audit(
            &self.config,
            final_request_id,
            scim_action(&event.operation),
            before_payload,
            &next_payload,
            &event_id,
            now,
        )?;
        let commit = StateCommit::new(
            final_identity.clone(),
            event_digest.clone(),
            stream_id,
            intent_revision,
            next_payload,
            vec![NewOutboxEvent::internal(
                event_id,
                SCIM_TOPIC,
                receipt_payload,
            )],
        )
        .with_pending_audit_event(audit);
        match self.storage.commit(&commit) {
            Ok(_) => Ok(outcome),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => self
                .storage
                .load_receipt(&final_identity, &event_digest)?
                .map_or_else(
                    || Err(storage_unavailable()),
                    |receipt| replay_scim(&receipt),
                ),
            Err(error) => Err(error.into()),
        }
    }

    fn verify_scim_bearer(
        &mut self,
        bearer: &ScimBearerToken,
    ) -> Result<u64, EnterpriseProtocolError> {
        let client = self
            .scim_verifier
            .verify(bearer.as_str())
            .map_err(|error| verification_error(&error))?;
        validate_identifier(&client.client_id)?;
        if client.issuer != self.config.scim.issuer {
            return Err(EnterpriseProtocolError::new(
                EnterpriseProtocolErrorKind::IssuerMismatch,
            ));
        }
        if !client
            .audiences
            .iter()
            .any(|audience| audience == &self.config.scim.audience)
        {
            return Err(EnterpriseProtocolError::new(
                EnterpriseProtocolErrorKind::AudienceMismatch,
            ));
        }
        let now = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        if client.expires_at_millis <= now {
            return Err(EnterpriseProtocolError::new(
                EnterpriseProtocolErrorKind::Expired,
            ));
        }
        Ok(now)
    }

    fn apply_scim_lifecycle(
        &mut self,
        event: &ScimLifecycleEvent,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError> {
        match &event.operation {
            ScimOperation::ProvisionUser(user) => {
                self.lifecycle.provision_user(&ProvisionExternalUser {
                    operation_id: event.event_id.clone(),
                    identity: ExternalIdentityReference {
                        organization_id: self.config.organization_id.clone(),
                        provider: ExternalIdentityProvider::Scim,
                        issuer_sha256: sha256(self.config.scim.issuer.as_bytes()),
                        subject_sha256: sha256(user.external_subject.as_bytes()),
                    },
                    user_id: user.user_id.clone(),
                    display_name: user.display_name.clone(),
                    authorized_scopes: user.authorized_scopes.clone(),
                    team_ids: user.team_ids.clone(),
                    role_assignments: user.role_assignments.clone(),
                })
            }
            ScimOperation::DeprovisionUser(user) => {
                self.lifecycle.deprovision_user(&DeprovisionExternalUser {
                    operation_id: event.event_id.clone(),
                    identity: ExternalIdentityReference {
                        organization_id: self.config.organization_id.clone(),
                        provider: ExternalIdentityProvider::Scim,
                        issuer_sha256: sha256(self.config.scim.issuer.as_bytes()),
                        subject_sha256: sha256(user.external_subject.as_bytes()),
                    },
                    user_id: user.user_id.clone(),
                })
            }
            ScimOperation::UpsertTeam(team) => self.lifecycle.upsert_team(&UpsertExternalTeam {
                operation_id: event.event_id.clone(),
                organization_id: self.config.organization_id.clone(),
                team_id: team.team_id.clone(),
                display_name: team.display_name.clone(),
                state: team.state.clone(),
                role_assignments: team.role_assignments.clone(),
            }),
        }
    }

    fn commit_scim_phase(&mut self, phase: ScimPhase<'_>) -> Result<u64, EnterpriseProtocolError> {
        let request_id = derived_request_id(phase.event_id, phase.phase);
        let receipt_identity = self.receipt_identity(request_id)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&receipt_identity, phase.event_digest)?
        {
            return Ok(receipt.revision);
        }
        let payload = serde_json::to_vec(phase.cursor).map_err(|_| invalid())?;
        let commit = StateCommit::new(
            receipt_identity.clone(),
            phase.event_digest.clone(),
            phase.stream_id.to_owned(),
            phase.expected_revision,
            payload.clone(),
            vec![NewOutboxEvent::internal(
                protocol_event_id(phase.topic, phase.event_digest),
                phase.topic,
                payload,
            )],
        );
        match self.storage.commit(&commit) {
            Ok(receipt) => Ok(receipt.revision),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => self
                .storage
                .load_receipt(&receipt_identity, phase.event_digest)?
                .map_or_else(
                    || Err(storage_unavailable()),
                    |receipt| Ok(receipt.revision),
                ),
            Err(error) => Err(error.into()),
        }
    }

    fn receipt_identity(
        &self,
        request_id: RequestId,
    ) -> Result<winwincode_storage::ReceiptIdentity, EnterpriseProtocolError> {
        command_receipt_identity(
            &self.config.management_actor,
            &Scope::OrganizationScope(organization_scope(&self.config.organization_id)),
            request_id,
        )
        .map_err(|_| invalid())
    }
}

fn validate_signed_claims(
    claims: SignedClaimValidation<'_>,
    trusted: &TrustedProtocolParty,
    now: u64,
    config: &EnterpriseIdentityProtocolConfig,
) -> Result<(), EnterpriseProtocolError> {
    validate_identifier(claims.issuer)?;
    validate_identifier(claims.subject)?;
    validate_identifier(claims.replay_id)?;
    if claims.audiences.is_empty() || claims.audiences.len() > 32 {
        return Err(invalid());
    }
    for audience in claims.audiences {
        validate_identifier(audience)?;
    }
    if claims.issuer != trusted.issuer {
        return Err(EnterpriseProtocolError::new(
            EnterpriseProtocolErrorKind::IssuerMismatch,
        ));
    }
    if !claims
        .audiences
        .iter()
        .any(|audience| audience == &trusted.audience)
    {
        return Err(EnterpriseProtocolError::new(
            EnterpriseProtocolErrorKind::AudienceMismatch,
        ));
    }
    let skew = config.max_clock_skew_millis;
    if claims.issued_at > now.saturating_add(skew) || claims.not_before > now.saturating_add(skew) {
        return Err(EnterpriseProtocolError::new(
            EnterpriseProtocolErrorKind::NotYetValid,
        ));
    }
    if claims.expires_at <= now.saturating_sub(skew)
        || claims.expires_at <= claims.not_before
        || claims.expires_at <= claims.issued_at
    {
        return Err(EnterpriseProtocolError::new(
            EnterpriseProtocolErrorKind::Expired,
        ));
    }
    if claims
        .issued_at
        .saturating_add(config.max_assertion_age_millis)
        .saturating_add(skew)
        < now
    {
        return Err(EnterpriseProtocolError::new(
            EnterpriseProtocolErrorKind::Expired,
        ));
    }
    Ok(())
}

fn validate_scim_event(event: &ScimLifecycleEvent) -> Result<(), EnterpriseProtocolError> {
    validate_identifier(&event.event_id)?;
    if event.sequence == 0 || event.sequence > MAX_SAFE_INTEGER {
        return Err(invalid());
    }
    match &event.operation {
        ScimOperation::ProvisionUser(user) => {
            validate_identifier(&user.external_subject)?;
            validate_identifier(&user.display_name)?;
        }
        ScimOperation::DeprovisionUser(user) => validate_identifier(&user.external_subject)?,
        ScimOperation::UpsertTeam(team) => validate_identifier(&team.display_name)?,
    }
    Ok(())
}

fn decode_scim_cursor(
    stored: Option<&StoredState>,
) -> Result<ScimCursorState, EnterpriseProtocolError> {
    let Some(stored) = stored else {
        return Ok(ScimCursorState {
            schema: STATE_SCHEMA.to_owned(),
            last_sequence: 0,
            pending: None,
        });
    };
    let cursor: ScimCursorState =
        serde_json::from_slice(&stored.payload).map_err(|_| storage_unavailable())?;
    if cursor.schema != STATE_SCHEMA || cursor.last_sequence > MAX_SAFE_INTEGER {
        return Err(storage_unavailable());
    }
    Ok(cursor)
}

fn replay_authentication(
    receipt: &CommitReceipt,
) -> Result<ExternalIdentityPrincipal, EnterpriseProtocolError> {
    let event = exact_receipt_event(receipt, AUTH_TOPIC)?;
    let response: AuthenticationReceipt =
        serde_json::from_slice(&event.payload).map_err(|_| storage_unavailable())?;
    Ok(response.response.into())
}

fn replay_scim(
    receipt: &CommitReceipt,
) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseProtocolError> {
    let event = exact_receipt_event(receipt, SCIM_TOPIC)?;
    let response: ScimReceipt =
        serde_json::from_slice(&event.payload).map_err(|_| storage_unavailable())?;
    Ok(response.response.into())
}

fn exact_receipt_event<'a>(
    receipt: &'a CommitReceipt,
    topic: &str,
) -> Result<&'a winwincode_storage::OutboxEvent, EnterpriseProtocolError> {
    let events = receipt
        .events
        .iter()
        .filter(|event| event.topic == topic)
        .collect::<Vec<_>>();
    let [event] = events.as_slice() else {
        return Err(storage_unavailable());
    };
    Ok(event)
}

fn pending_audit(
    config: &EnterpriseIdentityProtocolConfig,
    request_id: RequestId,
    action: &str,
    before: Option<&[u8]>,
    after: &[u8],
    event_id: &str,
    now_millis: u64,
) -> Result<PendingAuditEvent, EnterpriseProtocolError> {
    let event = AuditEvent::state_change(
        AuditEventId::from_digest(&sha256(event_id.as_bytes())).map_err(|_| invalid())?,
        now_millis,
        audit_actor(&config.management_actor),
        AuditScope::organization(config.organization_id.clone()).map_err(|_| invalid())?,
        request_id,
        AuditAction::administration(action).map_err(|_| invalid())?,
        AuditState::changed(before.map(sha256), sha256(after)).map_err(|_| invalid())?,
        AuditOrigin::local(AUDIT_ORIGIN).map_err(|_| invalid())?,
        AuditSubject::new(),
        "completed",
        AuditRetention::Indefinite,
    )
    .map_err(|_| invalid())?;
    PendingAuditEvent::new(
        event.event_id().as_str(),
        serde_json::to_vec(&event).map_err(|_| invalid())?,
    )
    .map_err(Into::into)
}

fn audit_actor(actor: &Actor) -> AuditActor {
    match actor {
        Actor::UserActor(actor) => AuditActor::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => AuditActor::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => AuditActor::System(actor.id.clone()),
    }
}

fn scim_subject_stream(event: &ScimLifecycleEvent) -> String {
    let subject = match &event.operation {
        ScimOperation::ProvisionUser(user) => user.user_id.0.as_bytes(),
        ScimOperation::DeprovisionUser(user) => user.user_id.0.as_bytes(),
        ScimOperation::UpsertTeam(team) => team.team_id.0.as_bytes(),
    };
    format!("{STREAM_PREFIX}scim:{:x}", Sha256::digest(subject))
}

fn replay_stream(protocol: &str, replay_id: &str) -> String {
    let digest = Sha256::new()
        .chain_update(protocol.as_bytes())
        .chain_update([0])
        .chain_update(replay_id.as_bytes())
        .finalize();
    format!("{STREAM_PREFIX}authentication:{digest:x}")
}

fn protocol_event_id(topic: &str, digest: &Sha256Digest) -> String {
    format!(
        "protocol_{:x}",
        Sha256::new()
            .chain_update(topic.as_bytes())
            .chain_update(digest.0.as_bytes())
            .finalize()
    )
}

fn scim_action(operation: &ScimOperation) -> &'static str {
    match operation {
        ScimOperation::ProvisionUser(_) => "scim.user.provision",
        ScimOperation::DeprovisionUser(_) => "scim.user.deprovision",
        ScimOperation::UpsertTeam(team) if team.state == "archived" => "scim.team.deprovision",
        ScimOperation::UpsertTeam(_) => "scim.team.upsert",
    }
}

fn organization_scope(organization_id: &OrganizationId) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: organization_id.clone(),
    }
}

fn derived_request_id(identity: &str, phase: &[u8]) -> RequestId {
    let digest = Sha256::new()
        .chain_update(b"winwincode.enterprise-identity-protocol-request.v1\0")
        .chain_update((identity.len() as u64).to_be_bytes())
        .chain_update(identity.as_bytes())
        .chain_update((phase.len() as u64).to_be_bytes())
        .chain_update(phase)
        .finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    RequestId(format!("req_{suffix}"))
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn digest_serializable<T: Serialize + ?Sized>(
    value: &T,
) -> Result<Sha256Digest, EnterpriseProtocolError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(|_| invalid())
}

fn validate_credential(value: &[u8]) -> Result<(), EnterpriseProtocolError> {
    if value.is_empty() || value.len() > MAX_CREDENTIAL_BYTES || value.contains(&0) {
        return Err(invalid());
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Result<(), EnterpriseProtocolError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    Ok(())
}

fn verification_error(error: &ProtocolVerificationError) -> EnterpriseProtocolError {
    match error.kind() {
        ProtocolVerificationErrorKind::InvalidMessage => invalid(),
        ProtocolVerificationErrorKind::SignatureRejected => {
            EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::SignatureRejected)
        }
        ProtocolVerificationErrorKind::KeyUnavailable => {
            EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::VerificationUnavailable)
        }
    }
}

fn lifecycle_error(_error: EnterpriseIdentityLifecycleError) -> EnterpriseProtocolError {
    EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::LifecycleRejected)
}

const fn invalid() -> EnterpriseProtocolError {
    EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::InvalidRequest)
}

const fn replay_conflict() -> EnterpriseProtocolError {
    EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::ReplayConflict)
}

const fn storage_unavailable() -> EnterpriseProtocolError {
    EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::StorageUnavailable)
}

const fn clock_unavailable() -> EnterpriseProtocolError {
    EnterpriseProtocolError::new(EnterpriseProtocolErrorKind::ClockUnavailable)
}

impl From<StorageError> for EnterpriseProtocolError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => invalid(),
            StorageErrorKind::RequestConflict => replay_conflict(),
            StorageErrorKind::RevisionConflict
            | StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => storage_unavailable(),
        }
    }
}
