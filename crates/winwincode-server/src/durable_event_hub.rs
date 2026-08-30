// SPDX-License-Identifier: Apache-2.0

//! Durable standalone Server publication, subscription, and cursor recovery.
//!
//! The hub is a transport projection of the Control Plane durable outbox. It
//! persists an event before acknowledging its outbox row as published, so a
//! crash between those operations is recovered by an idempotent republish.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde_json::Value;
use tokio::sync::mpsc;
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketAckFrame, ControlPlaneWebSocketAcknowledgedCursor,
    ControlPlaneWebSocketAuthorizationRevokedFrame,
    ControlPlaneWebSocketAuthorizationRevokedFrameTypeValue,
    ControlPlaneWebSocketBackpressureFrame, ControlPlaneWebSocketBackpressureFrameTypeValue,
    ControlPlaneWebSocketClientFrame, ControlPlaneWebSocketControlPlaneSource,
    ControlPlaneWebSocketControlPlaneSourceKind, ControlPlaneWebSocketEventFrame,
    ControlPlaneWebSocketEventFrameTypeValue, ControlPlaneWebSocketEventPayload,
    ControlPlaneWebSocketEventSource, ControlPlaneWebSocketEventType,
    ControlPlaneWebSocketExecutionWorkerSource, ControlPlaneWebSocketExecutionWorkerSourceKind,
    ControlPlaneWebSocketResetRequiredFrame, ControlPlaneWebSocketResetRequiredFrameTypeValue,
    ControlPlaneWebSocketResumeAcceptedFrame, ControlPlaneWebSocketResumeAcceptedFrameTypeValue,
    ControlPlaneWebSocketResumeFrame, ControlPlaneWebSocketServerFrame,
    ControlPlaneWebSocketSessionExecutionWorkerSource,
    ControlPlaneWebSocketSessionExecutionWorkerSourceKind, ControlPlaneWebSocketSubscribeFrame,
    ControlPlaneWebSocketSubscribeOrigin, ControlPlaneWebSocketSubscribeStartAt,
    ControlPlaneWebSocketSubscription, ControlPlaneWebSocketSubscriptionAcceptedFrame,
    ControlPlaneWebSocketSubscriptionAcceptedFrameTypeValue, ControlPlaneWebSocketTransportLimits,
    DeliveryEventReadStream, DeliveryEventReadStreamKind, EventReadCursor, EventReadStream,
    LeaseEventReadStream, LeaseEventReadStreamKind, OrganizationScope, OrganizationScopeKind,
    ProductSessionEventReadStream, ProductSessionEventReadStreamKind, ProjectScope,
    ProjectScopeKind, RepositoryScope, RepositoryScopeKind, Scope, ScopeEventReadStream,
    ScopeEventReadStreamKind, ServiceAccountActor, ServiceAccountActorKind, SystemActor,
    SystemActorKind, UserActor, UserActorKind, WorkspaceScope, WorkspaceScopeKind,
};
use winwincode_domain::{
    ControlPlaneEventId, ControlPlaneWebSocketAuthorizationEpoch,
    ControlPlaneWebSocketEventSequence, ControlPlaneWebSocketSubscriptionId, Instant,
};
use winwincode_storage::{
    OutboxEvent, ProductStateStorage, ProjectionEventStream, PublicEventActor, PublicEventScope,
    PublicEventSource, PublicProjectionEventContext, StorageErrorKind,
};

use crate::{ApiError, AuthenticatedPrincipal, EventSubscription};
use winwincode_control_plane::{EventPublishError, EventPublisher};

const DATABASE_FILE: &str = "server-event-hub.sqlite3";
const SCHEMA_VERSION: i64 = 1;
const MAX_UNACKED_EVENTS: u32 = 256;
const HARD_UNACKED_EVENTS: u32 = 1_024;
const ACK_DEADLINE_MILLIS: u32 = 30_000;
const BACKPRESSURE_CLOSE_CODE: u16 = 4_408;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS hub_meta (
  key TEXT PRIMARY KEY NOT NULL,
  value INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hub_streams (
  scope_json TEXT NOT NULL,
  stream_json TEXT NOT NULL,
  head_sequence INTEGER NOT NULL,
  head_event_id TEXT,
  floor_sequence INTEGER NOT NULL,
  floor_event_id TEXT,
  PRIMARY KEY (scope_json, stream_json)
);
CREATE TABLE IF NOT EXISTS hub_events (
  event_id TEXT PRIMARY KEY NOT NULL,
  scope_json TEXT NOT NULL,
  stream_json TEXT NOT NULL,
  sequence INTEGER NOT NULL,
  topic TEXT NOT NULL,
  event_type_json TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  occurred_at_json TEXT NOT NULL,
  source_json TEXT NOT NULL,
  UNIQUE (scope_json, stream_json, sequence)
);
CREATE TABLE IF NOT EXISTS hub_authorizations (
  subject TEXT NOT NULL,
  scope_json TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  active INTEGER NOT NULL,
  PRIMARY KEY (subject, scope_json)
);
CREATE TABLE IF NOT EXISTS hub_subscriptions (
  subject TEXT NOT NULL,
  subscription_id TEXT PRIMARY KEY NOT NULL,
  scope_json TEXT NOT NULL,
  stream_json TEXT NOT NULL,
  event_types_json TEXT NOT NULL,
  authorization_epoch INTEGER NOT NULL,
  acknowledged_sequence INTEGER NOT NULL,
  acknowledged_event_id TEXT,
  sent_sequence INTEGER NOT NULL,
  state TEXT NOT NULL,
  backpressure_sent INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS hub_events_stream
  ON hub_events (scope_json, stream_json, sequence);
CREATE INDEX IF NOT EXISTS hub_events_stream_type
  ON hub_events (scope_json, stream_json, event_type_json, sequence);
";

/// Stable event-hub failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableEventHubErrorCode {
    InvalidInput,
    Conflict,
    Unauthorized,
    CursorExpired,
    Corrupt,
    Storage,
    Closed,
}

/// Secret-free event-hub failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableEventHubError {
    code: DurableEventHubErrorCode,
    message: String,
}

impl DurableEventHubError {
    fn new(code: DurableEventHubErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn storage(context: &str, error: impl fmt::Display) -> Self {
        Self::new(
            DurableEventHubErrorCode::Storage,
            format!("{context}: {error}"),
        )
    }

    /// Stable machine-readable category.
    #[must_use]
    pub const fn code(&self) -> DurableEventHubErrorCode {
        self.code
    }

    /// Converts an internal error to the public application boundary.
    #[must_use]
    pub fn api_error(&self) -> ApiError {
        let (status, code, message) = match self.code {
            DurableEventHubErrorCode::InvalidInput => {
                (400, "INVALID_REQUEST", "event request is invalid")
            }
            DurableEventHubErrorCode::Conflict => {
                (409, "WRONG_STATE", "event subscription state conflicts")
            }
            DurableEventHubErrorCode::Unauthorized => (
                403,
                "PERMISSION_DENIED",
                "event subscription is not authorized",
            ),
            DurableEventHubErrorCode::CursorExpired => (
                409,
                "READ_CURSOR_EXPIRED",
                "event cursor is no longer retained",
            ),
            DurableEventHubErrorCode::Corrupt => (500, "INTERNAL_ERROR", "event service failed"),
            DurableEventHubErrorCode::Storage | DurableEventHubErrorCode::Closed => (
                503,
                "SERVICE_UNAVAILABLE",
                "event service is temporarily unavailable",
            ),
        };
        ApiError::new(status, code, message)
    }
}

impl fmt::Display for DurableEventHubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DurableEventHubError {}

/// Public event envelope facts that are not duplicated in an outbox payload.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedEventContext {
    pub scope: Scope,
    pub stream: EventReadStream,
    pub occurred_at: Instant,
    pub source: ControlPlaneWebSocketEventSource,
}

impl TryFrom<&PublicProjectionEventContext> for CommittedEventContext {
    type Error = DurableEventHubError;

    fn try_from(context: &PublicProjectionEventContext) -> Result<Self, Self::Error> {
        Ok(Self {
            scope: scope_from_storage(context.scope()),
            stream: stream_from_storage(context.stream()),
            occurred_at: context.occurred_at().clone(),
            source: source_from_storage(context.source()),
        })
    }
}

fn scope_from_storage(scope: &PublicEventScope) -> Scope {
    match scope {
        PublicEventScope::Organization { organization_id } => {
            Scope::OrganizationScope(OrganizationScope {
                kind: OrganizationScopeKind::Organization,
                organization_id: organization_id.clone(),
            })
        }
        PublicEventScope::Workspace {
            organization_id,
            workspace_id,
        } => Scope::WorkspaceScope(WorkspaceScope {
            kind: WorkspaceScopeKind::Workspace,
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
        }),
        PublicEventScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => Scope::ProjectScope(ProjectScope {
            kind: ProjectScopeKind::Project,
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
        }),
        PublicEventScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => Scope::RepositoryScope(RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
        }),
    }
}

