// SPDX-License-Identifier: Apache-2.0

//! Scope-bound, secret-free Credential reference lifecycle.
//!
//! This module persists only the public reference identity and lifecycle
//! metadata. A vault locator is accepted at the generated write boundary so a
//! `SecretStore` adapter can complete its own atomic operation, but the locator
//! is never copied into aggregate state, events, audit records, projections,
//! resolutions, or errors.

use std::{collections::BTreeMap, fmt};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource,
    CredentialReferenceCreateCommand, CredentialReferenceCreateCompletedResponse,
    CredentialReferenceCreateCompletedResponseCommand,
    CredentialReferenceCreateCompletedResponseOutcome, CredentialReferenceDeleteCommand,
    CredentialReferenceDeleteCompletedResponse, CredentialReferenceDeleteCompletedResponseCommand,
    CredentialReferenceDeleteCompletedResponseOutcome, CredentialReferenceGetQuery,
    CredentialReferenceGetResultResponse, CredentialReferenceGetResultResponseQuery,
    CredentialReferenceListQuery, CredentialReferenceListResultResponse,
    CredentialReferenceListResultResponseQuery, CredentialReferencePage,
    CredentialReferencePageKind, CredentialReferenceProjection, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCompletedResponse, CredentialReferenceRevokeCompletedResponseCommand,
    CredentialReferenceRevokeCompletedResponseOutcome, CredentialReferenceRotateCommand,
    CredentialReferenceRotateCompletedResponse, CredentialReferenceRotateCompletedResponseCommand,
    CredentialReferenceRotateCompletedResponseOutcome, MutationReceipt, PageInfo, Scope,
};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditSubject,
};
use winwincode_domain::{
    CredentialReferenceId, Instant, OpaqueCursor, RequestId, Revision, SchemaVersion, Sha256Digest,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, CommitReceipt,
    LoadedAggregateJournal, NewOutboxEvent, PendingAuditEvent, ProductStateStorage, StorageError,
    StorageErrorKind, StoredState,
};

use crate::credential_leak_gate::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary,
};
use crate::session_binding_transaction::instant_millis;
use crate::{
    StateChange, command_receipt,
    model_route_availability::model_route_availability_invalidated_event, receipt_scope_key,
    storage_commit,
};

const STATE_SCHEMA: &str = "winwincode.credential-reference.v1";
const STREAM_PREFIX: &str = "credential-reference:";
const EVENT_TOPIC: &str = "credential.reference.lifecycle.v1";
const AUDIT_ORIGIN: &str = "control-plane.credential-reference";
const CATALOG_SCHEMA: &str = "winwincode.credential-reference.catalog.v1";
const CATALOG_AGGREGATE_TYPE: &str = "credential-reference-catalog";
const CURSOR_SCHEMA: u8 = 1;
const MAX_PAGE_SIZE: usize = 200;
const MAX_CURSOR_BYTES: usize = 2_048;
const MAX_CATALOG_COMMIT_ATTEMPTS: usize = 32;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Stable, secret-free failure categories for Credential reference operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialReferenceErrorKind {
    InvalidRequest,
    ScopeDenied,
    NotFound,
    Revoked,
    WrongState,
    RevisionConflict,
    RequestConflict,
    CursorInvalid,
    CredentialLeak,
    Storage,
}

/// A bounded Credential reference error that never copies caller payload text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialReferenceError {
    kind: CredentialReferenceErrorKind,
    message: &'static str,
}

impl CredentialReferenceError {
    fn new(kind: CredentialReferenceErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    fn invalid() -> Self {
        Self::new(
            CredentialReferenceErrorKind::InvalidRequest,
            "Credential reference request is invalid",
        )
    }

    fn scope_denied() -> Self {
        Self::new(
            CredentialReferenceErrorKind::ScopeDenied,
            "Credential reference belongs to another scope",
        )
    }

    fn not_found() -> Self {
        Self::new(
            CredentialReferenceErrorKind::NotFound,
            "Credential reference was not found",
        )
    }

    fn revoked() -> Self {
        Self::new(
            CredentialReferenceErrorKind::Revoked,
            "Credential reference is revoked",
        )
    }

    fn wrong_state() -> Self {
        Self::new(
            CredentialReferenceErrorKind::WrongState,
            "Credential reference lifecycle state rejects this operation",
        )
    }

    fn revision_conflict() -> Self {
        Self::new(
            CredentialReferenceErrorKind::RevisionConflict,
            "Credential reference revision does not match",
        )
    }

    fn cursor_invalid() -> Self {
        Self::new(
            CredentialReferenceErrorKind::CursorInvalid,
            "Credential reference page cursor is invalid or stale",
        )
    }

    fn credential_leak() -> Self {
        Self::new(
            CredentialReferenceErrorKind::CredentialLeak,
            "Credential reference output was rejected by the leak gate",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> CredentialReferenceErrorKind {
        self.kind
    }
}

impl fmt::Display for CredentialReferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CredentialReferenceError {}

impl From<CredentialLeakError> for CredentialReferenceError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::credential_leak()
    }
}

impl From<StorageError> for CredentialReferenceError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RevisionConflict => Self::revision_conflict(),
            StorageErrorKind::RequestConflict => Self::new(
                CredentialReferenceErrorKind::RequestConflict,
                "Credential reference requestId was reused with different input",
            ),
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => {
                Self::invalid()
            }
            StorageErrorKind::EventCursorExpired
            | StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => Self::new(
                CredentialReferenceErrorKind::Storage,
                "Credential reference storage operation failed",
            ),
        }
    }
}

/// The exact scope-bound handle supplied to a `SecretStore` adapter.
///
/// It contains no vault locator or secret material. A caller must obtain a new
/// value after every rotation, so a stale version cannot silently resolve.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialReferenceResolution {
    credential_reference_id: CredentialReferenceId,
    scope: Scope,
    provider_id: String,
    rotation_version: u64,
}

/// Opaque secret bytes returned only across the `SecretStore` boundary.
///
/// Debug output is always redacted, serialization is intentionally absent,
/// cloning is intentionally absent, and the owned buffer is cleared on drop.
pub struct ResolvedSecret {
    bytes: Vec<u8>,
}

impl ResolvedSecret {
    /// Takes ownership of non-empty bytes loaded by a `SecretStore` adapter.
    ///
    /// # Errors
    ///
    /// Rejects an empty secret without retaining it in an error.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, SecretStoreError> {
        if bytes.is_empty() {
            return Err(SecretStoreError::corrupt());
        }
        Ok(Self { bytes })
    }

    /// Exposes bytes only at the provider-call boundary.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for ResolvedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResolvedSecret([REDACTED])")
    }
}

