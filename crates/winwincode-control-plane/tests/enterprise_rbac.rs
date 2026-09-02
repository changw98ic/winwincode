// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use winwincode_api::generated::{
    Actor, ActorId, EnterpriseMembershipUpdateCommand, EnterpriseMembershipUpdateCommandCommand,
    EnterpriseMembershipUpdatePayload, EnterpriseOrganizationUpdateCommand,
    EnterpriseOrganizationUpdateCommandCommand, EnterpriseOrganizationUpdatePayload,
    EnterprisePermission, EnterpriseRoleAssignment, EnterpriseRoleListParameters,
    EnterpriseRoleListQuery, EnterpriseRoleListQueryQuery, EnterpriseRolePermissionRule,
    EnterpriseRoleUpdateCommand, EnterpriseRoleUpdateCommandCommand, EnterpriseRoleUpdatePayload,
    EnterpriseRoleVersionReference, EnterpriseTeamUpdateCommand,
    EnterpriseTeamUpdateCommandCommand, EnterpriseTeamUpdatePayload, OrganizationScope,
    OrganizationScopeKind, PageRequest, RepositoryScope, RepositoryScopeKind, Scope,
    ServiceAccountActor, ServiceAccountActorKind, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    EnterpriseRbacClock, EnterpriseRbacClockError, EnterpriseRbacErrorKind, EnterpriseRbacService,
    RbacDenialReason,
};
use winwincode_domain::{
    EnterpriseMembershipId, EnterpriseRoleId, EnterpriseRoleVersion, EnterpriseTeamId, Instant,
    OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, ServiceAccountId,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit, StorageErrorKind,
};

const NOW_MILLIS: u64 = 1_700_000_000_000;
const START: &str = "2023-11-14T22:13:20.100Z";
const END: &str = "2023-11-14T22:13:30.000Z";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct SharedClock(Arc<AtomicU64>);

impl EnterpriseRbacClock for SharedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseRbacClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[test]
fn deny_first_temporary_grants_and_restart_revocation_are_authoritative() {
    let fixture = Fixture::new("deny-first");
    let service = fixture.service();
    service
        .update_organization(&organization_command(&fixture, 1, 0))
        .expect("create Organization");
    service
        .update_role(&role_command(
            &fixture,
            2,
            1,
            role(1),
            "allow",
            EnterprisePermission::ProjectRead,
            "active",
        ))
        .expect("create allow Role");
    service
        .update_role(&role_command(
            &fixture,
            3,
            2,
            role(2),
            "deny",
            EnterprisePermission::ProjectRead,
            "active",
        ))
        .expect("create deny Role");
    let temporary = assignment(
        &fixture,
        role(1),
        Some(Instant(START.to_owned())),
        Some(Instant(END.to_owned())),
    );
    service
        .update_membership(&membership_command(
            &fixture,
            4,
            3,
            user_actor_id(&fixture),
            vec![temporary.clone()],
            "active",
        ))
        .expect("create Membership");

    let unauthenticated = service
        .authorize(
            &fixture.user_actor(),
            &[],
            &fixture.repository_scope(),
            &EnterprisePermission::ProjectRead,
        )
        .expect("unauthenticated scope decision");
    assert_eq!(
        unauthenticated.denial_reason,
        Some(RbacDenialReason::UnauthenticatedScope)
    );
    assert_denied(&service, &fixture, RbacDenialReason::DefaultDeny);
    fixture.clock.store(NOW_MILLIS + 200, Ordering::SeqCst);
    assert!(authorize(&service, &fixture).allowed);
    service
        .update_membership(&membership_command(
            &fixture,
            5,
            4,
            user_actor_id(&fixture),
            vec![temporary, assignment(&fixture, role(2), None, None)],
            "active",
        ))
        .expect("add deny Role");
    let denied = authorize(&service, &fixture);
    assert_eq!(denied.denial_reason, Some(RbacDenialReason::ExplicitDeny));
    let denied_seal = denied.authority_seal.expect("durable authority seal");

    service
        .update_role(&role_command(
            &fixture,
            6,
            5,
            role(2),
            "deny",
            EnterprisePermission::ProjectRead,
            "revoked",
        ))
        .expect("revoke deny Role");
    let restarted = fixture.service();
    let allowed = authorize(&restarted, &fixture);
    assert!(allowed.allowed);
    assert_ne!(
        allowed
            .authority_seal
            .expect("new authority seal")
            .state_sha256,
        denied_seal.state_sha256
    );
    fixture.clock.store(NOW_MILLIS + 10_000, Ordering::SeqCst);
    assert_denied(&restarted, &fixture, RbacDenialReason::DefaultDeny);
}