fn stream_from_storage(stream: &ProjectionEventStream) -> EventReadStream {
    match stream {
        ProjectionEventStream::Scope => {
            EventReadStream::ScopeEventReadStream(ScopeEventReadStream {
                kind: ScopeEventReadStreamKind::Scope,
            })
        }
        ProjectionEventStream::Delivery(delivery_id) => {
            EventReadStream::DeliveryEventReadStream(DeliveryEventReadStream {
                kind: DeliveryEventReadStreamKind::Delivery,
                delivery_id: delivery_id.clone(),
            })
        }
        ProjectionEventStream::ProductSession(product_session_id) => {
            EventReadStream::ProductSessionEventReadStream(ProductSessionEventReadStream {
                kind: ProductSessionEventReadStreamKind::ProductSession,
                product_session_id: product_session_id.clone(),
            })
        }
        ProjectionEventStream::Lease {
            worker_id,
            lease_id,
        } => EventReadStream::LeaseEventReadStream(LeaseEventReadStream {
            kind: LeaseEventReadStreamKind::Lease,
            worker_id: worker_id.clone(),
            lease_id: lease_id.clone(),
        }),
    }
}

fn actor_from_storage(actor: &PublicEventActor) -> Actor {
    match actor {
        PublicEventActor::User { id } => Actor::UserActor(UserActor {
            kind: UserActorKind::User,
            id: id.clone(),
        }),
        PublicEventActor::ServiceAccount { id } => {
            Actor::ServiceAccountActor(ServiceAccountActor {
                kind: ServiceAccountActorKind::ServiceAccount,
                id: id.clone(),
            })
        }
        PublicEventActor::System { id } => Actor::SystemActor(SystemActor {
            kind: SystemActorKind::System,
            id: id.clone(),
        }),
    }
}

fn source_from_storage(source: &PublicEventSource) -> ControlPlaneWebSocketEventSource {
    match source {
        PublicEventSource::ControlPlane { actor, component } => {
            ControlPlaneWebSocketEventSource::ControlPlaneWebSocketControlPlaneSource(
                ControlPlaneWebSocketControlPlaneSource {
                    actor: actor_from_storage(actor),
                    component: component.clone(),
                    kind: ControlPlaneWebSocketControlPlaneSourceKind::ControlPlane,
                },
            )
        }
        PublicEventSource::ExecutionWorker {
            worker_id,
            worker_session_id,
            lease_id,
            codex_thread_id,
        } => ControlPlaneWebSocketEventSource::ControlPlaneWebSocketExecutionWorkerSource(
            ControlPlaneWebSocketExecutionWorkerSource {
                worker_id: worker_id.clone(),
                worker_session_id: worker_session_id.clone(),
                lease_id: lease_id.clone(),
                codex_thread_id: codex_thread_id.clone(),
                kind: ControlPlaneWebSocketExecutionWorkerSourceKind::ExecutionWorker,
            },
        ),
        PublicEventSource::SessionExecutionWorker {
            worker_id,
            worker_session_id,
            lease_id,
            codex_thread_id,
            session_identity,
        } => ControlPlaneWebSocketEventSource::ControlPlaneWebSocketSessionExecutionWorkerSource(
            ControlPlaneWebSocketSessionExecutionWorkerSource {
                worker_id: worker_id.clone(),
                worker_session_id: worker_session_id.clone(),
                lease_id: lease_id.clone(),
                codex_thread_id: codex_thread_id.clone(),
                session_identity: session_identity.clone(),
                kind: ControlPlaneWebSocketSessionExecutionWorkerSourceKind::ExecutionWorker,
            },
        ),
    }
}

/// Millisecond clock used to derive the generated backpressure deadline.
pub trait DurableEventHubClock: Send + Sync {
    #[must_use]
    fn now_millis(&self) -> u64;
}

struct SystemDurableEventHubClock;

impl DurableEventHubClock for SystemDurableEventHubClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

/// Control Plane outbox publisher that writes public events into one shared
/// durable Server hub before the Control Plane acknowledges the outbox row.
pub struct DurableEventPublisher {
    hub: Arc<DurableEventHub>,
}

impl DurableEventPublisher {
    #[must_use]
    pub const fn new(hub: Arc<DurableEventHub>) -> Self {
        Self { hub }
    }
}

impl EventPublisher for DurableEventPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.hub
            .publish_committed(event)
            .map(|_| ())
            .map_err(|error| EventPublishError::new(error.to_string()))
    }
}

/// Bounded transport limits emitted in generated acceptance/backpressure frames.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableEventHubConfig {
    pub max_unacked_events: u32,
    pub hard_unacked_events: u32,
    pub ack_deadline_millis: u32,
    pub backpressure_close_code: u16,
}

impl DurableEventHubConfig {
    fn validate(&self) -> Result<(), DurableEventHubError> {
        if self.max_unacked_events != MAX_UNACKED_EVENTS
            || self.hard_unacked_events != HARD_UNACKED_EVENTS
            || self.ack_deadline_millis != ACK_DEADLINE_MILLIS
            || self.backpressure_close_code != BACKPRESSURE_CLOSE_CODE
        {
            return Err(DurableEventHubError::new(
                DurableEventHubErrorCode::InvalidInput,
                "event-hub transport limits are invalid",
            ));
        }
        Ok(())
    }
}

impl Default for DurableEventHubConfig {
    fn default() -> Self {
        Self {
            max_unacked_events: MAX_UNACKED_EVENTS,
            hard_unacked_events: HARD_UNACKED_EVENTS,
            ack_deadline_millis: ACK_DEADLINE_MILLIS,
            backpressure_close_code: BACKPRESSURE_CLOSE_CODE,
        }
    }
}

/// SQLite-backed event publisher and subscription cursor owner.
pub struct DurableEventHub {
    database_path: PathBuf,
    connection: Mutex<Option<Connection>>,
    live: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    config: DurableEventHubConfig,
    clock: Arc<dyn DurableEventHubClock>,
}

impl DurableEventHub {
    /// Opens or recovers the durable Server event catalog.
    ///
    /// # Errors
    ///
    /// Rejects invalid limits, schema mismatches, and database failures.
    pub fn open(
        directory: impl AsRef<Path>,
        config: DurableEventHubConfig,
    ) -> Result<Self, DurableEventHubError> {
        Self::open_with_clock(directory, config, Arc::new(SystemDurableEventHubClock))
    }