impl Drop for ResolvedSecret {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// Stable `SecretStore` failure categories. No adapter diagnostic or remote
/// response is accepted into this public error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretStoreErrorKind {
    Missing,
    VersionConflict,
    Unavailable,
    Corrupt,
}

/// Secret-safe failure returned by a [`SecretStorePort`] implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretStoreError {
    kind: SecretStoreErrorKind,
    message: &'static str,
}

impl SecretStoreError {
    #[must_use]
    pub const fn missing() -> Self {
        Self {
            kind: SecretStoreErrorKind::Missing,
            message: "Credential secret is missing",
        }
    }

    #[must_use]
    pub const fn version_conflict() -> Self {
        Self {
            kind: SecretStoreErrorKind::VersionConflict,
            message: "Credential secret version does not match",
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: SecretStoreErrorKind::Unavailable,
            message: "Credential secret store is unavailable",
        }
    }

    #[must_use]
    pub const fn corrupt() -> Self {
        Self {
            kind: SecretStoreErrorKind::Corrupt,
            message: "Credential secret record is invalid",
        }
    }

    #[must_use]
    pub const fn kind(&self) -> SecretStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for SecretStoreError {}

/// Port implemented by local and enterprise secret stores.
///
/// The versioned, scope-bound resolution is the only accepted lookup key;
/// adapters never receive an unscoped Credential reference ID.
pub trait SecretStorePort: Send + Sync {
    /// Resolves one exact current reference to owned secret bytes.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe categories.
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError>;
}

/// Failure from the ordered metadata-check then `SecretStore`-resolution path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialSecretResolutionError {
    Reference(CredentialReferenceError),
    SecretStore(SecretStoreError),
}

