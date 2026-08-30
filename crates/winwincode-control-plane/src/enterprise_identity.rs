// SPDX-License-Identifier: Apache-2.0

//! Durable enterprise principals and one-way API Token authentication.
//!
//! Web sessions remain owned by the Server session authority. This module owns
//! only Service Accounts, External Identity bindings, and API Token verifiers.

use std::{
    collections::BTreeMap,
    fmt,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, EnterpriseApiTokenIssuePayload, EnterpriseApiTokenProjection,
    EnterpriseApiTokenProjectionKind, EnterpriseApiTokenRevokePayload,
    EnterpriseExternalIdentityLinkPayload, EnterpriseExternalIdentityProjection,
    EnterpriseExternalIdentityProjectionKind, EnterpriseExternalIdentityRevokePayload,
    EnterpriseIdentityListQuery, EnterpriseIdentityListResultResponse,
    EnterpriseIdentityListResultResponseQuery, EnterpriseIdentityPage, EnterpriseIdentityPageKind,
    EnterpriseIdentityProjection, EnterpriseIdentityUpdateCommand,
    EnterpriseIdentityUpdateCompletedResponse, EnterpriseIdentityUpdateCompletedResponseCommand,
    EnterpriseIdentityUpdateCompletedResponseOutcome, EnterpriseIdentityUpdatePayload,
    EnterpriseServiceAccountProjection, EnterpriseServiceAccountProjectionKind,
    EnterpriseServiceAccountRevokePayload, EnterpriseServiceAccountUpsertPayload, PageInfo, Scope,
    ServiceAccountActor, ServiceAccountActorKind, UserActor, UserActorKind,
};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditSubject,
};
use winwincode_domain::{
    ApiTokenId, ExternalIdentityId, Instant, OpaqueCursor, OrganizationId, Revision, SchemaVersion,
    ServiceAccountId, Sha256Digest,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, PendingAuditEvent, ProductStateStorage, StateCommit,
    StateMutation, StateRevisionGuard, StorageError, StorageErrorKind, StoredState,
};

use crate::{command_receipt_identity, instant_from_millis};

