// SPDX-License-Identifier: Apache-2.0

//! Durable personal and repository knowledge with explicit confirmation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{RepositoryId, RequestId, Sha256Digest, UserId};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StorageError, StorageErrorKind, StoredState,
};

const STATE_SCHEMA: &str = "winwincode.knowledge-catalog.v1";
const STREAM_PREFIX: &str = "knowledge-catalog:";
const CHANGE_TOPIC: &str = "knowledge.catalog.changed.v1";
const MAX_ENTRIES: usize = 1_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Stable failure categories for knowledge commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnowledgeErrorKind {
    InvalidRequest,
    EntryNotFound,
    InvalidState,
    SourceDeleted,
    CatalogFull,
    RevisionConflict,
    RequestConflict,
    Storage,
}

/// Secret-safe knowledge service failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeError {
    kind: KnowledgeErrorKind,
    message: &'static str,
}

impl KnowledgeError {
    const fn new(kind: KnowledgeErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            KnowledgeErrorKind::InvalidRequest,
            "knowledge request is invalid",
        )
    }

    const fn not_found() -> Self {
        Self::new(
            KnowledgeErrorKind::EntryNotFound,
            "knowledge entry was not found",
        )
    }

    const fn invalid_state() -> Self {
        Self::new(
            KnowledgeErrorKind::InvalidState,
            "knowledge entry is not in the required state",
        )
    }

    const fn source_deleted() -> Self {
        Self::new(
            KnowledgeErrorKind::SourceDeleted,
            "knowledge source was deleted",
        )
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> KnowledgeErrorKind {
        self.kind
    }
}

impl fmt::Display for KnowledgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for KnowledgeError {}

impl From<StorageError> for KnowledgeError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RevisionConflict => Self::new(
                KnowledgeErrorKind::RevisionConflict,
                "knowledge catalog revision does not match",
            ),
            StorageErrorKind::RequestConflict => Self::new(
                KnowledgeErrorKind::RequestConflict,
                "knowledge requestId was reused with different input",
            ),
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => {
                Self::invalid()
            }
            StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => Self::new(
                KnowledgeErrorKind::Storage,
                "knowledge storage operation failed",
            ),
        }
    }
}

/// Community knowledge is either personal or bound to one repository.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum KnowledgeScope {
    Personal,
    Repository { repository_id: RepositoryId },
}

/// How the candidate knowledge entered the catalog.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeOrigin {
    MachineSuggestion,
    UserAuthored,
    UserCorrection,
}

/// Explicit lifecycle state. Only `Confirmed` entries can enter model context.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    Suggested,
    Confirmed,
    NeedsReconfirmation,
    Archived,
    Revoked,
    SourceDeleted,
}

/// Exact location of the source material without storing another source copy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum KnowledgeSourceLocator {
    Document {
        relative_path: String,
        start_line: u64,
        end_line: u64,
    },
    EventRange {
        stream_id: String,
        start_sequence: u64,
        end_sequence: u64,
    },
}

/// Source identity, current version, and an authorization-bearing repository.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeSource {
    pub source_id: String,
    pub repository_id: Option<RepositoryId>,
    pub version_digest: Sha256Digest,
    pub locator: KnowledgeSourceLocator,
}

/// Editable knowledge content. Every create or edit starts unconfirmed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeDraft {
    pub title: String,
    pub body: String,
    pub rule_key: String,
    pub scope: KnowledgeScope,
    pub origin: KnowledgeOrigin,
    pub source: KnowledgeSource,
    pub expires_at_millis: Option<u64>,
}

/// One durable catalog mutation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum KnowledgeAction {
    Create {
        entry_id: String,
        draft: KnowledgeDraft,
    },
    Edit {
        entry_id: String,
        draft: KnowledgeDraft,
    },
    Confirm {
        entry_id: String,
    },
    Archive {
        entry_id: String,
    },
    Revoke {
        entry_id: String,
    },
    SourceVersionChanged {
        source_id: String,
        version_digest: Sha256Digest,
    },
    DeleteSource {
        source_id: String,
    },
}

