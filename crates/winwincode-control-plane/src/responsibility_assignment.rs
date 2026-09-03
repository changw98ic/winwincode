// SPDX-License-Identifier: Apache-2.0

//! Durable collaboration responsibility assignments.
//!
//! This module owns only who is currently responsible for a `ProductSession`,
//! Delivery, Delivery stage, or review. Identity/RBAC and target lifecycle
//! remain external authorities and contribute revision guards to every write.

use std::{
    collections::BTreeMap,
    fmt,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{Actor, Scope};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditSubject,
};
use winwincode_delivery::domain::DeliveryStage;
use winwincode_domain::{DeliveryId, ProductSessionId, RequestId, Sha256Digest, UserId};
use winwincode_domain::{RepositoryScope, UserActor};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, PendingAuditEvent, ProductStateStorage, PublicEventScope,
    StateCommit, StateRevisionGuard, StorageError, StorageErrorKind, receipt_scope_key,
};

use crate::command_receipt_identity;

const CATALOG_SCHEMA: &str = "winwincode.responsibility-assignment.catalog.v1";
const STREAM_PREFIX: &str = "responsibility-assignments:";
const EVENT_TOPIC: &str = "responsibility.assignment.changed.v1";
const AUDIT_ORIGIN: &str = "control-plane.responsibility-assignment";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ASSIGNMENTS_PER_SCOPE: usize = 10_000;
const MAX_COMMIT_ATTEMPTS: usize = 32;

/// Stable assignment operation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsibilityAssignmentErrorKind {
    InvalidRequest,
    ScopeDenied,
    AuthorizationDenied,
    MemberInactive,
    RoleIneligible,
    TargetUnavailable,
    NotFound,
    WrongState,
    SeparationViolation,
    AssignmentExpired,
    RevisionConflict,
    RequestConflict,
    AuthorityChanged,
    CorruptState,
    Storage,
    ClockUnavailable,
}

/// Secret-free assignment operation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsibilityAssignmentError {
    kind: ResponsibilityAssignmentErrorKind,
    message: &'static str,
}

impl ResponsibilityAssignmentError {
    const fn new(kind: ResponsibilityAssignmentErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> ResponsibilityAssignmentErrorKind {
        self.kind
    }
}

impl fmt::Display for ResponsibilityAssignmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ResponsibilityAssignmentError {}

/// Trusted clock failure without environment details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsibilityAssignmentClockError;

/// Time authority used for acceptance and expiry decisions.
pub trait ResponsibilityAssignmentClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when current time cannot be resolved.
    fn now_millis(&mut self) -> Result<u64, ResponsibilityAssignmentClockError>;
}

/// Production wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResponsibilityAssignmentClock;

impl ResponsibilityAssignmentClock for SystemResponsibilityAssignmentClock {
    fn now_millis(&mut self) -> Result<u64, ResponsibilityAssignmentClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ResponsibilityAssignmentClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| ResponsibilityAssignmentClockError)
    }
}

/// One closed collaboration responsibility.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsibilityRole {
    Assignee,
    Reviewer,
    Approver,
}

/// Review responsibility owned by collaboration rather than Delivery state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsibilityReviewKind {
    Solution,
    Delivery,
}

/// Stable business object whose responsibility is being assigned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponsibilityTarget {
    ProductSession {
        product_session_id: ProductSessionId,
    },
    Delivery {
        delivery_id: DeliveryId,
    },
    DeliveryStage {
        delivery_id: DeliveryId,
        stage: DeliveryStage,
    },
    Review {
        delivery_id: DeliveryId,
        review: ResponsibilityReviewKind,
    },
}

/// Durable lifecycle of one responsibility record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsibilityAssignmentState {
    PendingAcceptance,
    Active,
    Expired,
    Revoked,
}

/// Deterministic identity for one target/role pair.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ResponsibilityAssignmentId(pub String);

/// Current collaboration-owned assignment state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponsibilityAssignment {
    pub id: ResponsibilityAssignmentId,
    pub scope: RepositoryScope,
    pub target: ResponsibilityTarget,
    pub role: ResponsibilityRole,
    pub principal_user_id: UserId,
    pub state: ResponsibilityAssignmentState,
    pub revision: u64,
    pub assigned_by: Actor,
    pub assigned_at_millis: u64,
    pub accepted_at_millis: Option<u64>,
    pub expires_at_millis: Option<u64>,
    pub ended_at_millis: Option<u64>,
    pub target_revision: u64,
    pub target_sha256: Sha256Digest,
    pub rbac_revision: u64,
    pub rbac_sha256: Sha256Digest,
}

impl ResponsibilityAssignment {
    /// Returns the lifecycle visible at a given trusted time without writing a
    /// second derived state.
    #[must_use]
    pub fn effective_state(&self, now_millis: u64) -> ResponsibilityAssignmentState {
        if matches!(
            self.state,
            ResponsibilityAssignmentState::PendingAcceptance
                | ResponsibilityAssignmentState::Active
        ) && self
            .expires_at_millis
            .is_some_and(|expires_at| now_millis >= expires_at)
        {
            ResponsibilityAssignmentState::Expired
        } else {
            self.state
        }
    }
}

/// Mutation requested against one assignment revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponsibilityAssignmentAction {
    Assign {
        principal_user_id: UserId,
        expires_at_millis: Option<u64>,
    },
    Accept,
    Reassign {
        principal_user_id: UserId,
        expires_at_millis: Option<u64>,
    },
    Expire,
    RevokeDeparted,
}

impl ResponsibilityAssignmentAction {
    #[must_use]
    pub const fn operation(&self) -> ResponsibilityAssignmentOperation {
        match self {
            Self::Assign { .. } => ResponsibilityAssignmentOperation::Assign,
            Self::Accept => ResponsibilityAssignmentOperation::Accept,
            Self::Reassign { .. } => ResponsibilityAssignmentOperation::Reassign,
            Self::Expire => ResponsibilityAssignmentOperation::Expire,
            Self::RevokeDeparted => ResponsibilityAssignmentOperation::RevokeDeparted,
        }
    }
}