    /// Opens the durable catalog with an explicit clock for deterministic
    /// deadline calculation.
    ///
    /// # Errors
    ///
    /// Rejects invalid limits, schema mismatches, and database failures.
    pub fn open_with_clock(
        directory: impl AsRef<Path>,
        config: DurableEventHubConfig,
        clock: Arc<dyn DurableEventHubClock>,
    ) -> Result<Self, DurableEventHubError> {
        config.validate()?;
        fs::create_dir_all(directory.as_ref())
            .map_err(|error| DurableEventHubError::storage("event-hub directory", error))?;
        let database_path = directory.as_ref().join(DATABASE_FILE);
        let connection = Connection::open(&database_path)
            .map_err(|error| DurableEventHubError::storage("event-hub database", error))?;
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|error| DurableEventHubError::storage("event-hub pragmas", error))?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|error| DurableEventHubError::storage("event-hub schema", error))?;
        initialize_schema_version(&connection)?;
        Ok(Self {
            database_path,
            connection: Mutex::new(Some(connection)),
            live: Mutex::new(HashMap::new()),
            config,
            clock,
        })
    }

    /// Path of the durable event catalog.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Records the current authorization epoch for one principal and scope.
    ///
    /// # Errors
    ///
    /// Rejects non-positive or regressing epochs.
    pub fn grant_authorization(
        &self,
        principal: &AuthenticatedPrincipal,
        scope: &Scope,
        epoch: &ControlPlaneWebSocketAuthorizationEpoch,
    ) -> Result<(), DurableEventHubError> {
        if epoch.0 <= 0 {
            return Err(invalid("authorization epoch must be positive"));
        }
        let scope_json = canonical_json(scope, "authorization scope")?;
        let connection = self.connection()?;
        let current = connection
            .query_row(
                "SELECT epoch FROM hub_authorizations WHERE subject=?1 AND scope_json=?2",
                params![principal.subject(), scope_json],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| DurableEventHubError::storage("authorization lookup", error))?;
        if current.is_some_and(|current| epoch.0 < current) {
            return Err(conflict("authorization epoch cannot regress"));
        }
        connection
            .execute(
                "INSERT INTO hub_authorizations(subject,scope_json,epoch,active) VALUES(?1,?2,?3,1) \
                 ON CONFLICT(subject,scope_json) DO UPDATE SET epoch=excluded.epoch,active=1",
                params![principal.subject(), scope_json, epoch.0],
            )
            .map_err(|error| DurableEventHubError::storage("authorization write", error))?;
        Ok(())
    }

    /// Revokes a scope, durably closes matching subscriptions, and returns
    /// generated revocation frames for the live connections.
    ///
    /// # Errors
    ///
    /// Rejects stale epochs and database failures.
    pub fn revoke_authorization(
        &self,
        principal: &AuthenticatedPrincipal,
        scope: &Scope,
        epoch: &ControlPlaneWebSocketAuthorizationEpoch,
    ) -> Result<Vec<Value>, DurableEventHubError> {
        let scope_json = canonical_json(scope, "authorization scope")?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("revocation transaction", error))?;
        let current = authorization_epoch(&transaction, principal.subject(), &scope_json)?;
        if epoch.0 <= current {
            return Err(conflict("authorization revocation epoch must advance"));
        }
        transaction
            .execute(
                "INSERT INTO hub_authorizations(subject,scope_json,epoch,active) VALUES(?1,?2,?3,0) \
                 ON CONFLICT(subject,scope_json) DO UPDATE SET epoch=excluded.epoch,active=0",
                params![principal.subject(), scope_json, epoch.0],
            )
            .map_err(|error| DurableEventHubError::storage("revocation write", error))?;
        let subscription_ids = query_strings(
            &transaction,
            "SELECT subscription_id FROM hub_subscriptions \
             WHERE subject=?1 AND scope_json=?2 AND state='active' ORDER BY subscription_id",
            params![principal.subject(), scope_json],
        )?;
        transaction
            .execute(
                "UPDATE hub_subscriptions SET state='revoked',authorization_epoch=?3 \
                 WHERE subject=?1 AND scope_json=?2 AND state='active'",
                params![principal.subject(), scope_json, epoch.0],
            )
            .map_err(|error| DurableEventHubError::storage("subscription revocation", error))?;
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("revocation commit", error))?;
        let frames = subscription_ids
            .iter()
            .map(|subscription_id| revocation_frame(subscription_id, epoch.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        self.send_live_frames(&subscription_ids, &frames);
        Ok(frames)
    }

    /// Copies public projection rows to the durable event catalog and drains
    /// internal rows without exposing them on the public WebSocket transport.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid generated public payloads or source/catalog
    /// storage failures.
    pub fn publish_pending(
        &self,
        storage: &mut dyn ProductStateStorage,
    ) -> Result<usize, DurableEventHubError> {
        let pending = storage
            .pending_events()
            .map_err(|error| DurableEventHubError::storage("durable outbox read", error))?;
        let mut inserted = 0_usize;
        for event in pending {
            inserted += usize::from(self.publish_committed(&event)?);
            storage
                .mark_published(&event.event_id)
                .map_err(|error| DurableEventHubError::storage("outbox publish marker", error))?;
        }
        Ok(inserted)
    }

    /// Persists and fans out one already committed outbox row. Internal rows
    /// are accepted but never become public frames.
    ///
    /// # Errors
    ///
    /// Rejects malformed public rows before they can be acknowledged by the
    /// Control Plane outbox owner.
    pub fn publish_committed(&self, event: &OutboxEvent) -> Result<bool, DurableEventHubError> {
        if event.projection_cursor.is_none() {
            return Ok(false);
        }
        let context = event
            .public_context
            .as_ref()
            .ok_or_else(|| corrupt("public event is missing its durable context"))?
            .try_into()?;
        let inserted = self.persist_event(event, &context)?;
        self.fan_out(&event.event_id)?;
        Ok(inserted)
    }

    /// Accepts one generated subscribe/resume frame and creates its bounded
    /// live channel after durable cursor and authorization checks.
    ///
    /// # Errors
    ///
    /// Rejects wrong-state frames, foreign scopes, duplicate identities, and
    /// invalid cursors.
    pub fn subscribe(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<EventSubscription, ApiError> {
        match frame {
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(frame) => self
                .subscribe_new(principal, &frame)
                .map_err(|error| error.api_error()),
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(frame) => self
                .resume(principal, &frame)
                .map_err(|error| error.api_error()),
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(_)
            | ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketPongFrame(_) => {
                Err(ApiError::new(
                    400,
                    "INVALID_REQUEST",
                    "first frame must subscribe or resume",
                ))
            }
        }
    }

    /// Applies one generated acknowledgement or pong frame.
    ///
    /// # Errors
    ///
    /// Rejects cross-stream, beyond-sent, unauthorized, and unknown cursors.
    pub fn event_control(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<Vec<Value>, ApiError> {
        match frame {
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(frame) => self
                .acknowledge(principal, &frame)
                .map_err(|error| error.api_error()),
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketPongFrame(_) => Ok(Vec::new()),
            ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(_)
            | ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(_) => {
                Err(ApiError::new(
                    409,
                    "WRONG_STATE",
                    "subscribe and resume require a new connection",
                ))
            }
        }
    }

    /// Advances retention while keeping one exact floor cursor for reset.
    ///
    /// # Errors
    ///
    /// Rejects a floor beyond the head or a missing floor event identity.
    pub fn retain_from(
        &self,
        scope: &Scope,
        stream: &EventReadStream,
        earliest_sequence: u64,
    ) -> Result<(), DurableEventHubError> {
        let scope_json = canonical_json(scope, "retention scope")?;
        let stream_json = canonical_json(stream, "retention stream")?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("retention transaction", error))?;
        let state = stream_state_or_empty(&transaction, &scope_json, &stream_json)?;
        let floor = earliest_sequence.saturating_sub(1);
        if floor < state.floor_sequence || floor > state.head_sequence {
            return Err(invalid("retention floor is outside the durable stream"));
        }
        let floor_event_id = event_id_at(&transaction, &scope_json, &stream_json, floor)?;
        if floor > 0 && floor_event_id.is_none() {
            return Err(corrupt("retention floor event is unavailable"));
        }
        transaction
            .execute(
                "DELETE FROM hub_events WHERE scope_json=?1 AND stream_json=?2 AND sequence < ?3",
                params![
                    scope_json,
                    stream_json,
                    to_i64(earliest_sequence, "retention sequence")?
                ],
            )
            .map_err(|error| DurableEventHubError::storage("retention delete", error))?;
        transaction
            .execute(
                "UPDATE hub_streams SET floor_sequence=?3,floor_event_id=?4 \
                 WHERE scope_json=?1 AND stream_json=?2",
                params![
                    scope_json,
                    stream_json,
                    to_i64(floor, "retention floor")?,
                    floor_event_id
                ],
            )
            .map_err(|error| DurableEventHubError::storage("retention floor write", error))?;
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("retention commit", error))?;
        Ok(())
    }

    /// Checkpoints and closes the durable catalog.
    ///
    /// # Errors
    ///
    /// Returns a database failure if checkpoint or close fails.
    pub fn close(&self) -> Result<(), DurableEventHubError> {
        self.live
            .lock()
            .map_err(|_| corrupt("live subscription lock is poisoned"))?
            .clear();
        let connection = self
            .connection
            .lock()
            .map_err(|_| corrupt("event-hub database lock is poisoned"))?
            .take()
            .ok_or_else(|| {
                DurableEventHubError::new(DurableEventHubErrorCode::Closed, "event hub is closed")
            })?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|error| DurableEventHubError::storage("event-hub checkpoint", error))?;
        connection
            .close()
            .map_err(|(_, error)| DurableEventHubError::storage("event-hub close", error))?;
        Ok(())
    }

    fn connection(&self) -> Result<ConnectionGuard<'_>, DurableEventHubError> {
        let guard = self
            .connection
            .lock()
            .map_err(|_| corrupt("event-hub database lock is poisoned"))?;
        if guard.is_none() {
            return Err(DurableEventHubError::new(
                DurableEventHubErrorCode::Closed,
                "event hub is closed",
            ));
        }
        Ok(ConnectionGuard(guard))
    }

    fn persist_event(
        &self,
        event: &OutboxEvent,
        context: &CommittedEventContext,
    ) -> Result<bool, DurableEventHubError> {
        let cursor = event
            .projection_cursor
            .as_ref()
            .ok_or_else(|| invalid("public event must own a projection cursor"))?;
        validate_storage_stream(cursor.key().stream(), &context.stream)?;
        let payload: ControlPlaneWebSocketEventPayload = serde_json::from_slice(&event.payload)
            .map_err(|_| invalid("outbox payload is not a generated public event"))?;
        let event_type = event_type(&payload)?;
        let scope_json = canonical_json(&context.scope, "event scope")?;
        let stream_json = canonical_json(&context.stream, "event stream")?;
        let event_type_json = canonical_json(&event_type, "event type")?;
        let payload_json = canonical_json(&payload, "event payload")?;
        let occurred_at_json = canonical_json(&context.occurred_at, "event time")?;
        let source_json = canonical_json(&context.source, "event source")?;
        let sequence = to_i64(cursor.sequence(), "event sequence")?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("event publish transaction", error))?;
        let existing = stored_event(&transaction, &event.event_id)?;
        if let Some(existing) = existing {
            let expected = StoredEvent::new(
                &event.event_id,
                &scope_json,
                &stream_json,
                sequence,
                &event.topic,
                &event_type_json,
                &payload_json,
                &occurred_at_json,
                &source_json,
            );
            if existing != expected {
                return Err(conflict(
                    "event id was already published with different facts",
                ));
            }
            transaction
                .commit()
                .map_err(|error| DurableEventHubError::storage("event replay commit", error))?;
            return Ok(false);
        }
        let state = stream_state_or_empty(&transaction, &scope_json, &stream_json)?;
        if cursor.sequence() != state.head_sequence.saturating_add(1) {
            return Err(conflict(
                "event sequence is not contiguous with the durable stream",
            ));
        }
        transaction
            .execute(
                "INSERT INTO hub_events(event_id,scope_json,stream_json,sequence,topic,event_type_json,\
                 payload_json,occurred_at_json,source_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![event.event_id, scope_json, stream_json, sequence, event.topic,
                    event_type_json, payload_json, occurred_at_json, source_json],
            )
            .map_err(|error| DurableEventHubError::storage("event catalog insert", error))?;
        transaction
            .execute(
                "INSERT INTO hub_streams(scope_json,stream_json,head_sequence,head_event_id,\
                 floor_sequence,floor_event_id) VALUES(?1,?2,?3,?4,0,NULL) \
                 ON CONFLICT(scope_json,stream_json) DO UPDATE SET \
                 head_sequence=excluded.head_sequence,head_event_id=excluded.head_event_id",
                params![scope_json, stream_json, sequence, event.event_id],
            )
            .map_err(|error| DurableEventHubError::storage("event stream head", error))?;
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("event publish commit", error))?;
        Ok(true)
    }

    fn subscribe_new(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: &ControlPlaneWebSocketSubscribeFrame,
    ) -> Result<EventSubscription, DurableEventHubError> {
        validate_subscription_id(&frame.subscription_id)?;
        validate_subscription(&frame.subscription)?;
        let scope_json = canonical_json(&frame.subscription.scope, "subscription scope")?;
        let stream_json = canonical_json(&frame.subscription.stream, "subscription stream")?;
        let event_types_json = canonical_json(&frame.subscription.event_types, "event types")?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("subscribe transaction", error))?;
        let epoch = require_authorization(&transaction, principal.subject(), &scope_json)?;
        let state = stream_state_or_empty(&transaction, &scope_json, &stream_json)?;
        let baseline = subscribe_baseline(
            &transaction,
            &scope_json,
            &stream_json,
            &frame.start_at,
            &state,
        )?;
        let initial = match baseline {
            Baseline::Expired => {
                let reset = reset_frame(
                    &frame.subscription_id,
                    cursor(
                        &frame.subscription.scope,
                        &frame.subscription.stream,
                        state.floor_sequence,
                        state.floor_event_id.clone(),
                    )?,
                )?;
                return Ok(Self::closed_subscription(vec![reset]));
            }
            Baseline::Cursor(cursor) => cursor,
        };
        transaction
            .execute(
                "INSERT INTO hub_subscriptions(subject,subscription_id,scope_json,stream_json,\
                 event_types_json,authorization_epoch,acknowledged_sequence,acknowledged_event_id,\
                 sent_sequence,state,backpressure_sent) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?7,'active',0)",
                params![principal.subject(), frame.subscription_id.0, scope_json, stream_json,
                    event_types_json, epoch, to_i64(initial.sequence, "subscription baseline")?, initial.event_id],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE") {
                    conflict("subscription id is already in use")
                } else {
                    DurableEventHubError::storage("subscription insert", error)
                }
            })?;
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("subscribe commit", error))?;
        drop(connection);
        let accepted = subscription_accepted(
            &frame.subscription_id,
            cursor(
                &frame.subscription.scope,
                &frame.subscription.stream,
                initial.sequence,
                initial.event_id,
            )?,
            ControlPlaneWebSocketAuthorizationEpoch(epoch),
            &self.config,
        )?;
        self.open_live(
            principal,
            &frame.subscription_id,
            &frame.subscription,
            vec![accepted],
        )
    }

    fn resume(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: &ControlPlaneWebSocketResumeFrame,
    ) -> Result<EventSubscription, DurableEventHubError> {
        validate_subscription_id(&frame.subscription_id)?;
        validate_subscription(&frame.subscription)?;
        let scope_json = canonical_json(&frame.subscription.scope, "resume scope")?;
        let stream_json = canonical_json(&frame.subscription.stream, "resume stream")?;
        let event_types_json =
            canonical_json(&frame.subscription.event_types, "resume event types")?;
        let requested = parsed_ack_cursor(&frame.after)?;
        if requested.scope_json != scope_json || requested.stream_json != stream_json {
            return Err(invalid("resume cursor belongs to another scope or stream"));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("resume transaction", error))?;
        let epoch = require_authorization(&transaction, principal.subject(), &scope_json)?;
        let state = stream_state_or_empty(&transaction, &scope_json, &stream_json)?;
        if !cursor_is_retained(&transaction, &scope_json, &stream_json, &requested, &state)? {
            let reset = reset_frame(
                &frame.subscription_id,
                cursor(
                    &frame.subscription.scope,
                    &frame.subscription.stream,
                    state.floor_sequence,
                    state.floor_event_id.clone(),
                )?,
            )?;
            return Ok(Self::closed_subscription(vec![reset]));
        }
        let stored = stored_subscription(&transaction, &frame.subscription_id.0)?;
        let (baseline, baseline_event_id) = if let Some(stored) = stored {
            if stored.subject != principal.subject()
                || stored.scope_json != scope_json
                || stored.stream_json != stream_json
                || stored.event_types_json != event_types_json
                || stored.state == "revoked"
            {
                return Err(DurableEventHubError::new(
                    DurableEventHubErrorCode::Unauthorized,
                    "subscription identity no longer has the same authority",
                ));
            }
            if requested.sequence >= stored.acknowledged_sequence {
                (requested.sequence, requested.event_id.clone())
            } else {
                (
                    stored.acknowledged_sequence,
                    stored.acknowledged_event_id.clone(),
                )
            }
        } else {
            (requested.sequence, requested.event_id.clone())
        };
        transaction
            .execute(
                "INSERT INTO hub_subscriptions(subject,subscription_id,scope_json,stream_json,event_types_json,\
                 authorization_epoch,acknowledged_sequence,acknowledged_event_id,sent_sequence,state,backpressure_sent)\
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?7,'active',0) ON CONFLICT(subscription_id) DO UPDATE SET \
                 authorization_epoch=excluded.authorization_epoch,acknowledged_sequence=excluded.acknowledged_sequence, \
                 acknowledged_event_id=excluded.acknowledged_event_id,sent_sequence=excluded.sent_sequence,state='active',backpressure_sent=0",
                params![principal.subject(), frame.subscription_id.0, scope_json, stream_json,
                    event_types_json, epoch, to_i64(baseline, "resume baseline")?, baseline_event_id],
            )
            .map_err(|error| DurableEventHubError::storage("resume cursor write", error))?;
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("resume commit", error))?;
        drop(connection);
        let replay_through = cursor(
            &frame.subscription.scope,
            &frame.subscription.stream,
            state.head_sequence,
            state.head_event_id,
        )?;
        let accepted = typed_value(
            &ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketResumeAcceptedFrame(
                ControlPlaneWebSocketResumeAcceptedFrame {
                    after: frame.after.clone(),
                    authorization_epoch: ControlPlaneWebSocketAuthorizationEpoch(epoch),
                    replay_through,
                    subscription_id: frame.subscription_id.clone(),
                    type_value:
                        ControlPlaneWebSocketResumeAcceptedFrameTypeValue::TransportResumeAcceptedV1,
                },
            ),
        )?;
        self.open_live(
            principal,
            &frame.subscription_id,
            &frame.subscription,
            vec![accepted],
        )
    }

    fn open_live(
        &self,
        principal: &AuthenticatedPrincipal,
        subscription_id: &ControlPlaneWebSocketSubscriptionId,
        subscription: &ControlPlaneWebSocketSubscription,
        mut initial_frames: Vec<Value>,
    ) -> Result<EventSubscription, DurableEventHubError> {
        let replay = self.replay_frames(principal, subscription_id, subscription)?;
        initial_frames.extend(replay);
        let capacity = usize::try_from(self.config.max_unacked_events)
            .map_err(|_| invalid("event channel capacity is unsupported"))?;
        let (sender, receiver) = mpsc::channel(capacity);
        self.live
            .lock()
            .map_err(|_| corrupt("live subscription lock is poisoned"))?
            .insert(subscription_id.0.clone(), sender);
        Ok(EventSubscription {
            initial_frames,
            events: receiver,
        })
    }

    fn closed_subscription(initial_frames: Vec<Value>) -> EventSubscription {
        let (_sender, receiver) = mpsc::channel(1);
        EventSubscription {
            initial_frames,
            events: receiver,
        }
    }

    fn replay_frames(
        &self,
        principal: &AuthenticatedPrincipal,
        subscription_id: &ControlPlaneWebSocketSubscriptionId,
        subscription: &ControlPlaneWebSocketSubscription,
    ) -> Result<Vec<Value>, DurableEventHubError> {
        let scope_json = canonical_json(&subscription.scope, "replay scope")?;
        let stream_json = canonical_json(&subscription.stream, "replay stream")?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("replay transaction", error))?;
        let stored = stored_subscription(&transaction, &subscription_id.0)?
            .ok_or_else(|| corrupt("durable subscription disappeared"))?;
        let epoch = require_authorization(&transaction, principal.subject(), &scope_json)?;
        let outstanding = matching_event_count(
            &transaction,
            &scope_json,
            &stream_json,
            stored.acknowledged_sequence,
            stored.sent_sequence,
            &stored.event_types,
        )?;
        let available = u64::from(self.config.max_unacked_events).saturating_sub(outstanding);
        let events = events_after_matching(
            &transaction,
            &scope_json,
            &stream_json,
            stored.sent_sequence,
            available,
            &stored.event_types,
        )?;
        let mut frames = Vec::new();
        let mut sent = stored.sent_sequence;
        for event in events {
            frames.push(event_frame(subscription_id, epoch, &event)?);
            sent = event.sequence;
        }
        transaction
            .execute(
                "UPDATE hub_subscriptions SET sent_sequence=?2,authorization_epoch=?3 \
                 WHERE subscription_id=?1",
                params![subscription_id.0, to_i64(sent, "sent sequence")?, epoch],
            )
            .map_err(|error| DurableEventHubError::storage("replay cursor write", error))?;
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("replay commit", error))?;
        Ok(frames)
    }

    fn acknowledge(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: &ControlPlaneWebSocketAckFrame,
    ) -> Result<Vec<Value>, DurableEventHubError> {
        let acknowledged = parsed_ack_cursor(&frame.cursor)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DurableEventHubError::storage("ack transaction", error))?;
        let stored = stored_subscription(&transaction, &frame.subscription_id.0)?
            .ok_or_else(|| invalid("subscription does not exist"))?;
        if stored.subject != principal.subject()
            || stored.scope_json != acknowledged.scope_json
            || stored.stream_json != acknowledged.stream_json
            || stored.state != "active"
        {
            return Err(DurableEventHubError::new(
                DurableEventHubErrorCode::Unauthorized,
                "acknowledgement does not belong to the active subscription",
            ));
        }
        require_authorization(&transaction, principal.subject(), &stored.scope_json)?;
        if acknowledged.sequence > stored.sent_sequence {
            return Err(invalid("acknowledgement is beyond the sent cursor"));
        }
        let state = stream_state_or_empty(&transaction, &stored.scope_json, &stored.stream_json)?;
        if !cursor_is_retained(
            &transaction,
            &stored.scope_json,
            &stored.stream_json,
            &acknowledged,
            &state,
        )? {
            return Err(DurableEventHubError::new(
                DurableEventHubErrorCode::CursorExpired,
                "acknowledgement cursor is no longer retained",
            ));
        }
        if acknowledged.sequence > stored.acknowledged_sequence {
            transaction
                .execute(
                    "UPDATE hub_subscriptions SET acknowledged_sequence=?2,acknowledged_event_id=?3,\
                     backpressure_sent=0 WHERE subscription_id=?1",
                    params![frame.subscription_id.0, to_i64(acknowledged.sequence, "ack sequence")?, acknowledged.event_id],
                )
                .map_err(|error| DurableEventHubError::storage("ack cursor write", error))?;
        }
        transaction
            .commit()
            .map_err(|error| DurableEventHubError::storage("ack commit", error))?;
        drop(connection);
        let subscription = ControlPlaneWebSocketSubscription {
            event_types: stored.event_types,
            scope: decode_json(&stored.scope_json, "ack subscription scope")?,
            stream: decode_json(&stored.stream_json, "ack subscription stream")?,
        };
        self.replay_frames(principal, &frame.subscription_id, &subscription)
    }

    fn fan_out(&self, event_id: &str) -> Result<(), DurableEventHubError> {
        let connection = self.connection()?;
        let event = stored_event(&connection, event_id)?
            .ok_or_else(|| corrupt("published event disappeared"))?
            .into_public()?;
        let subscriptions = subscriptions_for_event(&connection, &event)?;
        drop(connection);
        for subscription in subscriptions {
            let sender = self
                .live
                .lock()
                .map_err(|_| corrupt("live subscription lock is poisoned"))?
                .get(&subscription.subscription_id)
                .cloned();
            let Some(sender) = sender else {
                continue;
            };
            let connection = self.connection()?;
            let pending = matching_event_count(
                &connection,
                &subscription.scope_json,
                &subscription.stream_json,
                subscription.acknowledged_sequence,
                event.sequence,
                &subscription.event_types,
            )?;
            if subscription.backpressure_sent {
                drop(connection);
                if pending >= u64::from(self.config.hard_unacked_events) {
                    self.remove_live_sender(&subscription.subscription_id, &sender)?;
                }
                continue;
            }
            let backpressured = pending > u64::from(self.config.max_unacked_events);
            let frame = if backpressured {
                let ack_required_through = public_event_at(
                    &connection,
                    &subscription.scope_json,
                    &subscription.stream_json,
                    subscription.sent_sequence,
                )?
                .ok_or_else(|| corrupt("backpressure sent cursor is unavailable"))?;
                let disconnect_at = instant_from_millis(
                    self.clock
                        .now_millis()
                        .checked_add(u64::from(self.config.ack_deadline_millis))
                        .ok_or_else(|| invalid("backpressure deadline is out of range"))?,
                )?;
                backpressure_frame(
                    &subscription,
                    &ack_required_through,
                    &disconnect_at,
                    pending,
                    &self.config,
                )?
            } else {
                event_frame(
                    &ControlPlaneWebSocketSubscriptionId(subscription.subscription_id.clone()),
                    subscription.authorization_epoch,
                    &event,
                )?
            };
            if backpressured {
                connection
                    .execute(
                        "UPDATE hub_subscriptions SET backpressure_sent=1 WHERE subscription_id=?1",
                        [&subscription.subscription_id],
                    )
                    .map_err(|error| DurableEventHubError::storage("backpressure cursor", error))?;
            } else {
                connection
                    .execute(
                        "UPDATE hub_subscriptions SET sent_sequence=?2,authorization_epoch=?3 \
                         WHERE subscription_id=?1",
                        params![
                            subscription.subscription_id,
                            to_i64(event.sequence, "live sent sequence")?,
                            subscription.authorization_epoch
                        ],
                    )
                    .map_err(|error| DurableEventHubError::storage("live sent cursor", error))?;
            }
            drop(connection);
            let send_failed = sender.try_send(frame).is_err();
            if send_failed || pending >= u64::from(self.config.hard_unacked_events) {
                self.remove_live_sender(&subscription.subscription_id, &sender)?;
            }
        }
        Ok(())
    }

    fn send_live_frames(&self, subscription_ids: &[String], frames: &[Value]) {
        if let Ok(mut live) = self.live.lock() {
            for (subscription_id, frame) in subscription_ids.iter().zip(frames) {
                if let Some(sender) = live.remove(subscription_id) {
                    let _ = sender.try_send(frame.clone());
                }
            }
        }
    }

    fn remove_live_sender(
        &self,
        subscription_id: &str,
        expected: &mpsc::Sender<Value>,
    ) -> Result<(), DurableEventHubError> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| corrupt("live subscription lock is poisoned"))?;
        if live
            .get(subscription_id)
            .is_some_and(|current| current.same_channel(expected))
        {
            live.remove(subscription_id);
        }
        Ok(())
    }
}