/// Authenticated, idempotent mutation envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeCommand {
    pub actor_user_id: UserId,
    pub request_id: RequestId,
    pub expected_catalog_revision: u64,
    pub occurred_at_millis: u64,
    pub action: KnowledgeAction,
}

/// Durable result of a knowledge mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeMutationReceipt {
    pub request_id: RequestId,
    pub catalog_revision: u64,
    pub idempotent_replay: bool,
}

/// Caller authorization used by every read before matching or counting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeAccess {
    pub user_id: UserId,
    pub authorized_repository_ids: Vec<RepositoryId>,
}

/// Safe projection of an entry whose source remains visible to the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeEntry {
    pub entry_id: String,
    pub title: String,
    pub body: String,
    pub rule_key: String,
    pub scope: KnowledgeScope,
    pub origin: KnowledgeOrigin,
    pub source: KnowledgeSource,
    pub status: KnowledgeStatus,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
    pub expires_at_millis: Option<u64>,
}

/// Generic unavailability reasons returned without source title or content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnowledgeUnavailableReason {
    SourceAccessLost,
    SourceDeleted,
}

/// Exact entry lookup. Foreign users receive `NotFound` rather than a hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnowledgeLookup {
    Found(Box<KnowledgeEntry>),
    Unavailable(KnowledgeUnavailableReason),
    NotFound,
}

/// Bounded authorized search result; its count is derived only from returned rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeSearchResult {
    pub catalog_revision: u64,
    pub matched_count: usize,
    pub entries: Vec<KnowledgeEntry>,
}

/// Why an effective entry is inherited or directly scoped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnowledgeInheritance {
    Direct,
    InheritedPersonal,
}

/// One confirmed entry selected for model context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedKnowledge {
    pub entry: KnowledgeEntry,
    pub inheritance: KnowledgeInheritance,
    pub overrides_entry_id: Option<String>,
}

/// Authorized, precedence-resolved model context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnowledgeSelectionResult {
    pub catalog_revision: u64,
    pub entries: Vec<SelectedKnowledge>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KnowledgeRecord {
    entry_id: String,
    title: Option<String>,
    body: Option<String>,
    rule_key: Option<String>,
    scope: Option<KnowledgeScope>,
    origin: Option<KnowledgeOrigin>,
    source_id: String,
    source: Option<KnowledgeSource>,
    status: KnowledgeStatus,
    created_at_millis: u64,
    updated_at_millis: u64,
    expires_at_millis: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KnowledgeCatalogState {
    schema: String,
    owner_user_id: UserId,
    revision: u64,
    entries: BTreeMap<String, KnowledgeRecord>,
    source_tombstones: BTreeMap<String, u64>,
}

/// Durable Community knowledge service. Team sharing remains outside this catalog.
pub struct KnowledgeCatalogService<'a> {
    storage: &'a mut dyn ProductStateStorage,
}

impl<'a> KnowledgeCatalogService<'a> {
    #[must_use]
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Applies one create/edit/confirm/archive/revoke/source lifecycle command.
    ///
    /// # Errors
    ///
    /// Rejects invalid, stale, conflicting, missing, or illegal transitions.
    pub fn apply(
        &mut self,
        command: &KnowledgeCommand,
    ) -> Result<KnowledgeMutationReceipt, KnowledgeError> {
        validate_command(command)?;
        let (identity, digest) = command_identity(command)?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &digest)? {
            return Ok(mutation_receipt(command, &receipt));
        }

