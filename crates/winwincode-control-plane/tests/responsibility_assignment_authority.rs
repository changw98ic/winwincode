// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use winwincode_api::generated::{
    AcceptanceCriterionInput, Actor, ActorId, DeliveryCreateCommand, DeliveryCreateCommandCommand,
    DeliveryCreatePayload, DeliverySpecInput, EnterpriseMembershipUpdateCommand,
    EnterpriseMembershipUpdateCommandCommand, EnterpriseMembershipUpdatePayload,
    EnterpriseOrganizationUpdateCommand, EnterpriseOrganizationUpdateCommandCommand,
    EnterpriseOrganizationUpdatePayload, EnterprisePermission, EnterpriseRoleAssignment,
    EnterpriseRolePermissionRule, EnterpriseRoleUpdateCommand, EnterpriseRoleUpdateCommandCommand,
    EnterpriseRoleUpdatePayload, EnterpriseTeamUpdateCommand, EnterpriseTeamUpdateCommandCommand,
    EnterpriseTeamUpdatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind,
    RepositoryScope, RepositoryScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CollaborationInboxAudience, CollaborationInboxAuthorityPort, CollaborationInboxClock,
    CollaborationInboxClockError, CollaborationInboxFilter, CollaborationInboxListRequest,
    CollaborationInboxService, ControlPlane, CreateProductSessionCommand, DeliveryAdvanceAuthority,
    DeliveryAttentionAuthority, DeliveryAuthorityError, DeliveryAuthorityPort,
    DeliveryAuthorityRequest, DeliverySpecificationAuthority, DeliveryVerdictAuthority,
    DurableCollaborationInboxSource, EnterpriseCollaborationInboxAuthority, EnterpriseRbacService,
    EnterpriseResponsibilityAuthority, EventPublishError, EventPublisher, OutboxEvent,
    ProductSessionCommandContext, ProductSessionService, ResponsibilityAssignmentAction,
    ResponsibilityAssignmentClock, ResponsibilityAssignmentClockError,
    ResponsibilityAssignmentCommand, ResponsibilityAssignmentContext,
    ResponsibilityAssignmentErrorKind, ResponsibilityAssignmentListRequest,
    ResponsibilityAssignmentService, ResponsibilityAssignmentState, ResponsibilityReviewKind,
    ResponsibilityRole, ResponsibilityTarget,
};
use winwincode_delivery::domain::{
    DELIVERY_SCHEMA_VERSION, DeliveryStage, RepositoryKind, RepositoryRef,
};
use winwincode_domain::{
    ControlPlaneEventId, CredentialReferenceId, DeliveryId, EnterpriseMembershipId,
    EnterpriseRoleId, EnterpriseRoleVersion, EnterpriseTeamId, Instant, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, UserId,
    WorkspaceId,
};
use winwincode_storage::{
    PublicEventActor, PublicEventScope, ReceiptIdentity, SqliteStorage, receipt_actor_key,
    receipt_scope_key,
};

const NOW: u64 = 1_800_000_000_000;
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedClock;

impl ResponsibilityAssignmentClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, ResponsibilityAssignmentClockError> {
        Ok(NOW)
    }
}

impl CollaborationInboxClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, CollaborationInboxClockError> {
        Ok(NOW)
    }
}

struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct RepositoryAuthority;