struct ConnectionGuard<'a>(MutexGuard<'a, Option<Connection>>);

impl Deref for ConnectionGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("connection presence was checked before guard construction")
    }
}

impl DerefMut for ConnectionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_mut()
            .expect("connection presence was checked before guard construction")
    }
}

#[derive(Clone, Debug)]
struct CursorParts {
    scope_json: String,
    stream_json: String,
    sequence: u64,
    event_id: Option<String>,
}

#[derive(Clone, Debug)]
struct StreamState {
    head_sequence: u64,
    head_event_id: Option<String>,
    floor_sequence: u64,
    floor_event_id: Option<String>,
}

enum Baseline {
    Cursor(CursorParts),
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredEvent {
    event_id: String,
    scope_json: String,
    stream_json: String,
    sequence: i64,
    topic: String,
    event_type_json: String,
    payload_json: String,
    occurred_at_json: String,
    source_json: String,
}

impl StoredEvent {
    #[allow(clippy::too_many_arguments)]
    fn new(
        event_id: &str,
        scope_json: &str,
        stream_json: &str,
        sequence: i64,
        topic: &str,
        event_type_json: &str,
        payload_json: &str,
        occurred_at_json: &str,
        source_json: &str,
    ) -> Self {
        Self {
            event_id: event_id.to_owned(),
            scope_json: scope_json.to_owned(),
            stream_json: stream_json.to_owned(),
            sequence,
            topic: topic.to_owned(),
            event_type_json: event_type_json.to_owned(),
            payload_json: payload_json.to_owned(),
            occurred_at_json: occurred_at_json.to_owned(),
            source_json: source_json.to_owned(),
        }
    }