/// Closed authorization operation passed to Identity/RBAC.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsibilityAssignmentOperation {
    Assign,
    Accept,
    Reassign,
    Expire,
    RevokeDeparted,
    List,
}

/// Authenticated command context. Authentication material has no representation.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityAssignmentContext {
    pub actor: Actor,
    pub authenticated_scopes: Vec<Scope>,
    pub scope: RepositoryScope,
    pub request_id: RequestId,
    pub expected_revision: u64,
}

/// One assignment mutation request.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityAssignmentCommand {
    pub context: ResponsibilityAssignmentContext,
    pub target: ResponsibilityTarget,
    pub role: ResponsibilityRole,
    pub action: ResponsibilityAssignmentAction,
}

/// Read request isolated to one repository scope.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityAssignmentListRequest {
    pub actor: Actor,
    pub authenticated_scopes: Vec<Scope>,
    pub scope: RepositoryScope,
    pub target: Option<ResponsibilityTarget>,
    pub role: Option<ResponsibilityRole>,
    pub principal_user_id: Option<UserId>,
    pub include_ended: bool,
}

/// Immutable replay-safe mutation result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponsibilityAssignmentReceipt {
    pub assignment: ResponsibilityAssignment,
    pub catalog_revision: u64,
    pub occurred_at_millis: u64,
    pub replayed: bool,
}

/// Current principal facts resolved by Identity/RBAC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsibilityPrincipalAuthority {
    pub user_id: UserId,
    pub active: bool,
    pub role_eligible: bool,
}

/// Exact authority facts consumed by one assignment command.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityCommandAuthority {
    pub actor: Actor,
    pub scope: RepositoryScope,
    pub operation: ResponsibilityAssignmentOperation,
    pub target: ResponsibilityTarget,
    pub role: ResponsibilityRole,
    pub permission_granted: bool,
    pub actor_active: bool,
    pub principal: ResponsibilityPrincipalAuthority,
    pub target_revision: u64,
    pub target_sha256: Sha256Digest,
    pub rbac_revision: u64,
    pub rbac_sha256: Sha256Digest,
    pub target_guard: StateRevisionGuard,
    pub target_scope_guard: Option<StateRevisionGuard>,
    pub rbac_guard: StateRevisionGuard,
}

/// Exact current authorization for one scoped list.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityListAuthority {
    pub actor: Actor,
    pub scope: RepositoryScope,
    pub permission_granted: bool,
    pub actor_active: bool,
    pub rbac_revision: u64,
    pub rbac_sha256: Sha256Digest,
}

/// Sealed current authority used by the rebuildable collaboration Inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityInboxAuthority {
    pub actor: Actor,
    pub scope: RepositoryScope,
    pub permission_granted: bool,
    pub actor_active: bool,
    pub rbac_revision: u64,
    pub rbac_sha256: Sha256Digest,
    pub rbac_guard: StateRevisionGuard,
}

/// Current active assignment cut plus the exact RBAC and catalog guards.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityInboxSnapshot {
    pub assignments: Vec<ResponsibilityAssignment>,
    pub rbac_revision: u64,
    pub rbac_sha256: Sha256Digest,
    pub state_guards: Vec<StateRevisionGuard>,
}

/// Read-only request offered to the composite Identity/RBAC/target authority.
pub struct ResponsibilityAuthorityRequest<'request> {
    command: &'request ResponsibilityAssignmentCommand,
    current: Option<&'request ResponsibilityAssignment>,
}

impl<'request> ResponsibilityAuthorityRequest<'request> {
    #[must_use]
    pub const fn command(&self) -> &'request ResponsibilityAssignmentCommand {
        self.command
    }

    #[must_use]
    pub const fn current(&self) -> Option<&'request ResponsibilityAssignment> {
        self.current
    }

    #[must_use]
    pub fn requested_principal(&self) -> Option<&UserId> {
        match &self.command.action {
            ResponsibilityAssignmentAction::Assign {
                principal_user_id, ..
            }
            | ResponsibilityAssignmentAction::Reassign {
                principal_user_id, ..
            } => Some(principal_user_id),
            ResponsibilityAssignmentAction::Accept
            | ResponsibilityAssignmentAction::Expire
            | ResponsibilityAssignmentAction::RevokeDeparted => {
                self.current.map(|assignment| &assignment.principal_user_id)
            }
        }
    }
}

/// Trusted authority failure. Denials remain distinct from source unavailability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsibilityAuthorityError {
    Denied,
    Unavailable,
}

/// Composite read boundary over current Identity/RBAC and
/// `ProductSession`/Delivery facts.
pub trait ResponsibilityAuthorityPort: Send {
    /// Resolves exact current command authority and state guards.
    ///
    /// # Errors
    ///
    /// Returns denial or source unavailability without assignment state writes.
    fn command_authority(
        &mut self,
        request: ResponsibilityAuthorityRequest<'_>,
    ) -> Result<ResponsibilityCommandAuthority, ResponsibilityAuthorityError>;