impl fmt::Display for CredentialSecretResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reference(error) => error.fmt(formatter),
            Self::SecretStore(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CredentialSecretResolutionError {}

impl From<CredentialReferenceError> for CredentialSecretResolutionError {
    fn from(error: CredentialReferenceError) -> Self {
        Self::Reference(error)
    }
}

impl From<SecretStoreError> for CredentialSecretResolutionError {
    fn from(error: SecretStoreError) -> Self {
        Self::SecretStore(error)
    }
}

impl CredentialReferenceResolution {
    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }

    #[must_use]
    pub const fn scope(&self) -> &Scope {
        &self.scope
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub const fn rotation_version(&self) -> u64 {
        self.rotation_version
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LifecycleStatus {
    Available,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialReferenceRecord {
    schema: String,
    id: CredentialReferenceId,
    scope: Scope,
    provider_id: String,
    display_name: String,
    status: LifecycleStatus,
    rotation_version: u64,
    last_rotated_at: Option<Instant>,
    revoked_at: Option<Instant>,
    updated_at: Instant,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialReferenceTombstone {
    schema: String,
    id: CredentialReferenceId,
    scope: Scope,
    deleted_at: Instant,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CredentialReferenceState {
    Present(CredentialReferenceRecord),
    Deleted(CredentialReferenceTombstone),
}

impl CredentialReferenceState {
    fn id(&self) -> &CredentialReferenceId {
        match self {
            Self::Present(record) => &record.id,
            Self::Deleted(tombstone) => &tombstone.id,
        }
    }

    fn scope(&self) -> &Scope {
        match self {
            Self::Present(record) => &record.scope,
            Self::Deleted(tombstone) => &tombstone.scope,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LifecycleOperation {
    Created,
    Rotated,
    Revoked,
    Deleted,
}

impl LifecycleOperation {
    const fn action_name(self) -> &'static str {
        match self {
            Self::Created => "credential.reference.create",
            Self::Rotated => "credential.reference.rotate",
            Self::Revoked => "credential.reference.revoke",
            Self::Deleted => "credential.reference.delete",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialReferenceLifecycleEvent {
    operation: LifecycleOperation,
    credential_reference_id: CredentialReferenceId,
    revision: Revision,
    projection: Option<CredentialReferenceProjection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialReferenceCatalogManifest {
    schema: String,
    scope_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialReferenceCatalogMutation {
    schema: String,
    scope_sha256: String,
    catalog_revision: u64,
    operation: LifecycleOperation,
    credential_reference_id: CredentialReferenceId,
    resource_revision: u64,
}

struct LoadedCredentialReferenceCatalog {
    key: AggregateJournalKey,
    revision: u64,
    entries: BTreeMap<String, u64>,
    tail_digest: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialReferencePageCursor {
    schema_version: u8,
    scope_sha256: String,
    catalog_revision: u64,
    filter_sha256: String,
    after: String,
}

struct MutationResult {
    receipt: CommitReceipt,
    event: CredentialReferenceLifecycleEvent,
}

struct PreparedCredentialReferenceMutation {
    revision: u64,
    event: CredentialReferenceLifecycleEvent,
    change: StateChange,
    pending_audit: PendingAuditEvent,
    receipt_identity: winwincode_storage::ReceiptIdentity,
    command_digest: Sha256Digest,
    scope_key: winwincode_storage::ReceiptScopeKey,
}

/// Application service for the generated Credential reference HTTP types.
///
/// The service reuses the Control Plane's durable state/receipt transaction,
/// so exact request retries return the original result without rerunning the
/// lifecycle transition.
pub struct CredentialReferenceService<'a> {
    storage: &'a mut dyn ProductStateStorage,
}

impl<'a> CredentialReferenceService<'a> {
    #[must_use]
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Creates secret-free reference metadata after the `SecretStore` adapter
    /// has accepted the write-only locator.
    ///
    /// # Errors
    ///
    /// Rejects invalid fields, a reused identity, a foreign scope, a stale
    /// revision, or a conflicting request replay.
    pub fn create(
        &mut self,
        command: &CredentialReferenceCreateCommand,
        now_millis: u64,
    ) -> Result<CredentialReferenceCreateCompletedResponse, CredentialReferenceError> {
        validate_text(&command.payload.provider_id, 128)?;
        validate_text(&command.payload.display_name, 500)?;
        validate_write_only_locator(&command.payload.vault_locator)?;
        let envelope = envelope(command)?;
        validate_id(&command.payload.credential_reference_id)?;

        if let Some(replay) = self.replay(
            &envelope,
            LifecycleOperation::Created,
            &command.payload.credential_reference_id,
        )? {
            return create_response(command, replay);
        }
        let now = instant(now_millis)?;

        let stream_id = stream_id(&command.payload.credential_reference_id);
        if let Some(stored) = self.storage.load_state(&stream_id)? {
            // Another connection may have committed this exact request after
            // the first receipt read but before this state read. The state and
            // receipt are atomic, so a second receipt read now distinguishes
            // an exact replay from a genuinely different create.
            if let Some(replay) = self.replay(
                &envelope,
                LifecycleOperation::Created,
                &command.payload.credential_reference_id,
            )? {
                return create_response(command, replay);
            }
            let state = decode_state(&stored)?;
            ensure_scope(&command.scope, state.scope())?;
            return Err(CredentialReferenceError::wrong_state());
        }
        ensure_revision(command.expected_revision.0, 0)?;

        let record = CredentialReferenceRecord {
            schema: STATE_SCHEMA.to_owned(),
            id: command.payload.credential_reference_id.clone(),
            scope: command.scope.clone(),
            provider_id: command.payload.provider_id.clone(),
            display_name: command.payload.display_name.clone(),
            status: LifecycleStatus::Available,
            rotation_version: 1,
            last_rotated_at: None,
            revoked_at: None,
            updated_at: now,
        };
        let state = CredentialReferenceState::Present(record);
        let projection = projection(&state, 1)?.ok_or_else(CredentialReferenceError::invalid)?;
        let result = self.commit(
            &envelope,
            LifecycleOperation::Created,
            &state,
            None,
            Some(projection),
            now_millis,
        )?;
        create_response(command, result)
    }

    /// Commits the metadata version of an already successful atomic secret
    /// rotation. The write-only locator participates in request replay identity
    /// but is discarded before state, event, audit, or response construction.
    ///
    /// # Errors
    ///
    /// Rejects a missing, foreign, revoked, deleted, stale, or conflicting
    /// reference.
    pub fn rotate(
        &mut self,
        command: &CredentialReferenceRotateCommand,
        now_millis: u64,
    ) -> Result<CredentialReferenceRotateCompletedResponse, CredentialReferenceError> {
        validate_write_only_locator(&command.payload.vault_locator)?;
        let envelope = envelope(command)?;
        validate_id(&command.payload.credential_reference_id)?;
        if let Some(replay) = self.replay(
            &envelope,
            LifecycleOperation::Rotated,
            &command.payload.credential_reference_id,
        )? {
            return rotate_response(command, replay);
        }
        let now = instant(now_millis)?;

        let (mut record, stored) =
            self.load_present(&command.scope, &command.payload.credential_reference_id)?;
        ensure_revision(command.expected_revision.0, stored.revision)?;
        if record.status == LifecycleStatus::Revoked {
            return Err(CredentialReferenceError::revoked());
        }
        record.rotation_version = record
            .rotation_version
            .checked_add(1)
            .filter(|version| *version <= MAX_SAFE_INTEGER)
            .ok_or_else(CredentialReferenceError::invalid)?;
        record.last_rotated_at = Some(now.clone());
        record.updated_at = now;
        let state = CredentialReferenceState::Present(record);
        let projection = projection(&state, stored.revision + 1)?
            .ok_or_else(CredentialReferenceError::invalid)?;
        let result = self.commit(
            &envelope,
            LifecycleOperation::Rotated,
            &state,
            Some(&stored.payload),
            Some(projection),
            now_millis,
        )?;
        rotate_response(command, result)
    }

    /// Revokes a reference. Every later resolution checks this durable state
    /// before a `SecretStore` adapter can be called.
    ///
    /// # Errors
    ///
    /// Rejects a missing, foreign, already revoked, deleted, stale, or
    /// conflicting reference.
    pub fn revoke(
        &mut self,
        command: &CredentialReferenceRevokeCommand,
        now_millis: u64,
    ) -> Result<CredentialReferenceRevokeCompletedResponse, CredentialReferenceError> {
        let envelope = envelope(command)?;
        validate_id(&command.payload.credential_reference_id)?;
        if let Some(replay) = self.replay(
            &envelope,
            LifecycleOperation::Revoked,
            &command.payload.credential_reference_id,
        )? {
            return revoke_response(command, replay);
        }
        let now = instant(now_millis)?;

        let (mut record, stored) =
            self.load_present(&command.scope, &command.payload.credential_reference_id)?;
        ensure_revision(command.expected_revision.0, stored.revision)?;
        if record.status == LifecycleStatus::Revoked {
            return Err(CredentialReferenceError::wrong_state());
        }
        record.status = LifecycleStatus::Revoked;
        record.revoked_at = Some(now.clone());
        record.updated_at = now;
        let state = CredentialReferenceState::Present(record);
        let projection = projection(&state, stored.revision + 1)?
            .ok_or_else(CredentialReferenceError::invalid)?;
        let result = self.commit(
            &envelope,
            LifecycleOperation::Revoked,
            &state,
            Some(&stored.payload),
            Some(projection),
            now_millis,
        )?;
        revoke_response(command, result)
    }

    /// Replaces the metadata with a scope-bound tombstone. A deleted identity
    /// cannot be recreated under another scope.
    ///
    /// # Errors
    ///
    /// Rejects a missing, foreign, deleted, stale, or conflicting reference.
    pub fn delete(
        &mut self,
        command: &CredentialReferenceDeleteCommand,
        now_millis: u64,
    ) -> Result<CredentialReferenceDeleteCompletedResponse, CredentialReferenceError> {
        let envelope = envelope(command)?;
        validate_id(&command.payload.credential_reference_id)?;
        if let Some(replay) = self.replay(
            &envelope,
            LifecycleOperation::Deleted,
            &command.payload.credential_reference_id,
        )? {
            return delete_response(command, replay);
        }
        let now = instant(now_millis)?;

        let (_, stored) =
            self.load_present(&command.scope, &command.payload.credential_reference_id)?;
        ensure_revision(command.expected_revision.0, stored.revision)?;
        let state = CredentialReferenceState::Deleted(CredentialReferenceTombstone {
            schema: STATE_SCHEMA.to_owned(),
            id: command.payload.credential_reference_id.clone(),
            scope: command.scope.clone(),
            deleted_at: now,
        });
        let result = self.commit(
            &envelope,
            LifecycleOperation::Deleted,
            &state,
            Some(&stored.payload),
            None,
            now_millis,
        )?;
        delete_response(command, result)
    }

    /// Loads the generated, secret-free projection for one exact scope.
    ///
    /// # Errors
    ///
    /// Rejects an invalid request identity, a foreign scope, or a missing or
    /// deleted reference.
    pub fn get(
        &self,
        query: &CredentialReferenceGetQuery,
    ) -> Result<CredentialReferenceGetResultResponse, CredentialReferenceError> {
        validate_request_id(&query.request_id)?;
        receipt_scope_key(&query.scope)?;
        validate_id(&query.parameters.credential_reference_id)?;
        let stored = self
            .storage
            .load_state(&stream_id(&query.parameters.credential_reference_id))?
            .ok_or_else(CredentialReferenceError::not_found)?;
        let state = decode_state(&stored)?;
        ensure_scope(&query.scope, state.scope())?;
        let result =
            projection(&state, stored.revision)?.ok_or_else(CredentialReferenceError::not_found)?;
        checked_http_response(CredentialReferenceGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: CredentialReferenceGetResultResponseQuery::CredentialReferenceGet,
            request_id: query.request_id.clone(),
            result,
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Lists the secret-free references in one exact scope using a
    /// revision-bound stable cursor.
    ///
    /// Both available and revoked references are returned. Deleted references
    /// are absent from the scope catalog. A cursor expires explicitly when any
    /// reference in the scope is created, rotated, revoked, or deleted.
    ///
    /// # Errors
    ///
    /// Rejects an invalid scope, provider filter, page limit, stale/foreign
    /// cursor, or a catalog that no longer agrees with canonical reference
    /// state.
    pub fn list(
        &self,
        query: &CredentialReferenceListQuery,
    ) -> Result<CredentialReferenceListResultResponse, CredentialReferenceError> {
        validate_request_id(&query.request_id)?;
        let scope_key = receipt_scope_key(&query.scope)?;
        if let Some(provider_id) = &query.parameters.provider_id {
            validate_text(provider_id, 128)?;
        }
        let limit = usize::try_from(query.page.limit)
            .ok()
            .filter(|limit| (1..=MAX_PAGE_SIZE).contains(limit))
            .ok_or_else(CredentialReferenceError::invalid)?;
        let catalog = self.load_catalog(&scope_key)?;
        let filter_sha256 = digest_json(&query.parameters)?;
        let after = decode_page_cursor(
            query.page.cursor.as_ref(),
            &scope_key,
            catalog.revision,
            &filter_sha256,
        )?;
        if let Some(after) = &after
            && !catalog.entries.contains_key(after)
        {
            return Err(CredentialReferenceError::cursor_invalid());
        }

        let mut matches = Vec::with_capacity(limit.saturating_add(1));
        for (id, catalog_entry_revision) in &catalog.entries {
            if after.as_ref().is_some_and(|after| id <= after) {
                continue;
            }
            let credential_reference_id = CredentialReferenceId(id.clone());
            let stored = self
                .storage
                .load_state(&stream_id(&credential_reference_id))?;
            let Some(stored) = stored else {
                self.ensure_catalog_revision(&scope_key, catalog.revision)?;
                return Err(CredentialReferenceError::invalid());
            };
            if stored.revision != *catalog_entry_revision {
                self.ensure_catalog_revision(&scope_key, catalog.revision)?;
                return Err(CredentialReferenceError::invalid());
            }
            let state = decode_state(&stored)?;
            ensure_scope(&query.scope, state.scope())?;
            let CredentialReferenceState::Present(record) = &state else {
                self.ensure_catalog_revision(&scope_key, catalog.revision)?;
                return Err(CredentialReferenceError::invalid());
            };
            if query
                .parameters
                .provider_id
                .as_ref()
                .is_some_and(|provider_id| provider_id != &record.provider_id)
            {
                continue;
            }
            matches.push(
                projection(&state, stored.revision)?
                    .ok_or_else(CredentialReferenceError::invalid)?,
            );
            if matches.len() > limit {
                break;
            }
        }
        self.ensure_catalog_revision(&scope_key, catalog.revision)?;

        let has_more = matches.len() > limit;
        if has_more {
            matches.pop();
        }
        let next_cursor = if has_more {
            let after = matches
                .last()
                .map(|projection| projection.id.0.clone())
                .ok_or_else(CredentialReferenceError::invalid)?;
            Some(encode_page_cursor(
                &scope_key,
                catalog.revision,
                &filter_sha256,
                after,
            )?)
        } else {
            None
        };
        checked_http_response(CredentialReferenceListResultResponse {
            page: PageInfo {
                has_more,
                next_cursor,
            },
            query: CredentialReferenceListResultResponseQuery::CredentialReferenceList,
            request_id: query.request_id.clone(),
            result: CredentialReferencePage {
                items: matches,
                kind: CredentialReferencePageKind::CredentialReferencePage,
            },
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Returns a versioned, scope-bound reference for `SecretStore` resolution.
    /// Revocation is checked before this value can reach a `SecretStore` adapter.
    ///
    /// # Errors
    ///
    /// Rejects a foreign, missing, deleted, or revoked reference.
    pub fn resolve(
        &self,
        scope: &Scope,
        credential_reference_id: &CredentialReferenceId,
    ) -> Result<CredentialReferenceResolution, CredentialReferenceError> {
        receipt_scope_key(scope)?;
        validate_id(credential_reference_id)?;
        let stored = self
            .storage
            .load_state(&stream_id(credential_reference_id))?
            .ok_or_else(CredentialReferenceError::not_found)?;
        let state = decode_state(&stored)?;
        ensure_scope(scope, state.scope())?;
        let CredentialReferenceState::Present(record) = state else {
            return Err(CredentialReferenceError::not_found());
        };
        if record.status == LifecycleStatus::Revoked {
            return Err(CredentialReferenceError::revoked());
        }
        Ok(CredentialReferenceResolution {
            credential_reference_id: record.id,
            scope: record.scope,
            provider_id: record.provider_id,
            rotation_version: record.rotation_version,
        })
    }

    /// Checks current scope and revocation state, then calls the `SecretStore`
    /// exactly once with the current rotation version.
    ///
    /// # Errors
    ///
    /// A reference failure is returned before the `SecretStore` is accessed;
    /// otherwise the adapter's stable secret-safe error is preserved.
    pub fn resolve_secret(
        &self,
        secret_store: &dyn SecretStorePort,
        scope: &Scope,
        credential_reference_id: &CredentialReferenceId,
    ) -> Result<ResolvedSecret, CredentialSecretResolutionError> {
        let reference = self.resolve(scope, credential_reference_id)?;
        secret_store.resolve(&reference).map_err(Into::into)
    }

    fn load_present(
        &self,
        scope: &Scope,
        credential_reference_id: &CredentialReferenceId,
    ) -> Result<(CredentialReferenceRecord, StoredState), CredentialReferenceError> {
        let stored = self
            .storage
            .load_state(&stream_id(credential_reference_id))?
            .ok_or_else(CredentialReferenceError::not_found)?;
        let state = decode_state(&stored)?;
        ensure_scope(scope, state.scope())?;
        match state {
            CredentialReferenceState::Present(record) => Ok((record, stored)),
            CredentialReferenceState::Deleted(_) => Err(CredentialReferenceError::not_found()),
        }
    }

    fn load_catalog(
        &self,
        scope_key: &winwincode_storage::ReceiptScopeKey,
    ) -> Result<LoadedCredentialReferenceCatalog, CredentialReferenceError> {
        let key = catalog_key(scope_key)?;
        let scope_sha256 = scope_digest(scope_key);
        let loaded = self.storage.load_journal(&key)?;
        decode_catalog(key, &scope_sha256, loaded)
    }

    fn ensure_catalog_revision(
        &self,
        scope_key: &winwincode_storage::ReceiptScopeKey,
        expected_revision: u64,
    ) -> Result<(), CredentialReferenceError> {
        if self.load_catalog(scope_key)?.revision != expected_revision {
            return Err(CredentialReferenceError::cursor_invalid());
        }
        Ok(())
    }

    fn replay(
        &self,
        command: &CommandEnvelope,
        operation: LifecycleOperation,
        credential_reference_id: &CredentialReferenceId,
    ) -> Result<Option<MutationResult>, CredentialReferenceError> {
        let (identity, digest) = command_receipt(command)?;
        let Some(receipt) = self.storage.load_receipt(&identity, &digest)? else {
            return Ok(None);
        };
        let event = lifecycle_event(&receipt)?;
        if event.operation != operation
            || event.credential_reference_id != *credential_reference_id
            || event.revision.0 != i64::try_from(receipt.revision).unwrap_or(-1)
        {
            return Err(CredentialReferenceError::invalid());
        }
        Ok(Some(MutationResult { receipt, event }))
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        command: &CommandEnvelope,
        operation: LifecycleOperation,
        next: &CredentialReferenceState,
        previous_payload: Option<&[u8]>,
        next_projection: Option<CredentialReferenceProjection>,
        now_millis: u64,
    ) -> Result<MutationResult, CredentialReferenceError> {
        let prepared = prepare_mutation(
            command,
            operation,
            next,
            previous_payload,
            next_projection,
            now_millis,
        )?;
        let mut receipt = None;
        for attempt in 0..MAX_CATALOG_COMMIT_ATTEMPTS {
            let catalog = self.load_catalog(&prepared.scope_key)?;
            let catalog_revision = catalog
                .revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_INTEGER)
                .ok_or_else(CredentialReferenceError::invalid)?;
            let publication =
                match catalog_publication(catalog, operation, next.id(), prepared.revision) {
                    Ok(publication) => publication,
                    Err(error) => {
                        if let Some(replayed) = self
                            .storage
                            .load_receipt(&prepared.receipt_identity, &prepared.command_digest)?
                        {
                            receipt = Some(replayed);
                            break;
                        }
                        return Err(error);
                    }
                };
            let invalidation = model_route_availability_invalidated_event(
                &command.actor,
                next.scope(),
                ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::CredentialReference,
                catalog_revision,
                instant(now_millis)?,
                command.request_id.0.as_bytes(),
            )?;
            let mut change = prepared.change.clone();
            change.events.push(invalidation);
            let commit = storage_commit(command, change)?
                .with_pending_audit_event(prepared.pending_audit.clone())
                .with_journal_publication(publication);
            match self.storage.commit(&commit) {
                Ok(committed) => {
                    receipt = Some(committed);
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        StorageErrorKind::RequestConflict
                            | StorageErrorKind::RevisionConflict
                            | StorageErrorKind::JournalAlreadyExists
                            | StorageErrorKind::JournalConflict
                    ) =>
                {
                    if let Some(replayed) = self
                        .storage
                        .load_receipt(&prepared.receipt_identity, &prepared.command_digest)?
                    {
                        receipt = Some(replayed);
                        break;
                    }
                    if matches!(
                        error.kind(),
                        StorageErrorKind::JournalAlreadyExists | StorageErrorKind::JournalConflict
                    ) && attempt + 1 < MAX_CATALOG_COMMIT_ATTEMPTS
                    {
                        continue;
                    }
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            }
        }
        let receipt = receipt.ok_or_else(|| {
            CredentialReferenceError::new(
                CredentialReferenceErrorKind::Storage,
                "Credential reference storage operation failed",
            )
        })?;
        let durable_event = lifecycle_event(&receipt)?;
        if !receipt.idempotent_replay && durable_event != prepared.event {
            return Err(CredentialReferenceError::invalid());
        }
        Ok(MutationResult {
            receipt,
            event: durable_event,
        })
    }
}

fn prepare_mutation(
    command: &CommandEnvelope,
    operation: LifecycleOperation,
    next: &CredentialReferenceState,
    previous_payload: Option<&[u8]>,
    next_projection: Option<CredentialReferenceProjection>,
    now_millis: u64,
) -> Result<PreparedCredentialReferenceMutation, CredentialReferenceError> {
    let expected_revision = u64::try_from(command.expected_revision.0)
        .map_err(|_| CredentialReferenceError::invalid())?;
    let revision = expected_revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(CredentialReferenceError::invalid)?;
    let next_payload = serde_json::to_vec(next).map_err(|_| CredentialReferenceError::invalid())?;
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &next_payload)?;
    let event = CredentialReferenceLifecycleEvent {
        operation,
        credential_reference_id: next.id().clone(),
        revision: Revision(
            i64::try_from(revision).map_err(|_| CredentialReferenceError::invalid())?,
        ),
        projection: next_projection,
    };
    let event_payload =
        serde_json::to_vec(&event).map_err(|_| CredentialReferenceError::invalid())?;
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Event, &event_payload)?;
    let event_id = lifecycle_event_id(command, operation, next.id(), revision)?;
    let change = StateChange::new(
        stream_id(next.id()),
        next_payload.clone(),
        vec![NewOutboxEvent::internal(
            event_id,
            EVENT_TOPIC,
            event_payload,
        )],
    );
    let pending_audit = audit_event(
        command,
        operation,
        previous_payload,
        &next_payload,
        now_millis,
        next.id(),
        revision,
    )?;
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Audit, pending_audit.payload())?;
    let (receipt_identity, command_digest) = command_receipt(command)?;
    Ok(PreparedCredentialReferenceMutation {
        revision,
        event,
        change,
        pending_audit,
        receipt_identity,
        command_digest,
        scope_key: receipt_scope_key(next.scope())?,
    })
}

fn create_response(
    command: &CredentialReferenceCreateCommand,
    result: MutationResult,
) -> Result<CredentialReferenceCreateCompletedResponse, CredentialReferenceError> {
    checked_http_response(CredentialReferenceCreateCompletedResponse {
        command: CredentialReferenceCreateCompletedResponseCommand::CredentialReferenceCreate,
        current_revision: result.event.revision,
        outcome: CredentialReferenceCreateCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(result.receipt.revision)?,
        request_id: command.request_id.clone(),
        result: result
            .event
            .projection
            .ok_or_else(CredentialReferenceError::invalid)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn rotate_response(
    command: &CredentialReferenceRotateCommand,
    result: MutationResult,
) -> Result<CredentialReferenceRotateCompletedResponse, CredentialReferenceError> {
    checked_http_response(CredentialReferenceRotateCompletedResponse {
        command: CredentialReferenceRotateCompletedResponseCommand::CredentialReferenceRotate,
        current_revision: result.event.revision,
        outcome: CredentialReferenceRotateCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(result.receipt.revision)?,
        request_id: command.request_id.clone(),
        result: result
            .event
            .projection
            .ok_or_else(CredentialReferenceError::invalid)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn revoke_response(
    command: &CredentialReferenceRevokeCommand,
    result: MutationResult,
) -> Result<CredentialReferenceRevokeCompletedResponse, CredentialReferenceError> {
    checked_http_response(CredentialReferenceRevokeCompletedResponse {
        command: CredentialReferenceRevokeCompletedResponseCommand::CredentialReferenceRevoke,
        current_revision: result.event.revision,
        outcome: CredentialReferenceRevokeCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(result.receipt.revision)?,
        request_id: command.request_id.clone(),
        result: result
            .event
            .projection
            .ok_or_else(CredentialReferenceError::invalid)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn delete_response(
    command: &CredentialReferenceDeleteCommand,
    result: MutationResult,
) -> Result<CredentialReferenceDeleteCompletedResponse, CredentialReferenceError> {
    if result.event.projection.is_some() {
        return Err(CredentialReferenceError::invalid());
    }
    let revision = result.event.revision;
    checked_http_response(CredentialReferenceDeleteCompletedResponse {
        command: CredentialReferenceDeleteCompletedResponseCommand::CredentialReferenceDelete,
        current_revision: revision.clone(),
        outcome: CredentialReferenceDeleteCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(result.receipt.revision)?,
        request_id: command.request_id.clone(),
        result: MutationReceipt {
            resource_kind: "credential_reference".to_owned(),
            revision,
        },
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn checked_http_response<T: Serialize>(value: T) -> Result<T, CredentialReferenceError> {
    CredentialLeakGate::default().inspect_serializable(CredentialOutputBoundary::Http, &value)?;
    Ok(value)
}

fn catalog_key(
    scope_key: &winwincode_storage::ReceiptScopeKey,
) -> Result<AggregateJournalKey, CredentialReferenceError> {
    AggregateJournalKey::new(
        CATALOG_AGGREGATE_TYPE,
        scope_digest(scope_key).trim_start_matches("sha256:"),
    )
    .map_err(Into::into)
}

fn decode_catalog(
    key: AggregateJournalKey,
    scope_sha256: &str,
    loaded: Option<LoadedAggregateJournal>,
) -> Result<LoadedCredentialReferenceCatalog, CredentialReferenceError> {
    let Some(loaded) = loaded else {
        return Ok(LoadedCredentialReferenceCatalog {
            key,
            revision: 0,
            entries: BTreeMap::new(),
            tail_digest: None,
        });
    };
    let manifest: CredentialReferenceCatalogManifest = serde_json::from_slice(&loaded.manifest)
        .map_err(|_| CredentialReferenceError::invalid())?;
    if manifest.schema != CATALOG_SCHEMA || manifest.scope_sha256 != scope_sha256 {
        return Err(CredentialReferenceError::invalid());
    }
    let canonical_manifest =
        serde_json::to_vec(&manifest).map_err(|_| CredentialReferenceError::invalid())?;
    if canonical_manifest != loaded.manifest {
        return Err(CredentialReferenceError::invalid());
    }
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &loaded.manifest)?;

    let mut entries = BTreeMap::new();
    let mut tail_digest = None;
    for (index, record) in loaded.records.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(CredentialReferenceError::invalid)?;
        if record.sequence != expected_sequence
            || record.sequence > MAX_SAFE_INTEGER
            || record.digest != digest_bytes(&record.payload)
        {
            return Err(CredentialReferenceError::invalid());
        }
        CredentialLeakGate::default()
            .inspect_json_bytes(CredentialOutputBoundary::Persistence, &record.payload)?;
        let mutation: CredentialReferenceCatalogMutation = serde_json::from_slice(&record.payload)
            .map_err(|_| CredentialReferenceError::invalid())?;
        if mutation.schema != CATALOG_SCHEMA
            || mutation.scope_sha256 != scope_sha256
            || mutation.catalog_revision != record.sequence
            || validate_id(&mutation.credential_reference_id).is_err()
            || mutation.resource_revision == 0
            || mutation.resource_revision > MAX_SAFE_INTEGER
            || serde_json::to_vec(&mutation).map_err(|_| CredentialReferenceError::invalid())?
                != record.payload
            || apply_catalog_entry(
                &mut entries,
                mutation.operation,
                &mutation.credential_reference_id,
                mutation.resource_revision,
            )
            .is_err()
        {
            return Err(CredentialReferenceError::invalid());
        }
        tail_digest = Some(record.digest.clone());
    }
    let tail_digest = tail_digest.ok_or_else(CredentialReferenceError::invalid)?;
    let revision =
        u64::try_from(loaded.records.len()).map_err(|_| CredentialReferenceError::invalid())?;
    Ok(LoadedCredentialReferenceCatalog {
        key,
        revision,
        entries,
        tail_digest: Some(tail_digest),
    })
}

fn catalog_publication(
    mut catalog: LoadedCredentialReferenceCatalog,
    operation: LifecycleOperation,
    credential_reference_id: &CredentialReferenceId,
    resource_revision: u64,
) -> Result<AggregateJournalPublication, CredentialReferenceError> {
    apply_catalog_entry(
        &mut catalog.entries,
        operation,
        credential_reference_id,
        resource_revision,
    )?;

    let catalog_revision = catalog
        .revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(CredentialReferenceError::invalid)?;
    let scope_sha256 = catalog.key.aggregate_id();
    let scope_sha256 = format!("sha256:{scope_sha256}");
    let mutation = CredentialReferenceCatalogMutation {
        schema: CATALOG_SCHEMA.to_owned(),
        scope_sha256: scope_sha256.clone(),
        catalog_revision,
        operation,
        credential_reference_id: credential_reference_id.clone(),
        resource_revision,
    };
    let payload = serde_json::to_vec(&mutation).map_err(|_| CredentialReferenceError::invalid())?;
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &payload)?;
    let digest = digest_bytes(&payload);
    let record = AggregateJournalRecord::new(catalog_revision, digest, payload);
    if let Some(expected_tail_digest) = catalog.tail_digest {
        return Ok(AggregateJournalPublication::Append {
            key: catalog.key,
            expected_tail_sequence: catalog.revision,
            expected_tail_digest,
            record,
        });
    }
    if catalog.revision != 0 {
        return Err(CredentialReferenceError::invalid());
    }
    let manifest = CredentialReferenceCatalogManifest {
        schema: CATALOG_SCHEMA.to_owned(),
        scope_sha256,
    };
    let manifest =
        serde_json::to_vec(&manifest).map_err(|_| CredentialReferenceError::invalid())?;
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &manifest)?;
    Ok(AggregateJournalPublication::Create {
        key: catalog.key,
        manifest,
        first_record: record,
    })
}

fn apply_catalog_entry(
    entries: &mut BTreeMap<String, u64>,
    operation: LifecycleOperation,
    credential_reference_id: &CredentialReferenceId,
    resource_revision: u64,
) -> Result<(), CredentialReferenceError> {
    let previous_resource_revision = resource_revision
        .checked_sub(1)
        .ok_or_else(CredentialReferenceError::invalid)?;
    if operation == LifecycleOperation::Created {
        if previous_resource_revision != 0
            || entries
                .insert(credential_reference_id.0.clone(), resource_revision)
                .is_some()
        {
            return Err(CredentialReferenceError::invalid());
        }
        return Ok(());
    }
    if entries.get(&credential_reference_id.0) != Some(&previous_resource_revision) {
        return Err(CredentialReferenceError::revision_conflict());
    }
    if operation == LifecycleOperation::Deleted {
        entries.remove(&credential_reference_id.0);
    } else {
        entries.insert(credential_reference_id.0.clone(), resource_revision);
    }
    Ok(())
}

fn encode_page_cursor(
    scope_key: &winwincode_storage::ReceiptScopeKey,
    catalog_revision: u64,
    filter_sha256: &str,
    after: String,
) -> Result<OpaqueCursor, CredentialReferenceError> {
    let cursor = CredentialReferencePageCursor {
        schema_version: CURSOR_SCHEMA,
        scope_sha256: scope_digest(scope_key),
        catalog_revision,
        filter_sha256: filter_sha256.to_owned(),
        after,
    };
    let bytes = serde_json::to_vec(&cursor).map_err(|_| CredentialReferenceError::invalid())?;
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_page_cursor(
    cursor: Option<&OpaqueCursor>,
    scope_key: &winwincode_storage::ReceiptScopeKey,
    catalog_revision: u64,
    filter_sha256: &str,
) -> Result<Option<String>, CredentialReferenceError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES {
        return Err(CredentialReferenceError::cursor_invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| CredentialReferenceError::cursor_invalid())?;
    let decoded: CredentialReferencePageCursor =
        serde_json::from_slice(&bytes).map_err(|_| CredentialReferenceError::cursor_invalid())?;
    if decoded.schema_version != CURSOR_SCHEMA
        || decoded.scope_sha256 != scope_digest(scope_key)
        || decoded.catalog_revision != catalog_revision
        || decoded.filter_sha256 != filter_sha256
        || validate_id(&CredentialReferenceId(decoded.after.clone())).is_err()
    {
        return Err(CredentialReferenceError::cursor_invalid());
    }
    Ok(Some(decoded.after))
}

fn scope_digest(scope_key: &winwincode_storage::ReceiptScopeKey) -> String {
    digest_bytes(scope_key.as_bytes())
}

fn digest_json(value: &impl Serialize) -> Result<String, CredentialReferenceError> {
    let bytes = serde_json::to_vec(value).map_err(|_| CredentialReferenceError::invalid())?;
    Ok(digest_bytes(&bytes))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn previous_revision(revision: u64) -> Result<Revision, CredentialReferenceError> {
    let previous = revision
        .checked_sub(1)
        .ok_or_else(CredentialReferenceError::invalid)?;
    Ok(Revision(
        i64::try_from(previous).map_err(|_| CredentialReferenceError::invalid())?,
    ))
}

fn envelope(command: &impl Serialize) -> Result<CommandEnvelope, CredentialReferenceError> {
    let value = serde_json::to_value(command).map_err(|_| CredentialReferenceError::invalid())?;
    serde_json::from_value(value).map_err(|_| CredentialReferenceError::invalid())
}

fn decode_state(
    stored: &StoredState,
) -> Result<CredentialReferenceState, CredentialReferenceError> {
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &stored.payload)?;
    let state: CredentialReferenceState =
        serde_json::from_slice(&stored.payload).map_err(|_| CredentialReferenceError::invalid())?;
    if state.id().0.is_empty()
        || match &state {
            CredentialReferenceState::Present(record) => record.schema != STATE_SCHEMA,
            CredentialReferenceState::Deleted(tombstone) => tombstone.schema != STATE_SCHEMA,
        }
        || stored.stream_id != stream_id(state.id())
        || stored.revision == 0
        || stored.revision > MAX_SAFE_INTEGER
    {
        return Err(CredentialReferenceError::invalid());
    }
    Ok(state)
}

fn lifecycle_event(
    receipt: &CommitReceipt,
) -> Result<CredentialReferenceLifecycleEvent, CredentialReferenceError> {
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == EVENT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(CredentialReferenceError::invalid());
    };
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Event, &event.payload)?;
    let decoded: CredentialReferenceLifecycleEvent =
        serde_json::from_slice(&event.payload).map_err(|_| CredentialReferenceError::invalid())?;
    if serde_json::to_vec(&decoded).map_err(|_| CredentialReferenceError::invalid())?
        != event.payload
    {
        return Err(CredentialReferenceError::invalid());
    }
    Ok(decoded)
}

fn projection(
    state: &CredentialReferenceState,
    revision: u64,
) -> Result<Option<CredentialReferenceProjection>, CredentialReferenceError> {
    let CredentialReferenceState::Present(record) = state else {
        return Ok(None);
    };
    let rotation_version =
        i64::try_from(record.rotation_version).map_err(|_| CredentialReferenceError::invalid())?;
    let revision = i64::try_from(revision).map_err(|_| CredentialReferenceError::invalid())?;
    Ok(Some(CredentialReferenceProjection {
        display_name: record.display_name.clone(),
        id: record.id.clone(),
        last_rotated_at: record.last_rotated_at.clone(),
        provider_id: record.provider_id.clone(),
        revision: Revision(revision),
        revoked_at: record.revoked_at.clone(),
        rotation_version,
        secret_state: match record.status {
            LifecycleStatus::Available => "available",
            LifecycleStatus::Revoked => "revoked",
        }
        .to_owned(),
        updated_at: record.updated_at.clone(),
    }))
}

fn audit_event(
    command: &CommandEnvelope,
    operation: LifecycleOperation,
    previous_payload: Option<&[u8]>,
    next_payload: &[u8],
    now_millis: u64,
    credential_reference_id: &CredentialReferenceId,
    revision: u64,
) -> Result<PendingAuditEvent, CredentialReferenceError> {
    let before = previous_payload.map(digest);
    let after = digest(next_payload);
    let identity_digest = safe_identity_digest(
        b"winwincode.credential-reference-audit.v1",
        command,
        operation,
        credential_reference_id,
        revision,
    )?;
    let event_id = AuditEventId::from_digest(&identity_digest)
        .map_err(|_| CredentialReferenceError::invalid())?;
    let event = AuditEvent::state_change(
        event_id,
        now_millis,
        audit_actor(&command.actor),
        audit_scope(&command.scope)?,
        command.request_id.clone(),
        AuditAction::credential(operation.action_name(), credential_reference_id.clone())
            .map_err(|_| CredentialReferenceError::invalid())?,
        AuditState::changed(before, after).map_err(|_| CredentialReferenceError::invalid())?,
        AuditOrigin::local(AUDIT_ORIGIN).map_err(|_| CredentialReferenceError::invalid())?,
        AuditSubject::new(),
        "completed",
        AuditRetention::Indefinite,
    )
    .map_err(|_| CredentialReferenceError::invalid())?;
    let event_id = event.event_id().as_str().to_owned();
    let payload = serde_json::to_vec(&event).map_err(|_| CredentialReferenceError::invalid())?;
    PendingAuditEvent::new(event_id, payload).map_err(Into::into)
}

fn audit_actor(actor: &Actor) -> AuditActor {
    match actor {
        Actor::UserActor(actor) => AuditActor::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => AuditActor::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => AuditActor::System(actor.id.clone()),
    }
}

fn audit_scope(scope: &Scope) -> Result<AuditScope, CredentialReferenceError> {
    match scope {
        Scope::OrganizationScope(scope) => AuditScope::organization(scope.organization_id.clone()),
        Scope::WorkspaceScope(scope) => {
            AuditScope::workspace(scope.organization_id.clone(), scope.workspace_id.clone())
        }
        Scope::ProjectScope(scope) => AuditScope::project(
            scope.organization_id.clone(),
            scope.workspace_id.clone(),
            scope.project_id.clone(),
        ),
        Scope::RepositoryScope(scope) => AuditScope::repository(
            scope.organization_id.clone(),
            scope.workspace_id.clone(),
            scope.project_id.clone(),
            scope.repository_id.clone(),
        ),
    }
    .map_err(|_| CredentialReferenceError::invalid())
}

fn lifecycle_event_id(
    command: &CommandEnvelope,
    operation: LifecycleOperation,
    credential_reference_id: &CredentialReferenceId,
    revision: u64,
) -> Result<String, CredentialReferenceError> {
    Ok(format!(
        "credential-reference:{}",
        safe_identity_digest(
            b"winwincode.credential-reference-event.v1",
            command,
            operation,
            credential_reference_id,
            revision,
        )?
        .0
        .trim_start_matches("sha256:")
    ))
}

fn safe_identity_digest(
    domain: &[u8],
    command: &CommandEnvelope,
    operation: LifecycleOperation,
    credential_reference_id: &CredentialReferenceId,
    revision: u64,
) -> Result<Sha256Digest, CredentialReferenceError> {
    let scope =
        serde_json::to_vec(&command.scope).map_err(|_| CredentialReferenceError::invalid())?;
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update([0]);
    hash.update(operation.action_name().as_bytes());
    hash.update([0]);
    hash.update(credential_reference_id.0.as_bytes());
    hash.update([0]);
    hash.update(command.request_id.0.as_bytes());
    hash.update([0]);
    hash.update(revision.to_be_bytes());
    hash.update([0]);
    hash.update(scope);
    Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
}

fn digest(payload: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload)))
}

fn ensure_scope(requested: &Scope, stored: &Scope) -> Result<(), CredentialReferenceError> {
    if requested == stored {
        Ok(())
    } else {
        Err(CredentialReferenceError::scope_denied())
    }
}

fn ensure_revision(expected: i64, current: u64) -> Result<(), CredentialReferenceError> {
    if expected < 0 || u64::try_from(expected).ok() != Some(current) {
        Err(CredentialReferenceError::revision_conflict())
    } else {
        Ok(())
    }
}

fn stream_id(credential_reference_id: &CredentialReferenceId) -> String {
    format!("{STREAM_PREFIX}{}", credential_reference_id.0)
}

fn validate_id(value: &CredentialReferenceId) -> Result<(), CredentialReferenceError> {
    let Some(suffix) = value.0.strip_prefix("crd_") else {
        return Err(CredentialReferenceError::invalid());
    };
    if suffix.len() == 26 && suffix.bytes().all(|byte| {
        byte.is_ascii_digit()
            || matches!(byte, b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
    }) {
        Ok(())
    } else {
        Err(CredentialReferenceError::invalid())
    }
}

fn validate_request_id(value: &RequestId) -> Result<(), CredentialReferenceError> {
    let Some(suffix) = value.0.strip_prefix("req_") else {
        return Err(CredentialReferenceError::invalid());
    };
    if suffix.len() == 26 && suffix.bytes().all(|byte| {
        byte.is_ascii_digit()
            || matches!(byte, b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
    }) {
        Ok(())
    } else {
        Err(CredentialReferenceError::invalid())
    }
}

fn validate_text(value: &str, max_length: usize) -> Result<(), CredentialReferenceError> {
    if !value.is_empty() && value.chars().count() <= max_length {
        Ok(())
    } else {
        Err(CredentialReferenceError::invalid())
    }
}

fn validate_write_only_locator(value: &str) -> Result<(), CredentialReferenceError> {
    validate_text(value, 2048)
}

fn instant(now_millis: u64) -> Result<Instant, CredentialReferenceError> {
    let seconds = now_millis / 1_000;
    let millis = now_millis % 1_000;
    let days = i64::try_from(seconds / 86_400).map_err(|_| CredentialReferenceError::invalid())?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err(CredentialReferenceError::invalid());
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let instant = Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ));
    instant_millis(&instant)?;
    Ok(instant)
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
