// SPDX-License-Identifier: Apache-2.0

//! Canonical external-identity lifecycle orchestration.
//!
//! Protocol adapters normalize and verify OIDC, SAML, and SCIM input before
//! calling this module. User, Membership, Team, and browser-session state stays
//! in the existing Identity, RBAC, and Server session authorities.

use std::{fmt, sync::Arc};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ActorId, EnterpriseExternalIdentityLinkPayload,
    EnterpriseExternalIdentityLinkPayloadAction, EnterpriseExternalIdentityLinkPayloadKind,
    EnterpriseExternalIdentityProjection, EnterpriseExternalIdentityRevokePayload,
    EnterpriseExternalIdentityRevokePayloadAction, EnterpriseExternalIdentityRevokePayloadKind,
    EnterpriseIdentityUpdateCommand, EnterpriseIdentityUpdateCommandCommand,
    EnterpriseIdentityUpdatePayload, EnterpriseMembershipProjection,
    EnterpriseMembershipUpdateCommand, EnterpriseMembershipUpdateCommandCommand,
    EnterpriseMembershipUpdatePayload, EnterpriseRoleAssignment, EnterpriseTeamProjection,
    EnterpriseTeamUpdateCommand, EnterpriseTeamUpdateCommandCommand, EnterpriseTeamUpdatePayload,
    OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_domain::{
    EnterpriseMembershipId, EnterpriseTeamId, ExternalIdentityId, OrganizationId, RequestId,
    Revision, SchemaVersion, Sha256Digest, UserActor, UserActorKind, UserId,
};

use crate::{
    EnterpriseIdentityError, EnterpriseIdentityErrorKind, EnterpriseIdentityService,
    EnterpriseRbacError, EnterpriseRbacErrorKind, EnterpriseRbacService,
};

const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalIdentityProvider {
    Oidc,
    Saml,
    Scim,
}