        let (mut state, current_revision) = self.load_or_empty(&command.actor_user_id)?;
        if command.expected_catalog_revision != current_revision {
            return Err(KnowledgeError::new(
                KnowledgeErrorKind::RevisionConflict,
                "knowledge catalog revision does not match",
            ));
        }
        apply_action(&mut state, command)?;
        state.revision = next_revision(current_revision)?;
        let payload = serde_json::to_vec(&state).map_err(|_| KnowledgeError::invalid())?;
        let event_id = format!("knowledge-catalog:{}", &digest.0[7..]);
        let event_payload = serde_json::to_vec(&serde_json::json!({
            "catalogRevision": state.revision,
        }))
        .map_err(|_| KnowledgeError::invalid())?;
        let receipt = self.storage.commit(&StateCommit::new(
            identity,
            digest,
            catalog_stream_id(&command.actor_user_id),
            current_revision,
            payload,
            vec![NewOutboxEvent::internal(
                event_id,
                CHANGE_TOPIC,
                event_payload,
            )],
        ))?;
        Ok(mutation_receipt(command, &receipt))
    }

    /// Looks up one exact entry while withholding all metadata after source loss.
    ///
    /// # Errors
    ///
    /// Rejects malformed access or entry identities and corrupted storage.
    pub fn lookup(
        &self,
        access: &KnowledgeAccess,
        entry_id: &str,
    ) -> Result<KnowledgeLookup, KnowledgeError> {
        validate_access(access)?;
        validate_prefixed_id(entry_id, "knw_", 128)?;
        let (state, _) = self.load_or_empty(&access.user_id)?;
        let Some(record) = state.entries.get(entry_id) else {
            return Ok(KnowledgeLookup::NotFound);
        };
        if record.status == KnowledgeStatus::SourceDeleted {
            return Ok(KnowledgeLookup::Unavailable(
                KnowledgeUnavailableReason::SourceDeleted,
            ));
        }
        if !is_visible(record, access) {
            return Ok(KnowledgeLookup::Unavailable(
                KnowledgeUnavailableReason::SourceAccessLost,
            ));
        }
        Ok(KnowledgeLookup::Found(Box::new(project(record)?)))
    }

    /// Searches only rows visible to the caller; unauthorized rows affect no count.
    ///
    /// # Errors
    ///
    /// Rejects malformed access/query input and corrupted storage.
    pub fn search(
        &self,
        access: &KnowledgeAccess,
        query: &str,
    ) -> Result<KnowledgeSearchResult, KnowledgeError> {
        validate_access(access)?;
        if query.chars().count() > 200 || query.chars().any(|character| character == '\0') {
            return Err(KnowledgeError::invalid());
        }
        let (state, revision) = self.load_or_empty(&access.user_id)?;
        let query = query.to_lowercase();
        let mut entries = Vec::new();
        for record in state.entries.values() {
            if record.status == KnowledgeStatus::SourceDeleted || !is_visible(record, access) {
                continue;
            }
            let entry = project(record)?;
            if query.is_empty()
                || entry.title.to_lowercase().contains(&query)
                || entry.body.to_lowercase().contains(&query)
            {
                entries.push(entry);
            }
        }
        Ok(KnowledgeSearchResult {
            catalog_revision: revision,
            matched_count: entries.len(),
            entries,
        })
    }

    /// Selects confirmed, current knowledge with repository-over-personal precedence.
    /// Current user instructions and the approved Spec win by supplying their rule keys.
    ///
    /// # Errors
    ///
    /// Rejects malformed access/scope input and corrupted storage.
    pub fn select_context(
        &self,
        access: &KnowledgeAccess,
        repository_id: Option<&RepositoryId>,
        current_authority_rule_keys: &BTreeSet<String>,
        now_millis: u64,
    ) -> Result<KnowledgeSelectionResult, KnowledgeError> {
        validate_access(access)?;
        validate_millis(now_millis)?;
        if let Some(repository_id) = repository_id {
            validate_repository_id(repository_id)?;
            if !has_repository_access(access, repository_id) {
                return Ok(KnowledgeSelectionResult {
                    catalog_revision: self.load_or_empty(&access.user_id)?.1,
                    entries: Vec::new(),
                });
            }
        }
        for key in current_authority_rule_keys {
            validate_token(key, 200)?;
        }
        let (state, revision) = self.load_or_empty(&access.user_id)?;
        let mut candidates = state
            .entries
            .values()
            .filter(|record| {
                record.status == KnowledgeStatus::Confirmed
                    && record
                        .expires_at_millis
                        .is_none_or(|expires| expires > now_millis)
                    && is_visible(record, access)
                    && scope_applies(record, repository_id)
            })
            .map(|record| project(record).map(|entry| (scope_rank(&entry.scope), entry)))
            .collect::<Result<Vec<_>, _>>()?;
        candidates.sort_by(|left, right| {
            (left.0, left.1.updated_at_millis, &left.1.entry_id).cmp(&(
                right.0,
                right.1.updated_at_millis,
                &right.1.entry_id,
            ))
        });
        let mut selected = BTreeMap::<String, SelectedKnowledge>::new();
        for (_, entry) in candidates {
            if current_authority_rule_keys.contains(&entry.rule_key) {
                continue;
            }
            let inheritance =
                if repository_id.is_some() && matches!(entry.scope, KnowledgeScope::Personal) {
                    KnowledgeInheritance::InheritedPersonal
                } else {
                    KnowledgeInheritance::Direct
                };
            let overrides_entry_id = selected
                .get(&entry.rule_key)
                .map(|previous| previous.entry.entry_id.clone());
            selected.insert(
                entry.rule_key.clone(),
                SelectedKnowledge {
                    entry,
                    inheritance,
                    overrides_entry_id,
                },
            );
        }
        Ok(KnowledgeSelectionResult {
            catalog_revision: revision,
            entries: selected.into_values().collect(),
        })
    }

    fn load_or_empty(
        &self,
        owner_user_id: &UserId,
    ) -> Result<(KnowledgeCatalogState, u64), KnowledgeError> {
        validate_user_id(owner_user_id)?;
        match self.storage.load_state(&catalog_stream_id(owner_user_id))? {
            Some(stored) => {
                let state = decode_state(&stored, owner_user_id)?;
                Ok((state, stored.revision))
            }
            None => Ok((
                KnowledgeCatalogState {
                    schema: STATE_SCHEMA.to_owned(),
                    owner_user_id: owner_user_id.clone(),
                    revision: 0,
                    entries: BTreeMap::new(),
                    source_tombstones: BTreeMap::new(),
                },
                0,
            )),
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "keeping the small lifecycle transition table together makes illegal states visible"
)]
fn apply_action(
    state: &mut KnowledgeCatalogState,
    command: &KnowledgeCommand,
) -> Result<(), KnowledgeError> {
    match &command.action {
        KnowledgeAction::Create { entry_id, draft } => {
            if state.entries.contains_key(entry_id) {
                return Err(KnowledgeError::invalid_state());
            }
            if state.entries.len() >= MAX_ENTRIES {
                // ponytail: one bounded catalog keeps persistence atomic; shard per user if this
                // measured ceiling is reached in normal use.
                return Err(KnowledgeError::new(
                    KnowledgeErrorKind::CatalogFull,
                    "knowledge catalog reached its entry limit",
                ));
            }
            if state
                .source_tombstones
                .contains_key(&draft.source.source_id)
            {
                return Err(KnowledgeError::source_deleted());
            }
            state.entries.insert(
                entry_id.clone(),
                record_from_draft(entry_id, draft, command.occurred_at_millis),
            );
        }
        KnowledgeAction::Edit { entry_id, draft } => {
            if state
                .source_tombstones
                .contains_key(&draft.source.source_id)
            {
                return Err(KnowledgeError::source_deleted());
            }
            let record = active_record_mut(state, entry_id)?;
            record.title = Some(draft.title.clone());
            record.body = Some(draft.body.clone());
            record.rule_key = Some(draft.rule_key.clone());
            record.scope = Some(draft.scope.clone());
            record.origin = Some(draft.origin);
            record.source_id.clone_from(&draft.source.source_id);
            record.source = Some(draft.source.clone());
            record.status = KnowledgeStatus::Suggested;
            record.updated_at_millis = command.occurred_at_millis;
            record.expires_at_millis = draft.expires_at_millis;
        }
        KnowledgeAction::Confirm { entry_id } => {
            let record = active_record_mut(state, entry_id)?;
            if !matches!(
                record.status,
                KnowledgeStatus::Suggested | KnowledgeStatus::NeedsReconfirmation
            ) {
                return Err(KnowledgeError::invalid_state());
            }
            record.status = KnowledgeStatus::Confirmed;
            record.updated_at_millis = command.occurred_at_millis;
        }
        KnowledgeAction::Archive { entry_id } => {
            let record = active_record_mut(state, entry_id)?;
            if matches!(
                record.status,
                KnowledgeStatus::Archived | KnowledgeStatus::Revoked
            ) {
                return Err(KnowledgeError::invalid_state());
            }
            record.status = KnowledgeStatus::Archived;
            record.updated_at_millis = command.occurred_at_millis;
        }
        KnowledgeAction::Revoke { entry_id } => {
            let record = active_record_mut(state, entry_id)?;
            if record.status == KnowledgeStatus::Revoked {
                return Err(KnowledgeError::invalid_state());
            }
            record.status = KnowledgeStatus::Revoked;
            record.updated_at_millis = command.occurred_at_millis;
        }
        KnowledgeAction::SourceVersionChanged {
            source_id,
            version_digest,
        } => {
            if state.source_tombstones.contains_key(source_id) {
                return Err(KnowledgeError::source_deleted());
            }
            let mut found = false;
            for record in state.entries.values_mut() {
                if record.source_id != *source_id || record.status == KnowledgeStatus::SourceDeleted
                {
                    continue;
                }
                found = true;
                let source = record.source.as_mut().ok_or_else(KnowledgeError::invalid)?;
                if source.version_digest == *version_digest {
                    continue;
                }
                source.version_digest = version_digest.clone();
                if matches!(
                    record.status,
                    KnowledgeStatus::Suggested
                        | KnowledgeStatus::Confirmed
                        | KnowledgeStatus::NeedsReconfirmation
                ) {
                    record.status = KnowledgeStatus::NeedsReconfirmation;
                }
                record.updated_at_millis = command.occurred_at_millis;
            }
            if !found {
                return Err(KnowledgeError::not_found());
            }
        }
        KnowledgeAction::DeleteSource { source_id } => {
            if state.source_tombstones.contains_key(source_id) {
                return Err(KnowledgeError::source_deleted());
            }
            let mut found = false;
            for record in state.entries.values_mut() {
                if record.source_id != *source_id {
                    continue;
                }
                found = true;
                record.title = None;
                record.body = None;
                record.rule_key = None;
                record.scope = None;
                record.origin = None;
                record.source = None;
                record.status = KnowledgeStatus::SourceDeleted;
                record.updated_at_millis = command.occurred_at_millis;
                record.expires_at_millis = None;
            }
            if !found {
                return Err(KnowledgeError::not_found());
            }
            state
                .source_tombstones
                .insert(source_id.clone(), command.occurred_at_millis);
        }
    }
    Ok(())
}

