// SPDX-License-Identifier: Apache-2.0

//! Production Identity/RBAC and target authority for responsibility assignments.

use std::sync::Arc;

use winwincode_api::generated::{Actor, EnterprisePermission, Scope, UserActor, UserActorKind};
use winwincode_delivery::domain::DeliveryStage;
use winwincode_domain::{Revision, Sha256Digest};
use winwincode_storage::{
    ProductStateStorage, PublicEventScope, StateRevisionGuard, receipt_scope_key,
};

use crate::{
    DeliveryApplicationError, EnterpriseRbacService, ProductSessionPersistence,
    ProductSessionService, ProductSessionServiceErrorCode, RbacAuthoritySeal, RbacDecision,
    RbacDenialReason, ResponsibilityAssignmentAction, ResponsibilityAssignmentListRequest,
    ResponsibilityAuthorityError, ResponsibilityAuthorityPort, ResponsibilityAuthorityRequest,
    ResponsibilityCommandAuthority, ResponsibilityInboxAuthority, ResponsibilityListAuthority,
    ResponsibilityPrincipalAuthority, ResponsibilityReviewKind, ResponsibilityRole,
    ResponsibilityTarget, load_delivery_authority_seal,
};

/// Exact target facts returned by the ProductSession/Delivery owner.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsibilityTargetAuthoritySeal {
    pub target: ResponsibilityTarget,
    pub target_revision: u64,
    pub target_sha256: Sha256Digest,
    pub state_guard: StateRevisionGuard,
    pub scope_guard: Option<StateRevisionGuard>,
}

/// Closed target read boundary used by the RBAC composition.
pub trait ResponsibilityTargetAuthorityPort: Send {
    /// Resolves one exact target inside one repository scope.
    ///
    /// # Errors
    ///
    /// Returns denial for a foreign or missing target and unavailability for
    /// corrupt or inaccessible canonical state.
    fn target_authority(
        &mut self,
        scope: &winwincode_api::generated::RepositoryScope,
        target: &ResponsibilityTarget,
    ) -> Result<ResponsibilityTargetAuthoritySeal, ResponsibilityAuthorityError>;
}

/// Canonical SQLite-backed target reader. It delegates key construction and
/// state decoding to the `ProductSession` and Delivery owners.
pub struct DurableResponsibilityTargetAuthority {
    storage: Box<dyn ProductSessionPersistence>,
}

impl DurableResponsibilityTargetAuthority {
    #[must_use]
    pub fn new(storage: Box<dyn ProductSessionPersistence>) -> Self {
        Self { storage }
    }
}

impl ResponsibilityTargetAuthorityPort for DurableResponsibilityTargetAuthority {
    fn target_authority(
        &mut self,
        scope: &winwincode_api::generated::RepositoryScope,
        target: &ResponsibilityTarget,
    ) -> Result<ResponsibilityTargetAuthoritySeal, ResponsibilityAuthorityError> {
        match target {
            ResponsibilityTarget::ProductSession { product_session_id } => {
                let scope_key = receipt_scope_key(&PublicEventScope::Repository {
                    organization_id: scope.organization_id.clone(),
                    workspace_id: scope.workspace_id.clone(),
                    project_id: scope.project_id.clone(),
                    repository_id: scope.repository_id.clone(),
                })
                .map_err(|_| ResponsibilityAuthorityError::Denied)?;
                let seal = ProductSessionService::new(self.storage.as_mut())
                    .authority_seal(&scope_key, product_session_id)
                    .map_err(|error| product_session_error(&error))?;
                let session = seal.record.session();
                if session.id() != product_session_id
                    || session.project_id() != &scope.project_id
                    || session.repository_id() != &scope.repository_id
                {
                    return Err(ResponsibilityAuthorityError::Denied);
                }
                Ok(ResponsibilityTargetAuthoritySeal {
                    target: target.clone(),
                    target_revision: seal.target_revision,
                    target_sha256: seal.target_sha256,
                    state_guard: seal.state_guard,
                    scope_guard: None,
                })
            }
            ResponsibilityTarget::Delivery { delivery_id }
            | ResponsibilityTarget::DeliveryStage { delivery_id, .. }
            | ResponsibilityTarget::Review { delivery_id, .. } => {
                let seal = load_delivery_authority_seal(
                    self.storage.as_ref() as &dyn ProductStateStorage,
                    scope,
                    delivery_id,
                )
                .map_err(|error| delivery_error(&error))?;
                if !delivery_target_exists(&seal.delivery, target) {
                    return Err(ResponsibilityAuthorityError::Denied);
                }
                Ok(ResponsibilityTargetAuthoritySeal {
                    target: target.clone(),
                    target_revision: seal.target_revision,
                    target_sha256: seal.target_sha256,
                    state_guard: seal.state_guard,
                    scope_guard: Some(seal.scope_guard),
                })
            }
        }
    }
}

