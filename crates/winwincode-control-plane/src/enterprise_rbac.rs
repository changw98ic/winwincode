// SPDX-License-Identifier: Apache-2.0

//! Durable Organization, Team, Membership, and versioned RBAC authority.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ActorId, ControlPlaneWebSocketEnterpriseMembershipInvalidatedEvent,
    ControlPlaneWebSocketEnterpriseMembershipInvalidatedEventTypeValue,
    ControlPlaneWebSocketEnterpriseMembershipListReloadQuery,
    ControlPlaneWebSocketEnterpriseOrganizationInvalidatedEvent,
    ControlPlaneWebSocketEnterpriseOrganizationInvalidatedEventTypeValue,
    ControlPlaneWebSocketEnterpriseOrganizationListReloadQuery,
    ControlPlaneWebSocketEnterpriseRoleInvalidatedEvent,
    ControlPlaneWebSocketEnterpriseRoleInvalidatedEventTypeValue,
    ControlPlaneWebSocketEnterpriseRoleListReloadQuery,
    ControlPlaneWebSocketEnterpriseTeamInvalidatedEvent,
    ControlPlaneWebSocketEnterpriseTeamInvalidatedEventTypeValue,
    ControlPlaneWebSocketEnterpriseTeamListReloadQuery, EnterpriseMembershipListQuery,
    EnterpriseMembershipListResultResponse, EnterpriseMembershipListResultResponseQuery,
    EnterpriseMembershipPage, EnterpriseMembershipPageKind, EnterpriseMembershipProjection,
    EnterpriseMembershipUpdateCommand, EnterpriseMembershipUpdateCompletedResponse,
    EnterpriseMembershipUpdateCompletedResponseCommand,
    EnterpriseMembershipUpdateCompletedResponseOutcome, EnterpriseOrganizationListQuery,
    EnterpriseOrganizationListResultResponse, EnterpriseOrganizationListResultResponseQuery,
    EnterpriseOrganizationPage, EnterpriseOrganizationPageKind, EnterpriseOrganizationProjection,
    EnterpriseOrganizationUpdateCommand, EnterpriseOrganizationUpdateCompletedResponse,
    EnterpriseOrganizationUpdateCompletedResponseCommand,
    EnterpriseOrganizationUpdateCompletedResponseOutcome, EnterprisePermission,
    EnterpriseRoleAssignment, EnterpriseRoleListQuery, EnterpriseRoleListResultResponse,
    EnterpriseRoleListResultResponseQuery, EnterpriseRolePage, EnterpriseRolePageKind,
    EnterpriseRolePermissionRule, EnterpriseRoleProjection, EnterpriseRoleUpdateCommand,
    EnterpriseRoleUpdateCompletedResponse, EnterpriseRoleUpdateCompletedResponseCommand,
    EnterpriseRoleUpdateCompletedResponseOutcome, EnterpriseRoleVersionReference,
    EnterpriseTeamListQuery, EnterpriseTeamListResultResponse,
    EnterpriseTeamListResultResponseQuery, EnterpriseTeamPage, EnterpriseTeamPageKind,
    EnterpriseTeamProjection, EnterpriseTeamUpdateCommand, EnterpriseTeamUpdateCompletedResponse,
    EnterpriseTeamUpdateCompletedResponseCommand, EnterpriseTeamUpdateCompletedResponseOutcome,
    OrganizationScope, PageInfo, Scope,
};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditSubject,
};
use winwincode_domain::{
    ControlPlaneEventId, EnterpriseMembershipId, EnterpriseRoleId, EnterpriseRoleVersion,
    EnterpriseTeamId, Instant, OpaqueCursor, OrganizationId, RequestId, Revision, SchemaVersion,
    Sha256Digest,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, PendingAuditEvent, ProductStateStorage, ProjectionEventStream,
    PublicEventScope, PublicEventSource, StateCommit, StateRevisionGuard, StorageError,
    StorageErrorKind, StoredState,
};

use crate::{command_receipt_identity, instant_from_millis, public_event_actor};