fn active_record_mut<'a>(
    state: &'a mut KnowledgeCatalogState,
    entry_id: &str,
) -> Result<&'a mut KnowledgeRecord, KnowledgeError> {
    let record = state
        .entries
        .get_mut(entry_id)
        .ok_or_else(KnowledgeError::not_found)?;
    if record.status == KnowledgeStatus::SourceDeleted {
        Err(KnowledgeError::source_deleted())
    } else {
        Ok(record)
    }
}

fn record_from_draft(entry_id: &str, draft: &KnowledgeDraft, now: u64) -> KnowledgeRecord {
    KnowledgeRecord {
        entry_id: entry_id.to_owned(),
        title: Some(draft.title.clone()),
        body: Some(draft.body.clone()),
        rule_key: Some(draft.rule_key.clone()),
        scope: Some(draft.scope.clone()),
        origin: Some(draft.origin),
        source_id: draft.source.source_id.clone(),
        source: Some(draft.source.clone()),
        status: KnowledgeStatus::Suggested,
        created_at_millis: now,
        updated_at_millis: now,
        expires_at_millis: draft.expires_at_millis,
    }
}

fn project(record: &KnowledgeRecord) -> Result<KnowledgeEntry, KnowledgeError> {
    Ok(KnowledgeEntry {
        entry_id: record.entry_id.clone(),
        title: record.title.clone().ok_or_else(KnowledgeError::invalid)?,
        body: record.body.clone().ok_or_else(KnowledgeError::invalid)?,
        rule_key: record
            .rule_key
            .clone()
            .ok_or_else(KnowledgeError::invalid)?,
        scope: record.scope.clone().ok_or_else(KnowledgeError::invalid)?,
        origin: record.origin.ok_or_else(KnowledgeError::invalid)?,
        source: record.source.clone().ok_or_else(KnowledgeError::invalid)?,
        status: record.status,
        created_at_millis: record.created_at_millis,
        updated_at_millis: record.updated_at_millis,
        expires_at_millis: record.expires_at_millis,
    })
}