#[test]
fn inherited_role_versions_are_immutable_and_deny_first() {
    let fixture = Fixture::new("inheritance");
    let service = fixture.service();
    service
        .update_organization(&organization_command(&fixture, 50, 0))
        .expect("create Organization");
    service
        .update_role(&role_command(
            &fixture,
            51,
            1,
            role(11),
            "allow",
            EnterprisePermission::ProjectRead,
            "active",
        ))
        .expect("create inherited Role v1");
    let parent_v1 = inherited_role_command(&fixture, 52, 2, role(12), role(11), 1);
    service
        .update_role(&parent_v1)
        .expect("create parent Role v1");
    service
        .update_membership(&membership_command(
            &fixture,
            53,
            3,
            user_actor_id(&fixture),
            vec![assignment(&fixture, role(12), None, None)],
            "active",
        ))
        .expect("assign parent Role v1");
    assert!(authorize(&service, &fixture).allowed);

    service
        .update_role(&role_command(
            &fixture,
            54,
            4,
            role(11),
            "deny",
            EnterprisePermission::ProjectRead,
            "active",
        ))
        .expect("create inherited Role v2");
    assert!(authorize(&service, &fixture).allowed);
    service
        .update_role(&inherited_role_command(
            &fixture,
            55,
            5,
            role(12),
            role(11),
            2,
        ))
        .expect("create parent Role v2");
    let mut parent_v2 = assignment(&fixture, role(12), None, None);
    parent_v2.role_version = EnterpriseRoleVersion(2);
    service
        .update_membership(&membership_command(
            &fixture,
            56,
            6,
            user_actor_id(&fixture),
            vec![parent_v2],
            "active",
        ))
        .expect("assign parent Role v2");
    assert_eq!(
        authorize(&service, &fixture).denial_reason,
        Some(RbacDenialReason::ExplicitDeny)
    );
}

#[test]
fn separation_cross_tenant_and_service_account_revalidation_fail_closed() {
    let fixture = Fixture::new("scope-sod");
    let service = fixture.service();
    seed_separation_and_foreign_reference_guards(&fixture, &service);
    service
        .update_membership(&membership_command(
            &fixture,
            16,
            4,
            service_actor_id(&fixture),
            vec![assignment(&fixture, role(3), None, None)],
            "active",
        ))
        .expect("create Service Account Membership");
    let service_actor = fixture.service_actor();
    let service_decision = service
        .authorize(
            &service_actor,
            &[fixture.repository_scope()],
            &fixture.repository_scope(),
            &EnterprisePermission::AssignmentReview,
        )
        .expect("authorize Service Account");
    assert!(service_decision.allowed, "{service_decision:?}");
    let context = service
        .active_member_context(&service_actor, &fixture.organization_id)
        .expect("active member context");
    assert_eq!(context.authority_revision, context.authority_seal.revision);
    service
        .update_membership(&membership_command(
            &fixture,
            17,
            5,
            service_actor_id(&fixture),
            vec![assignment(&fixture, role(3), None, None)],
            "disabled",
        ))
        .expect("disable Membership");
    let restarted = fixture.service();
    let decision = restarted
        .authorize(
            &service_actor,
            &[fixture.repository_scope()],
            &fixture.repository_scope(),
            &EnterprisePermission::AssignmentReview,
        )
        .expect("revalidate Service Account");
    assert_eq!(
        decision.denial_reason,
        Some(RbacDenialReason::MembershipInactive)
    );
}

fn seed_separation_and_foreign_reference_guards(
    fixture: &Fixture,
    service: &EnterpriseRbacService,
) {
    service
        .update_organization(&organization_command(fixture, 10, 0))
        .expect("create Organization");
    service
        .update_role(&role_command(
            fixture,
            11,
            1,
            role(3),
            "allow",
            EnterprisePermission::AssignmentReview,
            "active",
        ))
        .expect("create reviewer Role");
    let mut approver = role_command(
        fixture,
        12,
        2,
        role(4),
        "allow",
        EnterprisePermission::AssignmentApprove,
        "active",
    );
    approver.payload.conflicting_role_ids = vec![role(3)];
    service
        .update_role(&approver)
        .expect("create approver Role");
    let non_overlapping = vec![
        assignment(fixture, role(3), None, Some(Instant(START.to_owned()))),
        assignment(fixture, role(4), Some(Instant(START.to_owned())), None),
    ];
    service
        .update_team(&team_command(fixture, 13, 3, non_overlapping))
        .expect("non-overlapping separation grants");
    let conflicting = vec![
        assignment(fixture, role(3), None, None),
        assignment(fixture, role(4), None, None),
    ];
    assert_eq!(
        service
            .update_team(&team_command(fixture, 14, 4, conflicting))
            .expect_err("separation conflict")
            .kind(),
        EnterpriseRbacErrorKind::WrongState
    );
    let mut foreign = assignment(fixture, role(3), None, None);
    foreign.scope = foreign_repository_scope(fixture);
    assert_eq!(
        service
            .update_membership(&membership_command(
                fixture,
                15,
                4,
                service_actor_id(fixture),
                vec![foreign],
                "active",
            ))
            .expect_err("cross-tenant grant")
            .kind(),
        EnterpriseRbacErrorKind::ScopeDenied
    );
}