const STATE_SCHEMA: &str = "winwincode.enterprise-identity.v1";
const CATALOG_SCHEMA: &str = "winwincode.enterprise-identity.catalog.v1";
const STREAM_PREFIX: &str = "enterprise-identity:";
const EVENT_TOPIC: &str = "enterprise.identity.lifecycle.v1";
const AUDIT_ORIGIN: &str = "control-plane.enterprise-identity";
const CURSOR_SCHEMA: u8 = 1;
const MAX_PAGE_SIZE: usize = 200;
const MAX_CURSOR_BYTES: usize = 2_048;
const MAX_COMMIT_ATTEMPTS: usize = 32;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const TOKEN_PREFIX: &str = "wwc_api_";
const TOKEN_SECRET_BYTES: usize = 32;
const TOKEN_SECRET_ENCODED_LEN: usize = 43;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseIdentityErrorKind {
    InvalidRequest,
    ScopeDenied,
    NotFound,
    WrongState,
    RevisionConflict,
    RequestConflict,
    Authentication,
    Storage,
    ClockUnavailable,
    EntropyUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseIdentityError {
    kind: EnterpriseIdentityErrorKind,
    message: &'static str,
}

impl EnterpriseIdentityError {
    const fn new(kind: EnterpriseIdentityErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            EnterpriseIdentityErrorKind::InvalidRequest,
            "Enterprise identity request is invalid",
        )
    }

    const fn scope_denied() -> Self {
        Self::new(
            EnterpriseIdentityErrorKind::ScopeDenied,
            "Enterprise identity scope is denied",
        )
    }

    const fn not_found() -> Self {
        Self::new(
            EnterpriseIdentityErrorKind::NotFound,
            "Enterprise identity was not found",
        )
    }

    const fn wrong_state() -> Self {
        Self::new(
            EnterpriseIdentityErrorKind::WrongState,
            "Enterprise identity state rejects this operation",
        )
    }

    const fn authentication() -> Self {
        Self::new(
            EnterpriseIdentityErrorKind::Authentication,
            "API Token authentication failed",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> EnterpriseIdentityErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for EnterpriseIdentityError {}

impl From<StorageError> for EnterpriseIdentityError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => {
                Self::invalid()
            }
            StorageErrorKind::RevisionConflict => revision_conflict(),
            StorageErrorKind::RequestConflict => Self::new(
                EnterpriseIdentityErrorKind::RequestConflict,
                "Enterprise identity request identity was reused with different input",
            ),
            StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => storage_unavailable(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnterpriseIdentityClockError;

pub trait EnterpriseIdentityClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when the clock is unavailable or out of range.
    fn now_millis(&mut self) -> Result<u64, EnterpriseIdentityClockError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnterpriseIdentityClock;

impl EnterpriseIdentityClock for SystemEnterpriseIdentityClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseIdentityClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EnterpriseIdentityClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| EnterpriseIdentityClockError)
    }
}

/// A locally generated raw API Token and its public write facts.
///
/// Debug, Clone, serialization, and raw-token getters are deliberately absent.
pub struct GeneratedApiToken {
    api_token_id: ApiTokenId,
    token_sha256: Sha256Digest,
    raw: Option<String>,
}

impl GeneratedApiToken {
    #[must_use]
    pub const fn api_token_id(&self) -> &ApiTokenId {
        &self.api_token_id
    }

    #[must_use]
    pub const fn token_sha256(&self) -> &Sha256Digest {
        &self.token_sha256
    }

    #[must_use]
    pub fn take_raw(&mut self) -> Option<String> {
        self.raw.take()
    }
}

/// Generates a 256-bit API Token locally. Only its digest enters a command.
///
/// # Errors
///
/// Rejects a non-canonical identity or unavailable operating-system entropy.
pub fn generate_api_token(
    api_token_id: ApiTokenId,
) -> Result<GeneratedApiToken, EnterpriseIdentityError> {
    validate_id(&api_token_id.0, "tok_")?;
    let suffix = api_token_id
        .0
        .strip_prefix("tok_")
        .ok_or_else(EnterpriseIdentityError::invalid)?;
    let mut secret = [0_u8; TOKEN_SECRET_BYTES];
    getrandom::fill(&mut secret).map_err(|_| {
        EnterpriseIdentityError::new(
            EnterpriseIdentityErrorKind::EntropyUnavailable,
            "API Token entropy is unavailable",
        )
    })?;
    let raw = format!("{TOKEN_PREFIX}{suffix}.{}", URL_SAFE_NO_PAD.encode(secret));
    Ok(GeneratedApiToken {
        api_token_id,
        token_sha256: digest_bytes(raw.as_bytes()),
        raw: Some(raw),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthenticatedEnterpriseIdentity {
    pub actor: Actor,
    pub authorized_scopes: Vec<Scope>,
    pub organization_id: OrganizationId,
    pub api_token_id: ApiTokenId,
}

pub struct EnterpriseIdentityService {
    inner: Mutex<EnterpriseIdentityInner>,
}

struct EnterpriseIdentityInner {
    storage: Box<dyn ProductStateStorage>,
    clock: Box<dyn EnterpriseIdentityClock>,
}

impl EnterpriseIdentityService {
    #[must_use]
    pub fn new(storage: Box<dyn ProductStateStorage>) -> Self {
        Self::with_clock(storage, Box::new(SystemEnterpriseIdentityClock))
    }

    #[must_use]
    pub fn with_clock(
        storage: Box<dyn ProductStateStorage>,
        clock: Box<dyn EnterpriseIdentityClock>,
    ) -> Self {
        Self {
            inner: Mutex::new(EnterpriseIdentityInner { storage, clock }),
        }
    }

    /// Applies one generated identity mutation with exact replay and atomic audit state.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope, stale revision, changed request reuse, invalid lifecycle,
    /// unavailable time, or durable storage failure.
    pub fn update(
        &self,
        command: &EnterpriseIdentityUpdateCommand,
    ) -> Result<EnterpriseIdentityUpdateCompletedResponse, EnterpriseIdentityError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .update(command)
    }

    /// Reads one stable organization page without verifier or raw-token fields.
    ///
    /// # Errors
    ///
    /// Rejects invalid filters, stale/foreign cursors, corrupt state, or storage failure.
    pub fn list(
        &self,
        query: &EnterpriseIdentityListQuery,
    ) -> Result<EnterpriseIdentityListResultResponse, EnterpriseIdentityError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .list(query)
    }

    /// Authenticates a bearer against current Token and Service Account state.
    ///
    /// # Errors
    ///
    /// Malformed, unknown, expired, rotated, revoked, and corrupt credentials
    /// share one secret-free authentication category.
    pub fn authenticate_bearer(
        &self,
        bearer: &str,
    ) -> Result<AuthenticatedEnterpriseIdentity, EnterpriseIdentityError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .authenticate_bearer(bearer)
    }

    /// Reads one exact external identity from the canonical identity authority.
    ///
    /// # Errors
    ///
    /// Rejects a foreign organization, corrupt state, unavailable time, or
    /// durable storage failure.
    pub fn external_identity(
        &self,
        organization_id: &OrganizationId,
        external_identity_id: &ExternalIdentityId,
    ) -> Result<Option<EnterpriseExternalIdentityProjection>, EnterpriseIdentityError> {
        self.inner
            .lock()
            .map_err(|_| storage_unavailable())?
            .external_identity(organization_id, external_identity_id)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LifecycleState {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum IdentityState {
    ServiceAccount {
        schema: String,
        organization_id: OrganizationId,
        id: ServiceAccountId,
        display_name: String,
        authorized_scopes: Vec<Scope>,
        state: LifecycleState,
        revision: u64,
        created_at: Instant,
        updated_at: Instant,
    },
    ExternalIdentity {
        schema: String,
        organization_id: OrganizationId,
        id: ExternalIdentityId,
        provider: String,
        issuer_sha256: Sha256Digest,
        subject_sha256: Sha256Digest,
        user_id: winwincode_domain::UserId,
        authorized_scopes: Vec<Scope>,
        state: LifecycleState,
        revision: u64,
        created_at: Instant,
        updated_at: Instant,
    },
    ApiToken {
        schema: String,
        organization_id: OrganizationId,
        id: ApiTokenId,
        service_account_id: ServiceAccountId,
        verifier_sha256: Sha256Digest,
        expires_at: Instant,
        rotation_version: u64,
        state: LifecycleState,
        revision: u64,
        created_at: Instant,
        rotated_at: Instant,
        revoked_at: Option<Instant>,
    },
}

impl IdentityState {
    const fn revision(&self) -> u64 {
        match self {
            Self::ServiceAccount { revision, .. }
            | Self::ExternalIdentity { revision, .. }
            | Self::ApiToken { revision, .. } => *revision,
        }
    }

    const fn organization_id(&self) -> &OrganizationId {
        match self {
            Self::ServiceAccount {
                organization_id, ..
            }
            | Self::ExternalIdentity {
                organization_id, ..
            }
            | Self::ApiToken {
                organization_id, ..
            } => organization_id,
        }
    }

    const fn lifecycle(&self) -> LifecycleState {
        match self {
            Self::ServiceAccount { state, .. }
            | Self::ExternalIdentity { state, .. }
            | Self::ApiToken { state, .. } => *state,
        }
    }

    const fn kind_name(&self) -> &'static str {
        match self {
            Self::ServiceAccount { .. } => "service_account",
            Self::ExternalIdentity { .. } => "external_identity",
            Self::ApiToken { .. } => "api_token",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdentityCatalog {
    schema: String,
    organization_id: OrganizationId,
    revision: u64,
    resources: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdentityCursor {
    schema: u8,
    organization_id: OrganizationId,
    catalog_revision: u64,
    filter_sha256: Sha256Digest,
    after: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdentityLifecycleEvent {
    response: EnterpriseIdentityUpdateCompletedResponse,
}

impl EnterpriseIdentityInner {
    fn external_identity(
        &mut self,
        organization_id: &OrganizationId,
        external_identity_id: &ExternalIdentityId,
    ) -> Result<Option<EnterpriseExternalIdentityProjection>, EnterpriseIdentityError> {
        validate_id(&external_identity_id.0, "xid_")?;
        let Some(state) = self.load_identity(&external_identity_stream(external_identity_id))?
        else {
            return Ok(None);
        };
        if state.organization_id() != organization_id {
            return Err(EnterpriseIdentityError::scope_denied());
        }
        let now_millis = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        match projection(&state, now_millis)? {
            EnterpriseIdentityProjection::EnterpriseExternalIdentityProjection(projection) => {
                Ok(Some(projection))
            }
            _ => Err(storage_unavailable()),
        }
    }

    fn update(
        &mut self,
        command: &EnterpriseIdentityUpdateCommand,
    ) -> Result<EnterpriseIdentityUpdateCompletedResponse, EnterpriseIdentityError> {
        let scope = Scope::OrganizationScope(command.scope.clone());
        let receipt_identity =
            command_receipt_identity(&command.actor, &scope, command.request_id.clone())?;
        let command_digest = digest_serializable(command)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&receipt_identity, &command_digest)?
        {
            return replay_response(&receipt);
        }
        let now_millis = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        let now = instant_from_millis(now_millis)?;
        let expected = expected_revision(command.expected_revision.0)?;
        let resource_stream = resource_stream(&command.payload);
        let current = self.load_identity(&resource_stream)?;
        if current
            .as_ref()
            .is_some_and(|state| state.organization_id() != &command.scope.organization_id)
        {
            return Err(EnterpriseIdentityError::scope_denied());
        }
        if current.as_ref().map_or(0, IdentityState::revision) != expected {
            return Err(revision_conflict());
        }
        let (next, account_guard) = self.apply_payload(command, current.as_ref(), &now)?;
        let previous_payload = current
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| EnterpriseIdentityError::invalid())?;
        let next_payload =
            serde_json::to_vec(&next).map_err(|_| EnterpriseIdentityError::invalid())?;
        let response = response(command, current.as_ref(), &next, now_millis)?;
        let event_payload = serde_json::to_vec(&IdentityLifecycleEvent {
            response: response.clone(),
        })
        .map_err(|_| EnterpriseIdentityError::invalid())?;
        let event_id = event_id(&receipt_identity, &command_digest);
        let pending_audit = pending_audit(
            command,
            previous_payload.as_deref(),
            &next_payload,
            now_millis,
            &event_id,
        )?;

        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let mut catalog = self.load_catalog(&command.scope.organization_id)?;
            let catalog_expected = catalog.revision;
            catalog.revision = catalog
                .revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_INTEGER)
                .ok_or_else(EnterpriseIdentityError::invalid)?;
            catalog
                .resources
                .insert(resource_stream.clone(), next.revision());
            let catalog_payload =
                serde_json::to_vec(&catalog).map_err(|_| EnterpriseIdentityError::invalid())?;
            let mut commit = StateCommit::new(
                receipt_identity.clone(),
                command_digest.clone(),
                catalog_stream(&command.scope.organization_id),
                catalog_expected,
                catalog_payload,
                vec![NewOutboxEvent::internal(
                    event_id.clone(),
                    EVENT_TOPIC,
                    event_payload.clone(),
                )],
            )
            .with_state_mutation(StateMutation::new(
                resource_stream.clone(),
                expected,
                next_payload.clone(),
            )?)
            .with_pending_audit_event(pending_audit.clone());
            if let Some((stream_id, revision)) = &account_guard {
                commit =
                    commit.with_state_guard(StateRevisionGuard::new(stream_id.clone(), *revision)?);
            }
            match self.storage.commit(&commit) {
                Ok(receipt) => return replay_response(&receipt),
                Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                    if let Some(receipt) = self
                        .storage
                        .load_receipt(&receipt_identity, &command_digest)?
                    {
                        return replay_response(&receipt);
                    }
                    if error.is_state_guard_conflict() || attempt + 1 == MAX_COMMIT_ATTEMPTS {
                        return Err(revision_conflict());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(storage_unavailable())
    }

    fn apply_payload(
        &self,
        command: &EnterpriseIdentityUpdateCommand,
        current: Option<&IdentityState>,
        now: &Instant,
    ) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
        let organization_id = &command.scope.organization_id;
        let next_revision = expected_revision(command.expected_revision.0)?
            .checked_add(1)
            .filter(|revision| *revision <= MAX_SAFE_INTEGER)
            .ok_or_else(EnterpriseIdentityError::invalid)?;
        match &command.payload {
            EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountUpsertPayload(payload) => {
                apply_service_account_upsert(organization_id, payload, current, next_revision, now)
            }
            EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountRevokePayload(payload) => {
                apply_service_account_revoke(payload, current, next_revision, now)
            }
            EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityLinkPayload(payload) => {
                apply_external_identity_link(organization_id, payload, current, next_revision, now)
            }
            EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityRevokePayload(payload) => {
                apply_external_identity_revoke(payload, current, next_revision, now)
            }
            EnterpriseIdentityUpdatePayload::EnterpriseApiTokenIssuePayload(payload) => {
                self.apply_token_issue(organization_id, payload, current, next_revision, now)
            }
            EnterpriseIdentityUpdatePayload::EnterpriseApiTokenRevokePayload(payload) => {
                apply_token_revoke(payload, current, next_revision, now)
            }
        }
    }

    fn apply_token_issue(
        &self,
        organization_id: &OrganizationId,
        payload: &EnterpriseApiTokenIssuePayload,
        current: Option<&IdentityState>,
        next_revision: u64,
        now: &Instant,
    ) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
        validate_id(&payload.api_token_id.0, "tok_")?;
        validate_id(&payload.service_account_id.0, "svc_")?;
        validate_digest(&payload.token_sha256)?;
        if !matches!(payload.action.as_str(), "issue" | "rotate") {
            return Err(EnterpriseIdentityError::invalid());
        }
        if instant_millis(&payload.expires_at)? <= instant_millis(now)? {
            return Err(EnterpriseIdentityError::invalid());
        }
        let account_stream = service_account_stream(&payload.service_account_id);
        let account = self
            .load_identity(&account_stream)?
            .ok_or_else(EnterpriseIdentityError::not_found)?;
        let IdentityState::ServiceAccount {
            organization_id: account_organization,
            state: LifecycleState::Active,
            revision: account_revision,
            ..
        } = account
        else {
            return Err(EnterpriseIdentityError::wrong_state());
        };
        if &account_organization != organization_id {
            return Err(EnterpriseIdentityError::scope_denied());
        }
        let (created_at, rotation_version) = match (payload.action.as_str(), current) {
            ("issue", None) => (now.clone(), 1),
            (
                "rotate",
                Some(IdentityState::ApiToken {
                    service_account_id,
                    state: LifecycleState::Active,
                    created_at,
                    rotation_version,
                    ..
                }),
            ) if service_account_id == &payload.service_account_id => (
                created_at.clone(),
                rotation_version
                    .checked_add(1)
                    .filter(|version| *version <= MAX_SAFE_INTEGER)
                    .ok_or_else(EnterpriseIdentityError::invalid)?,
            ),
            ("issue", Some(_)) | ("rotate", None) => {
                return Err(EnterpriseIdentityError::wrong_state());
            }
            _ => return Err(EnterpriseIdentityError::wrong_state()),
        };
        Ok((
            IdentityState::ApiToken {
                schema: STATE_SCHEMA.to_owned(),
                organization_id: organization_id.clone(),
                id: payload.api_token_id.clone(),
                service_account_id: payload.service_account_id.clone(),
                verifier_sha256: payload.token_sha256.clone(),
                expires_at: payload.expires_at.clone(),
                rotation_version,
                state: LifecycleState::Active,
                revision: next_revision,
                created_at,
                rotated_at: now.clone(),
                revoked_at: None,
            },
            Some((account_stream, account_revision)),
        ))
    }

    fn list(
        &mut self,
        query: &EnterpriseIdentityListQuery,
    ) -> Result<EnterpriseIdentityListResultResponse, EnterpriseIdentityError> {
        command_receipt_identity(
            &query.actor,
            &Scope::OrganizationScope(query.scope.clone()),
            query.request_id.clone(),
        )?;
        let limit = usize::try_from(query.page.limit)
            .ok()
            .filter(|limit| (1..=MAX_PAGE_SIZE).contains(limit))
            .ok_or_else(EnterpriseIdentityError::invalid)?;
        let kinds = normalized_filter(
            &query.parameters.kinds,
            &["service_account", "external_identity", "api_token"],
        )?;
        let states =
            normalized_filter(&query.parameters.states, &["active", "expired", "revoked"])?;
        let filter_sha256 = digest_serializable(&(kinds.clone(), states.clone()))?;
        let catalog = self.load_catalog(&query.scope.organization_id)?;
        let cursor = query.page.cursor.as_ref().map(decode_cursor).transpose()?;
        let after = if let Some(cursor) = cursor {
            if cursor.organization_id != query.scope.organization_id
                || cursor.catalog_revision != catalog.revision
                || cursor.filter_sha256 != filter_sha256
            {
                return Err(EnterpriseIdentityError::invalid());
            }
            cursor.after
        } else {
            String::new()
        };
        let now_millis = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        let mut matches = Vec::new();
        for (stream_id, state_revision) in catalog.resources.range(after.clone()..) {
            if !after.is_empty() && stream_id == &after {
                continue;
            }
            let stored = self
                .storage
                .load_state(stream_id)?
                .ok_or_else(storage_unavailable)?;
            if stored.revision != *state_revision {
                return Err(storage_unavailable());
            }
            let state = decode_state(&stored)?;
            if state.organization_id() != &query.scope.organization_id {
                return Err(storage_unavailable());
            }
            let state_name = effective_state(&state, now_millis)?;
            if (!kinds.is_empty() && !kinds.iter().any(|kind| kind == state.kind_name()))
                || (!states.is_empty() && !states.iter().any(|state| state == state_name))
            {
                continue;
            }
            matches.push((stream_id.clone(), projection(&state, now_millis)?));
            if matches.len() > limit {
                break;
            }
        }
        let has_more = matches.len() > limit;
        matches.truncate(limit);
        let next_cursor = if has_more {
            let after = matches
                .last()
                .map(|(stream_id, _)| stream_id.clone())
                .ok_or_else(EnterpriseIdentityError::invalid)?;
            Some(encode_cursor(&IdentityCursor {
                schema: CURSOR_SCHEMA,
                organization_id: query.scope.organization_id.clone(),
                catalog_revision: catalog.revision,
                filter_sha256,
                after,
            })?)
        } else {
            None
        };
        Ok(EnterpriseIdentityListResultResponse {
            page: PageInfo {
                has_more,
                next_cursor,
            },
            query: EnterpriseIdentityListResultResponseQuery::EnterpriseIdentityList,
            request_id: query.request_id.clone(),
            result: EnterpriseIdentityPage {
                items: matches.into_iter().map(|(_, item)| item).collect(),
                kind: EnterpriseIdentityPageKind::EnterpriseIdentityPage,
                snapshot_revision: revision(catalog.revision)?,
            },
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    fn authenticate_bearer(
        &mut self,
        bearer: &str,
    ) -> Result<AuthenticatedEnterpriseIdentity, EnterpriseIdentityError> {
        let token_id =
            parse_api_token_id(bearer).map_err(|_| EnterpriseIdentityError::authentication())?;
        let state = self
            .load_identity(&api_token_stream(&token_id))
            .map_err(|_| EnterpriseIdentityError::authentication())?
            .ok_or_else(EnterpriseIdentityError::authentication)?;
        let IdentityState::ApiToken {
            organization_id,
            id,
            service_account_id,
            verifier_sha256,
            expires_at,
            state: LifecycleState::Active,
            ..
        } = state
        else {
            return Err(EnterpriseIdentityError::authentication());
        };
        if !constant_time_eq(
            verifier_sha256.0.as_bytes(),
            digest_bytes(bearer.as_bytes()).0.as_bytes(),
        ) {
            return Err(EnterpriseIdentityError::authentication());
        }
        let now_millis = self
            .clock
            .now_millis()
            .map_err(|_| EnterpriseIdentityError::authentication())?;
        if instant_millis(&expires_at).map_err(|_| EnterpriseIdentityError::authentication())?
            <= now_millis
        {
            return Err(EnterpriseIdentityError::authentication());
        }
        let account = self
            .load_identity(&service_account_stream(&service_account_id))
            .map_err(|_| EnterpriseIdentityError::authentication())?
            .ok_or_else(EnterpriseIdentityError::authentication)?;
        let IdentityState::ServiceAccount {
            organization_id: account_organization,
            id: account_id,
            authorized_scopes,
            state: LifecycleState::Active,
            ..
        } = account
        else {
            return Err(EnterpriseIdentityError::authentication());
        };
        if account_organization != organization_id || account_id != service_account_id {
            return Err(EnterpriseIdentityError::authentication());
        }
        Ok(AuthenticatedEnterpriseIdentity {
            actor: Actor::ServiceAccountActor(ServiceAccountActor {
                id: service_account_id,
                kind: ServiceAccountActorKind::ServiceAccount,
            }),
            authorized_scopes,
            organization_id,
            api_token_id: id,
        })
    }

    fn load_identity(
        &self,
        stream_id: &str,
    ) -> Result<Option<IdentityState>, EnterpriseIdentityError> {
        self.storage
            .load_state(stream_id)?
            .as_ref()
            .map(decode_state)
            .transpose()
    }

    fn load_catalog(
        &self,
        organization_id: &OrganizationId,
    ) -> Result<IdentityCatalog, EnterpriseIdentityError> {
        validate_id(&organization_id.0, "org_")?;
        let Some(stored) = self.storage.load_state(&catalog_stream(organization_id))? else {
            return Ok(IdentityCatalog {
                schema: CATALOG_SCHEMA.to_owned(),
                organization_id: organization_id.clone(),
                revision: 0,
                resources: BTreeMap::new(),
            });
        };
        let catalog: IdentityCatalog =
            serde_json::from_slice(&stored.payload).map_err(|_| storage_unavailable())?;
        if catalog.schema != CATALOG_SCHEMA
            || &catalog.organization_id != organization_id
            || catalog.revision != stored.revision
            || serde_json::to_vec(&catalog).map_err(|_| storage_unavailable())? != stored.payload
        {
            return Err(storage_unavailable());
        }
        Ok(catalog)
    }
}

fn apply_service_account_upsert(
    organization_id: &OrganizationId,
    payload: &EnterpriseServiceAccountUpsertPayload,
    current: Option<&IdentityState>,
    next_revision: u64,
    now: &Instant,
) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
    validate_id(&payload.service_account_id.0, "svc_")?;
    validate_display_name(&payload.display_name)?;
    validate_authorized_scopes(organization_id, &payload.authorized_scopes)?;
    let created_at = match current {
        None => now.clone(),
        Some(IdentityState::ServiceAccount {
            created_at, state, ..
        }) if *state == LifecycleState::Active => created_at.clone(),
        Some(IdentityState::ServiceAccount { .. }) => {
            return Err(EnterpriseIdentityError::wrong_state());
        }
        Some(_) => return Err(EnterpriseIdentityError::invalid()),
    };
    Ok((
        IdentityState::ServiceAccount {
            schema: STATE_SCHEMA.to_owned(),
            organization_id: organization_id.clone(),
            id: payload.service_account_id.clone(),
            display_name: payload.display_name.clone(),
            authorized_scopes: payload.authorized_scopes.clone(),
            state: LifecycleState::Active,
            revision: next_revision,
            created_at,
            updated_at: now.clone(),
        },
        None,
    ))
}

fn apply_service_account_revoke(
    payload: &EnterpriseServiceAccountRevokePayload,
    current: Option<&IdentityState>,
    next_revision: u64,
    now: &Instant,
) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
    let Some(IdentityState::ServiceAccount {
        schema,
        organization_id,
        id,
        display_name,
        authorized_scopes,
        state: LifecycleState::Active,
        created_at,
        ..
    }) = current
    else {
        return Err(missing_or_wrong_state(current));
    };
    if id != &payload.service_account_id {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok((
        IdentityState::ServiceAccount {
            schema: schema.clone(),
            organization_id: organization_id.clone(),
            id: id.clone(),
            display_name: display_name.clone(),
            authorized_scopes: authorized_scopes.clone(),
            state: LifecycleState::Revoked,
            revision: next_revision,
            created_at: created_at.clone(),
            updated_at: now.clone(),
        },
        None,
    ))
}

fn apply_external_identity_link(
    organization_id: &OrganizationId,
    payload: &EnterpriseExternalIdentityLinkPayload,
    current: Option<&IdentityState>,
    next_revision: u64,
    now: &Instant,
) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
    validate_id(&payload.external_identity_id.0, "xid_")?;
    validate_id(&payload.user_id.0, "usr_")?;
    validate_digest(&payload.issuer_sha256)?;
    validate_digest(&payload.subject_sha256)?;
    validate_provider(&payload.provider)?;
    validate_authorized_scopes(organization_id, &payload.authorized_scopes)?;
    let created_at = match current {
        None => now.clone(),
        Some(IdentityState::ExternalIdentity {
            created_at, state, ..
        }) if *state == LifecycleState::Active => created_at.clone(),
        Some(IdentityState::ExternalIdentity { .. }) => {
            return Err(EnterpriseIdentityError::wrong_state());
        }
        Some(_) => return Err(EnterpriseIdentityError::invalid()),
    };
    Ok((
        IdentityState::ExternalIdentity {
            schema: STATE_SCHEMA.to_owned(),
            organization_id: organization_id.clone(),
            id: payload.external_identity_id.clone(),
            provider: payload.provider.clone(),
            issuer_sha256: payload.issuer_sha256.clone(),
            subject_sha256: payload.subject_sha256.clone(),
            user_id: payload.user_id.clone(),
            authorized_scopes: payload.authorized_scopes.clone(),
            state: LifecycleState::Active,
            revision: next_revision,
            created_at,
            updated_at: now.clone(),
        },
        None,
    ))
}

fn apply_external_identity_revoke(
    payload: &EnterpriseExternalIdentityRevokePayload,
    current: Option<&IdentityState>,
    next_revision: u64,
    now: &Instant,
) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
    let Some(IdentityState::ExternalIdentity {
        schema,
        organization_id,
        id,
        provider,
        issuer_sha256,
        subject_sha256,
        user_id,
        authorized_scopes,
        state: LifecycleState::Active,
        created_at,
        ..
    }) = current
    else {
        return Err(missing_or_wrong_state(current));
    };
    if id != &payload.external_identity_id {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok((
        IdentityState::ExternalIdentity {
            schema: schema.clone(),
            organization_id: organization_id.clone(),
            id: id.clone(),
            provider: provider.clone(),
            issuer_sha256: issuer_sha256.clone(),
            subject_sha256: subject_sha256.clone(),
            user_id: user_id.clone(),
            authorized_scopes: authorized_scopes.clone(),
            state: LifecycleState::Revoked,
            revision: next_revision,
            created_at: created_at.clone(),
            updated_at: now.clone(),
        },
        None,
    ))
}

fn apply_token_revoke(
    payload: &EnterpriseApiTokenRevokePayload,
    current: Option<&IdentityState>,
    next_revision: u64,
    now: &Instant,
) -> Result<(IdentityState, Option<(String, u64)>), EnterpriseIdentityError> {
    let Some(IdentityState::ApiToken {
        schema,
        organization_id,
        id,
        service_account_id,
        verifier_sha256,
        expires_at,
        rotation_version,
        state: LifecycleState::Active,
        created_at,
        rotated_at,
        ..
    }) = current
    else {
        return Err(missing_or_wrong_state(current));
    };
    if id != &payload.api_token_id {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok((
        IdentityState::ApiToken {
            schema: schema.clone(),
            organization_id: organization_id.clone(),
            id: id.clone(),
            service_account_id: service_account_id.clone(),
            verifier_sha256: verifier_sha256.clone(),
            expires_at: expires_at.clone(),
            rotation_version: *rotation_version,
            state: LifecycleState::Revoked,
            revision: next_revision,
            created_at: created_at.clone(),
            rotated_at: rotated_at.clone(),
            revoked_at: Some(now.clone()),
        },
        None,
    ))
}

fn response(
    command: &EnterpriseIdentityUpdateCommand,
    previous: Option<&IdentityState>,
    next: &IdentityState,
    now_millis: u64,
) -> Result<EnterpriseIdentityUpdateCompletedResponse, EnterpriseIdentityError> {
    Ok(EnterpriseIdentityUpdateCompletedResponse {
        command: EnterpriseIdentityUpdateCompletedResponseCommand::EnterpriseIdentityUpdate,
        current_revision: revision(next.revision())?,
        outcome: EnterpriseIdentityUpdateCompletedResponseOutcome::Completed,
        previous_revision: revision(previous.map_or(0, IdentityState::revision))?,
        request_id: command.request_id.clone(),
        result: projection(next, now_millis)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn projection(
    state: &IdentityState,
    now_millis: u64,
) -> Result<EnterpriseIdentityProjection, EnterpriseIdentityError> {
    match state {
        IdentityState::ServiceAccount {
            id,
            display_name,
            authorized_scopes,
            state,
            revision: state_revision,
            created_at,
            updated_at,
            ..
        } => Ok(
            EnterpriseIdentityProjection::EnterpriseServiceAccountProjection(
                EnterpriseServiceAccountProjection {
                    authorized_scopes: authorized_scopes.clone(),
                    created_at: created_at.clone(),
                    display_name: display_name.clone(),
                    id: id.clone(),
                    kind: EnterpriseServiceAccountProjectionKind::ServiceAccount,
                    revision: revision(*state_revision)?,
                    state: lifecycle_name(*state).to_owned(),
                    updated_at: updated_at.clone(),
                },
            ),
        ),
        IdentityState::ExternalIdentity {
            id,
            provider,
            issuer_sha256,
            subject_sha256,
            user_id,
            authorized_scopes,
            state,
            revision: state_revision,
            created_at,
            updated_at,
            ..
        } => Ok(
            EnterpriseIdentityProjection::EnterpriseExternalIdentityProjection(
                EnterpriseExternalIdentityProjection {
                    actor: UserActor {
                        id: user_id.clone(),
                        kind: UserActorKind::User,
                    },
                    authorized_scopes: authorized_scopes.clone(),
                    created_at: created_at.clone(),
                    id: id.clone(),
                    issuer_sha256: issuer_sha256.clone(),
                    kind: EnterpriseExternalIdentityProjectionKind::ExternalIdentity,
                    provider: provider.clone(),
                    revision: revision(*state_revision)?,
                    state: lifecycle_name(*state).to_owned(),
                    subject_sha256: subject_sha256.clone(),
                    updated_at: updated_at.clone(),
                },
            ),
        ),
        IdentityState::ApiToken {
            id,
            service_account_id,
            expires_at,
            rotation_version,
            state: lifecycle,
            revision: state_revision,
            created_at,
            rotated_at,
            revoked_at,
            ..
        } => Ok(EnterpriseIdentityProjection::EnterpriseApiTokenProjection(
            EnterpriseApiTokenProjection {
                created_at: created_at.clone(),
                expires_at: expires_at.clone(),
                id: id.clone(),
                kind: EnterpriseApiTokenProjectionKind::ApiToken,
                revision: revision(*state_revision)?,
                revoked_at: revoked_at.clone(),
                rotated_at: rotated_at.clone(),
                rotation_version: i64::try_from(*rotation_version)
                    .map_err(|_| EnterpriseIdentityError::invalid())?,
                service_account_id: service_account_id.clone(),
                state: if *lifecycle == LifecycleState::Revoked {
                    "revoked"
                } else if instant_millis(expires_at)? <= now_millis {
                    "expired"
                } else {
                    "active"
                }
                .to_owned(),
            },
        )),
    }
}

fn replay_response(
    receipt: &CommitReceipt,
) -> Result<EnterpriseIdentityUpdateCompletedResponse, EnterpriseIdentityError> {
    if receipt.events.len() != 1 || receipt.events[0].topic != EVENT_TOPIC {
        return Err(storage_unavailable());
    }
    let event: IdentityLifecycleEvent =
        serde_json::from_slice(&receipt.events[0].payload).map_err(|_| storage_unavailable())?;
    if event.response.current_revision.0
        != event
            .response
            .previous_revision
            .0
            .checked_add(1)
            .ok_or_else(storage_unavailable)?
    {
        return Err(storage_unavailable());
    }
    Ok(event.response)
}

fn pending_audit(
    command: &EnterpriseIdentityUpdateCommand,
    before: Option<&[u8]>,
    after: &[u8],
    now_millis: u64,
    event_id: &str,
) -> Result<PendingAuditEvent, EnterpriseIdentityError> {
    let event = AuditEvent::state_change(
        AuditEventId::from_digest(&digest_bytes(event_id.as_bytes()))
            .map_err(|_| EnterpriseIdentityError::invalid())?,
        now_millis,
        audit_actor(&command.actor),
        AuditScope::organization(command.scope.organization_id.clone())
            .map_err(|_| EnterpriseIdentityError::invalid())?,
        command.request_id.clone(),
        AuditAction::administration(action_name(&command.payload))
            .map_err(|_| EnterpriseIdentityError::invalid())?,
        AuditState::changed(before.map(digest_bytes), digest_bytes(after))
            .map_err(|_| EnterpriseIdentityError::invalid())?,
        AuditOrigin::local(AUDIT_ORIGIN).map_err(|_| EnterpriseIdentityError::invalid())?,
        AuditSubject::new(),
        "completed",
        AuditRetention::Indefinite,
    )
    .map_err(|_| EnterpriseIdentityError::invalid())?;
    let event_id = event.event_id().as_str().to_owned();
    let payload = serde_json::to_vec(&event).map_err(|_| EnterpriseIdentityError::invalid())?;
    PendingAuditEvent::new(event_id, payload).map_err(Into::into)
}

fn decode_state(stored: &StoredState) -> Result<IdentityState, EnterpriseIdentityError> {
    let state: IdentityState =
        serde_json::from_slice(&stored.payload).map_err(|_| storage_unavailable())?;
    if state.revision() != stored.revision
        || state.revision() == 0
        || state.revision() > MAX_SAFE_INTEGER
        || match &state {
            IdentityState::ServiceAccount { schema, .. }
            | IdentityState::ExternalIdentity { schema, .. }
            | IdentityState::ApiToken { schema, .. } => schema != STATE_SCHEMA,
        }
        || serde_json::to_vec(&state).map_err(|_| storage_unavailable())? != stored.payload
    {
        return Err(storage_unavailable());
    }
    Ok(state)
}

fn resource_stream(payload: &EnterpriseIdentityUpdatePayload) -> String {
    match payload {
        EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountUpsertPayload(payload) => {
            service_account_stream(&payload.service_account_id)
        }
        EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountRevokePayload(payload) => {
            service_account_stream(&payload.service_account_id)
        }
        EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityLinkPayload(payload) => {
            external_identity_stream(&payload.external_identity_id)
        }
        EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityRevokePayload(payload) => {
            external_identity_stream(&payload.external_identity_id)
        }
        EnterpriseIdentityUpdatePayload::EnterpriseApiTokenIssuePayload(payload) => {
            api_token_stream(&payload.api_token_id)
        }
        EnterpriseIdentityUpdatePayload::EnterpriseApiTokenRevokePayload(payload) => {
            api_token_stream(&payload.api_token_id)
        }
    }
}

fn service_account_stream(id: &ServiceAccountId) -> String {
    format!("{STREAM_PREFIX}service-account:{}", id.0)
}

fn external_identity_stream(id: &ExternalIdentityId) -> String {
    format!("{STREAM_PREFIX}external-identity:{}", id.0)
}

fn api_token_stream(id: &ApiTokenId) -> String {
    format!("{STREAM_PREFIX}api-token:{}", id.0)
}

fn catalog_stream(organization_id: &OrganizationId) -> String {
    format!("{STREAM_PREFIX}catalog:{}", organization_id.0)
}

fn action_name(payload: &EnterpriseIdentityUpdatePayload) -> &'static str {
    match payload {
        EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountUpsertPayload(_) => {
            "identity.service_account.upsert"
        }
        EnterpriseIdentityUpdatePayload::EnterpriseServiceAccountRevokePayload(_) => {
            "identity.service_account.revoke"
        }
        EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityLinkPayload(_) => {
            "identity.external_identity.link"
        }
        EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityRevokePayload(_) => {
            "identity.external_identity.revoke"
        }
        EnterpriseIdentityUpdatePayload::EnterpriseApiTokenIssuePayload(payload) => {
            if payload.action == "rotate" {
                "identity.api_token.rotate"
            } else {
                "identity.api_token.issue"
            }
        }
        EnterpriseIdentityUpdatePayload::EnterpriseApiTokenRevokePayload(_) => {
            "identity.api_token.revoke"
        }
    }
}

fn audit_actor(actor: &Actor) -> AuditActor {
    match actor {
        Actor::UserActor(actor) => AuditActor::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => AuditActor::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => AuditActor::System(actor.id.clone()),
    }
}

fn parse_api_token_id(bearer: &str) -> Result<ApiTokenId, EnterpriseIdentityError> {
    if bearer.len() != TOKEN_PREFIX.len() + 26 + 1 + TOKEN_SECRET_ENCODED_LEN {
        return Err(EnterpriseIdentityError::invalid());
    }
    let (id, secret) = bearer
        .strip_prefix(TOKEN_PREFIX)
        .and_then(|value| value.split_once('.'))
        .ok_or_else(EnterpriseIdentityError::invalid)?;
    if id.len() != 26 || !id.bytes().all(is_crockford_base32) {
        return Err(EnterpriseIdentityError::invalid());
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(secret)
        .map_err(|_| EnterpriseIdentityError::invalid())?;
    if secret.len() != TOKEN_SECRET_ENCODED_LEN || decoded.len() != TOKEN_SECRET_BYTES {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(ApiTokenId(format!("tok_{id}")))
}

fn validate_authorized_scopes(
    organization_id: &OrganizationId,
    scopes: &[Scope],
) -> Result<(), EnterpriseIdentityError> {
    if scopes.is_empty() || scopes.len() > 100 {
        return Err(EnterpriseIdentityError::invalid());
    }
    for scope in scopes {
        if scope_organization_id(scope) != organization_id {
            return Err(EnterpriseIdentityError::scope_denied());
        }
        command_receipt_identity(
            &Actor::ServiceAccountActor(ServiceAccountActor {
                id: ServiceAccountId("svc_00000000000000000000000000".to_owned()),
                kind: ServiceAccountActorKind::ServiceAccount,
            }),
            scope,
            winwincode_domain::RequestId("req_00000000000000000000000000".to_owned()),
        )?;
    }
    Ok(())
}

const fn scope_organization_id(scope: &Scope) -> &OrganizationId {
    match scope {
        Scope::OrganizationScope(scope) => &scope.organization_id,
        Scope::WorkspaceScope(scope) => &scope.organization_id,
        Scope::ProjectScope(scope) => &scope.organization_id,
        Scope::RepositoryScope(scope) => &scope.organization_id,
    }
}

fn effective_state(
    state: &IdentityState,
    now_millis: u64,
) -> Result<&'static str, EnterpriseIdentityError> {
    if state.lifecycle() == LifecycleState::Revoked {
        return Ok("revoked");
    }
    if let IdentityState::ApiToken { expires_at, .. } = state
        && instant_millis(expires_at)? <= now_millis
    {
        return Ok("expired");
    }
    Ok("active")
}

fn normalized_filter(
    values: &[String],
    accepted: &[&str],
) -> Result<Vec<String>, EnterpriseIdentityError> {
    if values.len() > accepted.len()
        || values
            .iter()
            .any(|value| !accepted.iter().any(|accepted| value == accepted))
    {
        return Err(EnterpriseIdentityError::invalid());
    }
    let mut normalized = values.to_vec();
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn encode_cursor(cursor: &IdentityCursor) -> Result<OpaqueCursor, EnterpriseIdentityError> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| EnterpriseIdentityError::invalid())?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(cursor: &OpaqueCursor) -> Result<IdentityCursor, EnterpriseIdentityError> {
    if cursor.0.len() > MAX_CURSOR_BYTES * 2 {
        return Err(EnterpriseIdentityError::invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| EnterpriseIdentityError::invalid())?;
    let decoded: IdentityCursor =
        serde_json::from_slice(&bytes).map_err(|_| EnterpriseIdentityError::invalid())?;
    if decoded.schema != CURSOR_SCHEMA
        || serde_json::to_vec(&decoded).map_err(|_| EnterpriseIdentityError::invalid())? != bytes
    {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(decoded)
}

fn validate_display_name(value: &str) -> Result<(), EnterpriseIdentityError> {
    if value.is_empty() || value.chars().count() > 200 || value.trim() != value {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(())
}

fn validate_provider(value: &str) -> Result<(), EnterpriseIdentityError> {
    if !matches!(value, "oidc" | "saml" | "scim") {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(())
}

fn validate_digest(value: &Sha256Digest) -> Result<(), EnterpriseIdentityError> {
    let Some(hex) = value.0.strip_prefix("sha256:") else {
        return Err(EnterpriseIdentityError::invalid());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(())
}

fn validate_id(value: &str, prefix: &str) -> Result<(), EnterpriseIdentityError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(EnterpriseIdentityError::invalid());
    };
    if suffix.len() != 26 || !suffix.bytes().all(is_crockford_base32) {
        return Err(EnterpriseIdentityError::invalid());
    }
    Ok(())
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn expected_revision(value: i64) -> Result<u64, EnterpriseIdentityError> {
    u64::try_from(value)
        .ok()
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(EnterpriseIdentityError::invalid)
}

fn revision(value: u64) -> Result<Revision, EnterpriseIdentityError> {
    i64::try_from(value)
        .ok()
        .map(Revision)
        .ok_or_else(EnterpriseIdentityError::invalid)
}

fn digest_serializable<T: Serialize + ?Sized>(
    value: &T,
) -> Result<Sha256Digest, EnterpriseIdentityError> {
    serde_json::to_vec(value)
        .map(|bytes| digest_bytes(&bytes))
        .map_err(|_| EnterpriseIdentityError::invalid())
}

fn digest_bytes(value: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(value)))
}

fn event_id(identity: &winwincode_storage::ReceiptIdentity, digest: &Sha256Digest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.enterprise-identity-event.v1\0");
    hasher.update(identity.actor_key().as_bytes());
    hasher.update(identity.scope_key().as_bytes());
    hasher.update(identity.request_id().0.as_bytes());
    hasher.update(digest.0.as_bytes());
    format!("evt_{:x}", hasher.finalize())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

const fn lifecycle_name(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Active => "active",
        LifecycleState::Revoked => "revoked",
    }
}

fn missing_or_wrong_state(current: Option<&IdentityState>) -> EnterpriseIdentityError {
    current.map_or_else(EnterpriseIdentityError::not_found, |_| {
        EnterpriseIdentityError::wrong_state()
    })
}

const fn revision_conflict() -> EnterpriseIdentityError {
    EnterpriseIdentityError::new(
        EnterpriseIdentityErrorKind::RevisionConflict,
        "Enterprise identity revision does not match",
    )
}

const fn storage_unavailable() -> EnterpriseIdentityError {
    EnterpriseIdentityError::new(
        EnterpriseIdentityErrorKind::Storage,
        "Enterprise identity storage operation failed",
    )
}

const fn clock_unavailable() -> EnterpriseIdentityError {
    EnterpriseIdentityError::new(
        EnterpriseIdentityErrorKind::ClockUnavailable,
        "Enterprise identity clock is unavailable",
    )
}

fn instant_millis(value: &Instant) -> Result<u64, EnterpriseIdentityError> {
    crate::session_binding_transaction::instant_millis(value).map_err(Into::into)
}