fn is_visible(record: &KnowledgeRecord, access: &KnowledgeAccess) -> bool {
    let scope_visible = match &record.scope {
        Some(KnowledgeScope::Personal) => true,
        Some(KnowledgeScope::Repository { repository_id }) => {
            has_repository_access(access, repository_id)
        }
        None => false,
    };
    let source_visible = record
        .source
        .as_ref()
        .and_then(|source| source.repository_id.as_ref())
        .is_none_or(|repository_id| has_repository_access(access, repository_id));
    scope_visible && source_visible
}

fn scope_applies(record: &KnowledgeRecord, repository_id: Option<&RepositoryId>) -> bool {
    match (&record.scope, repository_id) {
        (Some(KnowledgeScope::Personal), _) => true,
        (
            Some(KnowledgeScope::Repository {
                repository_id: entry_repository_id,
            }),
            Some(repository_id),
        ) => entry_repository_id == repository_id,
        _ => false,
    }
}

fn has_repository_access(access: &KnowledgeAccess, repository_id: &RepositoryId) -> bool {
    access
        .authorized_repository_ids
        .iter()
        .any(|authorized| authorized == repository_id)
}

const fn scope_rank(scope: &KnowledgeScope) -> u8 {
    match scope {
        KnowledgeScope::Personal => 0,
        KnowledgeScope::Repository { .. } => 1,
    }
}

