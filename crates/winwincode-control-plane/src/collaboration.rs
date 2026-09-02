// SPDX-License-Identifier: Apache-2.0

//! Durable Activity, per-user notification receipts, and leased Presence.
//!
//! Activity rows are immutable audit references. Notifications are a read view
//! over those rows plus one monotonic per-user acknowledgement receipt, so a
//! delivery or WebSocket failure never changes the originating business state.
//! Presence is an explicitly short-lived fact whose stale state is derived from
//! the trusted clock.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{
    Actor, CollaborationActivityCategory, CollaborationActivityListQuery,
    CollaborationActivityListResultResponse, CollaborationActivityListResultResponseQuery,
    CollaborationActivityPage, CollaborationActivityPageKind, CollaborationActivityProjection,
    CollaborationNotificationAckCommand, CollaborationNotificationAckCompletedResponse,
    CollaborationNotificationAckCompletedResponseCommand,
    CollaborationNotificationAckCompletedResponseOutcome, CollaborationNotificationListQuery,
    CollaborationNotificationListResultResponse, CollaborationNotificationListResultResponseQuery,
    CollaborationNotificationPage, CollaborationNotificationPageKind,
    CollaborationNotificationProjection, CollaborationNotificationReceiptProjection,
    CollaborationNotificationState, CollaborationPresenceListQuery,
    CollaborationPresenceListResultResponse, CollaborationPresenceListResultResponseQuery,
    CollaborationPresencePage, CollaborationPresencePageKind, CollaborationPresenceProjection,
    CollaborationPresenceState, CollaborationPresenceUpdateCommand,
    CollaborationPresenceUpdateCompletedResponse,
    CollaborationPresenceUpdateCompletedResponseCommand,
    CollaborationPresenceUpdateCompletedResponseOutcome,
    ControlPlaneWebSocketActivityRecordedEvent,
    ControlPlaneWebSocketActivityRecordedEventTypeValue, ControlPlaneWebSocketPresenceChangedEvent,
    ControlPlaneWebSocketPresenceChangedEventTypeValue, EnterprisePermission, PageInfo, Scope,
};
use winwincode_domain::{
    ControlPlaneEventId, Instant, OpaqueCursor, RequestId, Revision, SchemaVersion, Sha256Digest,
    UserId,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ProjectionEventStream, PublicEventSource,
    ReceiptIdentity, SqliteStorage, StateCommit, StateMutation, StorageError, StorageErrorKind,
    StoredState,
};

use crate::{
    EnterpriseRbacService, command_receipt_identity, instant_from_millis, public_event_actor,
    public_event_scope,
};

const ACTIVITY_HEAD_SCHEMA: &str = "winwincode.collaboration-activity-head.v1";
const ACTIVITY_ENTRY_SCHEMA: &str = "winwincode.collaboration-activity-entry.v1";
const ACTIVITY_SOURCE_SCHEMA: &str = "winwincode.collaboration-activity-source.v1";
const NOTIFICATION_SCHEMA: &str = "winwincode.collaboration-notification-receipt.v1";
const PRESENCE_SCHEMA: &str = "winwincode.collaboration-presence.v1";
const CURSOR_SCHEMA: &str = "winwincode.collaboration-page.v1";
const ACTIVITY_RECEIPT_TOPIC: &str = "collaboration.activity.receipt.internal.v1";
const NOTIFICATION_RECEIPT_TOPIC: &str = "collaboration.notification.receipt.internal.v1";
const PRESENCE_RECEIPT_TOPIC: &str = "collaboration.presence.receipt.internal.v1";
const ACTIVITY_PUBLIC_TOPIC: &str = "activity.recorded.v1";
const PRESENCE_PUBLIC_TOPIC: &str = "presence.changed.v1";
const COMPONENT: &str = "collaboration";
const MAX_PAGE_SIZE: usize = 200;
const SCAN_PAGE_SIZE: usize = 256;
const MAX_SCANNED_ROWS_PER_PAGE: usize = 1_024;
const MAX_CURSOR_BYTES: usize = 4_096;
const MAX_PRESENCE_RECORDS: usize = 20_000;
const MAX_PRESENCE_DIRECTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MIN_PRESENCE_LEASE_MILLIS: u64 = 5_000;
const MAX_PRESENCE_LEASE_MILLIS: u64 = 300_000;
const MAX_ACTIVITY_SOURCE_LENGTH: usize = 200;
const MAX_ACTIVITY_SUMMARY_CHARS: usize = 4_096;
const MAX_COMMIT_ATTEMPTS: usize = 8;

/// Stable error classes exposed by the collaboration application boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollaborationErrorKind {
    InvalidRequest,
    PermissionDenied,
    RevisionConflict,
    RequestConflict,
    CursorInvalid,
    Storage,
    Corrupt,
}

/// Bounded collaboration failure. It never exposes storage paths or SQL text.
#[derive(Debug)]
pub struct CollaborationError {
    kind: CollaborationErrorKind,
    message: &'static str,
}

impl CollaborationError {
    #[must_use]
    pub const fn kind(&self) -> CollaborationErrorKind {
        self.kind
    }
}

impl fmt::Display for CollaborationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CollaborationError {}

impl From<StorageError> for CollaborationError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => invalid(),
            StorageErrorKind::RevisionConflict => revision_conflict(),
            StorageErrorKind::RequestConflict => request_conflict(),
            StorageErrorKind::EventCursorExpired => cursor_invalid(),
            StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => storage(),
        }
    }
}

/// Trusted clock used for notification acknowledgement and Presence leases.
pub trait CollaborationClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted clock is unavailable.
    fn now_millis(&mut self) -> Result<u64, CollaborationClockError>;
}

/// Bounded trusted-clock failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CollaborationClockError;

/// System trusted-clock adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCollaborationClock;

impl CollaborationClock for SystemCollaborationClock {
    fn now_millis(&mut self) -> Result<u64, CollaborationClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CollaborationClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| CollaborationClockError)
    }
}

/// Immutable business-event reference recorded into Activity.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationActivityRecordRequest {
    pub actor: Actor,
    pub scope: Scope,
    pub request_id: RequestId,
    pub source: String,
    pub source_sequence: u64,
    pub source_digest: Sha256Digest,
    pub category: CollaborationActivityCategory,
    pub summary: String,
    pub delivery_id: Option<winwincode_domain::DeliveryId>,
    pub product_session_id: Option<winwincode_domain::ProductSessionId>,
    pub occurred_at: Instant,
}

/// Durable collaboration application service.
///
/// Authorization always delegates to the one injected enterprise RBAC service.
/// Storage owns Activity, notification receipts, Presence leases, and their
/// public outbox events in one canonical `SQLite` authority.
pub struct CollaborationService {
    inner: Mutex<CollaborationInner>,
    rbac: Arc<EnterpriseRbacService>,
    database_path: PathBuf,
}