    fn into_public(self) -> Result<PublicEvent, DurableEventHubError> {
        Ok(PublicEvent {
            event_id: ControlPlaneEventId(self.event_id),
            scope: decode_json(&self.scope_json, "stored event scope")?,
            stream: decode_json(&self.stream_json, "stored event stream")?,
            sequence: u64::try_from(self.sequence)
                .map_err(|_| corrupt("stored event sequence is negative"))?,
            event_type: decode_json(&self.event_type_json, "stored event type")?,
            payload: decode_json(&self.payload_json, "stored event payload")?,
            occurred_at: decode_json(&self.occurred_at_json, "stored event time")?,
            source: decode_json(&self.source_json, "stored event source")?,
        })
    }
}

struct PublicEvent {
    event_id: ControlPlaneEventId,
    scope: Scope,
    stream: EventReadStream,
    sequence: u64,
    event_type: ControlPlaneWebSocketEventType,
    payload: ControlPlaneWebSocketEventPayload,
    occurred_at: Instant,
    source: ControlPlaneWebSocketEventSource,
}

struct StoredSubscription {
    subject: String,
    subscription_id: String,
    scope_json: String,
    stream_json: String,
    event_types_json: String,
    event_types: Vec<ControlPlaneWebSocketEventType>,
    authorization_epoch: i64,
    acknowledged_sequence: u64,
    acknowledged_event_id: Option<String>,
    sent_sequence: u64,
    state: String,
    backpressure_sent: bool,
}

fn initialize_schema_version(connection: &Connection) -> Result<(), DurableEventHubError> {
    let current = connection
        .query_row(
            "SELECT value FROM hub_meta WHERE key='schema_version'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("event-hub version read", error))?;
    match current {
        None => connection
            .execute(
                "INSERT INTO hub_meta(key,value) VALUES('schema_version',?1)",
                [SCHEMA_VERSION],
            )
            .map(|_| ())
            .map_err(|error| DurableEventHubError::storage("event-hub version write", error)),
        Some(SCHEMA_VERSION) => Ok(()),
        Some(_) => Err(corrupt("event-hub schema version is unsupported")),
    }
}

fn validate_storage_stream(
    stored: &ProjectionEventStream,
    public: &EventReadStream,
) -> Result<(), DurableEventHubError> {
    let matches = match (stored, public) {
        (
            ProjectionEventStream::Scope,
            EventReadStream::ScopeEventReadStream(ScopeEventReadStream {
                kind: ScopeEventReadStreamKind::Scope,
            }),
        ) => true,
        (
            ProjectionEventStream::Delivery(stored),
            EventReadStream::DeliveryEventReadStream(public),
        ) => stored == &public.delivery_id,
        (
            ProjectionEventStream::ProductSession(stored),
            EventReadStream::ProductSessionEventReadStream(public),
        ) => stored == &public.product_session_id,
        (
            ProjectionEventStream::Lease {
                worker_id,
                lease_id,
            },
            EventReadStream::LeaseEventReadStream(public),
        ) => worker_id == &public.worker_id && lease_id == &public.lease_id,
        _ => false,
    };
    if !matches {
        return Err(invalid(
            "outbox projection stream differs from the public event stream",
        ));
    }
    Ok(())
}

fn event_type(
    payload: &ControlPlaneWebSocketEventPayload,
) -> Result<ControlPlaneWebSocketEventType, DurableEventHubError> {
    let value = serde_json::to_value(payload)
        .map_err(|error| DurableEventHubError::storage("event payload", error))?;
    serde_json::from_value(
        value
            .get("type")
            .cloned()
            .ok_or_else(|| invalid("event payload has no generated type"))?,
    )
    .map_err(|_| invalid("event payload type is unsupported"))
}

fn validate_subscription_id(
    id: &ControlPlaneWebSocketSubscriptionId,
) -> Result<(), DurableEventHubError> {
    if !id.0.starts_with("sub_")
        || id.0.len() > 200
        || id.0.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("subscription id is invalid"));
    }
    Ok(())
}