#[test]
fn replay_concurrency_audit_and_authority_guard_are_durable() {
    let fixture = Fixture::new("atomic");
    let service = fixture.service();
    service
        .update_organization(&organization_command(&fixture, 20, 0))
        .expect("create Organization");
    let role_update = role_command(
        &fixture,
        21,
        1,
        role(5),
        "allow",
        EnterprisePermission::RoleRead,
        "active",
    );
    let response = service.update_role(&role_update).expect("create Role");
    assert_eq!(
        service.update_role(&role_update).expect("exact replay"),
        response
    );
    let mut changed = role_update;
    changed.expected_revision = Revision(2);
    changed.payload.display_name = "Changed Role".to_owned();
    assert_eq!(
        service
            .update_role(&changed)
            .expect_err("changed replay")
            .kind(),
        EnterpriseRbacErrorKind::RequestConflict
    );
    assert_one_concurrent_team_winner(&fixture);

    let seal = fixture
        .service()
        .authority_seal(&fixture.organization_id)
        .expect("RBAC authority seal");
    fixture
        .service()
        .update_role(&role_command(
            &fixture,
            24,
            3,
            role(6),
            "allow",
            EnterprisePermission::TeamRead,
            "active",
        ))
        .expect("advance RBAC authority");
    let mut storage = SqliteStorage::open(&fixture.root).expect("open guarded storage");
    let guarded = StateCommit::new(
        receipt_identity(25),
        digest('a'),
        "responsibility:test",
        0,
        b"must-not-commit".to_vec(),
        vec![NewOutboxEvent::internal(
            "guarded-event",
            "guarded",
            b"{}".to_vec(),
        )],
    )
    .with_state_guard(seal.state_guard);
    let error = storage.commit(&guarded).expect_err("stale RBAC guard");
    assert_eq!(error.kind(), StorageErrorKind::RevisionConflict);
    assert!(error.is_state_guard_conflict());
    assert!(
        storage
            .load_state("responsibility:test")
            .expect("inspect guarded state")
            .is_none()
    );
    assert_pending_audit(&fixture, 21);
    Box::new(storage).close().expect("close guarded storage");
}