    /// Resolves current permission for one scoped list.
    ///
    /// # Errors
    ///
    /// Returns denial or source unavailability before assignment data is read.
    fn list_authority(
        &mut self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityListAuthority, ResponsibilityAuthorityError>;

    /// Resolves least-privilege collaboration-read authority and its exact guard.
    ///
    /// # Errors
    ///
    /// Returns denial or source unavailability before assignment data is read.
    fn inbox_authority(
        &mut self,
        _request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityInboxAuthority, ResponsibilityAuthorityError> {
        Err(ResponsibilityAuthorityError::Unavailable)
    }
}

/// Durable assignment application service.
pub struct ResponsibilityAssignmentService {
    inner: Mutex<ResponsibilityAssignmentInner>,
}

struct ResponsibilityAssignmentInner {
    storage: Box<dyn ProductStateStorage>,
    authority: Box<dyn ResponsibilityAuthorityPort>,
    clock: Box<dyn ResponsibilityAssignmentClock>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResponsibilityAssignmentCatalog {
    schema: String,
    scope: RepositoryScope,
    revision: u64,
    assignments: BTreeMap<String, ResponsibilityAssignment>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResponsibilityAssignmentEvent {
    receipt: ResponsibilityAssignmentReceipt,
}

impl ResponsibilityAssignmentService {
    #[must_use]
    pub fn new(
        storage: Box<dyn ProductStateStorage>,
        authority: Box<dyn ResponsibilityAuthorityPort>,
    ) -> Self {
        Self::with_clock(
            storage,
            authority,
            Box::new(SystemResponsibilityAssignmentClock),
        )
    }

    #[must_use]
    pub fn with_clock(
        storage: Box<dyn ProductStateStorage>,
        authority: Box<dyn ResponsibilityAuthorityPort>,
        clock: Box<dyn ResponsibilityAssignmentClock>,
    ) -> Self {
        Self {
            inner: Mutex::new(ResponsibilityAssignmentInner {
                storage,
                authority,
                clock,
            }),
        }
    }

    /// Applies one replay-safe responsibility mutation.
    ///
    /// # Errors
    ///
    /// Rejects invalid, unauthorized, stale, cross-tenant, conflicting,
    /// separated-duty, expired, or unavailable authority before state changes.
    pub fn apply(
        &self,
        command: &ResponsibilityAssignmentCommand,
    ) -> Result<ResponsibilityAssignmentReceipt, ResponsibilityAssignmentError> {
        self.inner
            .lock()
            .map_err(|_| storage_error())?
            .apply(command)
    }

    /// Lists one currently authorized repository scope.
    ///
    /// # Errors
    ///
    /// Rejects foreign/unauthorized scope, corrupt state, unavailable time, or
    /// authority/storage failure.
    pub fn list(
        &self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<Vec<ResponsibilityAssignment>, ResponsibilityAssignmentError> {
        self.inner
            .lock()
            .map_err(|_| storage_error())?
            .list(request)
    }

    /// Reads current active assignments under collaboration-read RBAC authority.
    ///
    /// The returned guards can be attached to a claim or annotation commit, so a
    /// concurrent membership, role or assignment change rejects that write.
    ///
    /// # Errors
    ///
    /// Rejects foreign/unauthorized scope, changed authority, corrupt state,
    /// unavailable time, or storage failure.
    pub fn inbox_snapshot(
        &self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityInboxSnapshot, ResponsibilityAssignmentError> {
        self.inner
            .lock()
            .map_err(|_| storage_error())?
            .inbox_snapshot(request)
    }
}

impl ResponsibilityAssignmentInner {
    fn apply(
        &mut self,
        command: &ResponsibilityAssignmentCommand,
    ) -> Result<ResponsibilityAssignmentReceipt, ResponsibilityAssignmentError> {
        validate_command(command)?;
        let scope = Scope::RepositoryScope(command.context.scope.clone());
        let identity = command_receipt_identity(
            &command.context.actor,
            &scope,
            command.context.request_id.clone(),
        )
        .map_err(|_| invalid())?;
        let command_digest = command_digest(command)?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &command_digest)? {
            return replay_receipt(&receipt, true);
        }
        let stream_id = catalog_stream(identity.scope_key().as_bytes());
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let catalog = self.load_catalog(&stream_id, &command.context.scope)?;
            let assignment_id =
                assignment_id(&command.context.scope, &command.target, command.role)?;
            let current = catalog.assignments.get(&assignment_id.0).cloned();
            require_expected_revision(current.as_ref(), command.context.expected_revision)?;
            let authority = self
                .authority
                .command_authority(ResponsibilityAuthorityRequest {
                    command,
                    current: current.as_ref(),
                })
                .map_err(authority_error)?;
            validate_command_authority(command, current.as_ref(), &authority, &stream_id)?;
            let now = self.clock.now_millis().map_err(|_| clock_unavailable())?;
            validate_time(now)?;
            let next = apply_action(
                command,
                current.as_ref(),
                assignment_id,
                &authority,
                now,
                &catalog,
            )?;
            let commit = assignment_commit(AssignmentCommitInput {
                identity: &identity,
                command_digest: &command_digest,
                stream_id: &stream_id,
                command,
                current: current.as_ref(),
                next,
                catalog,
                authority: &authority,
                now,
            })?;
            match self.storage.commit(&commit) {
                Ok(receipt) => return replay_receipt(&receipt, false),
                Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                    if let Some(receipt) = self.storage.load_receipt(&identity, &command_digest)? {
                        return replay_receipt(&receipt, true);
                    }
                    if error.is_state_guard_conflict() {
                        return Err(authority_changed());
                    }
                    if attempt + 1 == MAX_COMMIT_ATTEMPTS {
                        return Err(revision_conflict());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(storage_error())
    }

    fn list(
        &mut self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<Vec<ResponsibilityAssignment>, ResponsibilityAssignmentError> {
        validate_scope(&request.scope)?;
        let authority = self
            .authority
            .list_authority(request)
            .map_err(authority_error)?;
        validate_list_authority(request, &authority)?;
        let now = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        validate_time(now)?;
        let stream_id = catalog_stream_for_scope(&request.scope)?;
        let catalog = self.load_catalog(&stream_id, &request.scope)?;
        let mut assignments = catalog
            .assignments
            .into_values()
            .filter(|assignment| list_matches(assignment, request, now))
            .collect::<Vec<_>>();
        let confirmation = self
            .authority
            .list_authority(request)
            .map_err(authority_error)?;
        validate_list_authority(request, &confirmation)?;
        if confirmation != authority {
            return Err(authority_changed());
        }
        assignments.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(assignments)
    }

    fn inbox_snapshot(
        &mut self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityInboxSnapshot, ResponsibilityAssignmentError> {
        validate_scope(&request.scope)?;
        if request.include_ended {
            return Err(invalid());
        }
        let authority = self
            .authority
            .inbox_authority(request)
            .map_err(authority_error)?;
        validate_inbox_authority(request, &authority)?;
        let now = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        validate_time(now)?;
        let stream_id = catalog_stream_for_scope(&request.scope)?;
        let catalog = self.load_catalog(&stream_id, &request.scope)?;
        let catalog_revision = catalog.revision;
        let mut assignments = catalog
            .assignments
            .into_values()
            .filter(|assignment| list_matches(assignment, request, now))
            .collect::<Vec<_>>();
        let confirmation = self
            .authority
            .inbox_authority(request)
            .map_err(authority_error)?;
        validate_inbox_authority(request, &confirmation)?;
        if confirmation != authority {
            return Err(authority_changed());
        }
        assignments.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(ResponsibilityInboxSnapshot {
            assignments,
            rbac_revision: authority.rbac_revision,
            rbac_sha256: authority.rbac_sha256,
            state_guards: vec![
                StateRevisionGuard::new(stream_id, catalog_revision)?,
                authority.rbac_guard,
            ],
        })
    }

    fn load_catalog(
        &self,
        stream_id: &str,
        scope: &RepositoryScope,
    ) -> Result<ResponsibilityAssignmentCatalog, ResponsibilityAssignmentError> {
        let Some(stored) = self.storage.load_state(stream_id)? else {
            return Ok(ResponsibilityAssignmentCatalog {
                schema: CATALOG_SCHEMA.to_owned(),
                scope: scope.clone(),
                revision: 0,
                assignments: BTreeMap::new(),
            });
        };
        let catalog: ResponsibilityAssignmentCatalog =
            serde_json::from_slice(&stored.payload).map_err(|_| corrupt())?;
        validate_catalog(&catalog, stream_id, stored.revision, scope)?;
        Ok(catalog)
    }
}

struct AssignmentCommitInput<'input> {
    identity: &'input winwincode_storage::ReceiptIdentity,
    command_digest: &'input Sha256Digest,
    stream_id: &'input str,
    command: &'input ResponsibilityAssignmentCommand,
    current: Option<&'input ResponsibilityAssignment>,
    next: ResponsibilityAssignment,
    catalog: ResponsibilityAssignmentCatalog,
    authority: &'input ResponsibilityCommandAuthority,
    now: u64,
}

fn assignment_commit(
    input: AssignmentCommitInput<'_>,
) -> Result<StateCommit, ResponsibilityAssignmentError> {
    let previous_payload = input
        .current
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| corrupt())?;
    let next_payload = serde_json::to_vec(&input.next).map_err(|_| corrupt())?;
    let mut catalog = input.catalog;
    catalog.revision = next_revision(catalog.revision)?;
    catalog
        .assignments
        .insert(input.next.id.0.clone(), input.next.clone());
    if catalog.assignments.len() > MAX_ASSIGNMENTS_PER_SCOPE {
        return Err(invalid());
    }
    let response = ResponsibilityAssignmentReceipt {
        assignment: input.next,
        catalog_revision: catalog.revision,
        occurred_at_millis: input.now,
        replayed: false,
    };
    let event_id = event_id(input.identity, input.command_digest);
    let event_payload = serde_json::to_vec(&ResponsibilityAssignmentEvent { receipt: response })
        .map_err(|_| corrupt())?;
    let audit = pending_audit(
        input.command,
        previous_payload.as_deref(),
        &next_payload,
        input.now,
        &event_id,
    )?;
    let state = serde_json::to_vec(&catalog).map_err(|_| corrupt())?;
    let mut commit = StateCommit::new(
        input.identity.clone(),
        input.command_digest.clone(),
        input.stream_id,
        catalog.revision - 1,
        state,
        vec![NewOutboxEvent::internal(
            event_id,
            EVENT_TOPIC,
            event_payload,
        )],
    )
    .with_pending_audit_event(audit);
    commit = commit
        .with_state_guard(input.authority.target_guard.clone())
        .with_state_guard(input.authority.rbac_guard.clone());
    if let Some(scope_guard) = &input.authority.target_scope_guard {
        commit = commit.with_state_guard(scope_guard.clone());
    }
    Ok(commit)
}

fn apply_action(
    command: &ResponsibilityAssignmentCommand,
    current: Option<&ResponsibilityAssignment>,
    id: ResponsibilityAssignmentId,
    authority: &ResponsibilityCommandAuthority,
    now: u64,
    catalog: &ResponsibilityAssignmentCatalog,
) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
    AssignmentActionApplication {
        command,
        current,
        id,
        authority,
        now,
        catalog,
    }
    .apply()
}

struct AssignmentActionApplication<'input> {
    command: &'input ResponsibilityAssignmentCommand,
    current: Option<&'input ResponsibilityAssignment>,
    id: ResponsibilityAssignmentId,
    authority: &'input ResponsibilityCommandAuthority,
    now: u64,
    catalog: &'input ResponsibilityAssignmentCatalog,
}

impl AssignmentActionApplication<'_> {
    fn apply(&self) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        match &self.command.action {
            ResponsibilityAssignmentAction::Assign {
                principal_user_id,
                expires_at_millis,
            } => self.assign(principal_user_id, *expires_at_millis),
            ResponsibilityAssignmentAction::Accept => self.accept(),
            ResponsibilityAssignmentAction::Reassign {
                principal_user_id,
                expires_at_millis,
            } => self.reassign(principal_user_id, *expires_at_millis),
            ResponsibilityAssignmentAction::Expire => self.expire(),
            ResponsibilityAssignmentAction::RevokeDeparted => self.revoke_departed(),
        }
    }

    fn assign(
        &self,
        principal_user_id: &UserId,
        expires_at_millis: Option<u64>,
    ) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        if self.current.is_some() {
            return Err(wrong_state());
        }
        validate_new_responsibility(
            self.catalog,
            &self.command.target,
            self.command.role,
            principal_user_id,
            expires_at_millis,
            self.now,
        )?;
        Ok(new_assignment(
            self.command,
            self.id.clone(),
            principal_user_id.clone(),
            expires_at_millis,
            self.authority,
            self.now,
        ))
    }

    fn accept(&self) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        let current = self.current.ok_or_else(not_found)?;
        if current.effective_state(self.now) == ResponsibilityAssignmentState::Expired {
            return Err(assignment_expired());
        }
        if current.state != ResponsibilityAssignmentState::PendingAcceptance {
            return Err(wrong_state());
        }
        let Actor::UserActor(UserActor { id: actor_id, .. }) = &self.command.context.actor else {
            return Err(authorization_denied());
        };
        if actor_id != &current.principal_user_id {
            return Err(authorization_denied());
        }
        self.update(
            ResponsibilityAssignmentState::Active,
            current.principal_user_id.clone(),
            current.expires_at_millis,
            Some(self.now),
            None,
        )
    }

    fn reassign(
        &self,
        principal_user_id: &UserId,
        expires_at_millis: Option<u64>,
    ) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        self.current.ok_or_else(not_found)?;
        validate_new_responsibility(
            self.catalog,
            &self.command.target,
            self.command.role,
            principal_user_id,
            expires_at_millis,
            self.now,
        )?;
        self.update(
            ResponsibilityAssignmentState::PendingAcceptance,
            principal_user_id.clone(),
            expires_at_millis,
            None,
            None,
        )
    }

