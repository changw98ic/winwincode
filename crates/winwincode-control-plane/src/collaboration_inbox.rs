// SPDX-License-Identifier: Apache-2.0

//! Rebuildable collaboration Inbox over canonical Approval and Attention facts.
//!
//! The Inbox owns only claims and review annotations. Approval and Attention
//! lifecycle remains owned by their business aggregates, while assignment and
//! RBAC eligibility remains owned by the collaboration authority port.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{Actor, Scope};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    ApprovalId, AttentionItemId, DeliveryId, EnterpriseTeamId, OpaqueCursor, ProductSessionId,
    RequestId, Sha256Digest, UserId,
};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StateCommit, StateRevisionGuard,
    StorageError, StorageErrorKind,
};

use crate::{
    ResponsibilityAssignment, ResponsibilityAssignmentId, ResponsibilityAssignmentState,
    ResponsibilityRole, ResponsibilityTarget, command_receipt_identity,
};

const STATE_SCHEMA: &str = "winwincode.collaboration-inbox.v1";
const STREAM_PREFIX: &str = "collaboration-inbox:";
const EVENT_TOPIC: &str = "collaboration-inbox.receipt.internal.v1";
const CURSOR_SCHEMA: u8 = 1;
const MAX_PAGE_SIZE: usize = 200;
const MAX_CURSOR_BYTES: usize = 4_096;
const MAX_RECORDS: usize = 20_000;
const MAX_BOUNDED_TEXT: usize = 2_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Stable error classes exposed by the collaboration Inbox boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollaborationInboxErrorKind {
    Invalid,
    Unauthorized,
    NotFound,
    WrongState,
    CandidateChanged,
    RevisionConflict,
    RequestConflict,
    CursorExpired,
    AuthorityUnavailable,
    SourceUnavailable,
    Storage,
    Corrupt,
}

/// Bounded, secret-safe collaboration Inbox failure.
#[derive(Debug)]
pub struct CollaborationInboxError {
    kind: CollaborationInboxErrorKind,
    message: &'static str,
}

impl CollaborationInboxError {
    #[must_use]
    pub const fn kind(&self) -> CollaborationInboxErrorKind {
        self.kind
    }
}

impl fmt::Display for CollaborationInboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CollaborationInboxError {}

impl From<StorageError> for CollaborationInboxError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RevisionConflict => revision_conflict(),
            StorageErrorKind::RequestConflict => request_conflict(),
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => invalid(),
            StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => storage(),
        }
    }
}

/// Stable identity shared by a personal or Team Inbox item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum CollaborationInboxItemId {
    Approval(ApprovalId),
    GateAttention(AttentionItemId),
    DeliveryAttention(AttentionItemId),
}

impl CollaborationInboxItemId {
    fn sort_key(&self) -> (u8, &str) {
        match self {
            Self::Approval(id) => (0, &id.0),
            Self::GateAttention(id) => (1, &id.0),
            Self::DeliveryAttention(id) => (2, &id.0),
        }
    }
}

impl Ord for CollaborationInboxItemId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for CollaborationInboxItemId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Canonical source family. It determines the formal command and required role.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationInboxItemKind {
    Approval,
    GateAttention,
    DeliveryAttention,
}

/// Lifecycle projected from the current canonical source fact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationInboxItemState {
    Pending,
    Approved,
    Rejected,
    Resolved,
    Expired,
}

/// Exact candidate identity required by a review annotation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationCandidateIdentity {
    pub candidate_ref: String,
    pub candidate_digest: Sha256Digest,
    pub candidate_revision: u64,
}

/// Formal business command represented by one Inbox item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FormalCollaborationCommandRoute {
    ApprovalDecide {
        approval_id: ApprovalId,
        product_session_id: ProductSessionId,
    },
    GateAttentionRespond {
        attention_item_id: AttentionItemId,
        product_session_id: ProductSessionId,
    },
    DeliveryResolveAttention {
        attention_item_id: AttentionItemId,
        delivery_id: DeliveryId,
    },
}

/// One secret-safe fact read from the canonical Approval or Attention owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationInboxSourceItem {
    pub id: CollaborationInboxItemId,
    pub kind: CollaborationInboxItemKind,
    pub target: ResponsibilityTarget,
    pub responsibility_role: ResponsibilityRole,
    pub source_revision: u64,
    pub source_sha256: Sha256Digest,
    pub title_sha256: Sha256Digest,
    pub opened_at_millis: u64,
    pub expires_at_millis: Option<u64>,
    pub state: CollaborationInboxItemState,
    pub candidate: Option<CollaborationCandidateIdentity>,
    pub command_route: FormalCollaborationCommandRoute,
}

impl CollaborationInboxSourceItem {
    fn effective_state(&self, snapshot_at_millis: u64) -> CollaborationInboxItemState {
        if self.state == CollaborationInboxItemState::Pending
            && self
                .expires_at_millis
                .is_some_and(|expires_at| snapshot_at_millis >= expires_at)
        {
            CollaborationInboxItemState::Expired
        } else {
            self.state
        }
    }
}

/// One immutable read cut from the canonical Approval and Attention owners.
#[derive(Clone, Debug, PartialEq)]
pub struct CollaborationInboxSourceSnapshot {
    pub scope: RepositoryScope,
    pub revision: u64,
    pub snapshot_sha256: Sha256Digest,
    pub item_state_guards: BTreeMap<CollaborationInboxItemId, Vec<StateRevisionGuard>>,
    pub items: Vec<CollaborationInboxSourceItem>,
}

/// Failure from the canonical Approval/Attention source adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollaborationInboxSourceError;

/// Read-only canonical Approval/Attention boundary.
pub trait CollaborationInboxSourcePort: Send {
    /// Reads one atomic, bounded source cut for an exact repository scope.
    ///
    /// # Errors
    ///
    /// Returns an error rather than returning a partial or inferred cut.
    fn snapshot(
        &mut self,
        scope: &RepositoryScope,
    ) -> Result<CollaborationInboxSourceSnapshot, CollaborationInboxSourceError>;
}

/// Personal or Team view requested by the authenticated actor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum CollaborationInboxAudience {
    Personal(UserId),
    Team(EnterpriseTeamId),
}