#[test]
fn role_pagination_is_stable_across_restart_and_rejects_stale_cursor() {
    let fixture = Fixture::new("pagination");
    let service = fixture.service();
    service
        .update_organization(&organization_command(&fixture, 30, 0))
        .expect("create Organization");
    for (offset, role_id) in [role(7), role(8), role(9)].into_iter().enumerate() {
        service
            .update_role(&role_command(
                &fixture,
                u8::try_from(31 + offset).expect("request number"),
                i64::try_from(1 + offset).expect("revision"),
                role_id,
                "allow",
                EnterprisePermission::RoleRead,
                "active",
            ))
            .expect("create paginated Role");
    }
    let first = service
        .list_roles(&role_query(&fixture, 40, 1, None))
        .expect("first page");
    assert_eq!(first.result.items.len(), 1);
    assert!(first.page.has_more);
    let cursor = first.page.next_cursor.expect("next cursor");
    let restarted = fixture.service();
    let second = restarted
        .list_roles(&role_query(&fixture, 41, 1, Some(cursor.clone())))
        .expect("restart second page");
    assert_ne!(first.result.items[0].id, second.result.items[0].id);
    restarted
        .update_role(&role_command(
            &fixture,
            42,
            4,
            role(10),
            "allow",
            EnterprisePermission::RoleRead,
            "active",
        ))
        .expect("advance Role snapshot");
    assert_eq!(
        restarted
            .list_roles(&role_query(&fixture, 43, 1, Some(cursor)))
            .expect_err("stale cursor")
            .kind(),
        EnterpriseRbacErrorKind::InvalidRequest
    );
}

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    clock: Arc<AtomicU64>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    repository_id: RepositoryId,
    user_id: UserId,
    service_account_id: ServiceAccountId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-enterprise-rbac-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::remove_dir_all(&root).ok();
        Self {
            root,
            clock: Arc::new(AtomicU64::new(NOW_MILLIS)),
            organization_id: OrganizationId(format!("org_{}", suffix(1))),
            workspace_id: WorkspaceId(format!("wsp_{}", suffix(2))),
            project_id: ProjectId(format!("prj_{}", suffix(3))),
            repository_id: RepositoryId(format!("rep_{}", suffix(4))),
            user_id: UserId(format!("usr_{}", suffix(5))),
            service_account_id: ServiceAccountId(format!("svc_{}", suffix(6))),
        }
    }

    fn service(&self) -> EnterpriseRbacService {
        EnterpriseRbacService::with_clock(
            Box::new(SqliteStorage::open(&self.root).expect("open RBAC storage")),
            Box::new(SharedClock(Arc::clone(&self.clock))),
        )
    }

    fn user_actor(&self) -> Actor {
        Actor::UserActor(UserActor {
            id: self.user_id.clone(),
            kind: UserActorKind::User,
        })
    }

    fn service_actor(&self) -> Actor {
        Actor::ServiceAccountActor(ServiceAccountActor {
            id: self.service_account_id.clone(),
            kind: ServiceAccountActorKind::ServiceAccount,
        })
    }

    fn organization_scope(&self) -> OrganizationScope {
        OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: self.organization_id.clone(),
        }
    }

    fn repository_scope(&self) -> Scope {
        Scope::RepositoryScope(RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            project_id: self.project_id.clone(),
            repository_id: self.repository_id.clone(),
        })
    }
}