const STATE_SCHEMA: &str = "winwincode.enterprise-rbac.v1";
const STREAM_PREFIX: &str = "enterprise-rbac:";
const RECEIPT_TOPIC: &str = "enterprise.rbac.lifecycle.v1";
const AUDIT_ORIGIN: &str = "control-plane.enterprise-rbac";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_SAFE_INTEGER_I64: i64 = 9_007_199_254_740_991;
const MAX_PAGE_SIZE: usize = 200;
const MAX_CURSOR_BYTES: usize = 2_048;
const CURSOR_SCHEMA: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseRbacErrorKind {
    InvalidRequest,
    ScopeDenied,
    NotFound,
    WrongState,
    RevisionConflict,
    RequestConflict,
    Storage,
    ClockUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseRbacError {
    kind: EnterpriseRbacErrorKind,
    message: &'static str,
}

impl EnterpriseRbacError {
    const fn new(kind: EnterpriseRbacErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> EnterpriseRbacErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseRbacError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for EnterpriseRbacError {}

impl From<StorageError> for EnterpriseRbacError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => invalid(),
            StorageErrorKind::RevisionConflict => revision_conflict(),
            StorageErrorKind::RequestConflict => EnterpriseRbacError::new(
                EnterpriseRbacErrorKind::RequestConflict,
                "RBAC request identity was reused with different input",
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
pub struct EnterpriseRbacClockError;

pub trait EnterpriseRbacClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a bounded error when the trusted clock is unavailable.
    fn now_millis(&mut self) -> Result<u64, EnterpriseRbacClockError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnterpriseRbacClock;

impl EnterpriseRbacClock for SystemEnterpriseRbacClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseRbacClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EnterpriseRbacClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| EnterpriseRbacClockError)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RbacDenialReason {
    UnauthenticatedScope,
    OrganizationUnavailable,
    OrganizationInactive,
    MembershipMissing,
    MembershipInactive,
    SeparationOfDuties,
    ExplicitDeny,
    DefaultDeny,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluatedRoleVersion {
    pub role_id: EnterpriseRoleId,
    pub role_version: EnterpriseRoleVersion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RbacDecision {
    pub allowed: bool,
    pub denial_reason: Option<RbacDenialReason>,
    pub authority_revision: Revision,
    pub membership_id: Option<EnterpriseMembershipId>,
    pub evaluated_role_versions: Vec<EvaluatedRoleVersion>,
    pub authority_seal: Option<RbacAuthoritySeal>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RbacAuthoritySeal {
    pub revision: Revision,
    pub state_sha256: Sha256Digest,
    pub state_guard: StateRevisionGuard,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveMemberContext {
    pub membership_id: EnterpriseMembershipId,
    pub actor: Actor,
    pub organization_id: OrganizationId,
    pub authority_revision: Revision,
    pub active_role_versions: Vec<EvaluatedRoleVersion>,
    pub authority_seal: RbacAuthoritySeal,
}

/// Current active Team membership sealed to the same RBAC state revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTeamContext {
    pub team_ids: Vec<EnterpriseTeamId>,
    pub authority_seal: RbacAuthoritySeal,
}

pub struct EnterpriseRbacService {
    inner: Mutex<EnterpriseRbacInner>,
}

struct EnterpriseRbacInner {
    storage: Box<dyn ProductStateStorage>,
    clock: Box<dyn EnterpriseRbacClock>,
}

impl EnterpriseRbacService {
    #[must_use]
    pub fn new(storage: Box<dyn ProductStateStorage>) -> Self {
        Self::with_clock(storage, Box::new(SystemEnterpriseRbacClock))
    }

    #[must_use]
    pub fn with_clock(
        storage: Box<dyn ProductStateStorage>,
        clock: Box<dyn EnterpriseRbacClock>,
    ) -> Self {
        Self {
            inner: Mutex::new(EnterpriseRbacInner { storage, clock }),
        }
    }

    /// Creates or updates the one organization authority head.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope, stale revision, changed replay, or unavailable storage.
    pub fn update_organization(
        &self,
        command: &EnterpriseOrganizationUpdateCommand,
    ) -> Result<EnterpriseOrganizationUpdateCompletedResponse, EnterpriseRbacError> {
        let Scope::OrganizationScope(scope) = &command.scope else {
            return Err(scope_denied());
        };
        if scope.organization_id != command.payload.organization_id {
            return Err(scope_denied());
        }
        let meta = CommandMeta::new(
            &command.actor,
            scope,
            &command.request_id,
            &command.schema_version,
            command.expected_revision.0,
            "rbac.organization.update",
            EventKind::Organization,
        )?;
        self.lock()?.mutate(command, &meta, |state, now, revision| {
            validate_display_name(&command.payload.display_name)?;
            validate_slug(&command.payload.slug)?;
            validate_value(&command.payload.state, &["active", "suspended", "archived"])?;
            state.organization = Some(OrganizationRecord {
                id: command.payload.organization_id.clone(),
                slug: command.payload.slug.clone(),
                display_name: command.payload.display_name.clone(),
                state: command.payload.state.clone(),
                revision,
                updated_at: now.clone(),
            });
            Ok(EnterpriseOrganizationUpdateCompletedResponse {
                command: EnterpriseOrganizationUpdateCompletedResponseCommand::EnterpriseOrganizationUpdate,
                current_revision: Revision(i64_revision(revision)?),
                outcome: EnterpriseOrganizationUpdateCompletedResponseOutcome::Completed,
                previous_revision: Revision(i64_revision(revision - 1)?),
                request_id: command.request_id.clone(),
                result: organization_projection(
                    state.organization.as_ref().ok_or_else(storage_unavailable)?,
                )?,
                schema_version: command.schema_version.clone(),
            })
        })
    }

    /// Writes a versioned role head. Every successful write creates a new immutable role version.
    ///
    /// # Errors
    ///
    /// Rejects cycles, foreign references, invalid rules, stale revisions, and reactivation.
    pub fn update_role(
        &self,
        command: &EnterpriseRoleUpdateCommand,
    ) -> Result<EnterpriseRoleUpdateCompletedResponse, EnterpriseRbacError> {
        let meta = CommandMeta::new(
            &command.actor,
            &command.scope,
            &command.request_id,
            &command.schema_version,
            command.expected_revision.0,
            "rbac.role.update",
            EventKind::Role,
        )?;
        self.lock()?.mutate(command, &meta, |state, now, revision| {
            require_active_organization(state)?;
            validate_role_payload(command, state)?;
            let key = command.payload.role_id.0.clone();
            let next_version = state
                .roles
                .get(&key)
                .map_or(1, |role| role.current_version + 1);
            if state
                .roles
                .get(&key)
                .is_some_and(|role| role.state == "revoked")
                && command.payload.state != "revoked"
            {
                return Err(wrong_state());
            }
            let version = RoleVersionRecord {
                version: next_version,
                rules: command.payload.rules.clone(),
                inherited_roles: command.payload.inherited_roles.clone(),
                conflicting_role_ids: command.payload.conflicting_role_ids.clone(),
            };
            let role = state.roles.entry(key).or_insert_with(|| RoleRecord {
                id: command.payload.role_id.clone(),
                display_name: command.payload.display_name.clone(),
                state: command.payload.state.clone(),
                current_version: next_version,
                versions: BTreeMap::new(),
                revision,
                updated_at: now.clone(),
            });
            role.display_name.clone_from(&command.payload.display_name);
            role.state.clone_from(&command.payload.state);
            role.current_version = next_version;
            role.versions.insert(next_version, version);
            role.revision = revision;
            role.updated_at.clone_from(now);
            validate_role_graph(state, &command.payload.role_id, next_version)?;
            validate_active_members(state, instant_millis(now)?)?;
            Ok(EnterpriseRoleUpdateCompletedResponse {
                command: EnterpriseRoleUpdateCompletedResponseCommand::EnterpriseRoleUpdate,
                current_revision: Revision(i64_revision(revision)?),
                outcome: EnterpriseRoleUpdateCompletedResponseOutcome::Completed,
                previous_revision: Revision(i64_revision(revision - 1)?),
                request_id: command.request_id.clone(),
                result: role_projection(
                    &meta.scope.organization_id,
                    state
                        .roles
                        .get(&command.payload.role_id.0)
                        .ok_or_else(storage_unavailable)?,
                )?,
                schema_version: command.schema_version.clone(),
            })
        })
    }

    /// Creates or updates one Team and its exact role-version grants.
    ///
    /// # Errors
    ///
    /// Rejects foreign scope, conflicting grants, inactive roles, and stale revisions.
    pub fn update_team(
        &self,
        command: &EnterpriseTeamUpdateCommand,
    ) -> Result<EnterpriseTeamUpdateCompletedResponse, EnterpriseRbacError> {
        let meta = CommandMeta::new(
            &command.actor,
            &command.scope,
            &command.request_id,
            &command.schema_version,
            command.expected_revision.0,
            "rbac.team.update",
            EventKind::Team,
        )?;
        self.lock()?.mutate(command, &meta, |state, now, revision| {
            require_active_organization(state)?;
            validate_display_name(&command.payload.display_name)?;
            validate_value(&command.payload.state, &["active", "disabled"])?;
            validate_assignments(
                state,
                &meta.scope.organization_id,
                &command.payload.role_assignments,
            )?;
            let now_millis = instant_millis(now)?;
            validate_separation(state, &command.payload.role_assignments, now_millis)?;
            let record = TeamRecord {
                id: command.payload.team_id.clone(),
                display_name: command.payload.display_name.clone(),
                state: command.payload.state.clone(),
                role_assignments: command.payload.role_assignments.clone(),
                revision,
                updated_at: now.clone(),
            };
            state
                .teams
                .insert(command.payload.team_id.0.clone(), record);
            validate_active_members(state, now_millis)?;
            Ok(EnterpriseTeamUpdateCompletedResponse {
                command: EnterpriseTeamUpdateCompletedResponseCommand::EnterpriseTeamUpdate,
                current_revision: Revision(i64_revision(revision)?),
                outcome: EnterpriseTeamUpdateCompletedResponseOutcome::Completed,
                previous_revision: Revision(i64_revision(revision - 1)?),
                request_id: command.request_id.clone(),
                result: team_projection(
                    &meta.scope.organization_id,
                    state
                        .teams
                        .get(&command.payload.team_id.0)
                        .ok_or_else(storage_unavailable)?,
                )?,
                schema_version: command.schema_version.clone(),
            })
        })
    }

    /// Creates or updates one actor Membership.
    ///
    /// # Errors
    ///
    /// Rejects duplicate actors, foreign Team/Role references, conflicting grants, and stale revisions.
    pub fn update_membership(
        &self,
        command: &EnterpriseMembershipUpdateCommand,
    ) -> Result<EnterpriseMembershipUpdateCompletedResponse, EnterpriseRbacError> {
        let meta = CommandMeta::new(
            &command.actor,
            &command.scope,
            &command.request_id,
            &command.schema_version,
            command.expected_revision.0,
            "rbac.membership.update",
            EventKind::Membership,
        )?;
        self.lock()?.mutate(command, &meta, |state, now, revision| {
            require_active_organization(state)?;
            validate_display_name(&command.payload.display_name)?;
            validate_value(&command.payload.state, &["invited", "active", "disabled"])?;
            validate_actor_id(&command.payload.actor_id)?;
            let durable_actor_id = durable_actor_id(&command.payload.actor_id);
            if state.memberships.values().any(|member| {
                member.id != command.payload.membership_id
                    && member.actor_id == durable_actor_id
                    && member.state != "disabled"
            }) {
                return Err(invalid());
            }
            for team_id in &command.payload.team_ids {
                let team = state.teams.get(&team_id.0).ok_or_else(not_found)?;
                if team.state != "active" {
                    return Err(wrong_state());
                }
            }
            validate_assignments(
                state,
                &meta.scope.organization_id,
                &command.payload.role_assignments,
            )?;
            let record = MembershipRecord {
                id: command.payload.membership_id.clone(),
                actor_id: durable_actor_id,
                display_name: command.payload.display_name.clone(),
                state: command.payload.state.clone(),
                team_ids: command.payload.team_ids.clone(),
                role_assignments: command.payload.role_assignments.clone(),
                revision,
                updated_at: now.clone(),
            };
            state
                .memberships
                .insert(command.payload.membership_id.0.clone(), record);
            validate_active_members(state, instant_millis(now)?)?;
            Ok(EnterpriseMembershipUpdateCompletedResponse {
                command:
                    EnterpriseMembershipUpdateCompletedResponseCommand::EnterpriseMembershipUpdate,
                current_revision: Revision(i64_revision(revision)?),
                outcome: EnterpriseMembershipUpdateCompletedResponseOutcome::Completed,
                previous_revision: Revision(i64_revision(revision - 1)?),
                request_id: command.request_id.clone(),
                result: membership_projection(
                    &meta.scope.organization_id,
                    state
                        .memberships
                        .get(&command.payload.membership_id.0)
                        .ok_or_else(storage_unavailable)?,
                )?,
                schema_version: command.schema_version.clone(),
            })
        })
    }

    /// Evaluates one permission against current durable membership and role facts.
    ///
    /// # Errors
    ///
    /// Returns an availability error only when current authority cannot be read safely.
    pub fn authorize(
        &self,
        actor: &Actor,
        authenticated_scopes: &[Scope],
        requested_scope: &Scope,
        permission: &EnterprisePermission,
    ) -> Result<RbacDecision, EnterpriseRbacError> {
        if !authenticated_scopes
            .iter()
            .any(|scope| scope == requested_scope)
        {
            return Ok(denied(
                0,
                None,
                RbacDenialReason::UnauthenticatedScope,
                None,
            ));
        }
        self.member_is_eligible(actor, requested_scope, permission)
    }

    /// Evaluates whether one current active member is eligible for a target action.
    ///
    /// This method does not authenticate the caller. Assignment authorities use it
    /// for the subject member after separately authorizing the acting principal,
    /// then attach the returned authority seal to the same durable commit.
    ///
    /// # Errors
    ///
    /// Returns an availability error only when current authority cannot be read safely.
    pub fn member_is_eligible(
        &self,
        actor: &Actor,
        requested_scope: &Scope,
        permission: &EnterprisePermission,
    ) -> Result<RbacDecision, EnterpriseRbacError> {
        let organization_id = scope_organization_id(requested_scope);
        let mut inner = self.lock()?;
        let sealed = inner.load_sealed(&organization_id)?;
        let Some((state, authority_seal)) = sealed else {
            return Ok(denied(
                0,
                None,
                RbacDenialReason::OrganizationUnavailable,
                None,
            ));
        };
        let revision = state.revision;
        let Some(organization) = &state.organization else {
            return Ok(denied(
                revision,
                None,
                RbacDenialReason::OrganizationUnavailable,
                Some(authority_seal),
            ));
        };
        if organization.state != "active" {
            return Ok(denied(
                revision,
                None,
                RbacDenialReason::OrganizationInactive,
                Some(authority_seal),
            ));
        }
        let actor_id = actor_id(actor);
        let Some(member) = state
            .memberships
            .values()
            .find(|member| member.actor_id == actor_id)
        else {
            return Ok(denied(
                revision,
                None,
                RbacDenialReason::MembershipMissing,
                Some(authority_seal),
            ));
        };
        if member.state != "active" {
            return Ok(denied(
                revision,
                Some(member.id.clone()),
                RbacDenialReason::MembershipInactive,
                Some(authority_seal),
            ));
        }
        let now = inner.clock.now_millis().map_err(|_| clock_unavailable())?;
        evaluate_member(
            &state,
            member,
            requested_scope,
            permission,
            now,
            authority_seal,
        )
    }

    /// Loads the active member facts used by assignment authorities.
    ///
    /// # Errors
    ///
    /// Rejects missing, inactive, or unavailable current membership.
    pub fn active_member_context(
        &self,
        actor: &Actor,
        organization_id: &OrganizationId,
    ) -> Result<ActiveMemberContext, EnterpriseRbacError> {
        let mut inner = self.lock()?;
        let (state, authority_seal) = inner.load_sealed(organization_id)?.ok_or_else(not_found)?;
        require_active_organization(&state)?;
        let actor_id = actor_id(actor);
        let member = state
            .memberships
            .values()
            .find(|member| member.actor_id == actor_id)
            .ok_or_else(not_found)?;
        if member.state != "active" {
            return Err(wrong_state());
        }
        let now = inner.clock.now_millis().map_err(|_| clock_unavailable())?;
        let assignments = member_assignments(&state, member);
        let active_role_versions = evaluated_versions(&state, &assignments, now)?;
        Ok(ActiveMemberContext {
            membership_id: member.id.clone(),
            actor: actor.clone(),
            organization_id: organization_id.clone(),
            authority_revision: Revision(i64_revision(state.revision)?),
            active_role_versions,
            authority_seal,
        })
    }

    /// Loads active Team memberships for one current active member.
    ///
    /// # Errors
    ///
    /// Rejects missing or inactive Organization, member, Team, or corrupt
    /// current RBAC state rather than inferring Team membership.
    pub fn active_team_context(
        &self,
        actor: &Actor,
        organization_id: &OrganizationId,
    ) -> Result<ActiveTeamContext, EnterpriseRbacError> {
        let inner = self.lock()?;
        let (state, authority_seal) = inner.load_sealed(organization_id)?.ok_or_else(not_found)?;
        require_active_organization(&state)?;
        let actor_id = actor_id(actor);
        let member = state
            .memberships
            .values()
            .find(|member| member.actor_id == actor_id)
            .ok_or_else(not_found)?;
        if member.state != "active" {
            return Err(wrong_state());
        }
        let mut team_ids = member
            .team_ids
            .iter()
            .map(|team_id| {
                let team = state
                    .teams
                    .get(&team_id.0)
                    .ok_or_else(storage_unavailable)?;
                if team.state != "active" {
                    return Err(wrong_state());
                }
                Ok(team_id.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        team_ids.sort_by(|left, right| left.0.cmp(&right.0));
        team_ids.dedup_by(|left, right| left.0 == right.0);
        Ok(ActiveTeamContext {
            team_ids,
            authority_seal,
        })
    }

    /// Returns the exact current authority state seal and compare-and-swap guard.
    ///
    /// # Errors
    ///
    /// Rejects a missing or corrupt organization authority.
    pub fn authority_seal(
        &self,
        organization_id: &OrganizationId,
    ) -> Result<RbacAuthoritySeal, EnterpriseRbacError> {
        self.lock()?
            .load_sealed(organization_id)?
            .map(|(_, seal)| seal)
            .ok_or_else(not_found)
    }

    /// Reads the exact current Membership projection for one Actor.
    ///
    /// This includes inactive Memberships so lifecycle adapters can preserve
    /// the canonical Team and role assignments while disabling an account.
    ///
    /// # Errors
    ///
    /// Rejects corrupt or unavailable current authority state.
    pub fn membership_by_actor(
        &self,
        actor: &Actor,
        organization_id: &OrganizationId,
    ) -> Result<Option<EnterpriseMembershipProjection>, EnterpriseRbacError> {
        let state = self.lock()?.load(organization_id)?;
        let actor_id = actor_id(actor);
        state
            .as_ref()
            .and_then(|state| {
                state
                    .memberships
                    .values()
                    .find(|membership| membership.actor_id == actor_id)
            })
            .map(|membership| membership_projection(organization_id, membership))
            .transpose()
    }

    /// Reads one exact current Team projection from the canonical RBAC authority.
    ///
    /// # Errors
    ///
    /// Rejects corrupt or unavailable current authority state.
    pub fn team(
        &self,
        organization_id: &OrganizationId,
        team_id: &EnterpriseTeamId,
    ) -> Result<Option<EnterpriseTeamProjection>, EnterpriseRbacError> {
        self.lock()?
            .load(organization_id)?
            .as_ref()
            .and_then(|state| state.teams.get(&team_id.0))
            .map(|team| team_projection(organization_id, team))
            .transpose()
    }

    /// Lists one stable page of Organizations inside the exact query scope.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope, stale cursors, corrupt state, or unavailable storage.
    pub fn list_organizations(
        &self,
        query: &EnterpriseOrganizationListQuery,
    ) -> Result<EnterpriseOrganizationListResultResponse, EnterpriseRbacError> {
        let scope = require_organization_scope(&query.scope)?;
        let state = self.lock()?.load(&scope.organization_id)?;
        let revision = state.as_ref().map_or(0, |state| state.revision);
        let mut items = state
            .and_then(|state| state.organization)
            .filter(|organization| {
                query.parameters.states.is_empty()
                    || query.parameters.states.contains(&organization.state)
            })
            .map(|organization| organization_projection(&organization))
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let page = page_slice(
            &mut items,
            query.page.limit,
            query.page.cursor.as_ref(),
            "organization",
            &scope.organization_id,
            revision,
            &query.parameters.states,
        )?;
        Ok(EnterpriseOrganizationListResultResponse {
            page: page.page,
            query: EnterpriseOrganizationListResultResponseQuery::EnterpriseOrganizationList,
            request_id: query.request_id.clone(),
            result: EnterpriseOrganizationPage {
                items: page.items,
                kind: EnterpriseOrganizationPageKind::EnterpriseOrganizationPage,
                snapshot_revision: Revision(i64_revision(revision)?),
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Lists one stable page of Memberships inside the exact query scope.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope, stale cursors, corrupt state, or unavailable storage.
    pub fn list_memberships(
        &self,
        query: &EnterpriseMembershipListQuery,
    ) -> Result<EnterpriseMembershipListResultResponse, EnterpriseRbacError> {
        let scope = require_organization_scope(&query.scope)?;
        let state = self
            .lock()?
            .load(&scope.organization_id)?
            .unwrap_or_else(|| empty_state(&scope.organization_id));
        let filter = (
            &query.parameters.states,
            &query.parameters.team_ids,
            &query.parameters.role_ids,
        );
        let mut items = state
            .memberships
            .values()
            .filter(|member| {
                (query.parameters.states.is_empty()
                    || query.parameters.states.contains(&member.state))
                    && (query.parameters.team_ids.is_empty()
                        || query
                            .parameters
                            .team_ids
                            .iter()
                            .all(|id| member.team_ids.contains(id)))
                    && (query.parameters.role_ids.is_empty()
                        || query
                            .parameters
                            .role_ids
                            .iter()
                            .all(|id| member_has_role(&state, member, id)))
            })
            .map(|member| membership_projection(&scope.organization_id, member))
            .collect::<Result<Vec<_>, _>>()?;
        items.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let page = page_slice(
            &mut items,
            query.page.limit,
            query.page.cursor.as_ref(),
            "membership",
            &scope.organization_id,
            state.revision,
            &filter,
        )?;
        Ok(EnterpriseMembershipListResultResponse {
            page: page.page,
            query: EnterpriseMembershipListResultResponseQuery::EnterpriseMembershipList,
            request_id: query.request_id.clone(),
            result: EnterpriseMembershipPage {
                items: page.items,
                kind: EnterpriseMembershipPageKind::EnterpriseMembershipPage,
                snapshot_revision: Revision(i64_revision(state.revision)?),
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Lists one stable page of Teams inside the exact Organization scope.
    ///
    /// # Errors
    ///
    /// Rejects stale cursors, corrupt state, or unavailable storage.
    pub fn list_teams(
        &self,
        query: &EnterpriseTeamListQuery,
    ) -> Result<EnterpriseTeamListResultResponse, EnterpriseRbacError> {
        let state = self
            .lock()?
            .load(&query.scope.organization_id)?
            .unwrap_or_else(|| empty_state(&query.scope.organization_id));
        let mut items = state
            .teams
            .values()
            .filter(|team| {
                query.parameters.states.is_empty() || query.parameters.states.contains(&team.state)
            })
            .map(|team| team_projection(&query.scope.organization_id, team))
            .collect::<Result<Vec<_>, _>>()?;
        items.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let page = page_slice(
            &mut items,
            query.page.limit,
            query.page.cursor.as_ref(),
            "team",
            &query.scope.organization_id,
            state.revision,
            &query.parameters.states,
        )?;
        Ok(EnterpriseTeamListResultResponse {
            page: page.page,
            query: EnterpriseTeamListResultResponseQuery::EnterpriseTeamList,
            request_id: query.request_id.clone(),
            result: EnterpriseTeamPage {
                items: page.items,
                kind: EnterpriseTeamPageKind::EnterpriseTeamPage,
                snapshot_revision: Revision(i64_revision(state.revision)?),
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Lists one stable page of immutable Role heads inside the exact Organization scope.
    ///
    /// # Errors
    ///
    /// Rejects stale cursors, corrupt state, or unavailable storage.
    pub fn list_roles(
        &self,
        query: &EnterpriseRoleListQuery,
    ) -> Result<EnterpriseRoleListResultResponse, EnterpriseRbacError> {
        let state = self
            .lock()?
            .load(&query.scope.organization_id)?
            .unwrap_or_else(|| empty_state(&query.scope.organization_id));
        let filter = (&query.parameters.states, &query.parameters.permissions);
        let mut items = state
            .roles
            .values()
            .filter(|role| {
                (query.parameters.states.is_empty()
                    || query.parameters.states.contains(&role.state))
                    && (query.parameters.permissions.is_empty()
                        || role
                            .versions
                            .get(&role.current_version)
                            .is_some_and(|version| {
                                query.parameters.permissions.iter().all(|permission| {
                                    version
                                        .rules
                                        .iter()
                                        .any(|rule| &rule.permission == permission)
                                })
                            }))
            })
            .map(|role| role_projection(&query.scope.organization_id, role))
            .collect::<Result<Vec<_>, _>>()?;
        items.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let page = page_slice(
            &mut items,
            query.page.limit,
            query.page.cursor.as_ref(),
            "role",
            &query.scope.organization_id,
            state.revision,
            &filter,
        )?;
        Ok(EnterpriseRoleListResultResponse {
            page: page.page,
            query: EnterpriseRoleListResultResponseQuery::EnterpriseRoleList,
            request_id: query.request_id.clone(),
            result: EnterpriseRolePage {
                items: page.items,
                kind: EnterpriseRolePageKind::EnterpriseRolePage,
                snapshot_revision: Revision(i64_revision(state.revision)?),
            },
            schema_version: query.schema_version.clone(),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, EnterpriseRbacInner>, EnterpriseRbacError> {
        self.inner.lock().map_err(|_| storage_unavailable())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthorityState {
    schema: String,
    organization_id: OrganizationId,
    revision: u64,
    organization: Option<OrganizationRecord>,
    roles: BTreeMap<String, RoleRecord>,
    teams: BTreeMap<String, TeamRecord>,
    memberships: BTreeMap<String, MembershipRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OrganizationRecord {
    id: OrganizationId,
    slug: String,
    display_name: String,
    state: String,
    revision: u64,
    updated_at: Instant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoleRecord {
    id: EnterpriseRoleId,
    display_name: String,
    state: String,
    current_version: i64,
    versions: BTreeMap<i64, RoleVersionRecord>,
    revision: u64,
    updated_at: Instant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoleVersionRecord {
    version: i64,
    rules: Vec<EnterpriseRolePermissionRule>,
    inherited_roles: Vec<EnterpriseRoleVersionReference>,
    conflicting_role_ids: Vec<EnterpriseRoleId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TeamRecord {
    id: EnterpriseTeamId,
    display_name: String,
    state: String,
    role_assignments: Vec<EnterpriseRoleAssignment>,
    revision: u64,
    updated_at: Instant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MembershipRecord {
    id: EnterpriseMembershipId,
    actor_id: DurableActorId,
    display_name: String,
    state: String,
    team_ids: Vec<EnterpriseTeamId>,
    role_assignments: Vec<EnterpriseRoleAssignment>,
    revision: u64,
    updated_at: Instant,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "snake_case",
    tag = "kind",
    content = "id",
    deny_unknown_fields
)]
enum DurableActorId {
    User(winwincode_domain::UserId),
    ServiceAccount(winwincode_domain::ServiceAccountId),
    System(winwincode_domain::SystemActorId),
}

#[derive(Clone, Copy)]
enum EventKind {
    Organization,
    Membership,
    Team,
    Role,
}

struct CommandMeta<'a> {
    actor: &'a Actor,
    scope: OrganizationScope,
    request_id: &'a RequestId,
    schema_version: &'a SchemaVersion,
    expected_revision: u64,
    action: &'static str,
    event_kind: EventKind,
}

impl<'a> CommandMeta<'a> {
    fn new(
        actor: &'a Actor,
        scope: &OrganizationScope,
        request_id: &'a RequestId,
        schema_version: &'a SchemaVersion,
        expected_revision: i64,
        action: &'static str,
        event_kind: EventKind,
    ) -> Result<Self, EnterpriseRbacError> {
        validate_id(&scope.organization_id.0, "org_")?;
        Ok(Self {
            actor,
            scope: scope.clone(),
            request_id,
            schema_version,
            expected_revision: u64_revision(expected_revision)?,
            action,
            event_kind,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LifecycleEvent<T> {
    response: T,
}

impl EnterpriseRbacInner {
    fn mutate<C, R, F>(
        &mut self,
        command: &C,
        meta: &CommandMeta<'_>,
        apply: F,
    ) -> Result<R, EnterpriseRbacError>
    where
        C: Serialize,
        R: Clone + DeserializeOwned + Serialize,
        F: FnOnce(&mut AuthorityState, &Instant, u64) -> Result<R, EnterpriseRbacError>,
    {
        if meta.schema_version != &SchemaVersion::WinwincodeV1 {
            return Err(invalid());
        }
        let scope = Scope::OrganizationScope(meta.scope.clone());
        let receipt_identity =
            command_receipt_identity(meta.actor, &scope, meta.request_id.clone())?;
        let command_digest = digest(command)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&receipt_identity, &command_digest)?
        {
            return replay(&receipt);
        }
        let before = self.load(&meta.scope.organization_id)?;
        let current_revision = before.as_ref().map_or(0, |state| state.revision);
        if current_revision != meta.expected_revision {
            return Err(revision_conflict());
        }
        let now_millis = self.clock.now_millis().map_err(|_| clock_unavailable())?;
        let now = instant_from_millis(now_millis)?;
        let mut next = before
            .clone()
            .unwrap_or_else(|| empty_state(&meta.scope.organization_id));
        let next_revision = current_revision
            .checked_add(1)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(invalid)?;
        next.revision = next_revision;
        let response = apply(&mut next, &now, next_revision)?;
        let next_payload = serde_json::to_vec(&next).map_err(|_| invalid())?;
        let receipt_payload = serde_json::to_vec(&LifecycleEvent {
            response: response.clone(),
        })
        .map_err(|_| invalid())?;
        let internal_id = format!("rbac_{:x}", Sha256::digest(command_digest.0.as_bytes()));
        let public_event = public_event(meta, &command_digest, next_revision, &now)?;
        let audit = pending_audit(meta, before.as_ref(), &next, now_millis, &internal_id)?;
        let commit = StateCommit::new(
            receipt_identity.clone(),
            command_digest.clone(),
            stream_id(&meta.scope.organization_id),
            current_revision,
            next_payload,
            vec![
                NewOutboxEvent::internal(internal_id, RECEIPT_TOPIC, receipt_payload),
                public_event,
            ],
        )
        .with_pending_audit_event(audit);
        match self.storage.commit(&commit) {
            Ok(receipt) => replay(&receipt),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => self
                .storage
                .load_receipt(&receipt_identity, &command_digest)?
                .map_or_else(|| Err(revision_conflict()), |receipt| replay(&receipt)),
            Err(error) => Err(error.into()),
        }
    }

    fn load(
        &self,
        organization_id: &OrganizationId,
    ) -> Result<Option<AuthorityState>, EnterpriseRbacError> {
        self.load_sealed(organization_id)
            .map(|sealed| sealed.map(|(state, _)| state))
    }

    fn load_sealed(
        &self,
        organization_id: &OrganizationId,
    ) -> Result<Option<(AuthorityState, RbacAuthoritySeal)>, EnterpriseRbacError> {
        let Some(stored) = self.storage.load_state(&stream_id(organization_id))? else {
            return Ok(None);
        };
        let state = decode_state(&stored, organization_id)?;
        let seal = RbacAuthoritySeal {
            revision: Revision(i64_revision(stored.revision)?),
            state_sha256: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&stored.payload))),
            state_guard: StateRevisionGuard::new(stream_id(organization_id), stored.revision)?,
        };
        Ok(Some((state, seal)))
    }
}

fn evaluate_member(
    state: &AuthorityState,
    member: &MembershipRecord,
    requested_scope: &Scope,
    permission: &EnterprisePermission,
    now: u64,
    authority_seal: RbacAuthoritySeal,
) -> Result<RbacDecision, EnterpriseRbacError> {
    let assignments = member_assignments(state, member);
    if separation_conflict(state, &assignments, now)? {
        return Ok(denied(
            state.revision,
            Some(member.id.clone()),
            RbacDenialReason::SeparationOfDuties,
            Some(authority_seal),
        ));
    }
    let mut evaluated = BTreeSet::new();
    let mut allow = false;
    for assignment in assignments.iter().filter(|assignment| {
        assignment_active(assignment, now).unwrap_or(false)
            && assignment_matches_scope(assignment, requested_scope)
    }) {
        let mut visiting = BTreeSet::new();
        let effect = evaluate_role(
            state,
            &assignment.role_id,
            assignment.role_version.0,
            permission,
            &mut visiting,
            &mut evaluated,
        )?;
        if effect == RoleEffect::Deny {
            return Ok(RbacDecision {
                allowed: false,
                denial_reason: Some(RbacDenialReason::ExplicitDeny),
                authority_revision: Revision(i64_revision(state.revision)?),
                membership_id: Some(member.id.clone()),
                evaluated_role_versions: evaluated
                    .into_iter()
                    .map(|(id, version)| EvaluatedRoleVersion {
                        role_id: EnterpriseRoleId(id),
                        role_version: EnterpriseRoleVersion(version),
                    })
                    .collect(),
                authority_seal: Some(authority_seal),
            });
        }
        allow |= effect == RoleEffect::Allow;
    }
    let evaluated_role_versions = evaluated
        .into_iter()
        .map(|(id, version)| EvaluatedRoleVersion {
            role_id: EnterpriseRoleId(id),
            role_version: EnterpriseRoleVersion(version),
        })
        .collect();
    Ok(RbacDecision {
        allowed: allow,
        denial_reason: (!allow).then_some(RbacDenialReason::DefaultDeny),
        authority_revision: Revision(i64_revision(state.revision)?),
        membership_id: Some(member.id.clone()),
        evaluated_role_versions,
        authority_seal: Some(authority_seal),
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RoleEffect {
    None,
    Allow,
    Deny,
}

fn evaluate_role(
    state: &AuthorityState,
    role_id: &EnterpriseRoleId,
    version: i64,
    permission: &EnterprisePermission,
    visiting: &mut BTreeSet<(String, i64)>,
    evaluated: &mut BTreeSet<(String, i64)>,
) -> Result<RoleEffect, EnterpriseRbacError> {
    let key = (role_id.0.clone(), version);
    if !visiting.insert(key.clone()) {
        return Err(storage_unavailable());
    }
    evaluated.insert(key.clone());
    let role = state
        .roles
        .get(&role_id.0)
        .ok_or_else(storage_unavailable)?;
    if role.state != "active" {
        visiting.remove(&key);
        return Ok(RoleEffect::None);
    }
    let role_version = role
        .versions
        .get(&version)
        .ok_or_else(storage_unavailable)?;
    let mut effect = RoleEffect::None;
    for rule in &role_version.rules {
        if &rule.permission == permission {
            if rule.effect == "deny" {
                visiting.remove(&key);
                return Ok(RoleEffect::Deny);
            }
            if rule.effect == "allow" {
                effect = RoleEffect::Allow;
            }
        }
    }
    for inherited in &role_version.inherited_roles {
        match evaluate_role(
            state,
            &inherited.role_id,
            inherited.role_version.0,
            permission,
            visiting,
            evaluated,
        )? {
            RoleEffect::Deny => {
                visiting.remove(&key);
                return Ok(RoleEffect::Deny);
            }
            RoleEffect::Allow => effect = RoleEffect::Allow,
            RoleEffect::None => {}
        }
    }
    visiting.remove(&key);
    Ok(effect)
}

fn member_assignments(
    state: &AuthorityState,
    member: &MembershipRecord,
) -> Vec<EnterpriseRoleAssignment> {
    let mut assignments = member.role_assignments.clone();
    for team_id in &member.team_ids {
        if let Some(team) = state
            .teams
            .get(&team_id.0)
            .filter(|team| team.state == "active")
        {
            assignments.extend(team.role_assignments.clone());
        }
    }
    assignments
}

fn evaluated_versions(
    state: &AuthorityState,
    assignments: &[EnterpriseRoleAssignment],
    now: u64,
) -> Result<Vec<EvaluatedRoleVersion>, EnterpriseRbacError> {
    let mut evaluated = BTreeSet::new();
    for assignment in assignments {
        if !assignment_active(assignment, now)? {
            continue;
        }
        collect_role_versions(
            state,
            &assignment.role_id,
            assignment.role_version.0,
            &mut BTreeSet::new(),
            &mut evaluated,
        )?;
    }
    Ok(evaluated
        .into_iter()
        .map(|(id, version)| EvaluatedRoleVersion {
            role_id: EnterpriseRoleId(id),
            role_version: EnterpriseRoleVersion(version),
        })
        .collect())
}

fn collect_role_versions(
    state: &AuthorityState,
    role_id: &EnterpriseRoleId,
    version: i64,
    visiting: &mut BTreeSet<(String, i64)>,
    collected: &mut BTreeSet<(String, i64)>,
) -> Result<(), EnterpriseRbacError> {
    let key = (role_id.0.clone(), version);
    if !visiting.insert(key.clone()) {
        return Err(storage_unavailable());
    }
    let role = state
        .roles
        .get(&role_id.0)
        .ok_or_else(storage_unavailable)?;
    if role.state == "active" {
        let role_version = role
            .versions
            .get(&version)
            .ok_or_else(storage_unavailable)?;
        collected.insert(key.clone());
        for inherited in &role_version.inherited_roles {
            collect_role_versions(
                state,
                &inherited.role_id,
                inherited.role_version.0,
                visiting,
                collected,
            )?;
        }
    }
    visiting.remove(&key);
    Ok(())
}

fn validate_role_payload(
    command: &EnterpriseRoleUpdateCommand,
    state: &AuthorityState,
) -> Result<(), EnterpriseRbacError> {
    validate_display_name(&command.payload.display_name)?;
    validate_value(&command.payload.state, &["active", "revoked"])?;
    if command.payload.rules.is_empty() {
        return Err(invalid());
    }
    for rule in &command.payload.rules {
        validate_value(&rule.effect, &["allow", "deny"])?;
    }
    for inherited in &command.payload.inherited_roles {
        if inherited.role_id == command.payload.role_id {
            return Err(invalid());
        }
        let role = state
            .roles
            .get(&inherited.role_id.0)
            .ok_or_else(not_found)?;
        if role.state != "active" || !role.versions.contains_key(&inherited.role_version.0) {
            return Err(wrong_state());
        }
    }
    if command
        .payload
        .conflicting_role_ids
        .iter()
        .any(|id| id == &command.payload.role_id || !state.roles.contains_key(&id.0))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_role_graph(
    state: &AuthorityState,
    role_id: &EnterpriseRoleId,
    version: i64,
) -> Result<(), EnterpriseRbacError> {
    collect_role_versions(
        state,
        role_id,
        version,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
    )
    .map_err(|_| invalid())
}

fn validate_assignments(
    state: &AuthorityState,
    organization_id: &OrganizationId,
    assignments: &[EnterpriseRoleAssignment],
) -> Result<(), EnterpriseRbacError> {
    let mut identities = BTreeSet::new();
    for assignment in assignments {
        if scope_organization_id(&assignment.scope) != *organization_id
            || !matches!(assignment.scope_mode.as_str(), "exact" | "descendants")
        {
            return Err(scope_denied());
        }
        let role = state
            .roles
            .get(&assignment.role_id.0)
            .ok_or_else(not_found)?;
        if role.state != "active" || !role.versions.contains_key(&assignment.role_version.0) {
            return Err(wrong_state());
        }
        if let (Some(start), Some(end)) = (&assignment.not_before, &assignment.expires_at)
            && instant_millis(start)? >= instant_millis(end)?
        {
            return Err(invalid());
        }
        let identity = serde_json::to_vec(assignment).map_err(|_| invalid())?;
        if !identities.insert(identity) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_active_members(state: &AuthorityState, now: u64) -> Result<(), EnterpriseRbacError> {
    for member in state
        .memberships
        .values()
        .filter(|member| member.state == "active")
    {
        let assignments = member_assignments(state, member);
        if separation_conflict(state, &assignments, now)? {
            return Err(wrong_state());
        }
    }
    Ok(())
}

fn validate_separation(
    state: &AuthorityState,
    assignments: &[EnterpriseRoleAssignment],
    now: u64,
) -> Result<(), EnterpriseRbacError> {
    if separation_conflict(state, assignments, now)? {
        Err(wrong_state())
    } else {
        Ok(())
    }
}

fn separation_conflict(
    state: &AuthorityState,
    assignments: &[EnterpriseRoleAssignment],
    now: u64,
) -> Result<bool, EnterpriseRbacError> {
    let mut evaluated = BTreeSet::new();
    for assignment in assignments {
        if !assignment_active(assignment, now)? {
            continue;
        }
        collect_role_versions(
            state,
            &assignment.role_id,
            assignment.role_version.0,
            &mut BTreeSet::new(),
            &mut evaluated,
        )?;
    }
    let ids = evaluated
        .iter()
        .map(|(role_id, _)| role_id.as_str())
        .collect::<BTreeSet<_>>();
    for (role_id, version_number) in &evaluated {
        let role = state.roles.get(role_id).ok_or_else(storage_unavailable)?;
        let version = role
            .versions
            .get(version_number)
            .ok_or_else(storage_unavailable)?;
        if version
            .conflicting_role_ids
            .iter()
            .any(|conflict| ids.contains(conflict.0.as_str()))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn assignment_active(
    assignment: &EnterpriseRoleAssignment,
    now: u64,
) -> Result<bool, EnterpriseRbacError> {
    Ok(assignment
        .not_before
        .as_ref()
        .map(instant_millis)
        .transpose()?
        .is_none_or(|start| start <= now)
        && assignment
            .expires_at
            .as_ref()
            .map(instant_millis)
            .transpose()?
            .is_none_or(|end| now < end))
}

fn assignment_matches_scope(assignment: &EnterpriseRoleAssignment, requested: &Scope) -> bool {
    assignment.scope == *requested
        || (assignment.scope_mode == "descendants" && scope_contains(&assignment.scope, requested))
}

fn scope_contains(parent: &Scope, child: &Scope) -> bool {
    match (parent, child) {
        (Scope::OrganizationScope(parent), child) => {
            parent.organization_id == scope_organization_id(child)
        }
        (Scope::WorkspaceScope(parent), Scope::WorkspaceScope(child)) => parent == child,
        (Scope::WorkspaceScope(parent), Scope::ProjectScope(child)) => {
            parent.organization_id == child.organization_id
                && parent.workspace_id == child.workspace_id
        }
        (Scope::WorkspaceScope(parent), Scope::RepositoryScope(child)) => {
            parent.organization_id == child.organization_id
                && parent.workspace_id == child.workspace_id
        }
        (Scope::ProjectScope(parent), Scope::ProjectScope(child)) => parent == child,
        (Scope::ProjectScope(parent), Scope::RepositoryScope(child)) => {
            parent.organization_id == child.organization_id
                && parent.workspace_id == child.workspace_id
                && parent.project_id == child.project_id
        }
        (Scope::RepositoryScope(parent), Scope::RepositoryScope(child)) => parent == child,
        _ => false,
    }
}

fn decode_state(
    stored: &StoredState,
    organization_id: &OrganizationId,
) -> Result<AuthorityState, EnterpriseRbacError> {
    let state: AuthorityState =
        serde_json::from_slice(&stored.payload).map_err(|_| storage_unavailable())?;
    if state.schema != STATE_SCHEMA
        || &state.organization_id != organization_id
        || state.revision != stored.revision
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
        || serde_json::to_vec(&state).map_err(|_| storage_unavailable())? != stored.payload
    {
        return Err(storage_unavailable());
    }
    Ok(state)
}

fn empty_state(organization_id: &OrganizationId) -> AuthorityState {
    AuthorityState {
        schema: STATE_SCHEMA.to_owned(),
        organization_id: organization_id.clone(),
        revision: 0,
        organization: None,
        roles: BTreeMap::new(),
        teams: BTreeMap::new(),
        memberships: BTreeMap::new(),
    }
}

fn public_event(
    meta: &CommandMeta<'_>,
    digest: &Sha256Digest,
    revision: u64,
    occurred_at: &Instant,
) -> Result<NewOutboxEvent, EnterpriseRbacError> {
    let snapshot_revision = Revision(i64_revision(revision)?);
    let (topic, payload) = match meta.event_kind {
        EventKind::Organization => ("enterprise-organization.invalidated.v1", serde_json::to_vec(&ControlPlaneWebSocketEnterpriseOrganizationInvalidatedEvent { reload_queries: (ControlPlaneWebSocketEnterpriseOrganizationListReloadQuery::EnterpriseOrganizationList,), snapshot_revision, type_value: ControlPlaneWebSocketEnterpriseOrganizationInvalidatedEventTypeValue::EnterpriseOrganizationInvalidatedV1 })),
        EventKind::Membership => ("enterprise-membership.invalidated.v1", serde_json::to_vec(&ControlPlaneWebSocketEnterpriseMembershipInvalidatedEvent { reload_queries: (ControlPlaneWebSocketEnterpriseMembershipListReloadQuery::EnterpriseMembershipList,), snapshot_revision, type_value: ControlPlaneWebSocketEnterpriseMembershipInvalidatedEventTypeValue::EnterpriseMembershipInvalidatedV1 })),
        EventKind::Team => ("enterprise-team.invalidated.v1", serde_json::to_vec(&ControlPlaneWebSocketEnterpriseTeamInvalidatedEvent { reload_queries: (ControlPlaneWebSocketEnterpriseTeamListReloadQuery::EnterpriseTeamList,), snapshot_revision, type_value: ControlPlaneWebSocketEnterpriseTeamInvalidatedEventTypeValue::EnterpriseTeamInvalidatedV1 })),
        EventKind::Role => ("enterprise-role.invalidated.v1", serde_json::to_vec(&ControlPlaneWebSocketEnterpriseRoleInvalidatedEvent { reload_queries: (ControlPlaneWebSocketEnterpriseRoleListReloadQuery::EnterpriseRoleList,), snapshot_revision, type_value: ControlPlaneWebSocketEnterpriseRoleInvalidatedEventTypeValue::EnterpriseRoleInvalidatedV1 })),
    };
    let payload = payload.map_err(|_| invalid())?;
    let mut id_digest = Sha256::new();
    id_digest.update(b"winwincode.enterprise-rbac-event.v1\0");
    id_digest.update(digest.0.as_bytes());
    id_digest.update(topic.as_bytes());
    NewOutboxEvent::public_projection(
        ControlPlaneEventId(format!("evt_{:x}", id_digest.finalize())),
        topic,
        payload,
        ProjectionEventStream::Scope,
        PublicEventScope::Organization {
            organization_id: meta.scope.organization_id.clone(),
        },
        occurred_at.clone(),
        PublicEventSource::ControlPlane {
            actor: public_event_actor(meta.actor),
            component: "enterprise-rbac".to_owned(),
        },
    )
    .map_err(Into::into)
}

fn pending_audit(
    meta: &CommandMeta<'_>,
    before: Option<&AuthorityState>,
    after: &AuthorityState,
    now_millis: u64,
    event_id: &str,
) -> Result<PendingAuditEvent, EnterpriseRbacError> {
    let event = AuditEvent::state_change(
        AuditEventId::from_digest(&digest(event_id.as_bytes())?).map_err(|_| invalid())?,
        now_millis,
        audit_actor(meta.actor),
        AuditScope::organization(meta.scope.organization_id.clone()).map_err(|_| invalid())?,
        meta.request_id.clone(),
        AuditAction::administration(meta.action).map_err(|_| invalid())?,
        AuditState::changed(before.map(digest).transpose()?, digest(after)?)
            .map_err(|_| invalid())?,
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

fn replay<T: DeserializeOwned>(receipt: &CommitReceipt) -> Result<T, EnterpriseRbacError> {
    let events = receipt
        .events
        .iter()
        .filter(|event| event.topic == RECEIPT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = events.as_slice() else {
        return Err(storage_unavailable());
    };
    let lifecycle: LifecycleEvent<T> =
        serde_json::from_slice(&event.payload).map_err(|_| storage_unavailable())?;
    Ok(lifecycle.response)
}

fn organization_projection(
    record: &OrganizationRecord,
) -> Result<EnterpriseOrganizationProjection, EnterpriseRbacError> {
    Ok(EnterpriseOrganizationProjection {
        display_name: record.display_name.clone(),
        id: record.id.clone(),
        revision: Revision(i64_revision(record.revision)?),
        slug: record.slug.clone(),
        state: record.state.clone(),
        updated_at: record.updated_at.clone(),
    })
}
fn role_projection(
    organization_id: &OrganizationId,
    record: &RoleRecord,
) -> Result<EnterpriseRoleProjection, EnterpriseRbacError> {
    let version = record
        .versions
        .get(&record.current_version)
        .ok_or_else(storage_unavailable)?;
    Ok(EnterpriseRoleProjection {
        conflicting_role_ids: version.conflicting_role_ids.clone(),
        display_name: record.display_name.clone(),
        id: record.id.clone(),
        inherited_roles: version.inherited_roles.clone(),
        organization_id: organization_id.clone(),
        revision: Revision(i64_revision(record.revision)?),
        rules: version.rules.clone(),
        state: record.state.clone(),
        updated_at: record.updated_at.clone(),
        version: EnterpriseRoleVersion(record.current_version),
    })
}
fn team_projection(
    organization_id: &OrganizationId,
    record: &TeamRecord,
) -> Result<EnterpriseTeamProjection, EnterpriseRbacError> {
    Ok(EnterpriseTeamProjection {
        display_name: record.display_name.clone(),
        id: record.id.clone(),
        organization_id: organization_id.clone(),
        revision: Revision(i64_revision(record.revision)?),
        role_assignments: record.role_assignments.clone(),
        state: record.state.clone(),
        updated_at: record.updated_at.clone(),
    })
}
fn membership_projection(
    organization_id: &OrganizationId,
    record: &MembershipRecord,
) -> Result<EnterpriseMembershipProjection, EnterpriseRbacError> {
    Ok(EnterpriseMembershipProjection {
        actor_id: public_actor_id(&record.actor_id),
        display_name: record.display_name.clone(),
        id: record.id.clone(),
        organization_id: organization_id.clone(),
        revision: Revision(i64_revision(record.revision)?),
        role_assignments: record.role_assignments.clone(),
        state: record.state.clone(),
        team_ids: record.team_ids.clone(),
        updated_at: record.updated_at.clone(),
    })
}

struct PageSlice<T> {
    items: Vec<T>,
    page: PageInfo,
}
fn page_slice<T, F: Serialize>(
    items: &mut Vec<T>,
    limit: i64,
    cursor: Option<&OpaqueCursor>,
    kind: &str,
    organization_id: &OrganizationId,
    revision: u64,
    filter: &F,
) -> Result<PageSlice<T>, EnterpriseRbacError> {
    let limit = usize::try_from(limit)
        .ok()
        .filter(|value| (1..=MAX_PAGE_SIZE).contains(value))
        .ok_or_else(invalid)?;
    let filter_sha256 = digest(filter)?;
    let offset = cursor.map(decode_cursor).transpose()?.map_or(0, |cursor| {
        if cursor.organization_id != *organization_id
            || cursor.kind != kind
            || cursor.revision != revision
            || cursor.filter_sha256 != filter_sha256
        {
            usize::MAX
        } else {
            cursor.offset
        }
    });
    if offset == usize::MAX || offset > items.len() {
        return Err(invalid());
    }
    let mut tail = items.drain(offset..).collect::<Vec<_>>();
    let has_more = tail.len() > limit;
    tail.truncate(limit);
    let next_cursor = has_more
        .then(|| {
            encode_cursor(&RbacCursor {
                schema: CURSOR_SCHEMA,
                organization_id: organization_id.clone(),
                kind: kind.to_owned(),
                revision,
                filter_sha256,
                offset: offset + tail.len(),
            })
        })
        .transpose()?;
    Ok(PageSlice {
        items: tail,
        page: PageInfo {
            has_more,
            next_cursor,
        },
    })
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RbacCursor {
    schema: u8,
    organization_id: OrganizationId,
    kind: String,
    revision: u64,
    filter_sha256: Sha256Digest,
    offset: usize,
}
fn encode_cursor(cursor: &RbacCursor) -> Result<OpaqueCursor, EnterpriseRbacError> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| invalid())?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}
fn decode_cursor(cursor: &OpaqueCursor) -> Result<RbacCursor, EnterpriseRbacError> {
    if cursor.0.len() > MAX_CURSOR_BYTES * 2 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(&cursor.0).map_err(|_| invalid())?;
    let value: RbacCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if value.schema != CURSOR_SCHEMA || serde_json::to_vec(&value).map_err(|_| invalid())? != bytes
    {
        return Err(invalid());
    }
    Ok(value)
}

fn require_organization_scope(scope: &Scope) -> Result<&OrganizationScope, EnterpriseRbacError> {
    let Scope::OrganizationScope(scope) = scope else {
        return Err(scope_denied());
    };
    Ok(scope)
}
fn require_active_organization(state: &AuthorityState) -> Result<(), EnterpriseRbacError> {
    match state
        .organization
        .as_ref()
        .map(|organization| organization.state.as_str())
    {
        Some("active") => Ok(()),
        Some(_) => Err(wrong_state()),
        None => Err(not_found()),
    }
}
fn member_has_role(
    state: &AuthorityState,
    member: &MembershipRecord,
    role_id: &EnterpriseRoleId,
) -> bool {
    member_assignments(state, member)
        .iter()
        .any(|assignment| &assignment.role_id == role_id)
}
fn denied(
    revision: u64,
    membership_id: Option<EnterpriseMembershipId>,
    reason: RbacDenialReason,
    authority_seal: Option<RbacAuthoritySeal>,
) -> RbacDecision {
    RbacDecision {
        allowed: false,
        denial_reason: Some(reason),
        authority_revision: Revision(i64::try_from(revision).unwrap_or(i64::MAX)),
        membership_id,
        evaluated_role_versions: Vec::new(),
        authority_seal,
    }
}
fn actor_id(actor: &Actor) -> DurableActorId {
    match actor {
        Actor::UserActor(actor) => DurableActorId::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => DurableActorId::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => DurableActorId::System(actor.id.clone()),
    }
}
fn durable_actor_id(actor: &ActorId) -> DurableActorId {
    match actor {
        ActorId::UserId(id) => DurableActorId::User(id.clone()),
        ActorId::ServiceAccountId(id) => DurableActorId::ServiceAccount(id.clone()),
        ActorId::SystemActorId(id) => DurableActorId::System(id.clone()),
    }
}
fn public_actor_id(actor: &DurableActorId) -> ActorId {
    match actor {
        DurableActorId::User(id) => ActorId::UserId(id.clone()),
        DurableActorId::ServiceAccount(id) => ActorId::ServiceAccountId(id.clone()),
        DurableActorId::System(id) => ActorId::SystemActorId(id.clone()),
    }
}
fn audit_actor(actor: &Actor) -> AuditActor {
    match actor {
        Actor::UserActor(actor) => AuditActor::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => AuditActor::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => AuditActor::System(actor.id.clone()),
    }
}
fn scope_organization_id(scope: &Scope) -> OrganizationId {
    match scope {
        Scope::OrganizationScope(scope) => scope.organization_id.clone(),
        Scope::WorkspaceScope(scope) => scope.organization_id.clone(),
        Scope::ProjectScope(scope) => scope.organization_id.clone(),
        Scope::RepositoryScope(scope) => scope.organization_id.clone(),
    }
}
fn stream_id(organization_id: &OrganizationId) -> String {
    format!("{STREAM_PREFIX}{}", organization_id.0)
}
fn digest<T: Serialize + ?Sized>(value: &T) -> Result<Sha256Digest, EnterpriseRbacError> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}
fn instant_millis(value: &Instant) -> Result<u64, EnterpriseRbacError> {
    crate::session_binding_transaction::instant_millis(value).map_err(Into::into)
}
fn validate_display_name(value: &str) -> Result<(), EnterpriseRbacError> {
    if value.is_empty() || value.chars().count() > 256 || value.trim() != value {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn validate_slug(value: &str) -> Result<(), EnterpriseRbacError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || value.starts_with('-')
        || value.ends_with('-')
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn validate_value(value: &str, accepted: &[&str]) -> Result<(), EnterpriseRbacError> {
    if accepted.contains(&value) {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn validate_actor_id(value: &ActorId) -> Result<(), EnterpriseRbacError> {
    match value {
        ActorId::UserId(value) => validate_id(&value.0, "usr_"),
        ActorId::ServiceAccountId(value) => validate_id(&value.0, "svc_"),
        ActorId::SystemActorId(value) => validate_id(&value.0, "sys_"),
    }
}
fn validate_id(value: &str, prefix: &str) -> Result<(), EnterpriseRbacError> {
    let suffix = value.strip_prefix(prefix).ok_or_else(invalid)?;
    if suffix.len() == 26 && suffix.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')) { Ok(()) } else { Err(invalid()) }
}
fn u64_revision(value: i64) -> Result<u64, EnterpriseRbacError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(invalid)
}
fn i64_revision(value: u64) -> Result<i64, EnterpriseRbacError> {
    i64::try_from(value)
        .ok()
        .filter(|value| *value <= MAX_SAFE_INTEGER_I64)
        .ok_or_else(invalid)
}
const fn invalid() -> EnterpriseRbacError {
    EnterpriseRbacError::new(
        EnterpriseRbacErrorKind::InvalidRequest,
        "RBAC request is invalid",
    )
}
const fn scope_denied() -> EnterpriseRbacError {
    EnterpriseRbacError::new(EnterpriseRbacErrorKind::ScopeDenied, "RBAC scope is denied")
}
const fn not_found() -> EnterpriseRbacError {
    EnterpriseRbacError::new(
        EnterpriseRbacErrorKind::NotFound,
        "RBAC resource was not found",
    )
}
const fn wrong_state() -> EnterpriseRbacError {
    EnterpriseRbacError::new(
        EnterpriseRbacErrorKind::WrongState,
        "RBAC resource state rejects this operation",
    )
}
const fn revision_conflict() -> EnterpriseRbacError {
    EnterpriseRbacError::new(
        EnterpriseRbacErrorKind::RevisionConflict,
        "RBAC authority revision does not match",
    )
}
const fn storage_unavailable() -> EnterpriseRbacError {
    EnterpriseRbacError::new(
        EnterpriseRbacErrorKind::Storage,
        "RBAC authority is unavailable",
    )
}
const fn clock_unavailable() -> EnterpriseRbacError {
    EnterpriseRbacError::new(
        EnterpriseRbacErrorKind::ClockUnavailable,
        "RBAC clock is unavailable",
    )
}