    fn expire(&self) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        let current = self.current.ok_or_else(not_found)?;
        if !matches!(
            current.state,
            ResponsibilityAssignmentState::PendingAcceptance
                | ResponsibilityAssignmentState::Active
        ) || current
            .expires_at_millis
            .is_none_or(|expires_at| self.now < expires_at)
        {
            return Err(wrong_state());
        }
        self.update(
            ResponsibilityAssignmentState::Expired,
            current.principal_user_id.clone(),
            current.expires_at_millis,
            current.accepted_at_millis,
            Some(self.now),
        )
    }

    fn revoke_departed(&self) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        let current = self.current.ok_or_else(not_found)?;
        if !matches!(
            current.state,
            ResponsibilityAssignmentState::PendingAcceptance
                | ResponsibilityAssignmentState::Active
        ) || self.authority.principal.active
        {
            return Err(wrong_state());
        }
        self.update(
            ResponsibilityAssignmentState::Revoked,
            current.principal_user_id.clone(),
            current.expires_at_millis,
            current.accepted_at_millis,
            Some(self.now),
        )
    }

    fn update(
        &self,
        state: ResponsibilityAssignmentState,
        principal_user_id: UserId,
        expires_at_millis: Option<u64>,
        accepted_at_millis: Option<u64>,
        ended_at_millis: Option<u64>,
    ) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
        updated_assignment(
            self.current.ok_or_else(not_found)?,
            self.command,
            self.authority,
            AssignmentUpdate {
                state,
                principal_user_id,
                expires_at_millis,
                accepted_at_millis,
                ended_at_millis,
                now: self.now,
            },
        )
    }
}