impl DeliveryAuthorityPort for RepositoryAuthority {
    fn specification(
        &mut self,
        _request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliverySpecificationAuthority, DeliveryAuthorityError> {
        Ok(DeliverySpecificationAuthority {
            now_millis: NOW,
            repository: RepositoryRef {
                schema_version: DELIVERY_SCHEMA_VERSION,
                kind: RepositoryKind::LocalGit,
                locator: "file:///workspace/repository".to_owned(),
            },
            source_ref: None,
            max_rework_attempts: 2,
            criterion_verification_methods: vec![(
                "criterion-1".to_owned(),
                "cargo test".to_owned(),
            )],
        })
    }

    fn advance(
        &mut self,
        _request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAdvanceAuthority, DeliveryAuthorityError> {
        Err(DeliveryAuthorityError::new("advance is not used"))
    }

    fn resolve_attention(
        &mut self,
        _request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAttentionAuthority, DeliveryAuthorityError> {
        Err(DeliveryAuthorityError::new("Attention is not used"))
    }

    fn verdict(
        &mut self,
        _request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryVerdictAuthority, DeliveryAuthorityError> {
        Err(DeliveryAuthorityError::new("verdict is not used"))
    }
}

struct Fixture {
    root: PathBuf,
    scope: RepositoryScope,
    admin_id: UserId,
    reviewer_id: UserId,
    rbac_revision: i64,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-responsibility-authority-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).expect("fixture directory");
        Self {
            root,
            scope: RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id: OrganizationId(id("org", 1)),
                workspace_id: WorkspaceId(id("wsp", 1)),
                project_id: ProjectId(id("prj", 1)),
                repository_id: RepositoryId(id("rep", 1)),
            },
            admin_id: UserId(id("usr", 1)),
            reviewer_id: UserId(id("usr", 2)),
            rbac_revision: 0,
        }
    }

    fn setup_rbac(&mut self) {
        let service = self.rbac();
        service
            .update_organization(&organization_command(self, 1, self.rbac_revision))
            .expect("create Organization");
        self.rbac_revision += 1;
        for (request_number, role_number, permission) in [
            (2, 1, EnterprisePermission::AssignmentAssign),
            (3, 2, EnterprisePermission::AssignmentReassign),
            (4, 3, EnterprisePermission::AssignmentReview),
            (5, 4, EnterprisePermission::CollaborationRead),
        ] {
            service
                .update_role(&role_command(
                    self,
                    request_number,
                    self.rbac_revision,
                    role_number,
                    permission,
                ))
                .expect("create Role");
            self.rbac_revision += 1;
        }
        service
            .update_team(&team_command(self, 6, self.rbac_revision))
            .expect("create collaboration Team");
        self.rbac_revision += 1;
        service
            .update_membership(&membership_command(
                self,
                7,
                self.rbac_revision,
                1,
                self.admin_id.clone(),
                vec![
                    role_assignment(self, 1),
                    role_assignment(self, 2),
                    role_assignment(self, 4),
                ],
                "active",
            ))
            .expect("create administrator Membership");
        self.rbac_revision += 1;
        service
            .update_membership(&membership_command(
                self,
                8,
                self.rbac_revision,
                2,
                self.reviewer_id.clone(),
                vec![role_assignment(self, 3)],
                "active",
            ))
            .expect("create reviewer Membership");
        self.rbac_revision += 1;
    }

    fn rbac(&self) -> EnterpriseRbacService {
        EnterpriseRbacService::new(Box::new(
            SqliteStorage::open(&self.root).expect("open RBAC storage"),
        ))
    }

    fn assignment_service(&self) -> ResponsibilityAssignmentService {
        ResponsibilityAssignmentService::with_clock(
            Box::new(SqliteStorage::open(&self.root).expect("open assignment storage")),
            Box::new(EnterpriseResponsibilityAuthority::new(
                Arc::new(self.rbac()),
                Box::new(SqliteStorage::open(&self.root).expect("open target storage")),
            )),
            Box::new(FixedClock),
        )
    }

    fn admin(&self) -> Actor {
        user_actor(self.admin_id.clone())
    }

    fn reviewer(&self) -> Actor {
        user_actor(self.reviewer_id.clone())
    }

    fn repository_scope(&self) -> Scope {
        Scope::RepositoryScope(self.scope.clone())
    }

    fn create_product_session(&self, number: u64) -> ProductSessionId {
        let session_id = ProductSessionId(id("psn", number));
        let public_scope = PublicEventScope::Repository {
            organization_id: self.scope.organization_id.clone(),
            workspace_id: self.scope.workspace_id.clone(),
            project_id: self.scope.project_id.clone(),
            repository_id: self.scope.repository_id.clone(),
        };
        let public_actor = PublicEventActor::User {
            id: self.admin_id.clone(),
        };
        let context = ProductSessionCommandContext {
            receipt_identity: ReceiptIdentity::new(
                receipt_actor_key(&public_actor).expect("actor key"),
                receipt_scope_key(&public_scope).expect("scope key"),
                RequestId(id("req", 100 + number)),
            )
            .expect("receipt identity"),
            expected_revision: 0,
            event_id: ControlPlaneEventId(id("evt", 100 + number)),
            occurred_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
            public_actor,
            public_scope,
        };
        let mut storage = SqliteStorage::open(&self.root).expect("open ProductSession storage");
        ProductSessionService::new(&mut storage)
            .create(&CreateProductSessionCommand {
                context,
                product_session_id: session_id.clone(),
                project_id: self.scope.project_id.clone(),
                repository_id: self.scope.repository_id.clone(),
                title: "Review session".to_owned(),
                model_route: ModelRoute {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    model_id: "fixture-model".to_owned(),
                    provider_id: "fixture-provider".to_owned(),
                },
            })
            .expect("create ProductSession");
        session_id
    }

    fn create_delivery(&self, number: u64) -> DeliveryId {
        let delivery_id = DeliveryId(id("dlv", number));
        let mut control_plane = ControlPlane::start(
            Box::new(SqliteStorage::open(&self.root).expect("open Delivery storage")),
            Box::new(NoopPublisher),
        )
        .expect("start Control Plane");
        control_plane
            .install_delivery_authority_port(Box::new(RepositoryAuthority))
            .expect("install Delivery authority");
        control_plane
            .delivery_create(&DeliveryCreateCommand {
                actor: self.admin(),
                command: DeliveryCreateCommandCommand::DeliveryCreate,
                expected_revision: Revision(0),
                payload: DeliveryCreatePayload {
                    delivery_id: delivery_id.clone(),
                    spec: DeliverySpecInput {
                        acceptance_criteria: vec![AcceptanceCriterionInput {
                            id: "criterion-1".to_owned(),
                            required: true,
                            title: "Tests pass".to_owned(),
                        }],
                        base_revision: "0123456789abcdef".to_owned(),
                        goal: "Ship the exact implementation".to_owned(),
                        scope: vec!["src".to_owned()],
                        out_of_scope: vec!["target".to_owned()],
                        constraints: vec!["tests pass".to_owned()],
                        source_product_session_id: None,
                        publication_target: None,
                        repository_id: self.scope.repository_id.clone(),
                        title: "Assignment target".to_owned(),
                    },
                    tasks: Vec::new(),
                },
                request_id: RequestId(id("req", 200 + number)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: self.scope.clone(),
            })
            .expect("create Delivery");
        control_plane.shutdown().expect("shutdown Control Plane");
        delivery_id
    }

    fn revoke_reviewer(&mut self) {
        self.rbac()
            .update_membership(&membership_command(
                self,
                20,
                self.rbac_revision,
                2,
                self.reviewer_id.clone(),
                vec![role_assignment(self, 3)],
                "disabled",
            ))
            .expect("revoke reviewer Membership");
        self.rbac_revision += 1;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

#[test]
fn product_session_assignment_uses_live_rbac_and_restart_safe_receipts() {
    let mut fixture = Fixture::new("product-session");
    fixture.setup_rbac();
    let session_id = fixture.create_product_session(1);
    let target = ResponsibilityTarget::ProductSession {
        product_session_id: session_id,
    };
    let assign = assignment_command(
        &fixture,
        fixture.admin(),
        30,
        0,
        target.clone(),
        ResponsibilityRole::Reviewer,
        ResponsibilityAssignmentAction::Assign {
            principal_user_id: fixture.reviewer_id.clone(),
            expires_at_millis: None,
        },
    );
    let assigned = fixture
        .assignment_service()
        .apply(&assign)
        .expect("assign reviewer");
    assert_eq!(
        assigned.assignment.state,
        ResponsibilityAssignmentState::PendingAcceptance
    );
    let accept = assignment_command(
        &fixture,
        fixture.reviewer(),
        31,
        1,
        target.clone(),
        ResponsibilityRole::Reviewer,
        ResponsibilityAssignmentAction::Accept,
    );
    let accepted = fixture
        .assignment_service()
        .apply(&accept)
        .expect("accept responsibility");
    assert_eq!(
        accepted.assignment.state,
        ResponsibilityAssignmentState::Active
    );
    assert_production_inbox_composition(&fixture);

    fixture.revoke_reviewer();
    let replay = fixture
        .assignment_service()
        .apply(&accept)
        .expect("exact replay reads immutable receipt first");
    assert!(replay.replayed);
    assert_eq!(replay.assignment, accepted.assignment);
    let revoked = fixture
        .assignment_service()
        .apply(&assignment_command(
            &fixture,
            fixture.admin(),
            32,
            2,
            target.clone(),
            ResponsibilityRole::Reviewer,
            ResponsibilityAssignmentAction::RevokeDeparted,
        ))
        .expect("revoke departed reviewer");
    assert_eq!(
        revoked.assignment.state,
        ResponsibilityAssignmentState::Revoked
    );

    let listed = fixture
        .assignment_service()
        .list(&ResponsibilityAssignmentListRequest {
            actor: fixture.admin(),
            authenticated_scopes: vec![fixture.repository_scope()],
            scope: fixture.scope.clone(),
            target: Some(target),
            role: None,
            principal_user_id: None,
            include_ended: true,
        })
        .expect("list after restart");
    assert_eq!(listed, vec![revoked.assignment]);
}

fn assert_production_inbox_composition(fixture: &Fixture) {
    let mut inbox_authority = EnterpriseCollaborationInboxAuthority::new(
        Arc::new(fixture.assignment_service()),
        Arc::new(fixture.rbac()),
    );
    let inbox_cut = inbox_authority
        .authorize(
            &fixture.admin(),
            &[fixture.repository_scope()],
            &fixture.scope,
            &CollaborationInboxAudience::Personal(fixture.admin_id.clone()),
        )
        .expect("load sealed collaboration authority after restart");
    assert_eq!(inbox_cut.visible_team_ids, vec![team(1)]);
    assert_eq!(inbox_cut.assignments.len(), 1);
    assert_eq!(inbox_cut.assignments[0].team_ids, vec![team(1)]);
    assert_eq!(inbox_cut.state_guards.len(), 2);
    let mut inbox = CollaborationInboxService::with_clock(
        Box::new(SqliteStorage::open(&fixture.root).expect("open Inbox catalog")),
        Box::new(DurableCollaborationInboxSource::new(Box::new(
            SqliteStorage::open(&fixture.root).expect("open Inbox source"),
        ))),
        Box::new(EnterpriseCollaborationInboxAuthority::new(
            Arc::new(fixture.assignment_service()),
            Arc::new(fixture.rbac()),
        )),
        Box::new(FixedClock),
    );
    let page = inbox
        .list(&CollaborationInboxListRequest {
            actor: fixture.admin(),
            authenticated_scopes: vec![fixture.repository_scope()],
            scope: fixture.scope.clone(),
            audience: CollaborationInboxAudience::Team(team(1)),
            filter: CollaborationInboxFilter::default(),
            limit: 20,
            cursor: None,
        })
        .expect("compose production Inbox after restart");
    assert!(page.items.is_empty());
}

#[test]
fn delivery_scope_and_stage_existence_are_checked_before_assignment_write() {
    let mut fixture = Fixture::new("delivery-target");
    let delivery_id = fixture.create_delivery(1);
    fixture.setup_rbac();
    let service = fixture.assignment_service();
    let delivery_target = ResponsibilityTarget::Delivery {
        delivery_id: delivery_id.clone(),
    };
    service
        .apply(&assignment_command(
            &fixture,
            fixture.admin(),
            40,
            0,
            delivery_target,
            ResponsibilityRole::Assignee,
            ResponsibilityAssignmentAction::Assign {
                principal_user_id: fixture.admin_id.clone(),
                expires_at_millis: None,
            },
        ))
        .expect("assign Delivery owner");

    let missing_stage = ResponsibilityTarget::DeliveryStage {
        delivery_id: delivery_id.clone(),
        stage: DeliveryStage::PlanReview,
    };
    let error = service
        .apply(&assignment_command(
            &fixture,
            fixture.admin(),
            41,
            0,
            missing_stage,
            ResponsibilityRole::Reviewer,
            ResponsibilityAssignmentAction::Assign {
                principal_user_id: fixture.reviewer_id.clone(),
                expires_at_millis: None,
            },
        ))
        .expect_err("a stage without a canonical StageRun is rejected");
    assert_eq!(
        error.kind(),
        ResponsibilityAssignmentErrorKind::AuthorizationDenied
    );

    let mut foreign_scope = fixture.scope.clone();
    foreign_scope.repository_id = RepositoryId(id("rep", 9));
    let foreign = ResponsibilityAssignmentCommand {
        context: ResponsibilityAssignmentContext {
            actor: fixture.admin(),
            authenticated_scopes: vec![Scope::RepositoryScope(foreign_scope.clone())],
            scope: foreign_scope,
            request_id: RequestId(id("req", 42)),
            expected_revision: 0,
        },
        target: ResponsibilityTarget::Review {
            delivery_id,
            review: ResponsibilityReviewKind::Solution,
        },
        role: ResponsibilityRole::Reviewer,
        action: ResponsibilityAssignmentAction::Assign {
            principal_user_id: fixture.reviewer_id.clone(),
            expires_at_millis: None,
        },
    };
    let error = service
        .apply(&foreign)
        .expect_err("foreign repository scope is denied");
    assert_eq!(
        error.kind(),
        ResponsibilityAssignmentErrorKind::AuthorizationDenied
    );

    let assignments = service
        .list(&ResponsibilityAssignmentListRequest {
            actor: fixture.admin(),
            authenticated_scopes: vec![fixture.repository_scope()],
            scope: fixture.scope.clone(),
            target: None,
            role: None,
            principal_user_id: None,
            include_ended: true,
        })
        .expect("list current repository assignments");
    assert_eq!(assignments.len(), 1);
}

fn organization_command(
    fixture: &Fixture,
    request_number: u64,
    expected_revision: i64,
) -> EnterpriseOrganizationUpdateCommand {
    EnterpriseOrganizationUpdateCommand {
        actor: fixture.admin(),
        command: EnterpriseOrganizationUpdateCommandCommand::EnterpriseOrganizationUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseOrganizationUpdatePayload {
            display_name: "Example Organization".to_owned(),
            organization_id: fixture.scope.organization_id.clone(),
            slug: "example-organization".to_owned(),
            state: "active".to_owned(),
        },
        request_id: RequestId(id("req", request_number)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: fixture.scope.organization_id.clone(),
        }),
    }
}

fn role_command(
    fixture: &Fixture,
    request_number: u64,
    expected_revision: i64,
    role_number: u64,
    permission: EnterprisePermission,
) -> EnterpriseRoleUpdateCommand {
    EnterpriseRoleUpdateCommand {
        actor: fixture.admin(),
        command: EnterpriseRoleUpdateCommandCommand::EnterpriseRoleUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseRoleUpdatePayload {
            conflicting_role_ids: Vec::new(),
            display_name: format!("Role {role_number}"),
            inherited_roles: Vec::new(),
            role_id: EnterpriseRoleId(id("rol", role_number)),
            rules: vec![EnterpriseRolePermissionRule {
                effect: "allow".to_owned(),
                permission,
            }],
            state: "active".to_owned(),
        },
        request_id: RequestId(id("req", request_number)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: fixture.scope.organization_id.clone(),
        },
    }
}

fn membership_command(
    fixture: &Fixture,
    request_number: u64,
    expected_revision: i64,
    membership_number: u64,
    user_id: UserId,
    role_assignments: Vec<EnterpriseRoleAssignment>,
    state: &str,
) -> EnterpriseMembershipUpdateCommand {
    EnterpriseMembershipUpdateCommand {
        actor: fixture.admin(),
        command: EnterpriseMembershipUpdateCommandCommand::EnterpriseMembershipUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseMembershipUpdatePayload {
            actor_id: ActorId::UserId(user_id),
            display_name: format!("Member {membership_number}"),
            membership_id: EnterpriseMembershipId(id("mem", membership_number)),
            role_assignments,
            state: state.to_owned(),
            team_ids: vec![team(1)],
        },
        request_id: RequestId(id("req", request_number)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: fixture.scope.organization_id.clone(),
        },
    }
}

fn team_command(
    fixture: &Fixture,
    request_number: u64,
    expected_revision: i64,
) -> EnterpriseTeamUpdateCommand {
    EnterpriseTeamUpdateCommand {
        actor: fixture.admin(),
        command: EnterpriseTeamUpdateCommandCommand::EnterpriseTeamUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseTeamUpdatePayload {
            display_name: "Review Team".to_owned(),
            role_assignments: Vec::new(),
            state: "active".to_owned(),
            team_id: team(1),
        },
        request_id: RequestId(id("req", request_number)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: fixture.scope.organization_id.clone(),
        },
    }
}

fn team(number: u64) -> EnterpriseTeamId {
    EnterpriseTeamId(id("tem", number))
}

fn role_assignment(fixture: &Fixture, role_number: u64) -> EnterpriseRoleAssignment {
    EnterpriseRoleAssignment {
        expires_at: None,
        not_before: None,
        role_id: EnterpriseRoleId(id("rol", role_number)),
        role_version: EnterpriseRoleVersion(1),
        scope: fixture.repository_scope(),
        scope_mode: "exact".to_owned(),
    }
}

fn assignment_command(
    fixture: &Fixture,
    actor: Actor,
    request_number: u64,
    expected_revision: u64,
    target: ResponsibilityTarget,
    role: ResponsibilityRole,
    action: ResponsibilityAssignmentAction,
) -> ResponsibilityAssignmentCommand {
    ResponsibilityAssignmentCommand {
        context: ResponsibilityAssignmentContext {
            actor,
            authenticated_scopes: vec![fixture.repository_scope()],
            scope: fixture.scope.clone(),
            request_id: RequestId(id("req", request_number)),
            expected_revision,
        },
        target,
        role,
        action,
    }
}

fn user_actor(id: UserId) -> Actor {
    Actor::UserActor(UserActor {
        id,
        kind: UserActorKind::User,
    })
}

fn id(prefix: &str, number: u64) -> String {
    format!("{prefix}_{number:026}")
}