struct CollaborationInner {
    storage: Box<dyn ProductStateStorage>,
    clock: Box<dyn CollaborationClock>,
}

impl CollaborationService {
    #[must_use]
    pub fn new(storage: SqliteStorage, rbac: Arc<EnterpriseRbacService>) -> Self {
        Self::with_clock(storage, rbac, Box::new(SystemCollaborationClock))
    }

    #[must_use]
    pub fn with_clock(
        storage: SqliteStorage,
        rbac: Arc<EnterpriseRbacService>,
        clock: Box<dyn CollaborationClock>,
    ) -> Self {
        let database_path = storage.database_path().to_path_buf();
        Self {
            inner: Mutex::new(CollaborationInner {
                storage: Box::new(storage),
                clock,
            }),
            rbac,
            database_path,
        }
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Records one immutable Activity reference and one public event atomically.
    /// Duplicate or out-of-order source receipts never create another row or event.
    ///
    /// # Errors
    ///
    /// Fails closed for invalid input, current RBAC denial, conflicting source
    /// receipts, exhausted concurrent retries, or durable storage failure.
    pub fn record_activity(
        &self,
        authenticated_scopes: &[Scope],
        request: &CollaborationActivityRecordRequest,
    ) -> Result<CollaborationActivityProjection, CollaborationError> {
        authorize(
            &self.rbac,
            &request.actor,
            authenticated_scopes,
            &request.scope,
            &EnterprisePermission::CollaborationWrite,
        )?;
        validate_activity_request(request)?;
        self.lock()?.record_activity(request)
    }

    /// Returns one stable keyset page from the immutable Activity ledger.
    ///
    /// # Errors
    ///
    /// Fails closed for current RBAC denial, malformed/stale cursor, corrupt
    /// durable state, or storage failure.
    pub fn activity_list(
        &self,
        authenticated_scopes: &[Scope],
        query: &CollaborationActivityListQuery,
    ) -> Result<CollaborationActivityListResultResponse, CollaborationError> {
        require_schema(&query.schema_version)?;
        authorize(
            &self.rbac,
            &query.actor,
            authenticated_scopes,
            &query.scope,
            &EnterprisePermission::CollaborationRead,
        )?;
        let filter = ActivityFilter {
            categories: query.parameters.categories.clone(),
            delivery_id: query.parameters.delivery_id.clone(),
            product_session_id: query.parameters.product_session_id.clone(),
            notification_states: Vec::new(),
            notification_user_id: None,
            view: "activity".to_owned(),
        };
        let page = self
            .lock()?
            .activity_page(&query.scope, &query.page, &filter, None)?;
        Ok(CollaborationActivityListResultResponse {
            page: page.page,
            query: CollaborationActivityListResultResponseQuery::CollaborationActivityList,
            request_id: query.request_id.clone(),
            result: CollaborationActivityPage {
                items: page.items,
                kind: CollaborationActivityPageKind::CollaborationActivityPage,
                snapshot_revision: Revision(public_i64(page.snapshot_revision)?),
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Returns the authenticated User's notification view over Activity.
    ///
    /// # Errors
    ///
    /// Fails closed for a non-User actor, current RBAC denial, invalid cursor,
    /// corrupt durable state, or storage failure.
    pub fn notification_list(
        &self,
        authenticated_scopes: &[Scope],
        query: &CollaborationNotificationListQuery,
    ) -> Result<CollaborationNotificationListResultResponse, CollaborationError> {
        require_schema(&query.schema_version)?;
        authorize(
            &self.rbac,
            &query.actor,
            authenticated_scopes,
            &query.scope,
            &EnterprisePermission::CollaborationRead,
        )?;
        let user_id = user_id(&query.actor)?;
        let filter = ActivityFilter {
            categories: query.parameters.categories.clone(),
            delivery_id: None,
            product_session_id: None,
            notification_states: query.parameters.states.clone(),
            notification_user_id: Some(user_id.clone()),
            view: "notification".to_owned(),
        };
        let mut inner = self.lock()?;
        let receipt = inner.load_notification_receipt(&query.scope, &user_id)?;
        let acknowledged = receipt.as_ref().map_or(0, |state| state.through_sequence);
        let acknowledged_at = receipt.map(|state| state.acknowledged_at);
        let page = inner.activity_page(
            &query.scope,
            &query.page,
            &filter,
            Some(&NotificationRead {
                through_sequence: acknowledged,
            }),
        )?;
        let items = page
            .items
            .into_iter()
            .map(|activity| {
                let read = u64::try_from(activity.sequence)
                    .ok()
                    .is_some_and(|sequence| sequence <= acknowledged);
                Ok(CollaborationNotificationProjection {
                    acknowledged_at: if read { acknowledged_at.clone() } else { None },
                    activity,
                    state: if read {
                        CollaborationNotificationState::Read
                    } else {
                        CollaborationNotificationState::Unread
                    },
                })
            })
            .collect::<Result<Vec<_>, CollaborationError>>()?;
        Ok(CollaborationNotificationListResultResponse {
            page: page.page,
            query: CollaborationNotificationListResultResponseQuery::CollaborationNotificationList,
            request_id: query.request_id.clone(),
            result: CollaborationNotificationPage {
                items,
                kind: CollaborationNotificationPageKind::CollaborationNotificationPage,
                snapshot_revision: Revision(public_i64(page.snapshot_revision)?),
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Advances one User's monotonic notification acknowledgement receipt.
    ///
    /// # Errors
    ///
    /// Fails closed for current RBAC denial, revision or request conflict,
    /// acknowledgement beyond current Activity, clock failure, or storage error.
    pub fn notification_ack(
        &self,
        authenticated_scopes: &[Scope],
        command: &CollaborationNotificationAckCommand,
    ) -> Result<CollaborationNotificationAckCompletedResponse, CollaborationError> {
        require_schema(&command.schema_version)?;
        authorize(
            &self.rbac,
            &command.actor,
            authenticated_scopes,
            &command.scope,
            &EnterprisePermission::CollaborationWrite,
        )?;
        let user_id = user_id(&command.actor)?;
        self.lock()?.notification_ack(command, &user_id)
    }

    /// Replaces one authenticated User's leased Presence fact.
    ///
    /// # Errors
    ///
    /// Fails closed for current RBAC denial, invalid lease pairing, revision or
    /// request conflict, clock failure, or durable storage error.
    pub fn presence_update(
        &self,
        authenticated_scopes: &[Scope],
        command: &CollaborationPresenceUpdateCommand,
    ) -> Result<CollaborationPresenceUpdateCompletedResponse, CollaborationError> {
        require_schema(&command.schema_version)?;
        authorize(
            &self.rbac,
            &command.actor,
            authenticated_scopes,
            &command.scope,
            &EnterprisePermission::CollaborationWrite,
        )?;
        let user_id = user_id(&command.actor)?;
        self.lock()?.presence_update(command, &user_id)
    }

    /// Returns current Presence, deriving expired leases as offline at one
    /// trusted read time and sealing that time in the page cursor.
    ///
    /// # Errors
    ///
    /// Fails closed for current RBAC denial, invalid cursor, clock failure,
    /// concurrent snapshot change, corrupt state, or storage failure.
    pub fn presence_list(
        &self,
        authenticated_scopes: &[Scope],
        query: &CollaborationPresenceListQuery,
    ) -> Result<CollaborationPresenceListResultResponse, CollaborationError> {
        require_schema(&query.schema_version)?;
        authorize(
            &self.rbac,
            &query.actor,
            authenticated_scopes,
            &query.scope,
            &EnterprisePermission::CollaborationRead,
        )?;
        self.lock()?.presence_list(query)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, CollaborationInner>, CollaborationError> {
        self.inner.lock().map_err(|_| storage())
    }
}

impl CollaborationInner {
    fn record_activity(
        &mut self,
        request: &CollaborationActivityRecordRequest,
    ) -> Result<CollaborationActivityProjection, CollaborationError> {
        let receipt_identity =
            command_receipt_identity(&request.actor, &request.scope, request.request_id.clone())?;
        let command_digest = digest(request)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&receipt_identity, &command_digest)?
        {
            return decode_activity_receipt(&receipt);
        }
        let scope_hash = scope_hash(&request.scope)?;
        let source_stream =
            activity_source_stream(&scope_hash, &request.source, request.source_sequence);
        for _ in 0..MAX_COMMIT_ATTEMPTS {
            if let Some(existing) = self.storage.load_state(&source_stream)? {
                return replay_source(&existing, request, &scope_hash);
            }
            let head_stream = activity_head_stream(&scope_hash);
            let head = self.load_activity_head(&head_stream, &scope_hash)?;
            let sequence = head.last_sequence.checked_add(1).ok_or_else(corrupt)?;
            if sequence > MAX_SAFE_INTEGER {
                return Err(corrupt());
            }
            let projection = CollaborationActivityProjection {
                actor: request.actor.clone(),
                category: request.category.clone(),
                delivery_id: request.delivery_id.clone(),
                occurred_at: request.occurred_at.clone(),
                product_session_id: request.product_session_id.clone(),
                sequence: public_i64(sequence)?,
                summary: request.summary.clone(),
            };
            let next_head = ActivityHead {
                schema: ACTIVITY_HEAD_SCHEMA.to_owned(),
                scope_sha256: scope_hash.clone(),
                last_sequence: sequence,
            };
            let entry = ActivityEntry {
                schema: ACTIVITY_ENTRY_SCHEMA.to_owned(),
                scope_sha256: scope_hash.clone(),
                projection: projection.clone(),
            };
            let source = ActivitySourceReceipt {
                schema: ACTIVITY_SOURCE_SCHEMA.to_owned(),
                scope_sha256: scope_hash.clone(),
                source: request.source.clone(),
                source_sequence: request.source_sequence,
                source_digest: request.source_digest.clone(),
                projection: projection.clone(),
            };
            let response = ActivityReceiptEvent {
                schema: ACTIVITY_RECEIPT_TOPIC.to_owned(),
                projection: projection.clone(),
            };
            let event_id = activity_event_id(&scope_hash, request);
            let public_event = activity_public_event(request, event_id.clone())?;
            let commit = StateCommit::new(
                receipt_identity.clone(),
                command_digest.clone(),
                head_stream,
                head.last_sequence,
                encode(&next_head)?,
                vec![
                    NewOutboxEvent::internal(
                        format!("internal:{}", event_id.0),
                        ACTIVITY_RECEIPT_TOPIC,
                        encode(&response)?,
                    ),
                    public_event,
                ],
            )
            .with_state_mutation(StateMutation::new(
                activity_entry_stream(&scope_hash, sequence),
                0,
                encode(&entry)?,
            )?)
            .with_state_mutation(StateMutation::new(
                source_stream.clone(),
                0,
                encode(&source)?,
            )?);
            match self.storage.commit(&commit) {
                Ok(receipt) => return decode_activity_receipt(&receipt),
                Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                    if let Some(receipt) = self
                        .storage
                        .load_receipt(&receipt_identity, &command_digest)?
                    {
                        return decode_activity_receipt(&receipt);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(revision_conflict())
    }

    fn activity_page(
        &mut self,
        scope: &Scope,
        page: &winwincode_api::generated::PageRequest,
        filter: &ActivityFilter,
        notification: Option<&NotificationRead>,
    ) -> Result<ActivityPageRead, CollaborationError> {
        let limit = page_limit(page.limit)?;
        let scope_hash = scope_hash(scope)?;
        let prefix = activity_entry_prefix(&scope_hash);
        let filter_sha256 = digest(filter)?;
        let cursor = decode_activity_cursor(page.cursor.as_ref(), &scope_hash, &filter_sha256)?;
        let (upper_bound, mut after, snapshot_revision) = if let Some(cursor) = cursor {
            (cursor.upper_bound, cursor.after, cursor.snapshot_revision)
        } else {
            let upper = self.storage.last_state_stream_id(&prefix)?;
            let revision = upper
                .as_deref()
                .map(|stream| activity_sequence_from_stream(&prefix, stream))
                .transpose()?
                .unwrap_or(0);
            (upper.unwrap_or_default(), String::new(), revision)
        };
        if upper_bound.is_empty() {
            return Ok(ActivityPageRead::empty(snapshot_revision));
        }
        let mut items = Vec::with_capacity(limit);
        let mut scanned = 0_usize;
        let mut exhausted = false;
        while items.len() < limit && scanned < MAX_SCANNED_ROWS_PER_PAGE {
            let remaining = MAX_SCANNED_ROWS_PER_PAGE - scanned;
            let rows = self.storage.scan_state_streams(
                &prefix,
                &after,
                &upper_bound,
                SCAN_PAGE_SIZE.min(remaining),
            )?;
            if rows.is_empty() {
                exhausted = true;
                break;
            }
            scanned += rows.len();
            let row_count = rows.len();
            for row in rows {
                after.clone_from(&row.stream_id);
                let projection = decode_activity_entry(&row, &scope_hash)?;
                if activity_matches(&projection, filter, notification) {
                    items.push(projection);
                    if items.len() == limit {
                        break;
                    }
                }
            }
            if items.len() < limit && (row_count < SCAN_PAGE_SIZE || after == upper_bound) {
                exhausted = true;
                break;
            }
        }
        let has_more = !exhausted && after < upper_bound;
        let next_cursor = has_more
            .then(|| {
                encode_cursor(&CollaborationCursor {
                    schema: CURSOR_SCHEMA.to_owned(),
                    kind: filter.view.clone(),
                    scope_sha256: scope_hash,
                    filter_sha256,
                    upper_bound,
                    after,
                    snapshot_revision,
                    snapshot_at_millis: None,
                    snapshot_digest: None,
                })
            })
            .transpose()?;
        Ok(ActivityPageRead {
            items,
            page: PageInfo {
                has_more,
                next_cursor,
            },
            snapshot_revision,
        })
    }

    fn notification_ack(
        &mut self,
        command: &CollaborationNotificationAckCommand,
        user_id: &UserId,
    ) -> Result<CollaborationNotificationAckCompletedResponse, CollaborationError> {
        let expected_revision = revision(command.expected_revision.0)?;
        let through_sequence = revision(command.payload.through_sequence)?;
        let identity =
            command_receipt_identity(&command.actor, &command.scope, command.request_id.clone())?;
        let command_digest = digest(command)?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &command_digest)? {
            return decode_notification_receipt(&receipt);
        }
        let scope_hash = scope_hash(&command.scope)?;
        let current_max = self.current_activity_sequence(&scope_hash)?;
        if through_sequence > current_max {
            return Err(invalid());
        }
        let stream_id = notification_stream(&scope_hash, user_id);
        let current = self.load_notification_receipt(&command.scope, user_id)?;
        let current_revision = current.as_ref().map_or(0, |state| state.revision);
        if current_revision != expected_revision {
            return Err(revision_conflict());
        }
        if current
            .as_ref()
            .is_some_and(|state| through_sequence < state.through_sequence)
        {
            return Err(invalid());
        }
        let now = instant_from_millis(self.clock.now_millis().map_err(|_| storage())?)?;
        let next_revision = checked_next_revision(current_revision)?;
        let state = NotificationReceiptState {
            schema: NOTIFICATION_SCHEMA.to_owned(),
            scope_sha256: scope_hash,
            user_id: user_id.clone(),
            through_sequence,
            revision: next_revision,
            acknowledged_at: now.clone(),
        };
        let response = CollaborationNotificationAckCompletedResponse {
            command:
                CollaborationNotificationAckCompletedResponseCommand::CollaborationNotificationAck,
            current_revision: Revision(public_i64(next_revision)?),
            outcome: CollaborationNotificationAckCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: CollaborationNotificationReceiptProjection {
                acknowledged_at: now,
                revision: Revision(public_i64(next_revision)?),
                through_sequence: public_i64(through_sequence)?,
                user_id: user_id.clone(),
            },
            schema_version: command.schema_version.clone(),
        };
        let commit = StateCommit::new(
            identity.clone(),
            command_digest.clone(),
            stream_id,
            current_revision,
            encode(&state)?,
            vec![NewOutboxEvent::internal(
                internal_event_id("notification", &identity, &command_digest),
                NOTIFICATION_RECEIPT_TOPIC,
                encode(&NotificationReceiptEvent {
                    schema: NOTIFICATION_RECEIPT_TOPIC.to_owned(),
                    response: response.clone(),
                })?,
            )],
        );
        match self.storage.commit(&commit) {
            Ok(receipt) => decode_notification_receipt(&receipt),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => self
                .storage
                .load_receipt(&identity, &command_digest)?
                .map_or_else(
                    || Err(revision_conflict()),
                    |receipt| decode_notification_receipt(&receipt),
                ),
            Err(error) => Err(error.into()),
        }
    }

    fn presence_update(
        &mut self,
        command: &CollaborationPresenceUpdateCommand,
        user_id: &UserId,
    ) -> Result<CollaborationPresenceUpdateCompletedResponse, CollaborationError> {
        validate_presence_payload(command)?;
        let expected_revision = revision(command.expected_revision.0)?;
        let identity =
            command_receipt_identity(&command.actor, &command.scope, command.request_id.clone())?;
        let command_digest = digest(command)?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &command_digest)? {
            return decode_presence_receipt(&receipt);
        }
        let scope_hash = scope_hash(&command.scope)?;
        let stream_id = presence_stream(
            &scope_hash,
            user_id,
            command.payload.product_session_id.as_ref(),
        )?;
        let current = self.storage.load_state(&stream_id)?;
        let current_revision = current.as_ref().map_or(0, |state| state.revision);
        if current_revision != expected_revision {
            return Err(revision_conflict());
        }
        if let Some(current) = current.as_ref() {
            decode_presence_state(current, &scope_hash)?;
        }
        let now_millis = self.clock.now_millis().map_err(|_| storage())?;
        let now = instant_from_millis(now_millis)?;
        let expires_at = command
            .payload
            .lease_duration_millis
            .map(revision)
            .transpose()?
            .map(|duration| {
                now_millis
                    .checked_add(duration)
                    .ok_or_else(invalid)
                    .and_then(|expires_at| instant_from_millis(expires_at).map_err(Into::into))
            })
            .transpose()?;
        let next_revision = checked_next_revision(current_revision)?;
        let projection = CollaborationPresenceProjection {
            expires_at,
            observed_at: now.clone(),
            product_session_id: command.payload.product_session_id.clone(),
            revision: Revision(public_i64(next_revision)?),
            state: command.payload.state.clone(),
            user_id: user_id.clone(),
        };
        let state = PresenceState {
            schema: PRESENCE_SCHEMA.to_owned(),
            scope_sha256: scope_hash,
            projection: projection.clone(),
        };
        let response = CollaborationPresenceUpdateCompletedResponse {
            command:
                CollaborationPresenceUpdateCompletedResponseCommand::CollaborationPresenceUpdate,
            current_revision: Revision(public_i64(next_revision)?),
            outcome: CollaborationPresenceUpdateCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection.clone(),
            schema_version: command.schema_version.clone(),
        };
        let event_id = presence_event_id(&identity, &command_digest);
        let public_event =
            presence_public_event(command, user_id, &projection, now, event_id.clone())?;
        let commit = StateCommit::new(
            identity.clone(),
            command_digest.clone(),
            stream_id,
            current_revision,
            encode(&state)?,
            vec![
                NewOutboxEvent::internal(
                    format!("internal:{}", event_id.0),
                    PRESENCE_RECEIPT_TOPIC,
                    encode(&PresenceReceiptEvent {
                        schema: PRESENCE_RECEIPT_TOPIC.to_owned(),
                        response: response.clone(),
                    })?,
                ),
                public_event,
            ],
        );
        match self.storage.commit(&commit) {
            Ok(receipt) => decode_presence_receipt(&receipt),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => self
                .storage
                .load_receipt(&identity, &command_digest)?
                .map_or_else(
                    || Err(revision_conflict()),
                    |receipt| decode_presence_receipt(&receipt),
                ),
            Err(error) => Err(error.into()),
        }
    }

    fn presence_list(
        &mut self,
        query: &CollaborationPresenceListQuery,
    ) -> Result<CollaborationPresenceListResultResponse, CollaborationError> {
        let limit = page_limit(query.page.limit)?;
        let scope_hash = scope_hash(&query.scope)?;
        let prefix = presence_prefix(&scope_hash);
        let filter = PresenceFilter {
            product_session_id: query.parameters.product_session_id.clone(),
            states: query.parameters.states.clone(),
        };
        let filter_sha256 = digest(&filter)?;
        let cursor =
            decode_presence_cursor(query.page.cursor.as_ref(), &scope_hash, &filter_sha256)?;
        let directory = self.presence_directory_snapshot(&prefix)?;
        let (upper_bound, after, snapshot_revision, snapshot_at_millis) =
            if let Some(cursor) = cursor {
                if cursor.snapshot_revision != directory.revision
                    || cursor.snapshot_digest.as_ref() != Some(&directory.digest)
                {
                    return Err(cursor_invalid());
                }
                (
                    cursor.upper_bound,
                    cursor.after,
                    cursor.snapshot_revision,
                    cursor.snapshot_at_millis.ok_or_else(cursor_invalid)?,
                )
            } else {
                (
                    self.storage
                        .last_state_stream_id(&prefix)?
                        .unwrap_or_default(),
                    String::new(),
                    directory.revision,
                    self.clock.now_millis().map_err(|_| storage())?,
                )
            };
        if upper_bound.is_empty() {
            return presence_response(
                query,
                Vec::new(),
                PageInfo {
                    has_more: false,
                    next_cursor: None,
                },
                snapshot_revision,
            );
        }
        let page = self.scan_presence_page(PresencePageRequest {
            prefix: &prefix,
            scope_hash: &scope_hash,
            filter: &filter,
            filter_sha256,
            upper_bound,
            after,
            limit,
            snapshot_revision,
            snapshot_digest: directory.digest,
            snapshot_at_millis,
        })?;
        presence_response(query, page.items, page.page, snapshot_revision)
    }

    fn scan_presence_page(
        &self,
        mut request: PresencePageRequest<'_>,
    ) -> Result<PresencePageRead, CollaborationError> {
        let mut items = Vec::with_capacity(request.limit);
        let mut scanned = 0_usize;
        let mut exhausted = false;
        while items.len() < request.limit && scanned < MAX_SCANNED_ROWS_PER_PAGE {
            let remaining = MAX_SCANNED_ROWS_PER_PAGE - scanned;
            let rows = self.storage.scan_state_streams(
                request.prefix,
                &request.after,
                &request.upper_bound,
                SCAN_PAGE_SIZE.min(remaining),
            )?;
            if rows.is_empty() {
                exhausted = true;
                break;
            }
            scanned += rows.len();
            let row_count = rows.len();
            for row in rows {
                request.after.clone_from(&row.stream_id);
                let mut projection = decode_presence_state(&row, request.scope_hash)?.projection;
                if projection
                    .expires_at
                    .as_ref()
                    .map(crate::session_binding_transaction::instant_millis)
                    .transpose()?
                    .is_some_and(|expires_at| request.snapshot_at_millis >= expires_at)
                {
                    projection.state = CollaborationPresenceState::Offline;
                    projection.expires_at = None;
                }
                if presence_matches(&projection, request.filter) {
                    items.push(projection);
                    if items.len() == request.limit {
                        break;
                    }
                }
            }
            if items.len() < request.limit
                && (row_count < SCAN_PAGE_SIZE || request.after == request.upper_bound)
            {
                exhausted = true;
                break;
            }
        }
        let current_directory = self.presence_directory_snapshot(request.prefix)?;
        if current_directory.revision != request.snapshot_revision
            || current_directory.digest != request.snapshot_digest
        {
            return Err(revision_conflict());
        }
        let has_more = !exhausted && request.after < request.upper_bound;
        let next_cursor = has_more
            .then(|| {
                encode_cursor(&CollaborationCursor {
                    schema: CURSOR_SCHEMA.to_owned(),
                    kind: "presence".to_owned(),
                    scope_sha256: request.scope_hash.to_owned(),
                    filter_sha256: request.filter_sha256,
                    upper_bound: request.upper_bound,
                    after: request.after,
                    snapshot_revision: request.snapshot_revision,
                    snapshot_at_millis: Some(request.snapshot_at_millis),
                    snapshot_digest: Some(request.snapshot_digest),
                })
            })
            .transpose()?;
        Ok(PresencePageRead {
            items,
            page: PageInfo {
                has_more,
                next_cursor,
            },
        })
    }

    fn load_activity_head(
        &self,
        stream_id: &str,
        scope_hash: &str,
    ) -> Result<ActivityHead, CollaborationError> {
        self.storage.load_state(stream_id)?.map_or_else(
            || {
                Ok(ActivityHead {
                    schema: ACTIVITY_HEAD_SCHEMA.to_owned(),
                    scope_sha256: scope_hash.to_owned(),
                    last_sequence: 0,
                })
            },
            |stored| {
                let head: ActivityHead = decode(&stored.payload)?;
                if head.schema != ACTIVITY_HEAD_SCHEMA
                    || head.scope_sha256 != scope_hash
                    || head.last_sequence != stored.revision
                {
                    return Err(corrupt());
                }
                Ok(head)
            },
        )
    }

    fn current_activity_sequence(&self, scope_hash: &str) -> Result<u64, CollaborationError> {
        let stream = activity_head_stream(scope_hash);
        Ok(self.load_activity_head(&stream, scope_hash)?.last_sequence)
    }

    fn load_notification_receipt(
        &self,
        scope: &Scope,
        user_id: &UserId,
    ) -> Result<Option<NotificationReceiptState>, CollaborationError> {
        let scope_hash = scope_hash(scope)?;
        let stream = notification_stream(&scope_hash, user_id);
        self.storage
            .load_state(&stream)?
            .map(|stored| {
                let state: NotificationReceiptState = decode(&stored.payload)?;
                if state.schema != NOTIFICATION_SCHEMA
                    || state.scope_sha256 != scope_hash
                    || state.user_id != *user_id
                    || state.revision != stored.revision
                {
                    return Err(corrupt());
                }
                crate::session_binding_transaction::instant_millis(&state.acknowledged_at)?;
                Ok(state)
            })
            .transpose()
    }

    fn presence_directory_snapshot(
        &self,
        prefix: &str,
    ) -> Result<PresenceDirectorySnapshot, CollaborationError> {
        let directory = self.storage.load_bounded_state_directory(
            prefix,
            MAX_PRESENCE_RECORDS,
            MAX_PRESENCE_DIRECTORY_BYTES,
        )?;
        let mut digest = Sha256::new();
        digest.update(b"winwincode.collaboration.presence-directory.v1");
        let revision = directory.into_iter().try_fold(0_u64, |total, row| {
            digest.update(row.stream_id.as_bytes());
            digest.update(row.revision.to_be_bytes());
            digest.update(row.payload_sha256.0.as_bytes());
            total
                .checked_add(row.revision)
                .filter(|value| *value <= MAX_SAFE_INTEGER)
                .ok_or_else(corrupt)
        })?;
        Ok(PresenceDirectorySnapshot {
            revision,
            digest: Sha256Digest(format!("sha256:{:x}", digest.finalize())),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActivityHead {
    schema: String,
    scope_sha256: String,
    last_sequence: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActivityEntry {
    schema: String,
    scope_sha256: String,
    projection: CollaborationActivityProjection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActivitySourceReceipt {
    schema: String,
    scope_sha256: String,
    source: String,
    source_sequence: u64,
    source_digest: Sha256Digest,
    projection: CollaborationActivityProjection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActivityReceiptEvent {
    schema: String,
    projection: CollaborationActivityProjection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotificationReceiptState {
    schema: String,
    scope_sha256: String,
    user_id: UserId,
    through_sequence: u64,
    revision: u64,
    acknowledged_at: Instant,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotificationReceiptEvent {
    schema: String,
    response: CollaborationNotificationAckCompletedResponse,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresenceState {
    schema: String,
    scope_sha256: String,
    projection: CollaborationPresenceProjection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresenceReceiptEvent {
    schema: String,
    response: CollaborationPresenceUpdateCompletedResponse,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActivityFilter {
    categories: Vec<CollaborationActivityCategory>,
    delivery_id: Option<winwincode_domain::DeliveryId>,
    product_session_id: Option<winwincode_domain::ProductSessionId>,
    notification_states: Vec<CollaborationNotificationState>,
    notification_user_id: Option<UserId>,
    view: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresenceFilter {
    product_session_id: Option<winwincode_domain::ProductSessionId>,
    states: Vec<CollaborationPresenceState>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CollaborationCursor {
    schema: String,
    kind: String,
    scope_sha256: String,
    filter_sha256: Sha256Digest,
    upper_bound: String,
    after: String,
    snapshot_revision: u64,
    snapshot_at_millis: Option<u64>,
    snapshot_digest: Option<Sha256Digest>,
}

struct ActivityPageRead {
    items: Vec<CollaborationActivityProjection>,
    page: PageInfo,
    snapshot_revision: u64,
}

impl ActivityPageRead {
    fn empty(snapshot_revision: u64) -> Self {
        Self {
            items: Vec::new(),
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            snapshot_revision,
        }
    }
}

struct NotificationRead {
    through_sequence: u64,
}

struct PresencePageRequest<'a> {
    prefix: &'a str,
    scope_hash: &'a str,
    filter: &'a PresenceFilter,
    filter_sha256: Sha256Digest,
    upper_bound: String,
    after: String,
    limit: usize,
    snapshot_revision: u64,
    snapshot_digest: Sha256Digest,
    snapshot_at_millis: u64,
}

struct PresencePageRead {
    items: Vec<CollaborationPresenceProjection>,
    page: PageInfo,
}

struct PresenceDirectorySnapshot {
    revision: u64,
    digest: Sha256Digest,
}

fn authorize(
    rbac: &EnterpriseRbacService,
    actor: &Actor,
    authenticated_scopes: &[Scope],
    scope: &Scope,
    permission: &EnterprisePermission,
) -> Result<(), CollaborationError> {
    let decision = rbac
        .authorize(actor, authenticated_scopes, scope, permission)
        .map_err(|_| storage())?;
    if decision.allowed {
        Ok(())
    } else {
        Err(permission_denied())
    }
}

fn presence_response(
    query: &CollaborationPresenceListQuery,
    items: Vec<CollaborationPresenceProjection>,
    page: PageInfo,
    snapshot_revision: u64,
) -> Result<CollaborationPresenceListResultResponse, CollaborationError> {
    Ok(CollaborationPresenceListResultResponse {
        page,
        query: CollaborationPresenceListResultResponseQuery::CollaborationPresenceList,
        request_id: query.request_id.clone(),
        result: CollaborationPresencePage {
            items,
            kind: CollaborationPresencePageKind::CollaborationPresencePage,
            snapshot_revision: Revision(public_i64(snapshot_revision)?),
        },
        schema_version: query.schema_version.clone(),
    })
}

fn validate_activity_request(
    request: &CollaborationActivityRecordRequest,
) -> Result<(), CollaborationError> {
    if request.source.is_empty()
        || request.source.len() > MAX_ACTIVITY_SOURCE_LENGTH
        || request.source.trim() != request.source
        || request.source_sequence == 0
        || request.source_sequence > MAX_SAFE_INTEGER
        || request.summary.is_empty()
        || request.summary.chars().count() > MAX_ACTIVITY_SUMMARY_CHARS
        || request.summary.trim() != request.summary
        || !valid_sha256(&request.source_digest)
    {
        return Err(invalid());
    }
    crate::session_binding_transaction::instant_millis(&request.occurred_at)?;
    Ok(())
}

fn validate_presence_payload(
    command: &CollaborationPresenceUpdateCommand,
) -> Result<(), CollaborationError> {
    let duration = command
        .payload
        .lease_duration_millis
        .map(revision)
        .transpose()?;
    match (&command.payload.state, duration) {
        (CollaborationPresenceState::Online | CollaborationPresenceState::Away, Some(value))
            if (MIN_PRESENCE_LEASE_MILLIS..=MAX_PRESENCE_LEASE_MILLIS).contains(&value) =>
        {
            Ok(())
        }
        (CollaborationPresenceState::Offline, None) => Ok(()),
        _ => Err(invalid()),
    }
}

fn activity_matches(
    projection: &CollaborationActivityProjection,
    filter: &ActivityFilter,
    notification: Option<&NotificationRead>,
) -> bool {
    if !filter.categories.is_empty() && !filter.categories.contains(&projection.category) {
        return false;
    }
    if filter.delivery_id.is_some() && filter.delivery_id != projection.delivery_id {
        return false;
    }
    if filter.product_session_id.is_some()
        && filter.product_session_id != projection.product_session_id
    {
        return false;
    }
    let Some(notification) = notification else {
        return true;
    };
    let read = u64::try_from(projection.sequence)
        .ok()
        .is_some_and(|sequence| sequence <= notification.through_sequence);
    filter.notification_states.is_empty()
        || filter.notification_states.iter().any(|state| {
            matches!(
                (state, read),
                (CollaborationNotificationState::Read, true)
                    | (CollaborationNotificationState::Unread, false)
            )
        })
}

fn presence_matches(projection: &CollaborationPresenceProjection, filter: &PresenceFilter) -> bool {
    (filter.product_session_id.is_none()
        || filter.product_session_id == projection.product_session_id)
        && (filter.states.is_empty() || filter.states.contains(&projection.state))
}

fn valid_persisted_presence(projection: &CollaborationPresenceProjection) -> bool {
    match &projection.state {
        CollaborationPresenceState::Online | CollaborationPresenceState::Away => {
            projection.expires_at.is_some()
        }
        CollaborationPresenceState::Offline => projection.expires_at.is_none(),
    }
}

fn decode_activity_entry(
    stored: &StoredState,
    scope_hash: &str,
) -> Result<CollaborationActivityProjection, CollaborationError> {
    let entry: ActivityEntry = decode(&stored.payload)?;
    let prefix = activity_entry_prefix(scope_hash);
    let sequence = activity_sequence_from_stream(&prefix, &stored.stream_id)?;
    if entry.schema != ACTIVITY_ENTRY_SCHEMA
        || entry.scope_sha256 != scope_hash
        || entry.projection.sequence != public_i64(sequence)?
        || stored.revision != 1
    {
        return Err(corrupt());
    }
    Ok(entry.projection)
}

fn decode_presence_state(
    stored: &StoredState,
    scope_hash: &str,
) -> Result<PresenceState, CollaborationError> {
    let state: PresenceState = decode(&stored.payload)?;
    if state.schema != PRESENCE_SCHEMA
        || state.scope_sha256 != scope_hash
        || revision(state.projection.revision.0)? != stored.revision
        || presence_stream(
            scope_hash,
            &state.projection.user_id,
            state.projection.product_session_id.as_ref(),
        )? != stored.stream_id
        || !valid_persisted_presence(&state.projection)
    {
        return Err(corrupt());
    }
    crate::session_binding_transaction::instant_millis(&state.projection.observed_at)?;
    if let Some(expires_at) = state.projection.expires_at.as_ref() {
        crate::session_binding_transaction::instant_millis(expires_at)?;
    }
    Ok(state)
}

fn replay_source(
    stored: &StoredState,
    request: &CollaborationActivityRecordRequest,
    scope_hash: &str,
) -> Result<CollaborationActivityProjection, CollaborationError> {
    let source: ActivitySourceReceipt = decode(&stored.payload)?;
    if source.schema != ACTIVITY_SOURCE_SCHEMA
        || source.scope_sha256 != scope_hash
        || source.source != request.source
        || source.source_sequence != request.source_sequence
        || stored.revision != 1
        || revision(source.projection.sequence).is_err()
    {
        return Err(corrupt());
    }
    if source.source_digest != request.source_digest {
        return Err(request_conflict());
    }
    Ok(source.projection)
}

fn decode_activity_receipt(
    receipt: &CommitReceipt,
) -> Result<CollaborationActivityProjection, CollaborationError> {
    let [internal, public] = receipt.events.as_slice() else {
        return Err(corrupt());
    };
    if internal.topic != ACTIVITY_RECEIPT_TOPIC || public.topic != ACTIVITY_PUBLIC_TOPIC {
        return Err(corrupt());
    }
    let event: ActivityReceiptEvent = decode(&internal.payload)?;
    if event.schema != ACTIVITY_RECEIPT_TOPIC
        || revision(event.projection.sequence)? != receipt.revision
    {
        return Err(corrupt());
    }
    Ok(event.projection)
}

fn decode_notification_receipt(
    receipt: &CommitReceipt,
) -> Result<CollaborationNotificationAckCompletedResponse, CollaborationError> {
    let [event] = receipt.events.as_slice() else {
        return Err(corrupt());
    };
    if event.topic != NOTIFICATION_RECEIPT_TOPIC {
        return Err(corrupt());
    }
    let event: NotificationReceiptEvent = decode(&event.payload)?;
    if event.schema != NOTIFICATION_RECEIPT_TOPIC
        || revision(event.response.current_revision.0)? != receipt.revision
    {
        return Err(corrupt());
    }
    Ok(event.response)
}

fn decode_presence_receipt(
    receipt: &CommitReceipt,
) -> Result<CollaborationPresenceUpdateCompletedResponse, CollaborationError> {
    let [internal, public] = receipt.events.as_slice() else {
        return Err(corrupt());
    };
    if internal.topic != PRESENCE_RECEIPT_TOPIC || public.topic != PRESENCE_PUBLIC_TOPIC {
        return Err(corrupt());
    }
    let event: PresenceReceiptEvent = decode(&internal.payload)?;
    if event.schema != PRESENCE_RECEIPT_TOPIC
        || revision(event.response.current_revision.0)? != receipt.revision
    {
        return Err(corrupt());
    }
    Ok(event.response)
}

fn activity_public_event(
    request: &CollaborationActivityRecordRequest,
    event_id: ControlPlaneEventId,
) -> Result<NewOutboxEvent, CollaborationError> {
    let payload = serde_json::to_vec(&ControlPlaneWebSocketActivityRecordedEvent {
        actor: request.actor.clone(),
        category: activity_category_name(&request.category).to_owned(),
        delivery_id: request.delivery_id.clone(),
        product_session_id: request.product_session_id.clone(),
        summary: request.summary.clone(),
        type_value: ControlPlaneWebSocketActivityRecordedEventTypeValue::ActivityRecordedV1,
    })
    .map_err(|_| invalid())?;
    NewOutboxEvent::public_projection(
        event_id,
        ACTIVITY_PUBLIC_TOPIC,
        payload,
        ProjectionEventStream::Scope,
        public_event_scope(&request.scope),
        request.occurred_at.clone(),
        PublicEventSource::ControlPlane {
            actor: public_event_actor(&request.actor),
            component: COMPONENT.to_owned(),
        },
    )
    .map_err(Into::into)
}

fn presence_public_event(
    command: &CollaborationPresenceUpdateCommand,
    user_id: &UserId,
    projection: &CollaborationPresenceProjection,
    observed_at: Instant,
    event_id: ControlPlaneEventId,
) -> Result<NewOutboxEvent, CollaborationError> {
    let payload = serde_json::to_vec(&ControlPlaneWebSocketPresenceChangedEvent {
        observed_at: observed_at.clone(),
        product_session_id: projection.product_session_id.clone(),
        state: presence_state_name(&projection.state).to_owned(),
        type_value: ControlPlaneWebSocketPresenceChangedEventTypeValue::PresenceChangedV1,
        user_id: user_id.clone(),
    })
    .map_err(|_| invalid())?;
    NewOutboxEvent::public_projection(
        event_id,
        PRESENCE_PUBLIC_TOPIC,
        payload,
        ProjectionEventStream::Scope,
        public_event_scope(&command.scope),
        observed_at,
        PublicEventSource::ControlPlane {
            actor: public_event_actor(&command.actor),
            component: COMPONENT.to_owned(),
        },
    )
    .map_err(Into::into)
}

fn decode_activity_cursor(
    cursor: Option<&OpaqueCursor>,
    scope_hash: &str,
    filter_sha256: &Sha256Digest,
) -> Result<Option<CollaborationCursor>, CollaborationError> {
    let decoded = decode_cursor(cursor)?;
    if let Some(decoded) = decoded.as_ref()
        && (decoded.kind != "activity" && decoded.kind != "notification"
            || decoded.scope_sha256 != scope_hash
            || decoded.filter_sha256 != *filter_sha256
            || decoded.snapshot_at_millis.is_some()
            || decoded.snapshot_digest.is_some())
    {
        return Err(cursor_invalid());
    }
    Ok(decoded)
}

fn decode_presence_cursor(
    cursor: Option<&OpaqueCursor>,
    scope_hash: &str,
    filter_sha256: &Sha256Digest,
) -> Result<Option<CollaborationCursor>, CollaborationError> {
    let decoded = decode_cursor(cursor)?;
    if let Some(decoded) = decoded.as_ref()
        && (decoded.kind != "presence"
            || decoded.scope_sha256 != scope_hash
            || decoded.filter_sha256 != *filter_sha256
            || decoded.snapshot_at_millis.is_none()
            || decoded.snapshot_digest.is_none())
    {
        return Err(cursor_invalid());
    }
    Ok(decoded)
}

fn encode_cursor(cursor: &CollaborationCursor) -> Result<OpaqueCursor, CollaborationError> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| invalid())?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
) -> Result<Option<CollaborationCursor>, CollaborationError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES * 2 {
        return Err(cursor_invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| cursor_invalid())?;
    let decoded: CollaborationCursor =
        serde_json::from_slice(&bytes).map_err(|_| cursor_invalid())?;
    if decoded.schema != CURSOR_SCHEMA
        || serde_json::to_vec(&decoded).map_err(|_| cursor_invalid())? != bytes
    {
        return Err(cursor_invalid());
    }
    Ok(Some(decoded))
}

fn activity_category_name(category: &CollaborationActivityCategory) -> &'static str {
    match category {
        CollaborationActivityCategory::Product => "product",
        CollaborationActivityCategory::Runtime => "runtime",
        CollaborationActivityCategory::Approval => "approval",
        CollaborationActivityCategory::Publication => "publication",
        CollaborationActivityCategory::Collaboration => "collaboration",
        CollaborationActivityCategory::Security => "security",
    }
}

fn presence_state_name(state: &CollaborationPresenceState) -> &'static str {
    match state {
        CollaborationPresenceState::Online => "online",
        CollaborationPresenceState::Away => "away",
        CollaborationPresenceState::Offline => "offline",
    }
}

fn activity_head_stream(scope_hash: &str) -> String {
    format!("collaboration-activity-head:{scope_hash}")
}

fn activity_entry_prefix(scope_hash: &str) -> String {
    format!("collaboration-activity:{scope_hash}:")
}

fn activity_entry_stream(scope_hash: &str, sequence: u64) -> String {
    format!("{}{sequence:020}", activity_entry_prefix(scope_hash))
}

fn activity_source_stream(scope_hash: &str, source: &str, sequence: u64) -> String {
    format!(
        "collaboration-source:{scope_hash}:{}:{sequence:020}",
        digest_bytes(source.as_bytes())
    )
}

fn notification_stream(scope_hash: &str, user_id: &UserId) -> String {
    format!(
        "collaboration-notification:{scope_hash}:{}",
        digest_bytes(user_id.0.as_bytes())
    )
}

fn presence_prefix(scope_hash: &str) -> String {
    format!("collaboration-presence:{scope_hash}:")
}

fn presence_stream(
    scope_hash: &str,
    user_id: &UserId,
    product_session_id: Option<&winwincode_domain::ProductSessionId>,
) -> Result<String, CollaborationError> {
    let identity = serde_json::to_vec(&(user_id, product_session_id)).map_err(|_| invalid())?;
    Ok(format!(
        "{}{}",
        presence_prefix(scope_hash),
        digest_bytes(&identity)
    ))
}

fn activity_sequence_from_stream(prefix: &str, stream: &str) -> Result<u64, CollaborationError> {
    stream
        .strip_prefix(prefix)
        .filter(|value| value.len() == 20 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(corrupt)
}

fn scope_hash(scope: &Scope) -> Result<String, CollaborationError> {
    serde_json::to_vec(scope)
        .map(|bytes| digest_bytes(&bytes))
        .map_err(|_| invalid())
}

fn activity_event_id(
    scope_hash: &str,
    request: &CollaborationActivityRecordRequest,
) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.collaboration.activity-event.v1");
    digest.update(scope_hash.as_bytes());
    digest.update(request.source.as_bytes());
    digest.update(request.source_sequence.to_be_bytes());
    digest.update(request.source_digest.0.as_bytes());
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

fn presence_event_id(
    identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.collaboration.presence-event.v1");
    digest.update(identity.actor_key().as_bytes());
    digest.update(identity.scope_key().as_bytes());
    digest.update(identity.request_id().0.as_bytes());
    digest.update(command_digest.0.as_bytes());
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

fn internal_event_id(
    namespace: &str,
    identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.collaboration.internal-event.v1");
    digest.update(namespace.as_bytes());
    digest.update(identity.actor_key().as_bytes());
    digest.update(identity.scope_key().as_bytes());
    digest.update(identity.request_id().0.as_bytes());
    digest.update(command_digest.0.as_bytes());
    format!("internal:{:x}", digest.finalize())
}

fn digest<T: Serialize + ?Sized>(value: &T) -> Result<Sha256Digest, CollaborationError> {
    serde_json::to_vec(value)
        .map(|bytes| Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
        .map_err(|_| invalid())
}

fn digest_bytes(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn valid_sha256(value: &Sha256Digest) -> bool {
    value.0.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CollaborationError> {
    serde_json::to_vec(value).map_err(|_| invalid())
}

fn decode<T: for<'de> Deserialize<'de>>(value: &[u8]) -> Result<T, CollaborationError> {
    serde_json::from_slice(value).map_err(|_| corrupt())
}

fn page_limit(value: i64) -> Result<usize, CollaborationError> {
    usize::try_from(value)
        .ok()
        .filter(|value| (1..=MAX_PAGE_SIZE).contains(value))
        .ok_or_else(invalid)
}

fn revision(value: i64) -> Result<u64, CollaborationError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(invalid)
}

fn checked_next_revision(current: u64) -> Result<u64, CollaborationError> {
    current
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(corrupt)
}

fn public_i64(value: u64) -> Result<i64, CollaborationError> {
    i64::try_from(value).map_err(|_| corrupt())
}

fn require_schema(schema: &SchemaVersion) -> Result<(), CollaborationError> {
    if schema == &SchemaVersion::WinwincodeV1 {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn user_id(actor: &Actor) -> Result<UserId, CollaborationError> {
    let Actor::UserActor(actor) = actor else {
        return Err(permission_denied());
    };
    Ok(actor.id.clone())
}

const fn invalid() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::InvalidRequest,
        message: "Collaboration request is invalid",
    }
}

const fn permission_denied() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::PermissionDenied,
        message: "Collaboration permission is denied",
    }
}

const fn revision_conflict() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::RevisionConflict,
        message: "Collaboration revision conflicts with current state",
    }
}

const fn request_conflict() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::RequestConflict,
        message: "Collaboration request conflicts with a durable receipt",
    }
}

const fn cursor_invalid() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::CursorInvalid,
        message: "Collaboration cursor is invalid",
    }
}

const fn storage() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::Storage,
        message: "Collaboration storage is unavailable",
    }
}

const fn corrupt() -> CollaborationError {
    CollaborationError {
        kind: CollaborationErrorKind::Corrupt,
        message: "Collaboration durable state is inconsistent",
    }
}