struct AssignmentUpdate {
    state: ResponsibilityAssignmentState,
    principal_user_id: UserId,
    expires_at_millis: Option<u64>,
    accepted_at_millis: Option<u64>,
    ended_at_millis: Option<u64>,
    now: u64,
}

fn updated_assignment(
    current: &ResponsibilityAssignment,
    command: &ResponsibilityAssignmentCommand,
    authority: &ResponsibilityCommandAuthority,
    update: AssignmentUpdate,
) -> Result<ResponsibilityAssignment, ResponsibilityAssignmentError> {
    Ok(ResponsibilityAssignment {
        id: current.id.clone(),
        scope: current.scope.clone(),
        target: current.target.clone(),
        role: current.role,
        principal_user_id: update.principal_user_id,
        state: update.state,
        revision: next_revision(current.revision)?,
        assigned_by: command.context.actor.clone(),
        assigned_at_millis: if matches!(
            command.action,
            ResponsibilityAssignmentAction::Reassign { .. }
        ) {
            update.now
        } else {
            current.assigned_at_millis
        },
        accepted_at_millis: update.accepted_at_millis,
        expires_at_millis: update.expires_at_millis,
        ended_at_millis: update.ended_at_millis,
        target_revision: authority.target_revision,
        target_sha256: authority.target_sha256.clone(),
        rbac_revision: authority.rbac_revision,
        rbac_sha256: authority.rbac_sha256.clone(),
    })
}

fn new_assignment(
    command: &ResponsibilityAssignmentCommand,
    id: ResponsibilityAssignmentId,
    principal_user_id: UserId,
    expires_at_millis: Option<u64>,
    authority: &ResponsibilityCommandAuthority,
    now: u64,
) -> ResponsibilityAssignment {
    ResponsibilityAssignment {
        id,
        scope: command.context.scope.clone(),
        target: command.target.clone(),
        role: command.role,
        principal_user_id,
        state: ResponsibilityAssignmentState::PendingAcceptance,
        revision: 1,
        assigned_by: command.context.actor.clone(),
        assigned_at_millis: now,
        accepted_at_millis: None,
        expires_at_millis,
        ended_at_millis: None,
        target_revision: authority.target_revision,
        target_sha256: authority.target_sha256.clone(),
        rbac_revision: authority.rbac_revision,
        rbac_sha256: authority.rbac_sha256.clone(),
    }
}

fn validate_new_responsibility(
    catalog: &ResponsibilityAssignmentCatalog,
    target: &ResponsibilityTarget,
    role: ResponsibilityRole,
    principal: &UserId,
    expires_at: Option<u64>,
    now: u64,
) -> Result<(), ResponsibilityAssignmentError> {
    validate_id(&principal.0, "usr_")?;
    if expires_at.is_some_and(|expires_at| expires_at <= now || expires_at > MAX_SAFE_INTEGER) {
        return Err(invalid());
    }
    if catalog.assignments.values().any(|assignment| {
        duty_boundary(&assignment.target) == duty_boundary(target)
            && assignment.role != role
            && assignment.principal_user_id == *principal
            && matches!(
                assignment.effective_state(now),
                ResponsibilityAssignmentState::PendingAcceptance
                    | ResponsibilityAssignmentState::Active
            )
    }) {
        return Err(separation_violation());
    }
    Ok(())
}

fn duty_boundary(target: &ResponsibilityTarget) -> (&'static str, &str) {
    match target {
        ResponsibilityTarget::ProductSession { product_session_id } => {
            ("product-session", product_session_id.0.as_str())
        }
        ResponsibilityTarget::Delivery { delivery_id }
        | ResponsibilityTarget::DeliveryStage { delivery_id, .. }
        | ResponsibilityTarget::Review { delivery_id, .. } => ("delivery", delivery_id.0.as_str()),
    }
}