/// Unique production composition over current RBAC and durable target facts.
pub struct EnterpriseResponsibilityAuthority {
    rbac: Arc<EnterpriseRbacService>,
    targets: Box<dyn ResponsibilityTargetAuthorityPort>,
}

impl EnterpriseResponsibilityAuthority {
    #[must_use]
    pub fn new(
        rbac: Arc<EnterpriseRbacService>,
        storage: Box<dyn ProductSessionPersistence>,
    ) -> Self {
        Self {
            rbac,
            targets: Box::new(DurableResponsibilityTargetAuthority::new(storage)),
        }
    }

    fn authorize_caller(
        &self,
        actor: &Actor,
        authenticated_scopes: &[Scope],
        scope: &Scope,
        permission: &EnterprisePermission,
    ) -> Result<(RbacDecision, RbacAuthoritySeal), ResponsibilityAuthorityError> {
        let decision = self
            .rbac
            .authorize(actor, authenticated_scopes, scope, permission)
            .map_err(|_| ResponsibilityAuthorityError::Unavailable)?;
        if !decision.allowed {
            return Err(ResponsibilityAuthorityError::Denied);
        }
        let seal = decision
            .authority_seal
            .clone()
            .ok_or(ResponsibilityAuthorityError::Unavailable)?;
        Ok((decision, seal))
    }
}

impl ResponsibilityAuthorityPort for EnterpriseResponsibilityAuthority {
    fn command_authority(
        &mut self,
        request: ResponsibilityAuthorityRequest<'_>,
    ) -> Result<ResponsibilityCommandAuthority, ResponsibilityAuthorityError> {
        let command = request.command();
        let scope = Scope::RepositoryScope(command.context.scope.clone());
        let (_, caller_seal) = self.authorize_caller(
            &command.context.actor,
            &command.context.authenticated_scopes,
            &scope,
            &caller_permission(&command.action, command.role),
        )?;
        let principal_user_id = request
            .requested_principal()
            .cloned()
            .ok_or(ResponsibilityAuthorityError::Denied)?;
        let principal = self
            .rbac
            .member_is_eligible(
                &Actor::UserActor(UserActor {
                    id: principal_user_id.clone(),
                    kind: UserActorKind::User,
                }),
                &scope,
                &role_permission(command.role),
            )
            .map_err(|_| ResponsibilityAuthorityError::Unavailable)?;
        let principal_seal = principal
            .authority_seal
            .as_ref()
            .ok_or(ResponsibilityAuthorityError::Denied)?;
        if principal_seal != &caller_seal {
            return Err(ResponsibilityAuthorityError::Unavailable);
        }
        let target = self
            .targets
            .target_authority(&command.context.scope, &command.target)?;
        if target.target != command.target {
            return Err(ResponsibilityAuthorityError::Unavailable);
        }
        Ok(ResponsibilityCommandAuthority {
            actor: command.context.actor.clone(),
            scope: command.context.scope.clone(),
            operation: command.action.operation(),
            target: command.target.clone(),
            role: command.role,
            permission_granted: true,
            actor_active: true,
            principal: ResponsibilityPrincipalAuthority {
                user_id: principal_user_id,
                active: principal_is_active(&principal),
                role_eligible: principal.allowed,
            },
            target_revision: target.target_revision,
            target_sha256: target.target_sha256,
            rbac_revision: seal_revision(&caller_seal)?,
            rbac_sha256: caller_seal.state_sha256,
            target_guard: target.state_guard,
            target_scope_guard: target.scope_guard,
            rbac_guard: caller_seal.state_guard,
        })
    }