fn validate_subscription(
    subscription: &ControlPlaneWebSocketSubscription,
) -> Result<(), DurableEventHubError> {
    if subscription.event_types.is_empty() || subscription.event_types.len() > 32 {
        return Err(invalid("subscription eventTypes are invalid"));
    }
    let json = subscription
        .event_types
        .iter()
        .map(|event_type| canonical_json(event_type, "event type"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sorted = json.clone();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != json.len() {
        return Err(invalid("subscription eventTypes contain duplicates"));
    }
    Ok(())
}

fn subscribe_baseline(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
    start: &ControlPlaneWebSocketSubscribeStartAt,
    state: &StreamState,
) -> Result<Baseline, DurableEventHubError> {
    match start {
        ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
            ControlPlaneWebSocketSubscribeOrigin::Latest,
        ) => Ok(Baseline::Cursor(CursorParts {
            scope_json: scope_json.to_owned(),
            stream_json: stream_json.to_owned(),
            sequence: state.head_sequence,
            event_id: state.head_event_id.clone(),
        })),
        ControlPlaneWebSocketSubscribeStartAt::ControlPlaneWebSocketSubscribeOrigin(
            ControlPlaneWebSocketSubscribeOrigin::EarliestAvailable,
        ) => Ok(Baseline::Cursor(CursorParts {
            scope_json: scope_json.to_owned(),
            stream_json: stream_json.to_owned(),
            sequence: state.floor_sequence,
            event_id: state.floor_event_id.clone(),
        })),
        ControlPlaneWebSocketSubscribeStartAt::EventReadCursor(cursor) => {
            let parsed = parsed_cursor(cursor)?;
            if parsed.scope_json != scope_json || parsed.stream_json != stream_json {
                return Err(invalid(
                    "subscription cursor belongs to another scope or stream",
                ));
            }
            if cursor_is_retained(connection, scope_json, stream_json, &parsed, state)? {
                Ok(Baseline::Cursor(parsed))
            } else {
                Ok(Baseline::Expired)
            }
        }
    }
}

fn cursor_is_retained(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
    cursor: &CursorParts,
    state: &StreamState,
) -> Result<bool, DurableEventHubError> {
    if cursor.sequence < state.floor_sequence || cursor.sequence > state.head_sequence {
        return Ok(false);
    }
    if cursor.sequence == 0 {
        return Ok(cursor.event_id.is_none());
    }
    let expected = if cursor.sequence == state.floor_sequence {
        state.floor_event_id.clone()
    } else {
        event_id_at(connection, scope_json, stream_json, cursor.sequence)?
    };
    Ok(expected == cursor.event_id)
}

fn parsed_cursor(cursor: &EventReadCursor) -> Result<CursorParts, DurableEventHubError> {
    let value = serde_json::to_value(cursor)
        .map_err(|error| DurableEventHubError::storage("cursor", error))?;
    cursor_parts(&value)
}

fn parsed_ack_cursor(
    cursor: &ControlPlaneWebSocketAcknowledgedCursor,
) -> Result<CursorParts, DurableEventHubError> {
    let value = serde_json::to_value(cursor)
        .map_err(|error| DurableEventHubError::storage("ack cursor", error))?;
    cursor_parts(&value)
}

fn cursor_parts(value: &Value) -> Result<CursorParts, DurableEventHubError> {
    let sequence = value
        .get("sequence")
        .and_then(Value::as_i64)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| invalid("cursor sequence is invalid"))?;
    let event_id = value
        .get("eventId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if (sequence == 0) != event_id.is_none() {
        return Err(invalid("cursor sequence and eventId disagree"));
    }
    Ok(CursorParts {
        scope_json: canonical_value(
            value
                .get("scope")
                .ok_or_else(|| invalid("cursor scope is missing"))?,
        )?,
        stream_json: canonical_value(
            value
                .get("stream")
                .ok_or_else(|| invalid("cursor stream is missing"))?,
        )?,
        sequence,
        event_id,
    })
}

fn cursor(
    scope: &Scope,
    stream: &EventReadStream,
    sequence: u64,
    event_id: Option<String>,
) -> Result<EventReadCursor, DurableEventHubError> {
    let value = serde_json::json!({
        "scope": scope,
        "stream": stream,
        "sequence": to_i64(sequence, "cursor sequence")?,
        "eventId": event_id.map(ControlPlaneEventId),
    });
    serde_json::from_value(value).map_err(|_| corrupt("generated event cursor cannot be encoded"))
}