fn validate_command_authority(
    command: &ResponsibilityAssignmentCommand,
    current: Option<&ResponsibilityAssignment>,
    authority: &ResponsibilityCommandAuthority,
    assignment_stream_id: &str,
) -> Result<(), ResponsibilityAssignmentError> {
    let requested_principal = match &command.action {
        ResponsibilityAssignmentAction::Assign {
            principal_user_id, ..
        }
        | ResponsibilityAssignmentAction::Reassign {
            principal_user_id, ..
        } => principal_user_id,
        ResponsibilityAssignmentAction::Accept
        | ResponsibilityAssignmentAction::Expire
        | ResponsibilityAssignmentAction::RevokeDeparted => {
            &current.ok_or_else(not_found)?.principal_user_id
        }
    };
    if authority.actor != command.context.actor
        || authority.scope != command.context.scope
        || authority.operation != command.action.operation()
        || authority.target != command.target
        || authority.role != command.role
        || authority.principal.user_id != *requested_principal
    {
        return Err(scope_denied());
    }
    validate_digest(&authority.target_sha256)?;
    validate_digest(&authority.rbac_sha256)?;
    if authority.target_revision == 0
        || authority.target_revision > MAX_SAFE_INTEGER
        || authority.rbac_revision == 0
        || authority.rbac_revision > MAX_SAFE_INTEGER
    {
        return Err(invalid());
    }
    if !authority.permission_granted || !authority.actor_active {
        return Err(authorization_denied());
    }
    match command.action {
        ResponsibilityAssignmentAction::Assign { .. }
        | ResponsibilityAssignmentAction::Accept
        | ResponsibilityAssignmentAction::Reassign { .. } => {
            if !authority.principal.active {
                return Err(member_inactive());
            }
            if !authority.principal.role_eligible {
                return Err(role_ineligible());
            }
        }
        ResponsibilityAssignmentAction::Expire => {}
        ResponsibilityAssignmentAction::RevokeDeparted => {
            if authority.principal.active {
                return Err(wrong_state());
            }
        }
    }
    validate_authority_guards(authority, assignment_stream_id)
}

fn validate_list_authority(
    request: &ResponsibilityAssignmentListRequest,
    authority: &ResponsibilityListAuthority,
) -> Result<(), ResponsibilityAssignmentError> {
    if authority.actor != request.actor || authority.scope != request.scope {
        return Err(scope_denied());
    }
    validate_digest(&authority.rbac_sha256)?;
    if authority.rbac_revision == 0 || authority.rbac_revision > MAX_SAFE_INTEGER {
        return Err(invalid());
    }
    if !authority.permission_granted || !authority.actor_active {
        return Err(authorization_denied());
    }
    Ok(())
}

fn validate_inbox_authority(
    request: &ResponsibilityAssignmentListRequest,
    authority: &ResponsibilityInboxAuthority,
) -> Result<(), ResponsibilityAssignmentError> {
    if authority.actor != request.actor || authority.scope != request.scope {
        return Err(scope_denied());
    }
    validate_digest(&authority.rbac_sha256)?;
    if authority.rbac_revision == 0
        || authority.rbac_revision > MAX_SAFE_INTEGER
        || authority.rbac_guard.expected_revision() != authority.rbac_revision
        || authority.rbac_guard.stream_id().is_empty()
    {
        return Err(target_unavailable());
    }
    if !authority.permission_granted || !authority.actor_active {
        return Err(authorization_denied());
    }
    Ok(())
}

fn validate_authority_guards(
    authority: &ResponsibilityCommandAuthority,
    assignment_stream_id: &str,
) -> Result<(), ResponsibilityAssignmentError> {
    if authority.target_guard.stream_id() == authority.rbac_guard.stream_id()
        || authority.target_guard.stream_id() == assignment_stream_id
        || authority.rbac_guard.stream_id() == assignment_stream_id
        || authority.target_guard.expected_revision() == 0
        || authority.target_guard.expected_revision() > MAX_SAFE_INTEGER
        || authority.rbac_guard.expected_revision() != authority.rbac_revision
    {
        return Err(target_unavailable());
    }
    if let Some(scope_guard) = &authority.target_scope_guard
        && (scope_guard.expected_revision() == 0
            || scope_guard.expected_revision() > MAX_SAFE_INTEGER
            || scope_guard.stream_id() == authority.target_guard.stream_id()
            || scope_guard.stream_id() == authority.rbac_guard.stream_id()
            || scope_guard.stream_id() == assignment_stream_id)
    {
        return Err(target_unavailable());
    }
    Ok(())
}

fn validate_command(
    command: &ResponsibilityAssignmentCommand,
) -> Result<(), ResponsibilityAssignmentError> {
    validate_scope(&command.context.scope)?;
    validate_request_id(&command.context.request_id)?;
    if command.context.expected_revision > MAX_SAFE_INTEGER
        || command.context.authenticated_scopes.is_empty()
    {
        return Err(invalid());
    }
    validate_target(&command.target)
}

fn validate_target(target: &ResponsibilityTarget) -> Result<(), ResponsibilityAssignmentError> {
    match target {
        ResponsibilityTarget::ProductSession { product_session_id } => {
            validate_id(&product_session_id.0, "psn_")
        }
        ResponsibilityTarget::Delivery { delivery_id }
        | ResponsibilityTarget::DeliveryStage { delivery_id, .. }
        | ResponsibilityTarget::Review { delivery_id, .. } => validate_id(&delivery_id.0, "dlv_"),
    }
}

fn validate_scope(scope: &RepositoryScope) -> Result<(), ResponsibilityAssignmentError> {
    validate_id(&scope.organization_id.0, "org_")?;
    validate_id(&scope.workspace_id.0, "wsp_")?;
    validate_id(&scope.project_id.0, "prj_")?;
    validate_id(&scope.repository_id.0, "rep_")
}