impl ExternalIdentityProvider {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Saml => "saml",
            Self::Scim => "scim",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalIdentityReference {
    pub organization_id: OrganizationId,
    pub provider: ExternalIdentityProvider,
    pub issuer_sha256: Sha256Digest,
    pub subject_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalIdentityPrincipal {
    pub actor: Actor,
    pub authorized_scopes: Vec<Scope>,
    pub organization_id: OrganizationId,
    pub external_identity_id: ExternalIdentityId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProvisionExternalUser {
    pub operation_id: String,
    pub identity: ExternalIdentityReference,
    pub user_id: UserId,
    pub display_name: String,
    pub authorized_scopes: Vec<Scope>,
    pub team_ids: Vec<EnterpriseTeamId>,
    pub role_assignments: Vec<EnterpriseRoleAssignment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprovisionExternalUser {
    pub operation_id: String,
    pub identity: ExternalIdentityReference,
    pub user_id: UserId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UpsertExternalTeam {
    pub operation_id: String,
    pub organization_id: OrganizationId,
    pub team_id: EnterpriseTeamId,
    pub display_name: String,
    pub state: String,
    pub role_assignments: Vec<EnterpriseRoleAssignment>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExternalIdentityLifecycleOutcome {
    User(ExternalIdentityPrincipal),
    Team(EnterpriseTeamProjection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseIdentityLifecycleErrorKind {
    InvalidRequest,
    IdentityUnavailable,
    RbacUnavailable,
    SessionUnavailable,
    NotFound,
    WrongState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseIdentityLifecycleError {
    kind: EnterpriseIdentityLifecycleErrorKind,
}

impl EnterpriseIdentityLifecycleError {
    const fn new(kind: EnterpriseIdentityLifecycleErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> EnterpriseIdentityLifecycleErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseIdentityLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            EnterpriseIdentityLifecycleErrorKind::InvalidRequest => {
                "external identity lifecycle request is invalid"
            }
            EnterpriseIdentityLifecycleErrorKind::IdentityUnavailable => {
                "external identity authority is unavailable"
            }
            EnterpriseIdentityLifecycleErrorKind::RbacUnavailable => {
                "external identity RBAC authority is unavailable"
            }
            EnterpriseIdentityLifecycleErrorKind::SessionUnavailable => {
                "external identity session authority is unavailable"
            }
            EnterpriseIdentityLifecycleErrorKind::NotFound => "external identity was not found",
            EnterpriseIdentityLifecycleErrorKind::WrongState => {
                "external identity lifecycle state rejects this operation"
            }
        })
    }
}

impl std::error::Error for EnterpriseIdentityLifecycleError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserSessionLifecycleError;

pub trait BrowserSessionLifecyclePort: Send + Sync {
    /// Replaces all current browser-session scopes for one Actor.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when the canonical session authority is unavailable.
    fn replace_authorized_scopes(
        &self,
        actor: &Actor,
        authorized_scopes: Vec<Scope>,
    ) -> Result<usize, BrowserSessionLifecycleError>;

    /// Revokes all current browser sessions for one Actor.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when the canonical session authority is unavailable.
    fn revoke_actor_sessions(&self, actor: &Actor) -> Result<usize, BrowserSessionLifecycleError>;
}

pub trait EnterpriseIdentityLifecyclePort: Send {
    /// Resolves one verified external subject through the current Identity and RBAC authorities.
    ///
    /// # Errors
    ///
    /// Rejects missing, revoked, mismatched, or inactive principals.
    fn authenticate_external(
        &mut self,
        identity: &ExternalIdentityReference,
    ) -> Result<ExternalIdentityPrincipal, EnterpriseIdentityLifecycleError>;

    /// Creates or updates one SCIM-managed user using the canonical authorities.
    ///
    /// # Errors
    ///
    /// Rejects invalid or conflicting identity/RBAC facts and authority failures.
    fn provision_user(
        &mut self,
        request: &ProvisionExternalUser,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError>;

    /// Disables one SCIM-managed user and revokes every live browser session.
    ///
    /// # Errors
    ///
    /// Rejects mismatched identity facts and authority failures.
    fn deprovision_user(
        &mut self,
        request: &DeprovisionExternalUser,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError>;

    /// Creates, updates, or archives one SCIM-managed Team.
    ///
    /// # Errors
    ///
    /// Rejects invalid references, stale authority, or durable failures.
    fn upsert_team(
        &mut self,
        request: &UpsertExternalTeam,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError>;
}

pub struct CanonicalEnterpriseIdentityLifecycle {
    identity: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
    sessions: Arc<dyn BrowserSessionLifecyclePort>,
    management_actor: Actor,
}

impl CanonicalEnterpriseIdentityLifecycle {
    #[must_use]
    pub fn new(
        identity: Arc<EnterpriseIdentityService>,
        rbac: Arc<EnterpriseRbacService>,
        sessions: Arc<dyn BrowserSessionLifecyclePort>,
        management_actor: Actor,
    ) -> Self {
        Self {
            identity,
            rbac,
            sessions,
            management_actor,
        }
    }

    fn current_external_identity(
        &self,
        reference: &ExternalIdentityReference,
    ) -> Result<Option<EnterpriseExternalIdentityProjection>, EnterpriseIdentityLifecycleError>
    {
        self.identity
            .external_identity(&reference.organization_id, &external_identity_id(reference))
            .map_err(|error| identity_error(&error))
    }

    fn current_membership(
        &self,
        actor: &Actor,
        organization_id: &OrganizationId,
    ) -> Result<Option<EnterpriseMembershipProjection>, EnterpriseIdentityLifecycleError> {
        self.rbac
            .membership_by_actor(actor, organization_id)
            .map_err(|error| rbac_error(&error))
    }

    fn ensure_external_identity(
        &self,
        request: &ProvisionExternalUser,
    ) -> Result<(), EnterpriseIdentityLifecycleError> {
        let desired_id = external_identity_id(&request.identity);
        let current = self.current_external_identity(&request.identity)?;
        if let Some(current) = &current {
            if current.state != "active"
                || current.actor.id != request.user_id
                || current.provider != request.identity.provider.as_str()
                || current.issuer_sha256 != request.identity.issuer_sha256
                || current.subject_sha256 != request.identity.subject_sha256
            {
                return Err(EnterpriseIdentityLifecycleError::new(
                    EnterpriseIdentityLifecycleErrorKind::WrongState,
                ));
            }
            if current.authorized_scopes == request.authorized_scopes {
                return Ok(());
            }
        }
        self.identity
            .update(&EnterpriseIdentityUpdateCommand {
                actor: self.management_actor.clone(),
                command: EnterpriseIdentityUpdateCommandCommand::EnterpriseIdentityUpdate,
                expected_revision: current
                    .as_ref()
                    .map_or(Revision(0), |value| value.revision.clone()),
                payload: EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityLinkPayload(
                    EnterpriseExternalIdentityLinkPayload {
                        action: EnterpriseExternalIdentityLinkPayloadAction::Link,
                        authorized_scopes: request.authorized_scopes.clone(),
                        external_identity_id: desired_id,
                        issuer_sha256: request.identity.issuer_sha256.clone(),
                        kind: EnterpriseExternalIdentityLinkPayloadKind::ExternalIdentity,
                        provider: request.identity.provider.as_str().to_owned(),
                        subject_sha256: request.identity.subject_sha256.clone(),
                        user_id: request.user_id.clone(),
                    },
                ),
                request_id: derived_request_id(&request.operation_id, b"identity-link"),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: organization_scope(&request.identity.organization_id),
            })
            .map_err(|error| identity_error(&error))?;
        Ok(())
    }

    fn ensure_membership(
        &self,
        request: &ProvisionExternalUser,
        actor: &Actor,
    ) -> Result<(), EnterpriseIdentityLifecycleError> {
        let desired_id = membership_id(&request.user_id);
        let current = self.current_membership(actor, &request.identity.organization_id)?;
        if let Some(current) = &current {
            if current.id != desired_id {
                return Err(EnterpriseIdentityLifecycleError::new(
                    EnterpriseIdentityLifecycleErrorKind::WrongState,
                ));
            }
            if current.state == "active"
                && current.display_name == request.display_name
                && current.team_ids == request.team_ids
                && current.role_assignments == request.role_assignments
            {
                return Ok(());
            }
        }
        let authority = self
            .rbac
            .authority_seal(&request.identity.organization_id)
            .map_err(|error| rbac_error(&error))?;
        self.rbac
            .update_membership(&EnterpriseMembershipUpdateCommand {
                actor: self.management_actor.clone(),
                command: EnterpriseMembershipUpdateCommandCommand::EnterpriseMembershipUpdate,
                expected_revision: authority.revision,
                payload: EnterpriseMembershipUpdatePayload {
                    actor_id: ActorId::UserId(request.user_id.clone()),
                    display_name: request.display_name.clone(),
                    membership_id: desired_id,
                    role_assignments: request.role_assignments.clone(),
                    state: "active".to_owned(),
                    team_ids: request.team_ids.clone(),
                },
                request_id: derived_request_id(&request.operation_id, b"membership-active"),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: organization_scope(&request.identity.organization_id),
            })
            .map_err(|error| rbac_error(&error))?;
        Ok(())
    }
}

impl EnterpriseIdentityLifecyclePort for CanonicalEnterpriseIdentityLifecycle {
    fn authenticate_external(
        &mut self,
        identity: &ExternalIdentityReference,
    ) -> Result<ExternalIdentityPrincipal, EnterpriseIdentityLifecycleError> {
        let projection = self.current_external_identity(identity)?.ok_or_else(|| {
            EnterpriseIdentityLifecycleError::new(EnterpriseIdentityLifecycleErrorKind::NotFound)
        })?;
        if projection.state != "active"
            || projection.provider != identity.provider.as_str()
            || projection.issuer_sha256 != identity.issuer_sha256
            || projection.subject_sha256 != identity.subject_sha256
        {
            return Err(EnterpriseIdentityLifecycleError::new(
                EnterpriseIdentityLifecycleErrorKind::WrongState,
            ));
        }
        let actor = Actor::UserActor(projection.actor.clone());
        self.rbac
            .active_member_context(&actor, &identity.organization_id)
            .map_err(|error| rbac_error(&error))?;
        Ok(ExternalIdentityPrincipal {
            actor,
            authorized_scopes: projection.authorized_scopes,
            organization_id: identity.organization_id.clone(),
            external_identity_id: projection.id,
        })
    }

    fn provision_user(
        &mut self,
        request: &ProvisionExternalUser,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError> {
        validate_operation_id(&request.operation_id)?;
        let actor = user_actor(&request.user_id);
        self.ensure_membership(request, &actor)?;
        self.ensure_external_identity(request)?;
        self.sessions
            .replace_authorized_scopes(&actor, request.authorized_scopes.clone())
            .map_err(|_| {
                EnterpriseIdentityLifecycleError::new(
                    EnterpriseIdentityLifecycleErrorKind::SessionUnavailable,
                )
            })?;
        self.authenticate_external(&request.identity)
            .map(ExternalIdentityLifecycleOutcome::User)
    }

    fn deprovision_user(
        &mut self,
        request: &DeprovisionExternalUser,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError> {
        validate_operation_id(&request.operation_id)?;
        let actor = user_actor(&request.user_id);
        let current_identity = self.current_external_identity(&request.identity)?;
        if let Some(identity) = &current_identity
            && (identity.actor.id != request.user_id
                || identity.provider != request.identity.provider.as_str()
                || identity.issuer_sha256 != request.identity.issuer_sha256
                || identity.subject_sha256 != request.identity.subject_sha256)
        {
            return Err(EnterpriseIdentityLifecycleError::new(
                EnterpriseIdentityLifecycleErrorKind::WrongState,
            ));
        }
        if let Some(member) = self.current_membership(&actor, &request.identity.organization_id)?
            && member.state != "disabled"
        {
            let authority = self
                .rbac
                .authority_seal(&request.identity.organization_id)
                .map_err(|error| rbac_error(&error))?;
            self.rbac
                .update_membership(&EnterpriseMembershipUpdateCommand {
                    actor: self.management_actor.clone(),
                    command: EnterpriseMembershipUpdateCommandCommand::EnterpriseMembershipUpdate,
                    expected_revision: authority.revision,
                    payload: EnterpriseMembershipUpdatePayload {
                        actor_id: ActorId::UserId(request.user_id.clone()),
                        display_name: member.display_name,
                        membership_id: member.id,
                        role_assignments: member.role_assignments,
                        state: "disabled".to_owned(),
                        team_ids: member.team_ids,
                    },
                    request_id: derived_request_id(&request.operation_id, b"membership-disabled"),
                    schema_version: SchemaVersion::WinwincodeV1,
                    scope: organization_scope(&request.identity.organization_id),
                })
                .map_err(|error| rbac_error(&error))?;
        }
        self.sessions.revoke_actor_sessions(&actor).map_err(|_| {
            EnterpriseIdentityLifecycleError::new(
                EnterpriseIdentityLifecycleErrorKind::SessionUnavailable,
            )
        })?;
        if let Some(identity) = current_identity
            && identity.state == "active"
        {
            self.identity
                .update(&EnterpriseIdentityUpdateCommand {
                    actor: self.management_actor.clone(),
                    command: EnterpriseIdentityUpdateCommandCommand::EnterpriseIdentityUpdate,
                    expected_revision: identity.revision,
                    payload:
                        EnterpriseIdentityUpdatePayload::EnterpriseExternalIdentityRevokePayload(
                            EnterpriseExternalIdentityRevokePayload {
                                action: EnterpriseExternalIdentityRevokePayloadAction::Revoke,
                                external_identity_id: identity.id,
                                kind: EnterpriseExternalIdentityRevokePayloadKind::ExternalIdentity,
                            },
                        ),
                    request_id: derived_request_id(&request.operation_id, b"identity-revoked"),
                    schema_version: SchemaVersion::WinwincodeV1,
                    scope: organization_scope(&request.identity.organization_id),
                })
                .map_err(|error| identity_error(&error))?;
        }
        Ok(ExternalIdentityLifecycleOutcome::User(
            ExternalIdentityPrincipal {
                actor,
                authorized_scopes: Vec::new(),
                organization_id: request.identity.organization_id.clone(),
                external_identity_id: external_identity_id(&request.identity),
            },
        ))
    }

    fn upsert_team(
        &mut self,
        request: &UpsertExternalTeam,
    ) -> Result<ExternalIdentityLifecycleOutcome, EnterpriseIdentityLifecycleError> {
        validate_operation_id(&request.operation_id)?;
        let canonical_state = match request.state.as_str() {
            "active" => "active",
            "archived" => "disabled",
            _ => {
                return Err(EnterpriseIdentityLifecycleError::new(
                    EnterpriseIdentityLifecycleErrorKind::InvalidRequest,
                ));
            }
        };
        let current = self
            .rbac
            .team(&request.organization_id, &request.team_id)
            .map_err(|error| rbac_error(&error))?;
        if let Some(team) = &current
            && team.display_name == request.display_name
            && team.state == canonical_state
            && team.role_assignments == request.role_assignments
        {
            return Ok(ExternalIdentityLifecycleOutcome::Team(team.clone()));
        }
        let authority = self
            .rbac
            .authority_seal(&request.organization_id)
            .map_err(|error| rbac_error(&error))?;
        let response = self
            .rbac
            .update_team(&EnterpriseTeamUpdateCommand {
                actor: self.management_actor.clone(),
                command: EnterpriseTeamUpdateCommandCommand::EnterpriseTeamUpdate,
                expected_revision: authority.revision,
                payload: EnterpriseTeamUpdatePayload {
                    display_name: request.display_name.clone(),
                    role_assignments: request.role_assignments.clone(),
                    state: canonical_state.to_owned(),
                    team_id: request.team_id.clone(),
                },
                request_id: derived_request_id(&request.operation_id, b"team-upsert"),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: organization_scope(&request.organization_id),
            })
            .map_err(|error| rbac_error(&error))?;
        Ok(ExternalIdentityLifecycleOutcome::Team(response.result))
    }
}

#[must_use]
pub fn external_identity_id(reference: &ExternalIdentityReference) -> ExternalIdentityId {
    ExternalIdentityId(derived_id(
        "xid_",
        b"winwincode.external-identity.v1\0",
        &[
            reference.organization_id.0.as_bytes(),
            reference.provider.as_str().as_bytes(),
            reference.issuer_sha256.0.as_bytes(),
            reference.subject_sha256.0.as_bytes(),
        ],
    ))
}

#[must_use]
pub fn membership_id(user_id: &UserId) -> EnterpriseMembershipId {
    EnterpriseMembershipId(derived_id(
        "mbr_",
        b"winwincode.enterprise-membership.v1\0",
        &[user_id.0.as_bytes()],
    ))
}

fn organization_scope(organization_id: &OrganizationId) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: organization_id.clone(),
    }
}

fn user_actor(user_id: &UserId) -> Actor {
    Actor::UserActor(UserActor {
        id: user_id.clone(),
        kind: UserActorKind::User,
    })
}

fn derived_request_id(operation_id: &str, phase: &[u8]) -> RequestId {
    RequestId(derived_id(
        "req_",
        b"winwincode.enterprise-identity-lifecycle-request.v1\0",
        &[operation_id.as_bytes(), phase],
    ))
}

fn derived_id(prefix: &str, namespace: &[u8], facts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    for fact in facts {
        digest.update((fact.len() as u64).to_be_bytes());
        digest.update(fact);
    }
    let digest = digest.finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    format!("{prefix}{suffix}")
}

fn validate_operation_id(value: &str) -> Result<(), EnterpriseIdentityLifecycleError> {
    if value.is_empty()
        || value.len() > 512
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(EnterpriseIdentityLifecycleError::new(
            EnterpriseIdentityLifecycleErrorKind::InvalidRequest,
        ));
    }
    Ok(())
}

fn identity_error(error: &EnterpriseIdentityError) -> EnterpriseIdentityLifecycleError {
    let kind = match error.kind() {
        EnterpriseIdentityErrorKind::InvalidRequest | EnterpriseIdentityErrorKind::ScopeDenied => {
            EnterpriseIdentityLifecycleErrorKind::InvalidRequest
        }
        EnterpriseIdentityErrorKind::NotFound => EnterpriseIdentityLifecycleErrorKind::NotFound,
        EnterpriseIdentityErrorKind::WrongState => EnterpriseIdentityLifecycleErrorKind::WrongState,
        EnterpriseIdentityErrorKind::RevisionConflict
        | EnterpriseIdentityErrorKind::RequestConflict
        | EnterpriseIdentityErrorKind::Authentication
        | EnterpriseIdentityErrorKind::Storage
        | EnterpriseIdentityErrorKind::ClockUnavailable
        | EnterpriseIdentityErrorKind::EntropyUnavailable => {
            EnterpriseIdentityLifecycleErrorKind::IdentityUnavailable
        }
    };
    EnterpriseIdentityLifecycleError::new(kind)
}

fn rbac_error(error: &EnterpriseRbacError) -> EnterpriseIdentityLifecycleError {
    let kind = match error.kind() {
        EnterpriseRbacErrorKind::InvalidRequest | EnterpriseRbacErrorKind::ScopeDenied => {
            EnterpriseIdentityLifecycleErrorKind::InvalidRequest
        }
        EnterpriseRbacErrorKind::NotFound => EnterpriseIdentityLifecycleErrorKind::NotFound,
        EnterpriseRbacErrorKind::WrongState => EnterpriseIdentityLifecycleErrorKind::WrongState,
        EnterpriseRbacErrorKind::RevisionConflict
        | EnterpriseRbacErrorKind::RequestConflict
        | EnterpriseRbacErrorKind::Storage
        | EnterpriseRbacErrorKind::ClockUnavailable => {
            EnterpriseIdentityLifecycleErrorKind::RbacUnavailable
        }
    };
    EnterpriseIdentityLifecycleError::new(kind)
}