fn acknowledged_cursor(
    event: &PublicEvent,
) -> Result<ControlPlaneWebSocketAcknowledgedCursor, DurableEventHubError> {
    Ok(ControlPlaneWebSocketAcknowledgedCursor {
        event_id: event.event_id.clone(),
        scope: event.scope.clone(),
        sequence: ControlPlaneWebSocketEventSequence(to_i64(event.sequence, "ack cursor")?),
        stream: event.stream.clone(),
    })
}

fn subscription_accepted(
    subscription_id: &ControlPlaneWebSocketSubscriptionId,
    cursor: EventReadCursor,
    epoch: ControlPlaneWebSocketAuthorizationEpoch,
    config: &DurableEventHubConfig,
) -> Result<Value, DurableEventHubError> {
    typed_value(&ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketSubscriptionAcceptedFrame(
        ControlPlaneWebSocketSubscriptionAcceptedFrame {
            authorization_epoch: epoch,
            cursor,
            limits: ControlPlaneWebSocketTransportLimits {
                ack_deadline_millis: f64::from(config.ack_deadline_millis),
                backpressure_close_code: f64::from(config.backpressure_close_code),
                hard_unacked_events: f64::from(config.hard_unacked_events),
                max_unacked_events: f64::from(config.max_unacked_events),
            },
            subscription_id: subscription_id.clone(),
            type_value: ControlPlaneWebSocketSubscriptionAcceptedFrameTypeValue::TransportSubscriptionAcceptedV1,
        },
    ))
}

fn event_frame(
    subscription_id: &ControlPlaneWebSocketSubscriptionId,
    epoch: i64,
    event: &PublicEvent,
) -> Result<Value, DurableEventHubError> {
    typed_value(
        &ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketEventFrame(
            ControlPlaneWebSocketEventFrame {
                authorization_epoch: ControlPlaneWebSocketAuthorizationEpoch(epoch),
                event: event.payload.clone(),
                event_id: event.event_id.clone(),
                occurred_at: event.occurred_at.clone(),
                scope: event.scope.clone(),
                sequence: ControlPlaneWebSocketEventSequence(to_i64(
                    event.sequence,
                    "event sequence",
                )?),
                source: event.source.clone(),
                stream: event.stream.clone(),
                subscription_id: subscription_id.clone(),
                type_value: ControlPlaneWebSocketEventFrameTypeValue::EventV1,
            },
        ),
    )
}

fn reset_frame(
    subscription_id: &ControlPlaneWebSocketSubscriptionId,
    earliest_available: EventReadCursor,
) -> Result<Value, DurableEventHubError> {
    typed_value(
        &ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketResetRequiredFrame(
            ControlPlaneWebSocketResetRequiredFrame {
                close_code: 4_409.0,
                earliest_available,
                reason: "requested cursor is no longer retained".to_owned(),
                subscription_id: subscription_id.clone(),
                type_value:
                    ControlPlaneWebSocketResetRequiredFrameTypeValue::TransportResetRequiredV1,
            },
        ),
    )
}

fn revocation_frame(
    subscription_id: &str,
    epoch: ControlPlaneWebSocketAuthorizationEpoch,
) -> Result<Value, DurableEventHubError> {
    typed_value(&ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketAuthorizationRevokedFrame(
        ControlPlaneWebSocketAuthorizationRevokedFrame {
            authorization_epoch: epoch,
            close_code: 4_403.0,
            subscription_id: ControlPlaneWebSocketSubscriptionId(subscription_id.to_owned()),
            type_value: ControlPlaneWebSocketAuthorizationRevokedFrameTypeValue::TransportAuthorizationRevokedV1,
        },
    ))
}

fn backpressure_frame(
    subscription: &StoredSubscription,
    ack_required_through: &PublicEvent,
    disconnect_at: &Instant,
    pending_event_count: u64,
    config: &DurableEventHubConfig,
) -> Result<Value, DurableEventHubError> {
    typed_value(
        &ControlPlaneWebSocketServerFrame::ControlPlaneWebSocketBackpressureFrame(
            ControlPlaneWebSocketBackpressureFrame {
                ack_required_through: acknowledged_cursor(ack_required_through)?,
                close_code: f64::from(config.backpressure_close_code),
                disconnect_at: disconnect_at.clone(),
                pending_event_count: to_i64(pending_event_count, "pending event count")?,
                subscription_id: ControlPlaneWebSocketSubscriptionId(
                    subscription.subscription_id.clone(),
                ),
                type_value:
                    ControlPlaneWebSocketBackpressureFrameTypeValue::TransportBackpressureV1,
            },
        ),
    )
}

fn typed_value(frame: &ControlPlaneWebSocketServerFrame) -> Result<Value, DurableEventHubError> {
    let value = serde_json::to_value(frame)
        .map_err(|error| DurableEventHubError::storage("generated server frame", error))?;
    serde_json::from_value::<ControlPlaneWebSocketServerFrame>(value.clone())
        .map_err(|_| corrupt("generated server frame failed round-trip validation"))?;
    Ok(value)
}

fn require_authorization(
    connection: &Connection,
    subject: &str,
    scope_json: &str,
) -> Result<i64, DurableEventHubError> {
    let result = connection
        .query_row(
            "SELECT epoch,active FROM hub_authorizations WHERE subject=?1 AND scope_json=?2",
            params![subject, scope_json],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("authorization read", error))?;
    match result {
        Some((epoch, 1)) if epoch > 0 => Ok(epoch),
        _ => Err(DurableEventHubError::new(
            DurableEventHubErrorCode::Unauthorized,
            "subscription scope is not currently authorized",
        )),
    }
}

fn authorization_epoch(
    connection: &Connection,
    subject: &str,
    scope_json: &str,
) -> Result<i64, DurableEventHubError> {
    connection
        .query_row(
            "SELECT epoch FROM hub_authorizations WHERE subject=?1 AND scope_json=?2",
            params![subject, scope_json],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|epoch| epoch.unwrap_or(0))
        .map_err(|error| DurableEventHubError::storage("authorization epoch read", error))
}

fn stream_state_or_empty(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
) -> Result<StreamState, DurableEventHubError> {
    Ok(
        stream_state(connection, scope_json, stream_json)?.unwrap_or(StreamState {
            head_sequence: 0,
            head_event_id: None,
            floor_sequence: 0,
            floor_event_id: None,
        }),
    )
}

fn stream_state(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
) -> Result<Option<StreamState>, DurableEventHubError> {
    connection
        .query_row(
            "SELECT head_sequence,head_event_id,floor_sequence,floor_event_id FROM hub_streams \
             WHERE scope_json=?1 AND stream_json=?2",
            params![scope_json, stream_json],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("stream state read", error))?
        .map(|(head, head_id, floor, floor_id)| {
            Ok(StreamState {
                head_sequence: u64::try_from(head)
                    .map_err(|_| corrupt("stream head is negative"))?,
                head_event_id: head_id,
                floor_sequence: u64::try_from(floor)
                    .map_err(|_| corrupt("stream floor is negative"))?,
                floor_event_id: floor_id,
            })
        })
        .transpose()
}

fn event_id_at(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
    sequence: u64,
) -> Result<Option<String>, DurableEventHubError> {
    if sequence == 0 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT event_id FROM hub_events WHERE scope_json=?1 AND stream_json=?2 AND sequence=?3",
            params![scope_json, stream_json, to_i64(sequence, "event lookup sequence")?],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("event cursor lookup", error))
}