/// One assignment plus the Teams for which current RBAC grants visibility.
#[derive(Clone, Debug, PartialEq)]
pub struct CollaborationResponsibilityEntitlement {
    pub assignment: ResponsibilityAssignment,
    pub team_ids: Vec<EnterpriseTeamId>,
}

/// Current sealed Identity/RBAC/Assignment authority for one Inbox operation.
#[derive(Clone, Debug, PartialEq)]
pub struct CollaborationInboxAuthoritySnapshot {
    pub scope: RepositoryScope,
    pub viewer_user_id: UserId,
    pub visible_team_ids: Vec<EnterpriseTeamId>,
    pub assignments: Vec<CollaborationResponsibilityEntitlement>,
    pub authority_revision: u64,
    pub authority_sha256: Sha256Digest,
    pub state_guards: Vec<StateRevisionGuard>,
}

/// Authority lookup failure. Details stay behind the authority boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollaborationInboxAuthorityError;

/// Read-only current Identity/RBAC/Assignment boundary.
pub trait CollaborationInboxAuthorityPort: Send {
    /// Authorizes one exact actor, authenticated scope set, repository and audience.
    ///
    /// # Errors
    ///
    /// Returns an error when current sealed authority is unavailable or denied.
    fn authorize(
        &mut self,
        actor: &Actor,
        authenticated_scopes: &[Scope],
        scope: &RepositoryScope,
        audience: &CollaborationInboxAudience,
    ) -> Result<CollaborationInboxAuthoritySnapshot, CollaborationInboxAuthorityError>;
}

/// Trusted Inbox clock failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollaborationInboxClockError;

/// Clock used to freeze expiration semantics into list cursors and receipts.
pub trait CollaborationInboxClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a bounded error when the trusted clock is unavailable.
    fn now_millis(&mut self) -> Result<u64, CollaborationInboxClockError>;
}

/// System implementation of the trusted Inbox clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCollaborationInboxClock;

impl CollaborationInboxClock for SystemCollaborationInboxClock {
    fn now_millis(&mut self) -> Result<u64, CollaborationInboxClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CollaborationInboxClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| CollaborationInboxClockError)
    }
}

/// Stable list filters included in the cursor digest.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationInboxFilter {
    pub kinds: Vec<CollaborationInboxItemKind>,
    pub states: Vec<CollaborationInboxItemState>,
    pub only_claimed_by_viewer: bool,
}

/// One stable, authorized Inbox page request.
#[derive(Clone, Debug, PartialEq)]
pub struct CollaborationInboxListRequest {
    pub actor: Actor,
    pub authenticated_scopes: Vec<Scope>,
    pub scope: RepositoryScope,
    pub audience: CollaborationInboxAudience,
    pub filter: CollaborationInboxFilter,
    pub limit: usize,
    pub cursor: Option<OpaqueCursor>,
}

/// Collaboration-owned claim overlay. It never resolves the business item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationClaim {
    pub item_id: CollaborationInboxItemId,
    pub audience: CollaborationInboxAudience,
    pub claimant_user_id: UserId,
    pub revision: u64,
    pub claimed_at_millis: u64,
    pub source_revision: u64,
    pub source_sha256: Sha256Digest,
}

/// Stable annotation identity.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CollaborationAnnotationId(pub String);

/// Exact graph, file or hunk coordinate inside one frozen candidate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationAnnotationTarget {
    Node {
        node_id: String,
    },
    File {
        path: String,
        blob_sha256: Sha256Digest,
    },
    Hunk {
        path: String,
        base_blob_sha256: Sha256Digest,
        start_line: u64,
        end_line: u64,
        hunk_sha256: Sha256Digest,
    },
}

/// Lifecycle of one collaboration annotation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationAnnotationState {
    Active,
    Revoked,
}

/// Review annotation bound to one exact candidate and source item revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationAnnotation {
    pub id: CollaborationAnnotationId,
    pub item_id: CollaborationInboxItemId,
    pub author_user_id: UserId,
    pub candidate: CollaborationCandidateIdentity,
    pub target: CollaborationAnnotationTarget,
    pub body_sha256: Sha256Digest,
    pub state: CollaborationAnnotationState,
    pub revision: u64,
    pub source_revision: u64,
    pub source_sha256: Sha256Digest,
    pub updated_at_millis: u64,
}

/// Business item plus collaboration overlays and its formal command route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollaborationInboxItem {
    pub source: CollaborationInboxSourceItem,
    pub effective_state: CollaborationInboxItemState,
    pub assignment_ids: Vec<ResponsibilityAssignmentId>,
    pub claim: Option<CollaborationClaim>,
    pub annotations: Vec<CollaborationAnnotation>,
}

/// Stable keyset-like page sealed to source, authority, overlay, filters and time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollaborationInboxPage {
    pub items: Vec<CollaborationInboxItem>,
    pub has_more: bool,
    pub next_cursor: Option<OpaqueCursor>,
    pub snapshot_at_millis: u64,
}

/// Authenticated context shared by claim and annotation mutations.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CollaborationInboxCommandContext {
    pub actor: Actor,
    pub authenticated_scopes: Vec<Scope>,
    pub scope: RepositoryScope,
    pub audience: CollaborationInboxAudience,
    pub request_id: RequestId,
    pub expected_revision: u64,
}

/// Claim lifecycle action.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum CollaborationClaimAction {
    Claim,
    Release,
}

/// Replay-safe claim mutation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CollaborationClaimCommand {
    pub context: CollaborationInboxCommandContext,
    pub item_id: CollaborationInboxItemId,
    pub action: CollaborationClaimAction,
}

/// Annotation lifecycle action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum CollaborationAnnotationAction {
    Upsert {
        candidate: CollaborationCandidateIdentity,
        target: CollaborationAnnotationTarget,
        body_sha256: Sha256Digest,
    },
    Revoke,
}

/// Replay-safe annotation mutation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CollaborationAnnotationCommand {
    pub context: CollaborationInboxCommandContext,
    pub item_id: CollaborationInboxItemId,
    pub annotation_id: CollaborationAnnotationId,
    pub action: CollaborationAnnotationAction,
}