    fn list_authority(
        &mut self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityListAuthority, ResponsibilityAuthorityError> {
        let scope = Scope::RepositoryScope(request.scope.clone());
        let (_, seal) = self.authorize_caller(
            &request.actor,
            &request.authenticated_scopes,
            &scope,
            &EnterprisePermission::AssignmentAssign,
        )?;
        Ok(ResponsibilityListAuthority {
            actor: request.actor.clone(),
            scope: request.scope.clone(),
            permission_granted: true,
            actor_active: true,
            rbac_revision: seal_revision(&seal)?,
            rbac_sha256: seal.state_sha256,
        })
    }

    fn inbox_authority(
        &mut self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityInboxAuthority, ResponsibilityAuthorityError> {
        let scope = Scope::RepositoryScope(request.scope.clone());
        let (_, seal) = self.authorize_caller(
            &request.actor,
            &request.authenticated_scopes,
            &scope,
            &EnterprisePermission::CollaborationRead,
        )?;
        Ok(ResponsibilityInboxAuthority {
            actor: request.actor.clone(),
            scope: request.scope.clone(),
            permission_granted: true,
            actor_active: true,
            rbac_revision: seal_revision(&seal)?,
            rbac_sha256: seal.state_sha256,
            rbac_guard: seal.state_guard,
        })
    }
}

fn caller_permission(
    action: &ResponsibilityAssignmentAction,
    role: ResponsibilityRole,
) -> EnterprisePermission {
    match action {
        ResponsibilityAssignmentAction::Assign { .. } => EnterprisePermission::AssignmentAssign,
        ResponsibilityAssignmentAction::Reassign { .. }
        | ResponsibilityAssignmentAction::Expire
        | ResponsibilityAssignmentAction::RevokeDeparted => {
            EnterprisePermission::AssignmentReassign
        }
        ResponsibilityAssignmentAction::Accept => role_permission(role),
    }
}

const fn role_permission(role: ResponsibilityRole) -> EnterprisePermission {
    match role {
        ResponsibilityRole::Assignee => EnterprisePermission::AssignmentAssign,
        ResponsibilityRole::Reviewer => EnterprisePermission::AssignmentReview,
        ResponsibilityRole::Approver => EnterprisePermission::AssignmentApprove,
    }
}

fn principal_is_active(decision: &RbacDecision) -> bool {
    !matches!(
        decision.denial_reason,
        Some(
            RbacDenialReason::OrganizationUnavailable
                | RbacDenialReason::OrganizationInactive
                | RbacDenialReason::MembershipMissing
                | RbacDenialReason::MembershipInactive
        )
    )
}

fn seal_revision(seal: &RbacAuthoritySeal) -> Result<u64, ResponsibilityAuthorityError> {
    let Revision(revision) = seal.revision;
    u64::try_from(revision)
        .ok()
        .filter(|revision| *revision > 0)
        .ok_or(ResponsibilityAuthorityError::Unavailable)
}

fn product_session_error(
    error: &crate::ProductSessionServiceError,
) -> ResponsibilityAuthorityError {
    match error.code() {
        ProductSessionServiceErrorCode::NotFound | ProductSessionServiceErrorCode::InvalidInput => {
            ResponsibilityAuthorityError::Denied
        }
        ProductSessionServiceErrorCode::AlreadyExists
        | ProductSessionServiceErrorCode::RevisionConflict
        | ProductSessionServiceErrorCode::RequestConflict
        | ProductSessionServiceErrorCode::InvalidState
        | ProductSessionServiceErrorCode::BindingIdentityMismatch
        | ProductSessionServiceErrorCode::BindingConflict
        | ProductSessionServiceErrorCode::WorkerSlotNotRunning
        | ProductSessionServiceErrorCode::MessageLimitExceeded
        | ProductSessionServiceErrorCode::StreamSequenceConflict
        | ProductSessionServiceErrorCode::CursorInvalid
        | ProductSessionServiceErrorCode::CredentialLeak
        | ProductSessionServiceErrorCode::ActorMismatch
        | ProductSessionServiceErrorCode::CorruptState
        | ProductSessionServiceErrorCode::Storage => ResponsibilityAuthorityError::Unavailable,
    }
}

fn delivery_error(error: &DeliveryApplicationError) -> ResponsibilityAuthorityError {
    match error {
        DeliveryApplicationError::InvalidRequest(_)
        | DeliveryApplicationError::ResourceNotFound(_) => ResponsibilityAuthorityError::Denied,
        DeliveryApplicationError::TrustedFactsUnavailable(_)
        | DeliveryApplicationError::ReadCursorExpired
        | DeliveryApplicationError::Command(_)
        | DeliveryApplicationError::Commit(_)
        | DeliveryApplicationError::Execution(_)
        | DeliveryApplicationError::Verdict(_)
        | DeliveryApplicationError::Storage(_) => ResponsibilityAuthorityError::Unavailable,
    }
}

fn delivery_target_exists(
    delivery: &winwincode_delivery::domain::Delivery,
    target: &ResponsibilityTarget,
) -> bool {
    let required_stage = match target {
        ResponsibilityTarget::Delivery { delivery_id } => return delivery.id() == delivery_id,
        ResponsibilityTarget::DeliveryStage { delivery_id, stage } => {
            if delivery.id() != delivery_id {
                return false;
            }
            *stage
        }
        ResponsibilityTarget::Review {
            delivery_id,
            review,
        } => {
            if delivery.id() != delivery_id {
                return false;
            }
            match review {
                ResponsibilityReviewKind::Solution => DeliveryStage::PlanReview,
                ResponsibilityReviewKind::Delivery => DeliveryStage::DeliveryReview,
            }
        }
        ResponsibilityTarget::ProductSession { .. } => return false,
    };
    delivery
        .snapshot()
        .stage_runs
        .iter()
        .any(|run| run.stage == required_stage)
}