fn validate_catalog(
    catalog: &ResponsibilityAssignmentCatalog,
    stream_id: &str,
    stored_revision: u64,
    scope: &RepositoryScope,
) -> Result<(), ResponsibilityAssignmentError> {
    if catalog.schema != CATALOG_SCHEMA
        || &catalog.scope != scope
        || catalog.revision != stored_revision
        || catalog.revision > MAX_SAFE_INTEGER
        || catalog.assignments.len() > MAX_ASSIGNMENTS_PER_SCOPE
    {
        return Err(corrupt());
    }
    if catalog
        .assignments
        .iter()
        .any(|(key, assignment)| validate_assignment_record(key, assignment, scope).is_err())
    {
        return Err(corrupt());
    }
    if stream_id != catalog_stream_for_scope(scope)? {
        return Err(corrupt());
    }
    Ok(())
}

fn validate_assignment_record(
    key: &str,
    assignment: &ResponsibilityAssignment,
    scope: &RepositoryScope,
) -> Result<(), ResponsibilityAssignmentError> {
    if key != assignment.id.0.as_str()
        || assignment.scope != *scope
        || assignment.revision == 0
        || assignment.revision > MAX_SAFE_INTEGER
        || assignment.target_revision == 0
        || assignment.target_revision > MAX_SAFE_INTEGER
        || assignment.rbac_revision == 0
        || assignment.rbac_revision > MAX_SAFE_INTEGER
        || validate_assignment_id(&assignment.id).is_err()
        || !assignment_id(scope, &assignment.target, assignment.role)
            .is_ok_and(|expected| expected == assignment.id)
        || validate_target(&assignment.target).is_err()
        || validate_id(&assignment.principal_user_id.0, "usr_").is_err()
        || validate_digest(&assignment.target_sha256).is_err()
        || validate_digest(&assignment.rbac_sha256).is_err()
        || validate_assignment_times(assignment).is_err()
    {
        return Err(corrupt());
    }
    Ok(())
}

fn validate_assignment_times(
    assignment: &ResponsibilityAssignment,
) -> Result<(), ResponsibilityAssignmentError> {
    validate_time(assignment.assigned_at_millis)?;
    for value in [
        assignment.accepted_at_millis,
        assignment.expires_at_millis,
        assignment.ended_at_millis,
    ]
    .into_iter()
    .flatten()
    {
        validate_time(value)?;
        if value < assignment.assigned_at_millis {
            return Err(corrupt());
        }
    }
    let lifecycle_valid = match assignment.state {
        ResponsibilityAssignmentState::PendingAcceptance => {
            assignment.accepted_at_millis.is_none() && assignment.ended_at_millis.is_none()
        }
        ResponsibilityAssignmentState::Active => {
            assignment.accepted_at_millis.is_some() && assignment.ended_at_millis.is_none()
        }
        ResponsibilityAssignmentState::Expired | ResponsibilityAssignmentState::Revoked => {
            assignment.ended_at_millis.is_some()
        }
    };
    if !lifecycle_valid {
        return Err(corrupt());
    }
    Ok(())
}

fn list_matches(
    assignment: &ResponsibilityAssignment,
    request: &ResponsibilityAssignmentListRequest,
    now: u64,
) -> bool {
    request
        .target
        .as_ref()
        .is_none_or(|target| &assignment.target == target)
        && request.role.is_none_or(|role| assignment.role == role)
        && request
            .principal_user_id
            .as_ref()
            .is_none_or(|principal| &assignment.principal_user_id == principal)
        && (request.include_ended
            || matches!(
                assignment.effective_state(now),
                ResponsibilityAssignmentState::PendingAcceptance
                    | ResponsibilityAssignmentState::Active
            ))
}

fn replay_receipt(
    receipt: &CommitReceipt,
    replayed: bool,
) -> Result<ResponsibilityAssignmentReceipt, ResponsibilityAssignmentError> {
    if receipt.events.len() != 1 || receipt.events[0].topic != EVENT_TOPIC {
        return Err(corrupt());
    }
    let event: ResponsibilityAssignmentEvent =
        serde_json::from_slice(&receipt.events[0].payload).map_err(|_| corrupt())?;
    if event.receipt.catalog_revision != receipt.revision || event.receipt.assignment.revision == 0
    {
        return Err(corrupt());
    }
    Ok(ResponsibilityAssignmentReceipt {
        replayed,
        ..event.receipt
    })
}

fn pending_audit(
    command: &ResponsibilityAssignmentCommand,
    before: Option<&[u8]>,
    after: &[u8],
    now: u64,
    event_id: &str,
) -> Result<PendingAuditEvent, ResponsibilityAssignmentError> {
    let audit_id =
        AuditEventId::from_digest(&digest_bytes(event_id.as_bytes())).map_err(|_| invalid())?;
    let audit = AuditEvent::state_change(
        audit_id.clone(),
        now,
        audit_actor(&command.context.actor),
        AuditScope::repository(
            command.context.scope.organization_id.clone(),
            command.context.scope.workspace_id.clone(),
            command.context.scope.project_id.clone(),
            command.context.scope.repository_id.clone(),
        )
        .map_err(|_| invalid())?,
        command.context.request_id.clone(),
        AuditAction::administration(action_name(command.action.operation()))
            .map_err(|_| invalid())?,
        AuditState::changed(before.map(digest_bytes), digest_bytes(after))
            .map_err(|_| invalid())?,
        AuditOrigin::local(AUDIT_ORIGIN).map_err(|_| invalid())?,
        AuditSubject::new(),
        "completed",
        AuditRetention::Indefinite,
    )
    .map_err(|_| invalid())?;
    let payload = serde_json::to_vec(&audit).map_err(|_| invalid())?;
    PendingAuditEvent::new(audit_id.as_str(), payload).map_err(Into::into)
}

fn audit_actor(actor: &Actor) -> AuditActor {
    match actor {
        Actor::UserActor(actor) => AuditActor::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => AuditActor::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => AuditActor::System(actor.id.clone()),
    }
}