fn organization_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
) -> EnterpriseOrganizationUpdateCommand {
    EnterpriseOrganizationUpdateCommand {
        actor: fixture.user_actor(),
        command: EnterpriseOrganizationUpdateCommandCommand::EnterpriseOrganizationUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseOrganizationUpdatePayload {
            display_name: "Example Organization".to_owned(),
            organization_id: fixture.organization_id.clone(),
            slug: "example-organization".to_owned(),
            state: "active".to_owned(),
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(fixture.organization_scope()),
    }
}

fn role_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    role_id: EnterpriseRoleId,
    effect: &str,
    permission: EnterprisePermission,
    state: &str,
) -> EnterpriseRoleUpdateCommand {
    EnterpriseRoleUpdateCommand {
        actor: fixture.user_actor(),
        command: EnterpriseRoleUpdateCommandCommand::EnterpriseRoleUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseRoleUpdatePayload {
            conflicting_role_ids: Vec::new(),
            display_name: format!("Role {}", role_id.0),
            inherited_roles: Vec::<EnterpriseRoleVersionReference>::new(),
            role_id,
            rules: vec![EnterpriseRolePermissionRule {
                effect: effect.to_owned(),
                permission,
            }],
            state: state.to_owned(),
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn inherited_role_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    role_id: EnterpriseRoleId,
    inherited_role_id: EnterpriseRoleId,
    inherited_version: i64,
) -> EnterpriseRoleUpdateCommand {
    let mut command = role_command(
        fixture,
        request_number,
        expected_revision,
        role_id,
        "allow",
        EnterprisePermission::OrganizationRead,
        "active",
    );
    command.payload.inherited_roles = vec![EnterpriseRoleVersionReference {
        role_id: inherited_role_id,
        role_version: EnterpriseRoleVersion(inherited_version),
    }];
    command
}

fn membership_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    actor_id: ActorId,
    role_assignments: Vec<EnterpriseRoleAssignment>,
    state: &str,
) -> EnterpriseMembershipUpdateCommand {
    EnterpriseMembershipUpdateCommand {
        actor: fixture.user_actor(),
        command: EnterpriseMembershipUpdateCommandCommand::EnterpriseMembershipUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseMembershipUpdatePayload {
            actor_id,
            display_name: "Member".to_owned(),
            membership_id: membership(1),
            role_assignments,
            state: state.to_owned(),
            team_ids: Vec::new(),
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn team_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    role_assignments: Vec<EnterpriseRoleAssignment>,
) -> EnterpriseTeamUpdateCommand {
    EnterpriseTeamUpdateCommand {
        actor: fixture.user_actor(),
        command: EnterpriseTeamUpdateCommandCommand::EnterpriseTeamUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseTeamUpdatePayload {
            display_name: "Review Team".to_owned(),
            role_assignments,
            state: "active".to_owned(),
            team_id: team(1),
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn role_query(
    fixture: &Fixture,
    request_number: u8,
    limit: i64,
    cursor: Option<winwincode_domain::OpaqueCursor>,
) -> EnterpriseRoleListQuery {
    EnterpriseRoleListQuery {
        actor: fixture.user_actor(),
        page: PageRequest { cursor, limit },
        parameters: EnterpriseRoleListParameters {
            permissions: Vec::new(),
            states: Vec::new(),
        },
        query: EnterpriseRoleListQueryQuery::EnterpriseRoleList,
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn assignment(
    fixture: &Fixture,
    role_id: EnterpriseRoleId,
    not_before: Option<Instant>,
    expires_at: Option<Instant>,
) -> EnterpriseRoleAssignment {
    EnterpriseRoleAssignment {
        expires_at,
        not_before,
        role_id,
        role_version: EnterpriseRoleVersion(1),
        scope: fixture.repository_scope(),
        scope_mode: "exact".to_owned(),
    }
}

fn authorize(
    service: &EnterpriseRbacService,
    fixture: &Fixture,
) -> winwincode_control_plane::RbacDecision {
    service
        .authorize(
            &fixture.user_actor(),
            &[fixture.repository_scope()],
            &fixture.repository_scope(),
            &EnterprisePermission::ProjectRead,
        )
        .expect("authorize User")
}

fn assert_denied(service: &EnterpriseRbacService, fixture: &Fixture, reason: RbacDenialReason) {
    let decision = authorize(service, fixture);
    assert!(!decision.allowed);
    assert_eq!(decision.denial_reason, Some(reason));
}

fn assert_one_concurrent_team_winner(fixture: &Fixture) {
    let barrier = Arc::new(Barrier::new(3));
    let attempts = [22_u8, 23]
        .into_iter()
        .map(|request_number| {
            let fixture = fixture.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let service = fixture.service();
                let command = team_command(
                    &fixture,
                    request_number,
                    2,
                    vec![assignment(&fixture, role(5), None, None)],
                );
                barrier.wait();
                service.update_team(&command)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = attempts
        .into_iter()
        .map(|attempt| attempt.join().expect("team update thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("concurrent loser")
            .kind(),
        EnterpriseRbacErrorKind::RevisionConflict
    );
}

fn assert_pending_audit(fixture: &Fixture, request_number: u8) {
    let storage = SqliteStorage::open(&fixture.root).expect("open audit storage");
    let identity = winwincode_control_plane::command_receipt_identity(
        &fixture.user_actor(),
        &Scope::OrganizationScope(fixture.organization_scope()),
        request(request_number),
    )
    .expect("canonical receipt identity");
    let audit = storage
        .load_pending_audit_event(&identity)
        .expect("read pending audit")
        .expect("pending audit exists");
    let json = String::from_utf8(audit.payload().to_vec()).expect("audit UTF-8");
    assert!(json.contains("rbac.role.update"));
    assert!(!json.contains("Role 5"));
    Box::new(storage).close().expect("close audit storage");
}

fn receipt_identity(request_number: u8) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"assignment-actor".to_vec()).expect("actor key"),
        ReceiptScopeKey::from_encoded(b"assignment-scope".to_vec()).expect("scope key"),
        request(request_number),
    )
    .expect("receipt identity")
}

fn foreign_repository_scope(fixture: &Fixture) -> Scope {
    let Scope::RepositoryScope(mut scope) = fixture.repository_scope() else {
        unreachable!("fixture scope is Repository")
    };
    scope.organization_id = OrganizationId(format!("org_{}", suffix(9)));
    scope.repository_id.0.replace_range(4.., &suffix(9));
    Scope::RepositoryScope(scope)
}

fn user_actor_id(fixture: &Fixture) -> ActorId {
    ActorId::UserId(fixture.user_id.clone())
}

fn service_actor_id(fixture: &Fixture) -> ActorId {
    ActorId::ServiceAccountId(fixture.service_account_id.clone())
}

fn role(number: u8) -> EnterpriseRoleId {
    EnterpriseRoleId(format!("rol_{}", suffix(number)))
}

fn team(number: u8) -> EnterpriseTeamId {
    EnterpriseTeamId(format!("tem_{}", suffix(number)))
}

fn membership(number: u8) -> EnterpriseMembershipId {
    EnterpriseMembershipId(format!("mem_{}", suffix(number)))
}

fn request(number: u8) -> RequestId {
    RequestId(format!("req_{}", suffix(number)))
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn suffix(number: u8) -> String {
    format!("{number:026}")
}