fn command_identity(
    command: &KnowledgeCommand,
) -> Result<(ReceiptIdentity, Sha256Digest), KnowledgeError> {
    let actor_key = ReceiptActorKey::from_encoded(
        format!("winwincode.knowledge.user.v1\0{}", command.actor_user_id.0).into_bytes(),
    )?;
    let scope_key = ReceiptScopeKey::from_encoded(
        format!(
            "winwincode.knowledge.catalog.v1\0{}",
            command.actor_user_id.0
        )
        .into_bytes(),
    )?;
    let identity = ReceiptIdentity::new(actor_key, scope_key, command.request_id.clone())?;
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.knowledge-command.v1\0");
    hasher.update(serde_json::to_vec(command).map_err(|_| KnowledgeError::invalid())?);
    Ok((
        identity,
        Sha256Digest(format!("sha256:{:x}", hasher.finalize())),
    ))
}

fn mutation_receipt(
    command: &KnowledgeCommand,
    receipt: &CommitReceipt,
) -> KnowledgeMutationReceipt {
    KnowledgeMutationReceipt {
        request_id: command.request_id.clone(),
        catalog_revision: receipt.revision,
        idempotent_replay: receipt.idempotent_replay,
    }
}

fn catalog_stream_id(owner_user_id: &UserId) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.knowledge-catalog-owner.v1\0");
    digest.update(owner_user_id.0.as_bytes());
    format!("{STREAM_PREFIX}{:x}", digest.finalize())
}