fn stored_event(
    connection: &Connection,
    event_id: &str,
) -> Result<Option<StoredEvent>, DurableEventHubError> {
    connection
        .query_row(
            "SELECT event_id,scope_json,stream_json,sequence,topic,event_type_json,payload_json,\
             occurred_at_json,source_json FROM hub_events WHERE event_id=?1",
            [event_id],
            |row| {
                Ok(StoredEvent {
                    event_id: row.get(0)?,
                    scope_json: row.get(1)?,
                    stream_json: row.get(2)?,
                    sequence: row.get(3)?,
                    topic: row.get(4)?,
                    event_type_json: row.get(5)?,
                    payload_json: row.get(6)?,
                    occurred_at_json: row.get(7)?,
                    source_json: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("event catalog read", error))
}

fn stored_subscription(
    connection: &Connection,
    subscription_id: &str,
) -> Result<Option<StoredSubscription>, DurableEventHubError> {
    connection
        .query_row(
            "SELECT subject,subscription_id,scope_json,stream_json,event_types_json,authorization_epoch,\
             acknowledged_sequence,acknowledged_event_id,sent_sequence,state,backpressure_sent \
             FROM hub_subscriptions \
             WHERE subscription_id=?1",
            [subscription_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?, row.get::<_, Option<String>>(7)?, row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?, row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("subscription read", error))?
        .map(|row| {
            Ok(StoredSubscription {
                subject: row.0,
                subscription_id: row.1,
                scope_json: row.2,
                stream_json: row.3,
                event_types: decode_json(&row.4, "stored event types")?,
                event_types_json: row.4,
                authorization_epoch: row.5,
                acknowledged_sequence: u64::try_from(row.6).map_err(|_| corrupt("stored ack is negative"))?,
                acknowledged_event_id: row.7,
                sent_sequence: u64::try_from(row.8).map_err(|_| corrupt("stored sent cursor is negative"))?,
                state: row.9,
                backpressure_sent: match row.10 {
                    0 => false,
                    1 => true,
                    _ => return Err(corrupt("stored backpressure state is invalid")),
                },
            })
        })
        .transpose()
}

fn events_after_matching(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
    after: u64,
    limit: u64,
    event_types: &[ControlPlaneWebSocketEventType],
) -> Result<Vec<PublicEvent>, DurableEventHubError> {
    if limit == 0 || event_types.is_empty() {
        return Ok(Vec::new());
    }
    let encoded_types = encoded_event_types(event_types)?;
    let placeholders = sql_placeholders(encoded_types.len());
    let sql = format!(
        "SELECT event_id,scope_json,stream_json,sequence,topic,event_type_json,payload_json,\
         occurred_at_json,source_json FROM hub_events WHERE scope_json=? AND stream_json=? \
         AND sequence>? AND event_type_json IN ({placeholders}) ORDER BY sequence LIMIT ?"
    );
    let mut values = Vec::with_capacity(encoded_types.len().saturating_add(4));
    values.push(SqlValue::Text(scope_json.to_owned()));
    values.push(SqlValue::Text(stream_json.to_owned()));
    values.push(SqlValue::Integer(to_i64(after, "replay cursor")?));
    values.extend(encoded_types.into_iter().map(SqlValue::Text));
    values.push(SqlValue::Integer(to_i64(limit, "replay limit")?));
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| DurableEventHubError::storage("event replay query", error))?;
    let rows = statement
        .query_map(params_from_iter(values.iter()), |row| {
            Ok(StoredEvent {
                event_id: row.get(0)?,
                scope_json: row.get(1)?,
                stream_json: row.get(2)?,
                sequence: row.get(3)?,
                topic: row.get(4)?,
                event_type_json: row.get(5)?,
                payload_json: row.get(6)?,
                occurred_at_json: row.get(7)?,
                source_json: row.get(8)?,
            })
        })
        .map_err(|error| DurableEventHubError::storage("event replay read", error))?;
    rows.map(|row| {
        row.map_err(|error| DurableEventHubError::storage("event replay row", error))?
            .into_public()
    })
    .collect()
}

fn matching_event_count(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
    after: u64,
    through: u64,
    event_types: &[ControlPlaneWebSocketEventType],
) -> Result<u64, DurableEventHubError> {
    if through <= after || event_types.is_empty() {
        return Ok(0);
    }
    let encoded_types = encoded_event_types(event_types)?;
    let placeholders = sql_placeholders(encoded_types.len());
    let sql = format!(
        "SELECT COUNT(*) FROM hub_events WHERE scope_json=? AND stream_json=? \
         AND sequence>? AND sequence<=? AND event_type_json IN ({placeholders})"
    );
    let mut values = Vec::with_capacity(encoded_types.len().saturating_add(4));
    values.push(SqlValue::Text(scope_json.to_owned()));
    values.push(SqlValue::Text(stream_json.to_owned()));
    values.push(SqlValue::Integer(to_i64(after, "pending cursor")?));
    values.push(SqlValue::Integer(to_i64(through, "pending head")?));
    values.extend(encoded_types.into_iter().map(SqlValue::Text));
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| DurableEventHubError::storage("pending event count query", error))?;
    let count = statement
        .query_row(params_from_iter(values.iter()), |row| row.get::<_, i64>(0))
        .map_err(|error| DurableEventHubError::storage("pending event count read", error))?;
    u64::try_from(count).map_err(|_| corrupt("pending event count is negative"))
}

fn encoded_event_types(
    event_types: &[ControlPlaneWebSocketEventType],
) -> Result<Vec<String>, DurableEventHubError> {
    event_types
        .iter()
        .map(|event_type| canonical_json(event_type, "subscription event type"))
        .collect()
}

fn sql_placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

fn public_event_at(
    connection: &Connection,
    scope_json: &str,
    stream_json: &str,
    sequence: u64,
) -> Result<Option<PublicEvent>, DurableEventHubError> {
    if sequence == 0 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT event_id,scope_json,stream_json,sequence,topic,event_type_json,payload_json,\
             occurred_at_json,source_json FROM hub_events WHERE scope_json=?1 AND stream_json=?2 \
             AND sequence=?3",
            params![
                scope_json,
                stream_json,
                to_i64(sequence, "public event lookup")?
            ],
            |row| {
                Ok(StoredEvent {
                    event_id: row.get(0)?,
                    scope_json: row.get(1)?,
                    stream_json: row.get(2)?,
                    sequence: row.get(3)?,
                    topic: row.get(4)?,
                    event_type_json: row.get(5)?,
                    payload_json: row.get(6)?,
                    occurred_at_json: row.get(7)?,
                    source_json: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|error| DurableEventHubError::storage("public event lookup", error))?
        .map(StoredEvent::into_public)
        .transpose()
}

fn subscriptions_for_event(
    connection: &Connection,
    event: &PublicEvent,
) -> Result<Vec<StoredSubscription>, DurableEventHubError> {
    let scope_json = canonical_json(&event.scope, "event scope")?;
    let stream_json = canonical_json(&event.stream, "event stream")?;
    let mut statement = connection
        .prepare(
            "SELECT subscription_id FROM hub_subscriptions WHERE scope_json=?1 AND stream_json=?2 \
             AND state='active' ORDER BY subscription_id",
        )
        .map_err(|error| DurableEventHubError::storage("fanout query", error))?;
    let ids = statement
        .query_map(params![scope_json, stream_json], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| DurableEventHubError::storage("fanout read", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DurableEventHubError::storage("fanout row", error))?;
    ids.iter()
        .map(|id| {
            let mut subscription = stored_subscription(connection, id)?
                .ok_or_else(|| corrupt("fanout subscription disappeared"))?;
            subscription.authorization_epoch =
                require_authorization(connection, &subscription.subject, &subscription.scope_json)?;
            Ok(subscription)
        })
        .filter(|result| {
            result.as_ref().map_or(true, |subscription| {
                event.sequence > subscription.sent_sequence
                    && subscription.event_types.contains(&event.event_type)
            })
        })
        .collect()
}

fn query_strings<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<String>, DurableEventHubError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| DurableEventHubError::storage("event-hub query", error))?;
    statement
        .query_map(parameters, |row| row.get::<_, String>(0))
        .map_err(|error| DurableEventHubError::storage("event-hub query", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DurableEventHubError::storage("event-hub row", error))
}

fn canonical_json<T: serde::Serialize>(
    value: &T,
    label: &str,
) -> Result<String, DurableEventHubError> {
    serde_json::to_string(value)
        .map_err(|error| DurableEventHubError::storage(&format!("{label} encoding"), error))
}

fn canonical_value(value: &Value) -> Result<String, DurableEventHubError> {
    serde_json::to_string(value)
        .map_err(|error| DurableEventHubError::storage("cursor encoding", error))
}

fn decode_json<T: serde::de::DeserializeOwned>(
    value: &str,
    label: &str,
) -> Result<T, DurableEventHubError> {
    serde_json::from_str(value).map_err(|_| corrupt(format!("{label} is corrupt")))
}

fn to_i64(value: u64, label: &str) -> Result<i64, DurableEventHubError> {
    i64::try_from(value).map_err(|_| invalid(format!("{label} is out of range")))
}

fn instant_from_millis(value: u64) -> Result<Instant, DurableEventHubError> {
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| invalid("backpressure deadline is out of range"))?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err(invalid("backpressure deadline is out of range"));
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = month_position + if month_position < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn invalid(message: impl Into<String>) -> DurableEventHubError {
    DurableEventHubError::new(DurableEventHubErrorCode::InvalidInput, message)
}

fn conflict(message: impl Into<String>) -> DurableEventHubError {
    DurableEventHubError::new(DurableEventHubErrorCode::Conflict, message)
}

fn corrupt(message: impl Into<String>) -> DurableEventHubError {
    DurableEventHubError::new(DurableEventHubErrorCode::Corrupt, message)
}

impl From<winwincode_storage::StorageError> for DurableEventHubError {
    fn from(error: winwincode_storage::StorageError) -> Self {
        let code = if error.kind() == StorageErrorKind::EventCursorExpired {
            DurableEventHubErrorCode::CursorExpired
        } else {
            DurableEventHubErrorCode::Storage
        };
        Self::new(code, error.to_string())
    }
}