/// Durable mutation result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationInboxReceipt {
    Claim {
        claim: Option<CollaborationClaim>,
        catalog_revision: u64,
        replayed: bool,
    },
    Annotation {
        annotation: CollaborationAnnotation,
        catalog_revision: u64,
        replayed: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CollaborationCatalog {
    schema: String,
    scope: RepositoryScope,
    revision: u64,
    claims: BTreeMap<String, CollaborationClaim>,
    annotations: BTreeMap<String, CollaborationAnnotation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorPayload {
    schema: u8,
    scope_sha256: Sha256Digest,
    audience_sha256: Sha256Digest,
    filter_sha256: Sha256Digest,
    source_revision: u64,
    source_sha256: Sha256Digest,
    authority_revision: u64,
    authority_sha256: Sha256Digest,
    catalog_revision: u64,
    catalog_sha256: Sha256Digest,
    snapshot_at_millis: u64,
    offset: usize,
}

/// Durable collaboration Inbox service.
pub struct CollaborationInboxService {
    storage: Box<dyn ProductStateStorage>,
    source: Box<dyn CollaborationInboxSourcePort>,
    authority: Box<dyn CollaborationInboxAuthorityPort>,
    clock: Box<dyn CollaborationInboxClock>,
}

impl CollaborationInboxService {
    #[must_use]
    pub fn new(
        storage: Box<dyn ProductStateStorage>,
        source: Box<dyn CollaborationInboxSourcePort>,
        authority: Box<dyn CollaborationInboxAuthorityPort>,
    ) -> Self {
        Self::with_clock(
            storage,
            source,
            authority,
            Box::new(SystemCollaborationInboxClock),
        )
    }

    #[must_use]
    pub fn with_clock(
        storage: Box<dyn ProductStateStorage>,
        source: Box<dyn CollaborationInboxSourcePort>,
        authority: Box<dyn CollaborationInboxAuthorityPort>,
        clock: Box<dyn CollaborationInboxClock>,
    ) -> Self {
        Self {
            storage,
            source,
            authority,
            clock,
        }
    }

    /// Rebuilds one stable authorized page without invoking a business decision.
    ///
    /// # Errors
    ///
    /// Rejects invalid, unauthorized, changed-cut, corrupt or unavailable reads.
    pub fn list(
        &mut self,
        request: &CollaborationInboxListRequest,
    ) -> Result<CollaborationInboxPage, CollaborationInboxError> {
        validate_list_request(request)?;
        let authority = self.authorize(
            &request.actor,
            &request.authenticated_scopes,
            &request.scope,
            &request.audience,
        )?;
        let source = self.source_snapshot(&request.scope)?;
        let (catalog, catalog_sha256) = self.load_catalog(&request.scope)?;
        let cursor = request.cursor.as_ref().map(decode_cursor).transpose()?;
        let snapshot_at_millis = match &cursor {
            Some(cursor) => cursor.snapshot_at_millis,
            None => self.now()?,
        };
        let cut = ListCut::new(request, &source, &authority, &catalog, catalog_sha256)?;
        let offset = cursor
            .as_ref()
            .map(|cursor| cut.validate_cursor(cursor))
            .transpose()?
            .unwrap_or(0);
        let mut items = rebuild_items(
            &source,
            &authority,
            &catalog,
            &request.audience,
            &request.filter,
            snapshot_at_millis,
        )?;
        if offset > items.len() {
            return Err(cursor_expired());
        }
        let end = offset.saturating_add(request.limit).min(items.len());
        let has_more = end < items.len();
        let next_cursor = has_more
            .then(|| cut.cursor(snapshot_at_millis, end))
            .transpose()?;
        let page_items = items.drain(offset..end).collect();
        Ok(CollaborationInboxPage {
            items: page_items,
            has_more,
            next_cursor,
            snapshot_at_millis,
        })
    }

    /// Claims or releases one current pending item without resolving it.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions, terminal items, revoked responsibility, foreign scope,
    /// request conflicts and authority/source changes before any write.
    pub fn apply_claim(
        &mut self,
        command: &CollaborationClaimCommand,
    ) -> Result<CollaborationInboxReceipt, CollaborationInboxError> {
        validate_context(&command.context)?;
        let identity = receipt_identity(&command.context)?;
        let digest = digest_json(&("claim", command))?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &digest)? {
            return replay_receipt(&receipt.events, true);
        }
        let authority = self.authorize_context(&command.context)?;
        let source = self.source_snapshot(&command.context.scope)?;
        let now = self.now()?;
        let item = current_eligible_item(
            &source,
            &authority,
            &command.context.audience,
            &command.item_id,
            now,
        )?;
        let (mut catalog, _) = self.load_catalog(&command.context.scope)?;
        require_catalog_revision(catalog.revision, command.context.expected_revision)?;
        let key = item_key(&command.item_id)?;
        let next_claim = apply_claim_action(
            catalog.claims.get(&key),
            command,
            item,
            &authority.viewer_user_id,
            now,
        )?;
        match &next_claim {
            Some(claim) => {
                catalog.claims.insert(key, claim.clone());
            }
            None => {
                catalog.claims.remove(&key);
            }
        }
        let receipt = CollaborationInboxReceipt::Claim {
            claim: next_claim,
            catalog_revision: next_revision(catalog.revision)?,
            replayed: false,
        };
        self.commit(
            &identity,
            &digest,
            &command.context.scope,
            catalog,
            source_guards(&source, &command.item_id),
            &authority.state_guards,
            &receipt,
        )
    }

    /// Creates, updates or revokes an annotation against the exact current candidate.
    ///
    /// # Errors
    ///
    /// Rejects old candidate identities, invalid coordinates, stale revisions,
    /// terminal items, revoked responsibility and cross-tenant requests with zero writes.
    pub fn apply_annotation(
        &mut self,
        command: &CollaborationAnnotationCommand,
    ) -> Result<CollaborationInboxReceipt, CollaborationInboxError> {
        validate_context(&command.context)?;
        validate_annotation_id(&command.annotation_id)?;
        let identity = receipt_identity(&command.context)?;
        let digest = digest_json(&("annotation", command))?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &digest)? {
            return replay_receipt(&receipt.events, true);
        }
        let authority = self.authorize_context(&command.context)?;
        let source = self.source_snapshot(&command.context.scope)?;
        let now = self.now()?;
        let item = current_eligible_item(
            &source,
            &authority,
            &command.context.audience,
            &command.item_id,
            now,
        )?;
        let (mut catalog, _) = self.load_catalog(&command.context.scope)?;
        require_catalog_revision(catalog.revision, command.context.expected_revision)?;
        let current = catalog.annotations.get(&command.annotation_id.0);
        let annotation =
            apply_annotation_action(current, command, item, &authority.viewer_user_id, now)?;
        catalog
            .annotations
            .insert(command.annotation_id.0.clone(), annotation.clone());
        let receipt = CollaborationInboxReceipt::Annotation {
            annotation,
            catalog_revision: next_revision(catalog.revision)?,
            replayed: false,
        };
        self.commit(
            &identity,
            &digest,
            &command.context.scope,
            catalog,
            source_guards(&source, &command.item_id),
            &authority.state_guards,
            &receipt,
        )
    }

    fn authorize_context(
        &mut self,
        context: &CollaborationInboxCommandContext,
    ) -> Result<CollaborationInboxAuthoritySnapshot, CollaborationInboxError> {
        self.authorize(
            &context.actor,
            &context.authenticated_scopes,
            &context.scope,
            &context.audience,
        )
    }

    fn authorize(
        &mut self,
        actor: &Actor,
        scopes: &[Scope],
        scope: &RepositoryScope,
        audience: &CollaborationInboxAudience,
    ) -> Result<CollaborationInboxAuthoritySnapshot, CollaborationInboxError> {
        let snapshot = self
            .authority
            .authorize(actor, scopes, scope, audience)
            .map_err(|_| authority_unavailable())?;
        validate_authority(&snapshot, actor, scopes, scope, audience)?;
        Ok(snapshot)
    }

    fn source_snapshot(
        &mut self,
        scope: &RepositoryScope,
    ) -> Result<CollaborationInboxSourceSnapshot, CollaborationInboxError> {
        let snapshot = self
            .source
            .snapshot(scope)
            .map_err(|_| source_unavailable())?;
        validate_source(&snapshot, scope)?;
        Ok(snapshot)
    }

    fn now(&mut self) -> Result<u64, CollaborationInboxError> {
        let now = self
            .clock
            .now_millis()
            .map_err(|_| authority_unavailable())?;
        validate_safe_integer(now)?;
        Ok(now)
    }

    fn load_catalog(
        &self,
        scope: &RepositoryScope,
    ) -> Result<(CollaborationCatalog, Sha256Digest), CollaborationInboxError> {
        let stream_id = catalog_stream(scope)?;
        let Some(stored) = self.storage.load_state(&stream_id)? else {
            let catalog = empty_catalog(scope);
            let digest = digest_json(&catalog)?;
            return Ok((catalog, digest));
        };
        let catalog: CollaborationCatalog = canonical_decode(&stored.payload)?;
        if stored.stream_id != stream_id
            || stored.revision != catalog.revision
            || catalog.schema != STATE_SCHEMA
            || &catalog.scope != scope
            || catalog
                .claims
                .len()
                .saturating_add(catalog.annotations.len())
                > MAX_RECORDS
        {
            return Err(corrupt());
        }
        validate_catalog(&catalog)?;
        let digest = digest_json(&catalog)?;
        Ok((catalog, digest))
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
        scope: &RepositoryScope,
        mut catalog: CollaborationCatalog,
        source_guards: &[StateRevisionGuard],
        authority_guards: &[StateRevisionGuard],
        receipt: &CollaborationInboxReceipt,
    ) -> Result<CollaborationInboxReceipt, CollaborationInboxError> {
        let expected_revision = catalog.revision;
        catalog.revision = next_revision(catalog.revision)?;
        if catalog
            .claims
            .len()
            .saturating_add(catalog.annotations.len())
            > MAX_RECORDS
        {
            return Err(invalid());
        }
        let event_payload = serde_json::to_vec(receipt).map_err(|_| corrupt())?;
        let event_id = event_id(identity, digest);
        let mut commit = StateCommit::new(
            identity.clone(),
            digest.clone(),
            catalog_stream(scope)?,
            expected_revision,
            serde_json::to_vec(&catalog).map_err(|_| corrupt())?,
            vec![NewOutboxEvent::internal(
                event_id,
                EVENT_TOPIC,
                event_payload,
            )],
        );
        let mut guarded = BTreeSet::new();
        for guard in source_guards.iter().chain(authority_guards) {
            if guarded.insert(guard.stream_id().to_owned()) {
                commit = commit.with_state_guard(guard.clone());
            } else {
                return Err(corrupt());
            }
        }
        match self.storage.commit(&commit) {
            Ok(stored) => replay_receipt(&stored.events, false),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                if let Some(stored) = self.storage.load_receipt(identity, digest)? {
                    return replay_receipt(&stored.events, true);
                }
                if error.is_state_guard_conflict() {
                    if self.any_guard_changed(source_guards)? {
                        return Err(source_unavailable());
                    }
                    if self.any_guard_changed(authority_guards)? {
                        return Err(authority_unavailable());
                    }
                    return Err(authority_unavailable());
                }
                Err(revision_conflict())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn any_guard_changed(
        &self,
        guards: &[StateRevisionGuard],
    ) -> Result<bool, CollaborationInboxError> {
        for guard in guards {
            let revision = self
                .storage
                .load_state(guard.stream_id())?
                .map_or(0, |state| state.revision);
            if revision != guard.expected_revision() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

struct ListCut {
    scope_sha256: Sha256Digest,
    audience_sha256: Sha256Digest,
    filter_sha256: Sha256Digest,
    source_revision: u64,
    source_sha256: Sha256Digest,
    authority_revision: u64,
    authority_sha256: Sha256Digest,
    catalog_revision: u64,
    catalog_sha256: Sha256Digest,
}

impl ListCut {
    fn new(
        request: &CollaborationInboxListRequest,
        source: &CollaborationInboxSourceSnapshot,
        authority: &CollaborationInboxAuthoritySnapshot,
        catalog: &CollaborationCatalog,
        catalog_sha256: Sha256Digest,
    ) -> Result<Self, CollaborationInboxError> {
        Ok(Self {
            scope_sha256: digest_json(&request.scope)?,
            audience_sha256: digest_json(&request.audience)?,
            filter_sha256: digest_json(&request.filter)?,
            source_revision: source.revision,
            source_sha256: source.snapshot_sha256.clone(),
            authority_revision: authority.authority_revision,
            authority_sha256: authority.authority_sha256.clone(),
            catalog_revision: catalog.revision,
            catalog_sha256,
        })
    }

    fn validate_cursor(&self, cursor: &CursorPayload) -> Result<usize, CollaborationInboxError> {
        if cursor.schema != CURSOR_SCHEMA
            || cursor.scope_sha256 != self.scope_sha256
            || cursor.audience_sha256 != self.audience_sha256
            || cursor.filter_sha256 != self.filter_sha256
            || cursor.source_revision != self.source_revision
            || cursor.source_sha256 != self.source_sha256
            || cursor.authority_revision != self.authority_revision
            || cursor.authority_sha256 != self.authority_sha256
            || cursor.catalog_revision != self.catalog_revision
            || cursor.catalog_sha256 != self.catalog_sha256
        {
            return Err(cursor_expired());
        }
        Ok(cursor.offset)
    }

    fn cursor(
        &self,
        snapshot_at_millis: u64,
        offset: usize,
    ) -> Result<OpaqueCursor, CollaborationInboxError> {
        encode_cursor(&CursorPayload {
            schema: CURSOR_SCHEMA,
            scope_sha256: self.scope_sha256.clone(),
            audience_sha256: self.audience_sha256.clone(),
            filter_sha256: self.filter_sha256.clone(),
            source_revision: self.source_revision,
            source_sha256: self.source_sha256.clone(),
            authority_revision: self.authority_revision,
            authority_sha256: self.authority_sha256.clone(),
            catalog_revision: self.catalog_revision,
            catalog_sha256: self.catalog_sha256.clone(),
            snapshot_at_millis,
            offset,
        })
    }
}

fn rebuild_items(
    source: &CollaborationInboxSourceSnapshot,
    authority: &CollaborationInboxAuthoritySnapshot,
    catalog: &CollaborationCatalog,
    audience: &CollaborationInboxAudience,
    filter: &CollaborationInboxFilter,
    snapshot_at_millis: u64,
) -> Result<Vec<CollaborationInboxItem>, CollaborationInboxError> {
    let mut items = Vec::new();
    for source_item in &source.items {
        let assignments =
            eligible_assignments(authority, source_item, audience, snapshot_at_millis);
        if assignments.is_empty() {
            continue;
        }
        let effective_state = source_item.effective_state(snapshot_at_millis);
        if (!filter.kinds.is_empty() && !filter.kinds.contains(&source_item.kind))
            || (!filter.states.is_empty() && !filter.states.contains(&effective_state))
        {
            continue;
        }
        let key = item_key(&source_item.id)?;
        let claim = catalog.claims.get(&key).cloned().filter(|claim| {
            effective_state == CollaborationInboxItemState::Pending
                && claim.source_revision == source_item.source_revision
                && claim.source_sha256 == source_item.source_sha256
        });
        if filter.only_claimed_by_viewer
            && claim.as_ref().map(|value| &value.claimant_user_id)
                != Some(&authority.viewer_user_id)
        {
            continue;
        }
        let mut annotations = catalog
            .annotations
            .values()
            .filter(|annotation| {
                annotation.item_id == source_item.id
                    && annotation.state == CollaborationAnnotationState::Active
                    && annotation.source_revision == source_item.source_revision
                    && annotation.source_sha256 == source_item.source_sha256
                    && source_item.candidate.as_ref() == Some(&annotation.candidate)
            })
            .cloned()
            .collect::<Vec<_>>();
        annotations.sort_by(|left, right| left.id.cmp(&right.id));
        items.push(CollaborationInboxItem {
            source: source_item.clone(),
            effective_state,
            assignment_ids: assignments
                .into_iter()
                .map(|value| value.assignment.id.clone())
                .collect(),
            claim,
            annotations,
        });
    }
    items.sort_by(|left, right| {
        state_priority(left.effective_state)
            .cmp(&state_priority(right.effective_state))
            .then_with(|| {
                left.source
                    .opened_at_millis
                    .cmp(&right.source.opened_at_millis)
            })
            .then_with(|| left.source.id.cmp(&right.source.id))
    });
    Ok(items)
}

fn current_eligible_item<'source>(
    source: &'source CollaborationInboxSourceSnapshot,
    authority: &CollaborationInboxAuthoritySnapshot,
    audience: &CollaborationInboxAudience,
    item_id: &CollaborationInboxItemId,
    now: u64,
) -> Result<&'source CollaborationInboxSourceItem, CollaborationInboxError> {
    let item = source
        .items
        .iter()
        .find(|item| &item.id == item_id)
        .ok_or_else(not_found)?;
    if item.effective_state(now) != CollaborationInboxItemState::Pending {
        return Err(wrong_state());
    }
    if eligible_assignments(authority, item, audience, now).is_empty() {
        return Err(unauthorized());
    }
    Ok(item)
}

fn eligible_assignments<'authority>(
    authority: &'authority CollaborationInboxAuthoritySnapshot,
    item: &CollaborationInboxSourceItem,
    audience: &CollaborationInboxAudience,
    now: u64,
) -> Vec<&'authority CollaborationResponsibilityEntitlement> {
    authority
        .assignments
        .iter()
        .filter(|entitlement| {
            let assignment = &entitlement.assignment;
            assignment.scope == authority.scope
                && assignment.target == item.target
                && assignment.role == item.responsibility_role
                && assignment.effective_state(now) == ResponsibilityAssignmentState::Active
                && match audience {
                    CollaborationInboxAudience::Personal(user_id) => {
                        &assignment.principal_user_id == user_id
                            && user_id == &authority.viewer_user_id
                    }
                    CollaborationInboxAudience::Team(team_id) => {
                        authority.visible_team_ids.contains(team_id)
                            && entitlement.team_ids.contains(team_id)
                    }
                }
        })
        .collect()
}

fn apply_claim_action(
    current: Option<&CollaborationClaim>,
    command: &CollaborationClaimCommand,
    item: &CollaborationInboxSourceItem,
    viewer: &UserId,
    now: u64,
) -> Result<Option<CollaborationClaim>, CollaborationInboxError> {
    match command.action {
        CollaborationClaimAction::Claim => {
            if current.is_some_and(|claim| {
                claim.claimant_user_id != *viewer || claim.audience != command.context.audience
            }) {
                return Err(wrong_state());
            }
            let revision = current
                .map(|claim| next_revision(claim.revision))
                .transpose()?
                .unwrap_or(1);
            Ok(Some(CollaborationClaim {
                item_id: command.item_id.clone(),
                audience: command.context.audience.clone(),
                claimant_user_id: viewer.clone(),
                revision,
                claimed_at_millis: now,
                source_revision: item.source_revision,
                source_sha256: item.source_sha256.clone(),
            }))
        }
        CollaborationClaimAction::Release => {
            let claim = current.ok_or_else(wrong_state)?;
            if claim.claimant_user_id != *viewer || claim.audience != command.context.audience {
                return Err(unauthorized());
            }
            Ok(None)
        }
    }
}

fn apply_annotation_action(
    current: Option<&CollaborationAnnotation>,
    command: &CollaborationAnnotationCommand,
    item: &CollaborationInboxSourceItem,
    viewer: &UserId,
    now: u64,
) -> Result<CollaborationAnnotation, CollaborationInboxError> {
    match &command.action {
        CollaborationAnnotationAction::Upsert {
            candidate,
            target,
            body_sha256,
        } => {
            validate_candidate(candidate)?;
            validate_annotation_target(target)?;
            validate_sha256(body_sha256)?;
            if item.candidate.as_ref() != Some(candidate) {
                return Err(candidate_changed());
            }
            if current.is_some_and(|annotation| {
                annotation.author_user_id != *viewer || annotation.item_id != command.item_id
            }) {
                return Err(unauthorized());
            }
            let revision = current
                .map(|annotation| next_revision(annotation.revision))
                .transpose()?
                .unwrap_or(1);
            Ok(CollaborationAnnotation {
                id: command.annotation_id.clone(),
                item_id: command.item_id.clone(),
                author_user_id: viewer.clone(),
                candidate: candidate.clone(),
                target: target.clone(),
                body_sha256: body_sha256.clone(),
                state: CollaborationAnnotationState::Active,
                revision,
                source_revision: item.source_revision,
                source_sha256: item.source_sha256.clone(),
                updated_at_millis: now,
            })
        }
        CollaborationAnnotationAction::Revoke => {
            let current = current.ok_or_else(not_found)?;
            if current.author_user_id != *viewer || current.item_id != command.item_id {
                return Err(unauthorized());
            }
            let mut annotation = current.clone();
            annotation.state = CollaborationAnnotationState::Revoked;
            annotation.revision = next_revision(annotation.revision)?;
            annotation.updated_at_millis = now;
            Ok(annotation)
        }
    }
}

fn validate_list_request(
    request: &CollaborationInboxListRequest,
) -> Result<(), CollaborationInboxError> {
    if !(1..=MAX_PAGE_SIZE).contains(&request.limit) {
        return Err(invalid());
    }
    validate_scope(&request.scope)?;
    validate_filters(&request.filter)
}

fn validate_context(
    context: &CollaborationInboxCommandContext,
) -> Result<(), CollaborationInboxError> {
    validate_scope(&context.scope)?;
    validate_safe_integer(context.expected_revision)?;
    if context.request_id.0.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

fn validate_filters(filter: &CollaborationInboxFilter) -> Result<(), CollaborationInboxError> {
    let kinds = filter.kinds.iter().copied().collect::<BTreeSet<_>>();
    let states = filter.states.iter().copied().collect::<BTreeSet<_>>();
    if kinds.len() != filter.kinds.len() || states.len() != filter.states.len() {
        return Err(invalid());
    }
    Ok(())
}

fn validate_authority(
    authority: &CollaborationInboxAuthoritySnapshot,
    actor: &Actor,
    scopes: &[Scope],
    scope: &RepositoryScope,
    audience: &CollaborationInboxAudience,
) -> Result<(), CollaborationInboxError> {
    let expected_scope = Scope::RepositoryScope(scope.clone());
    let viewer = match actor {
        Actor::UserActor(user) => &user.id,
        Actor::ServiceAccountActor(_) | Actor::SystemActor(_) => return Err(unauthorized()),
    };
    if &authority.scope != scope
        || &authority.viewer_user_id != viewer
        || !scopes.contains(&expected_scope)
        || !matches!(audience, CollaborationInboxAudience::Personal(user) if user == viewer)
            && !matches!(audience, CollaborationInboxAudience::Team(team) if authority.visible_team_ids.contains(team))
    {
        return Err(unauthorized());
    }
    validate_safe_integer(authority.authority_revision)?;
    validate_sha256(&authority.authority_sha256)?;
    if authority.state_guards.is_empty() {
        return Err(corrupt());
    }
    let mut ids = BTreeSet::new();
    for entitlement in &authority.assignments {
        if entitlement.assignment.scope != *scope
            || !ids.insert(entitlement.assignment.id.0.clone())
            || entitlement.assignment.principal_user_id.0.is_empty()
        {
            return Err(corrupt());
        }
    }
    validate_guards(&authority.state_guards)
}

fn validate_source(
    source: &CollaborationInboxSourceSnapshot,
    scope: &RepositoryScope,
) -> Result<(), CollaborationInboxError> {
    if &source.scope != scope {
        return Err(source_unavailable());
    }
    validate_safe_integer(source.revision)?;
    validate_sha256(&source.snapshot_sha256)?;
    for (item_id, guards) in &source.item_state_guards {
        if !source.items.iter().any(|item| &item.id == item_id) {
            return Err(corrupt());
        }
        validate_guards(guards)?;
    }
    let mut ids = BTreeSet::new();
    for item in &source.items {
        validate_source_item(item)?;
        if !ids.insert(item.id.clone())
            || source
                .item_state_guards
                .get(&item.id)
                .is_none_or(Vec::is_empty)
        {
            return Err(corrupt());
        }
    }
    let mut canonical = source.items.clone();
    canonical.sort_by(|left, right| left.id.cmp(&right.id));
    if digest_json(&canonical)? != source.snapshot_sha256 {
        return Err(corrupt());
    }
    Ok(())
}

fn source_guards<'source>(
    source: &'source CollaborationInboxSourceSnapshot,
    item_id: &CollaborationInboxItemId,
) -> &'source [StateRevisionGuard] {
    source
        .item_state_guards
        .get(item_id)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn validate_source_item(
    item: &CollaborationInboxSourceItem,
) -> Result<(), CollaborationInboxError> {
    validate_safe_integer(item.source_revision)?;
    validate_safe_integer(item.opened_at_millis)?;
    if let Some(expires_at) = item.expires_at_millis {
        validate_safe_integer(expires_at)?;
        if expires_at <= item.opened_at_millis {
            return Err(corrupt());
        }
    }
    validate_sha256(&item.source_sha256)?;
    validate_sha256(&item.title_sha256)?;
    if let Some(candidate) = &item.candidate {
        validate_candidate(candidate)?;
    }
    let exact = matches!(
        (&item.id, item.kind, &item.command_route),
        (
            CollaborationInboxItemId::Approval(left),
            CollaborationInboxItemKind::Approval,
            FormalCollaborationCommandRoute::ApprovalDecide { approval_id: right, .. }
        ) if left == right
    ) || matches!(
        (&item.id, item.kind, &item.command_route),
        (
            CollaborationInboxItemId::GateAttention(left),
            CollaborationInboxItemKind::GateAttention,
            FormalCollaborationCommandRoute::GateAttentionRespond { attention_item_id: right, .. }
        ) if left == right
    ) || matches!(
        (&item.id, item.kind, &item.command_route),
        (
            CollaborationInboxItemId::DeliveryAttention(left),
            CollaborationInboxItemKind::DeliveryAttention,
            FormalCollaborationCommandRoute::DeliveryResolveAttention { attention_item_id: right, .. }
        ) if left == right
    );
    let valid_role = match item.kind {
        CollaborationInboxItemKind::Approval => {
            item.responsibility_role == ResponsibilityRole::Approver
        }
        CollaborationInboxItemKind::GateAttention => {
            item.responsibility_role == ResponsibilityRole::Reviewer
        }
        CollaborationInboxItemKind::DeliveryAttention => true,
    };
    if !exact || !valid_role || !route_matches_target(&item.command_route, &item.target) {
        return Err(corrupt());
    }
    Ok(())
}

fn route_matches_target(
    route: &FormalCollaborationCommandRoute,
    target: &ResponsibilityTarget,
) -> bool {
    match (route, target) {
        (
            FormalCollaborationCommandRoute::ApprovalDecide {
                product_session_id, ..
            }
            | FormalCollaborationCommandRoute::GateAttentionRespond {
                product_session_id, ..
            },
            ResponsibilityTarget::ProductSession {
                product_session_id: target_id,
            },
        ) => product_session_id == target_id,
        (
            FormalCollaborationCommandRoute::ApprovalDecide { .. }
            | FormalCollaborationCommandRoute::GateAttentionRespond { .. },
            ResponsibilityTarget::Delivery { .. }
            | ResponsibilityTarget::DeliveryStage { .. }
            | ResponsibilityTarget::Review { .. },
        ) => true,
        (
            FormalCollaborationCommandRoute::DeliveryResolveAttention { delivery_id, .. },
            ResponsibilityTarget::Delivery {
                delivery_id: target_id,
            }
            | ResponsibilityTarget::DeliveryStage {
                delivery_id: target_id,
                ..
            }
            | ResponsibilityTarget::Review {
                delivery_id: target_id,
                ..
            },
        ) => delivery_id == target_id,
        (
            FormalCollaborationCommandRoute::DeliveryResolveAttention { .. },
            ResponsibilityTarget::ProductSession { .. },
        ) => false,
    }
}

fn validate_catalog(catalog: &CollaborationCatalog) -> Result<(), CollaborationInboxError> {
    validate_safe_integer(catalog.revision)?;
    for (key, claim) in &catalog.claims {
        if key != &item_key(&claim.item_id)? {
            return Err(corrupt());
        }
        validate_safe_integer(claim.revision)?;
        validate_safe_integer(claim.claimed_at_millis)?;
        validate_safe_integer(claim.source_revision)?;
        validate_sha256(&claim.source_sha256)?;
    }
    for (key, annotation) in &catalog.annotations {
        if key != &annotation.id.0 {
            return Err(corrupt());
        }
        validate_annotation_id(&annotation.id)?;
        validate_candidate(&annotation.candidate)?;
        validate_annotation_target(&annotation.target)?;
        validate_sha256(&annotation.body_sha256)?;
        validate_sha256(&annotation.source_sha256)?;
        validate_safe_integer(annotation.revision)?;
        validate_safe_integer(annotation.source_revision)?;
        validate_safe_integer(annotation.updated_at_millis)?;
    }
    Ok(())
}

fn validate_candidate(
    candidate: &CollaborationCandidateIdentity,
) -> Result<(), CollaborationInboxError> {
    if candidate.candidate_ref.is_empty()
        || candidate.candidate_ref.len() > MAX_BOUNDED_TEXT
        || candidate.candidate_revision == 0
    {
        return Err(invalid());
    }
    validate_safe_integer(candidate.candidate_revision)?;
    validate_sha256(&candidate.candidate_digest)
}

fn validate_annotation_target(
    target: &CollaborationAnnotationTarget,
) -> Result<(), CollaborationInboxError> {
    match target {
        CollaborationAnnotationTarget::Node { node_id } => validate_text(node_id),
        CollaborationAnnotationTarget::File { path, blob_sha256 } => {
            validate_path(path)?;
            validate_sha256(blob_sha256)
        }
        CollaborationAnnotationTarget::Hunk {
            path,
            base_blob_sha256,
            start_line,
            end_line,
            hunk_sha256,
        } => {
            validate_path(path)?;
            validate_sha256(base_blob_sha256)?;
            validate_sha256(hunk_sha256)?;
            if *start_line == 0 || start_line > end_line {
                return Err(invalid());
            }
            validate_safe_integer(*end_line)
        }
    }
}

fn validate_annotation_id(id: &CollaborationAnnotationId) -> Result<(), CollaborationInboxError> {
    if id.0.len() < 5
        || id.0.len() > 128
        || !id
            .0
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_scope(scope: &RepositoryScope) -> Result<(), CollaborationInboxError> {
    let values = [
        &scope.organization_id.0,
        &scope.workspace_id.0,
        &scope.project_id.0,
        &scope.repository_id.0,
    ];
    if values.iter().any(|value| value.is_empty()) {
        return Err(invalid());
    }
    Ok(())
}

fn validate_guards(guards: &[StateRevisionGuard]) -> Result<(), CollaborationInboxError> {
    let mut streams = BTreeSet::new();
    for guard in guards {
        validate_safe_integer(guard.expected_revision())?;
        if guard.stream_id().is_empty() || !streams.insert(guard.stream_id()) {
            return Err(corrupt());
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), CollaborationInboxError> {
    validate_text(path)?;
    if path.starts_with('/')
        || path.contains('\\')
        || path.split('/').any(|component| component == "..")
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), CollaborationInboxError> {
    if value.is_empty() || value.len() > MAX_BOUNDED_TEXT || value.contains('\0') {
        return Err(invalid());
    }
    Ok(())
}

fn validate_sha256(value: &Sha256Digest) -> Result<(), CollaborationInboxError> {
    let Some(hex) = value.0.strip_prefix("sha256:") else {
        return Err(invalid());
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(())
}

fn validate_safe_integer(value: u64) -> Result<(), CollaborationInboxError> {
    if value > MAX_SAFE_INTEGER {
        return Err(invalid());
    }
    Ok(())
}

fn require_catalog_revision(actual: u64, expected: u64) -> Result<(), CollaborationInboxError> {
    if actual != expected {
        return Err(revision_conflict());
    }
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, CollaborationInboxError> {
    let next = revision.checked_add(1).ok_or_else(revision_conflict)?;
    validate_safe_integer(next)?;
    Ok(next)
}

fn state_priority(state: CollaborationInboxItemState) -> u8 {
    match state {
        CollaborationInboxItemState::Pending => 0,
        CollaborationInboxItemState::Expired => 1,
        CollaborationInboxItemState::Rejected => 2,
        CollaborationInboxItemState::Resolved => 3,
        CollaborationInboxItemState::Approved => 4,
    }
}

fn receipt_identity(
    context: &CollaborationInboxCommandContext,
) -> Result<ReceiptIdentity, CollaborationInboxError> {
    command_receipt_identity(
        &context.actor,
        &Scope::RepositoryScope(context.scope.clone()),
        context.request_id.clone(),
    )
    .map_err(Into::into)
}

fn empty_catalog(scope: &RepositoryScope) -> CollaborationCatalog {
    CollaborationCatalog {
        schema: STATE_SCHEMA.to_owned(),
        scope: scope.clone(),
        revision: 0,
        claims: BTreeMap::new(),
        annotations: BTreeMap::new(),
    }
}

fn catalog_stream(scope: &RepositoryScope) -> Result<String, CollaborationInboxError> {
    Ok(format!("{STREAM_PREFIX}{}", digest_json(scope)?.0))
}

fn item_key(item: &CollaborationInboxItemId) -> Result<String, CollaborationInboxError> {
    Ok(digest_json(item)?.0)
}

fn event_id(identity: &ReceiptIdentity, digest: &Sha256Digest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.collaboration-inbox-event.v1\0");
    hasher.update(identity.actor_key().as_bytes());
    hasher.update(identity.scope_key().as_bytes());
    hasher.update(identity.request_id().0.as_bytes());
    hasher.update(digest.0.as_bytes());
    format!("collaboration-inbox:{:x}", hasher.finalize())
}

fn digest_json<T: Serialize>(value: &T) -> Result<Sha256Digest, CollaborationInboxError> {
    let bytes = serde_json::to_vec(value).map_err(|_| corrupt())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn canonical_decode<T>(bytes: &[u8]) -> Result<T, CollaborationInboxError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
    let decoded: T = serde_json::from_value(value.clone()).map_err(|_| corrupt())?;
    if serde_json::to_value(&decoded).map_err(|_| corrupt())? != value {
        return Err(corrupt());
    }
    Ok(decoded)
}

fn encode_cursor(cursor: &CursorPayload) -> Result<OpaqueCursor, CollaborationInboxError> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| invalid())?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(cursor: &OpaqueCursor) -> Result<CursorPayload, CollaborationInboxError> {
    if cursor.0.len() > MAX_CURSOR_BYTES.saturating_mul(2) {
        return Err(cursor_expired());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| cursor_expired())?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(cursor_expired());
    }
    canonical_decode(&bytes).map_err(|_| cursor_expired())
}

fn replay_receipt(
    events: &[winwincode_storage::OutboxEvent],
    replayed: bool,
) -> Result<CollaborationInboxReceipt, CollaborationInboxError> {
    let [event] = events else {
        return Err(corrupt());
    };
    if event.topic != EVENT_TOPIC {
        return Err(corrupt());
    }
    let mut receipt: CollaborationInboxReceipt = canonical_decode(&event.payload)?;
    match &mut receipt {
        CollaborationInboxReceipt::Claim {
            replayed: stored, ..
        }
        | CollaborationInboxReceipt::Annotation {
            replayed: stored, ..
        } => *stored = replayed,
    }
    Ok(receipt)
}

const fn error(
    kind: CollaborationInboxErrorKind,
    message: &'static str,
) -> CollaborationInboxError {
    CollaborationInboxError { kind, message }
}

const fn invalid() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::Invalid,
        "Inbox request is invalid",
    )
}

const fn unauthorized() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::Unauthorized,
        "Inbox authority denied the request",
    )
}

const fn not_found() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::NotFound,
        "Inbox item or annotation was not found",
    )
}

const fn wrong_state() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::WrongState,
        "Inbox item or collaboration overlay is in the wrong state",
    )
}

const fn candidate_changed() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::CandidateChanged,
        "review candidate changed before annotation submission",
    )
}

const fn revision_conflict() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::RevisionConflict,
        "Inbox collaboration revision changed",
    )
}

const fn request_conflict() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::RequestConflict,
        "Inbox request identity was reused with different input",
    )
}

const fn cursor_expired() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::CursorExpired,
        "Inbox cursor no longer identifies the same snapshot",
    )
}

const fn authority_unavailable() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::AuthorityUnavailable,
        "current Inbox authority is unavailable",
    )
}

const fn source_unavailable() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::SourceUnavailable,
        "canonical Approval or Attention source is unavailable",
    )
}

const fn storage() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::Storage,
        "Inbox storage is unavailable",
    )
}

const fn corrupt() -> CollaborationInboxError {
    error(
        CollaborationInboxErrorKind::Corrupt,
        "durable Inbox state is corrupt",
    )
}