fn action_name(operation: ResponsibilityAssignmentOperation) -> &'static str {
    match operation {
        ResponsibilityAssignmentOperation::Assign => "responsibility.assignment.assign",
        ResponsibilityAssignmentOperation::Accept => "responsibility.assignment.accept",
        ResponsibilityAssignmentOperation::Reassign => "responsibility.assignment.reassign",
        ResponsibilityAssignmentOperation::Expire => "responsibility.assignment.expire",
        ResponsibilityAssignmentOperation::RevokeDeparted => {
            "responsibility.assignment.revoke-departed"
        }
        ResponsibilityAssignmentOperation::List => "responsibility.assignment.list",
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandDigest<'command> {
    actor: &'command Actor,
    scope: &'command RepositoryScope,
    expected_revision: u64,
    target: &'command ResponsibilityTarget,
    role: ResponsibilityRole,
    action: &'command ResponsibilityAssignmentAction,
}

fn command_digest(
    command: &ResponsibilityAssignmentCommand,
) -> Result<Sha256Digest, ResponsibilityAssignmentError> {
    let bytes = serde_json::to_vec(&CommandDigest {
        actor: &command.context.actor,
        scope: &command.context.scope,
        expected_revision: command.context.expected_revision,
        target: &command.target,
        role: command.role,
        action: &command.action,
    })
    .map_err(|_| invalid())?;
    Ok(digest_bytes(&bytes))
}

fn assignment_id(
    scope: &RepositoryScope,
    target: &ResponsibilityTarget,
    role: ResponsibilityRole,
) -> Result<ResponsibilityAssignmentId, ResponsibilityAssignmentError> {
    let bytes = serde_json::to_vec(&(scope, target, role)).map_err(|_| invalid())?;
    let digest = Sha256::digest(bytes);
    Ok(ResponsibilityAssignmentId(format!(
        "asn_{}",
        hex_lower(&digest)
    )))
}

fn catalog_stream(scope_key: &[u8]) -> String {
    let digest = Sha256::digest(scope_key);
    format!("{STREAM_PREFIX}{}", hex_lower(&digest))
}

fn catalog_stream_for_scope(
    scope: &RepositoryScope,
) -> Result<String, ResponsibilityAssignmentError> {
    let scope_key = receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .map_err(|_| invalid())?;
    Ok(catalog_stream(scope_key.as_bytes()))
}

fn event_id(
    identity: &winwincode_storage::ReceiptIdentity,
    command_digest: &Sha256Digest,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.responsibility-assignment.event.v1\0");
    digest.update(identity.actor_key().as_bytes());
    digest.update([0]);
    digest.update(identity.scope_key().as_bytes());
    digest.update([0]);
    digest.update(identity.request_id().0.as_bytes());
    digest.update([0]);
    digest.update(command_digest.0.as_bytes());
    format!("assignment-{}", hex_lower(&digest.finalize()))
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", hex_lower(&Sha256::digest(bytes))))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), ResponsibilityAssignmentError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(invalid());
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_id(value: &str, prefix: &str) -> Result<(), ResponsibilityAssignmentError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(invalid());
    };
    if suffix.is_empty()
        || value.len() > 128
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_assignment_id(
    assignment_id: &ResponsibilityAssignmentId,
) -> Result<(), ResponsibilityAssignmentError> {
    let Some(digest) = assignment_id.0.strip_prefix("asn_") else {
        return Err(corrupt());
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(corrupt());
    }
    Ok(())
}

fn validate_request_id(request_id: &RequestId) -> Result<(), ResponsibilityAssignmentError> {
    validate_id(&request_id.0, "req_")
}

fn validate_time(value: u64) -> Result<(), ResponsibilityAssignmentError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(invalid());
    }
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, ResponsibilityAssignmentError> {
    revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(invalid)
}

fn require_expected_revision(
    current: Option<&ResponsibilityAssignment>,
    expected: u64,
) -> Result<(), ResponsibilityAssignmentError> {
    if current.map_or(0, |assignment| assignment.revision) != expected {
        return Err(revision_conflict());
    }
    Ok(())
}

fn authority_error(error: ResponsibilityAuthorityError) -> ResponsibilityAssignmentError {
    match error {
        ResponsibilityAuthorityError::Denied => authorization_denied(),
        ResponsibilityAuthorityError::Unavailable => target_unavailable(),
    }
}

impl From<StorageError> for ResponsibilityAssignmentError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => invalid(),
            StorageErrorKind::RevisionConflict => revision_conflict(),
            StorageErrorKind::RequestConflict => ResponsibilityAssignmentError::new(
                ResponsibilityAssignmentErrorKind::RequestConflict,
                "responsibility request identity was reused with changed input",
            ),
            StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => storage_error(),
        }
    }
}

const fn invalid() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::InvalidRequest,
        "responsibility assignment request is invalid",
    )
}

const fn scope_denied() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::ScopeDenied,
        "responsibility assignment scope is denied",
    )
}

const fn authorization_denied() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::AuthorizationDenied,
        "responsibility assignment authorization is denied",
    )
}

const fn member_inactive() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::MemberInactive,
        "responsibility principal is not an active organization member",
    )
}

const fn role_ineligible() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::RoleIneligible,
        "responsibility principal is not eligible for this role",
    )
}

const fn target_unavailable() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::TargetUnavailable,
        "responsibility target authority is unavailable",
    )
}

const fn not_found() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::NotFound,
        "responsibility assignment was not found",
    )
}

const fn wrong_state() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::WrongState,
        "responsibility assignment state rejects this operation",
    )
}

const fn separation_violation() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::SeparationViolation,
        "responsibility separation of duties would be violated",
    )
}

const fn assignment_expired() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::AssignmentExpired,
        "responsibility assignment has expired",
    )
}

const fn revision_conflict() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::RevisionConflict,
        "responsibility assignment revision is stale",
    )
}

const fn authority_changed() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::AuthorityChanged,
        "responsibility authority changed before commit",
    )
}

const fn corrupt() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::CorruptState,
        "responsibility assignment durable state is corrupt",
    )
}

const fn storage_error() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::Storage,
        "responsibility assignment storage is unavailable",
    )
}

const fn clock_unavailable() -> ResponsibilityAssignmentError {
    ResponsibilityAssignmentError::new(
        ResponsibilityAssignmentErrorKind::ClockUnavailable,
        "responsibility assignment clock is unavailable",
    )
}