fn decode_state(
    stored: &StoredState,
    owner_user_id: &UserId,
) -> Result<KnowledgeCatalogState, KnowledgeError> {
    let state: KnowledgeCatalogState =
        serde_json::from_slice(&stored.payload).map_err(|_| KnowledgeError::invalid())?;
    if state.schema != STATE_SCHEMA
        || state.owner_user_id != *owner_user_id
        || state.revision != stored.revision
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
        || stored.stream_id != catalog_stream_id(owner_user_id)
        || state.entries.len() > MAX_ENTRIES
    {
        return Err(KnowledgeError::invalid());
    }
    for (entry_id, record) in &state.entries {
        validate_prefixed_id(entry_id, "knw_", 128)?;
        if record.entry_id != *entry_id
            || record.created_at_millis == 0
            || record.created_at_millis > record.updated_at_millis
        {
            return Err(KnowledgeError::invalid());
        }
        validate_millis(record.updated_at_millis)?;
        validate_prefixed_id(&record.source_id, "src_", 128)?;
        if record.status == KnowledgeStatus::SourceDeleted {
            if record.title.is_some()
                || record.body.is_some()
                || record.rule_key.is_some()
                || record.scope.is_some()
                || record.origin.is_some()
                || record.source.is_some()
                || record.expires_at_millis.is_some()
                || !state.source_tombstones.contains_key(&record.source_id)
            {
                return Err(KnowledgeError::invalid());
            }
        } else {
            validate_record(record)?;
        }
    }
    for (source_id, deleted_at) in &state.source_tombstones {
        validate_prefixed_id(source_id, "src_", 128)?;
        validate_millis(*deleted_at)?;
    }
    Ok(state)
}

fn validate_record(record: &KnowledgeRecord) -> Result<(), KnowledgeError> {
    validate_title(
        record
            .title
            .as_deref()
            .ok_or_else(KnowledgeError::invalid)?,
    )?;
    validate_body(record.body.as_deref().ok_or_else(KnowledgeError::invalid)?)?;
    validate_token(
        record
            .rule_key
            .as_deref()
            .ok_or_else(KnowledgeError::invalid)?,
        200,
    )?;
    let scope = record.scope.as_ref().ok_or_else(KnowledgeError::invalid)?;
    let source = record.source.as_ref().ok_or_else(KnowledgeError::invalid)?;
    if source.source_id != record.source_id || record.origin.is_none() {
        return Err(KnowledgeError::invalid());
    }
    validate_scope_source(scope, source)?;
    if let Some(expires_at) = record.expires_at_millis {
        validate_millis(expires_at)?;
    }
    Ok(())
}

fn validate_command(command: &KnowledgeCommand) -> Result<(), KnowledgeError> {
    validate_user_id(&command.actor_user_id)?;
    validate_prefixed_id(&command.request_id.0, "req_", 128)?;
    validate_millis(command.occurred_at_millis)?;
    if command.expected_catalog_revision > MAX_SAFE_INTEGER {
        return Err(KnowledgeError::invalid());
    }
    match &command.action {
        KnowledgeAction::Create { entry_id, draft } | KnowledgeAction::Edit { entry_id, draft } => {
            validate_prefixed_id(entry_id, "knw_", 128)?;
            validate_draft(draft)?;
        }
        KnowledgeAction::Confirm { entry_id }
        | KnowledgeAction::Archive { entry_id }
        | KnowledgeAction::Revoke { entry_id } => {
            validate_prefixed_id(entry_id, "knw_", 128)?;
        }
        KnowledgeAction::SourceVersionChanged {
            source_id,
            version_digest,
        } => {
            validate_prefixed_id(source_id, "src_", 128)?;
            validate_digest(version_digest)?;
        }
        KnowledgeAction::DeleteSource { source_id } => {
            validate_prefixed_id(source_id, "src_", 128)?;
        }
    }
    Ok(())
}

fn validate_draft(draft: &KnowledgeDraft) -> Result<(), KnowledgeError> {
    validate_title(&draft.title)?;
    validate_body(&draft.body)?;
    validate_token(&draft.rule_key, 200)?;
    validate_scope_source(&draft.scope, &draft.source)?;
    if let Some(expires_at) = draft.expires_at_millis {
        validate_millis(expires_at)?;
    }
    Ok(())
}

fn validate_scope_source(
    scope: &KnowledgeScope,
    source: &KnowledgeSource,
) -> Result<(), KnowledgeError> {
    validate_prefixed_id(&source.source_id, "src_", 128)?;
    validate_digest(&source.version_digest)?;
    if let Some(repository_id) = &source.repository_id {
        validate_repository_id(repository_id)?;
    }
    if let KnowledgeScope::Repository { repository_id } = scope {
        validate_repository_id(repository_id)?;
        if source
            .repository_id
            .as_ref()
            .is_some_and(|source_repository_id| source_repository_id != repository_id)
        {
            return Err(KnowledgeError::invalid());
        }
    }
    match &source.locator {
        KnowledgeSourceLocator::Document {
            relative_path,
            start_line,
            end_line,
        } => {
            if relative_path.is_empty()
                || relative_path.len() > 1_000
                || relative_path.contains('\\')
                || *start_line == 0
                || start_line > end_line
                || *end_line > MAX_SAFE_INTEGER
                || Path::new(relative_path).is_absolute()
                || !Path::new(relative_path)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                return Err(KnowledgeError::invalid());
            }
        }
        KnowledgeSourceLocator::EventRange {
            stream_id,
            start_sequence,
            end_sequence,
        } => {
            validate_token(stream_id, 200)?;
            if *start_sequence == 0
                || start_sequence > end_sequence
                || *end_sequence > MAX_SAFE_INTEGER
            {
                return Err(KnowledgeError::invalid());
            }
        }
    }
    Ok(())
}

fn validate_access(access: &KnowledgeAccess) -> Result<(), KnowledgeError> {
    validate_user_id(&access.user_id)?;
    for repository_id in &access.authorized_repository_ids {
        validate_repository_id(repository_id)?;
    }
    Ok(())
}

fn validate_user_id(user_id: &UserId) -> Result<(), KnowledgeError> {
    validate_prefixed_id(&user_id.0, "usr_", 128)
}

fn validate_repository_id(repository_id: &RepositoryId) -> Result<(), KnowledgeError> {
    validate_prefixed_id(&repository_id.0, "rep_", 128)
}

fn validate_prefixed_id(value: &str, prefix: &str, max_bytes: usize) -> Result<(), KnowledgeError> {
    if !value.starts_with(prefix)
        || value.len() <= prefix.len()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Err(KnowledgeError::invalid())
    } else {
        Ok(())
    }
}

fn validate_token(value: &str, max_chars: usize) -> Result<(), KnowledgeError> {
    if value.is_empty()
        || value.len() > max_chars
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        Err(KnowledgeError::invalid())
    } else {
        Ok(())
    }
}

fn validate_title(value: &str) -> Result<(), KnowledgeError> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > 200
        || value.chars().any(char::is_control)
    {
        Err(KnowledgeError::invalid())
    } else {
        Ok(())
    }
}

fn validate_body(value: &str) -> Result<(), KnowledgeError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > 16 * 1024
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        Err(KnowledgeError::invalid())
    } else {
        Ok(())
    }
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), KnowledgeError> {
    if digest.0.len() != 71
        || !digest.0.starts_with("sha256:")
        || !digest.0[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Err(KnowledgeError::invalid())
    } else {
        Ok(())
    }
}

fn validate_millis(value: u64) -> Result<(), KnowledgeError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(KnowledgeError::invalid())
    } else {
        Ok(())
    }
}

fn next_revision(current: u64) -> Result<u64, KnowledgeError> {
    current
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(KnowledgeError::invalid)
}
