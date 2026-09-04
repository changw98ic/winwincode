// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use winwincode_api::generated::{Actor, Scope};
use winwincode_control_plane::{
    ResponsibilityAssignmentAction, ResponsibilityAssignmentClock,
    ResponsibilityAssignmentClockError, ResponsibilityAssignmentCommand,
    ResponsibilityAssignmentContext, ResponsibilityAssignmentErrorKind,
    ResponsibilityAssignmentListRequest, ResponsibilityAssignmentService,
    ResponsibilityAssignmentState, ResponsibilityAuthorityError, ResponsibilityAuthorityPort,
    ResponsibilityAuthorityRequest, ResponsibilityCommandAuthority, ResponsibilityListAuthority,
    ResponsibilityPrincipalAuthority, ResponsibilityReviewKind, ResponsibilityRole,
    ResponsibilityTarget,
};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest,
    UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, SqliteStorage, StateCommit, StateRevisionGuard,
};

const NOW: u64 = 1_700_000_000_000;
const RBAC_GUARD_STREAM: &str = "enterprise-rbac:test-authority";
const TARGET_GUARD_STREAM: &str = "responsibility-target:test-authority";

#[derive(Clone)]
struct SharedClock(Arc<AtomicU64>);

impl ResponsibilityAssignmentClock for SharedClock {
    fn now_millis(&mut self) -> Result<u64, ResponsibilityAssignmentClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Clone)]
struct AuthorityControl(Arc<Mutex<AuthorityMode>>);

#[derive(Clone, Debug)]
struct AuthorityMode {
    permission: PermissionMode,
    actor_status: MemberStatus,
    principal_status: MemberStatus,
    eligibility: Eligibility,
    scope_mode: ScopeMode,
    list_stability: ListStability,
    list_calls: u64,
    target_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionMode {
    Granted,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemberStatus {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Eligibility {
    Eligible,
    Ineligible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScopeMode {
    Exact,
    Foreign,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListStability {
    Stable,
    ChangeOnConfirmation,
}

impl Default for AuthorityMode {
    fn default() -> Self {
        Self {
            permission: PermissionMode::Granted,
            actor_status: MemberStatus::Active,
            principal_status: MemberStatus::Active,
            eligibility: Eligibility::Eligible,
            scope_mode: ScopeMode::Exact,
            list_stability: ListStability::Stable,
            list_calls: 0,
            target_revision: 1,
        }
    }
}

struct FakeAuthority {
    control: AuthorityControl,
}

impl ResponsibilityAuthorityPort for FakeAuthority {
    fn command_authority(
        &mut self,
        request: ResponsibilityAuthorityRequest<'_>,
    ) -> Result<ResponsibilityCommandAuthority, ResponsibilityAuthorityError> {
        let mode = self.control.0.lock().expect("authority mode").clone();
        let command = request.command();
        let principal = request
            .requested_principal()
            .expect("command principal")
            .clone();
        let mut scope = command.context.scope.clone();
        if mode.scope_mode == ScopeMode::Foreign {
            scope.repository_id = repository(99);
        }
        Ok(ResponsibilityCommandAuthority {
            actor: command.context.actor.clone(),
            scope,
            operation: command.action.operation(),
            target: command.target.clone(),
            role: command.role,
            permission_granted: mode.permission == PermissionMode::Granted,
            actor_active: mode.actor_status == MemberStatus::Active,
            principal: ResponsibilityPrincipalAuthority {
                user_id: principal,
                active: mode.principal_status == MemberStatus::Active,
                role_eligible: mode.eligibility == Eligibility::Eligible,
            },
            target_revision: mode.target_revision,
            target_sha256: digest('a'),
            rbac_revision: 1,
            rbac_sha256: digest('b'),
            target_guard: StateRevisionGuard::new(TARGET_GUARD_STREAM, mode.target_revision)
                .expect("target guard"),
            target_scope_guard: None,
            rbac_guard: StateRevisionGuard::new(RBAC_GUARD_STREAM, 1).expect("RBAC guard"),
        })
    }

    fn list_authority(
        &mut self,
        request: &ResponsibilityAssignmentListRequest,
    ) -> Result<ResponsibilityListAuthority, ResponsibilityAuthorityError> {
        let mut control = self.control.0.lock().expect("authority mode");
        let changed =
            control.list_stability == ListStability::ChangeOnConfirmation && control.list_calls > 0;
        control.list_calls += 1;
        let mode = control.clone();
        drop(control);
        let mut scope = request.scope.clone();
        if mode.scope_mode == ScopeMode::Foreign {
            scope.repository_id = repository(99);
        }
        Ok(ResponsibilityListAuthority {
            actor: request.actor.clone(),
            scope,
            permission_granted: mode.permission == PermissionMode::Granted,
            actor_active: mode.actor_status == MemberStatus::Active,
            rbac_revision: if changed { 2 } else { 1 },
            rbac_sha256: digest(if changed { 'c' } else { 'b' }),
        })
    }
}

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    clock: Arc<AtomicU64>,
    authority: AuthorityControl,
    scope: RepositoryScope,
    manager: UserId,
    first: UserId,
    second: UserId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = temporary_directory(label);
        let scope = repository_scope(1);
        let manager = user(1);
        seed_authority_states(&root, &scope, &manager);
        Self {
            root,
            clock: Arc::new(AtomicU64::new(NOW)),
            authority: AuthorityControl(Arc::new(Mutex::new(AuthorityMode::default()))),
            scope,
            manager,
            first: user(2),
            second: user(3),
        }
    }

    fn service(&self) -> ResponsibilityAssignmentService {
        let storage = SqliteStorage::open(&self.root).expect("open assignment storage");
        ResponsibilityAssignmentService::with_clock(
            Box::new(storage),
            Box::new(FakeAuthority {
                control: self.authority.clone(),
            }),
            Box::new(SharedClock(Arc::clone(&self.clock))),
        )
    }

    fn set_authority(&self, update: impl FnOnce(&mut AuthorityMode)) {
        update(&mut self.authority.0.lock().expect("authority mode"));
    }
}

#[test]
fn assign_accept_reassign_expire_and_restart_preserve_immutable_receipts() {
    let fixture = Fixture::new("lifecycle");
    let service = fixture.service();
    let target = product_session(1);
    let assign = command(
        &fixture,
        1,
        0,
        actor(&fixture.manager),
        target.clone(),
        ResponsibilityRole::Assignee,
        ResponsibilityAssignmentAction::Assign {
            principal_user_id: fixture.first.clone(),
            expires_at_millis: Some(NOW + 100),
        },
    );
    let assigned = service.apply(&assign).expect("assign");
    assert_eq!(assigned.assignment.revision, 1);
    assert_eq!(
        assigned.assignment.state,
        ResponsibilityAssignmentState::PendingAcceptance
    );
    let replay = service.apply(&assign).expect("exact replay");
    assert!(replay.replayed);
    assert_eq!(replay.assignment, assigned.assignment);
    assert_eq!(replay.occurred_at_millis, assigned.occurred_at_millis);

    let accept = command(
        &fixture,
        2,
        1,
        actor(&fixture.first),
        target.clone(),
        ResponsibilityRole::Assignee,
        ResponsibilityAssignmentAction::Accept,
    );
    let accepted = service.apply(&accept).expect("accept");
    assert_eq!(
        accepted.assignment.state,
        ResponsibilityAssignmentState::Active
    );
    assert_eq!(accepted.assignment.accepted_at_millis, Some(NOW));

    let reassign = command(
        &fixture,
        3,
        2,
        actor(&fixture.manager),
        target.clone(),
        ResponsibilityRole::Assignee,
        ResponsibilityAssignmentAction::Reassign {
            principal_user_id: fixture.second.clone(),
            expires_at_millis: Some(NOW + 200),
        },
    );
    let reassigned = service.apply(&reassign).expect("reassign");
    assert_eq!(reassigned.assignment.revision, 3);
    assert_eq!(reassigned.assignment.principal_user_id, fixture.second);

    let restarted = fixture.service();
    let listed = restarted
        .list(&list_request(&fixture))
        .expect("restart list");
    assert_eq!(listed, vec![reassigned.assignment.clone()]);
    fixture.clock.store(NOW + 200, Ordering::SeqCst);
    let expire = command(
        &fixture,
        4,
        3,
        actor(&fixture.manager),
        target,
        ResponsibilityRole::Assignee,
        ResponsibilityAssignmentAction::Expire,
    );
    let expired = restarted.apply(&expire).expect("expire");
    assert_eq!(
        expired.assignment.state,
        ResponsibilityAssignmentState::Expired
    );

    let inspection = SqliteStorage::open(&fixture.root).expect("open audit inspection");
    let identity = winwincode_control_plane::command_receipt_identity(
        &reassign.context.actor,
        &Scope::RepositoryScope(reassign.context.scope.clone()),
        reassign.context.request_id.clone(),
    )
    .expect("receipt identity");
    assert!(
        inspection
            .load_pending_audit_event(&identity)
            .expect("load assignment audit")
            .is_some()
    );
    Box::new(inspection).close().expect("close inspection");
}

#[test]
fn separation_cross_tenant_and_unauthorized_reassign_write_nothing() {
    let fixture = Fixture::new("denials");
    let service = fixture.service();
    let target = delivery(1);
    let assigned = service
        .apply(&command(
            &fixture,
            10,
            0,
            actor(&fixture.manager),
            target.clone(),
            ResponsibilityRole::Assignee,
            ResponsibilityAssignmentAction::Assign {
                principal_user_id: fixture.first.clone(),
                expires_at_millis: None,
            },
        ))
        .expect("assign owner");

    let reviewer = command(
        &fixture,
        11,
        0,
        actor(&fixture.manager),
        review(1),
        ResponsibilityRole::Reviewer,
        ResponsibilityAssignmentAction::Assign {
            principal_user_id: fixture.first.clone(),
            expires_at_millis: None,
        },
    );
    assert_eq!(
        service.apply(&reviewer).expect_err("separation").kind(),
        ResponsibilityAssignmentErrorKind::SeparationViolation
    );

    fixture.set_authority(|mode| mode.permission = PermissionMode::Denied);
    let reassign = command(
        &fixture,
        12,
        1,
        actor(&fixture.manager),
        target.clone(),
        ResponsibilityRole::Assignee,
        ResponsibilityAssignmentAction::Reassign {
            principal_user_id: fixture.second.clone(),
            expires_at_millis: None,
        },
    );
    assert_eq!(
        service.apply(&reassign).expect_err("permission").kind(),
        ResponsibilityAssignmentErrorKind::AuthorizationDenied
    );

    fixture.set_authority(|mode| {
        mode.permission = PermissionMode::Granted;
        mode.scope_mode = ScopeMode::Foreign;
    });
    assert_eq!(
        service
            .apply(&command(
                &fixture,
                13,
                1,
                actor(&fixture.manager),
                target,
                ResponsibilityRole::Assignee,
                ResponsibilityAssignmentAction::Reassign {
                    principal_user_id: fixture.second.clone(),
                    expires_at_millis: None,
                },
            ))
            .expect_err("foreign authority")
            .kind(),
        ResponsibilityAssignmentErrorKind::ScopeDenied
    );
    fixture.set_authority(|mode| mode.scope_mode = ScopeMode::Exact);
    assert_eq!(
        service
            .list(&list_request(&fixture))
            .expect("unchanged list"),
        vec![assigned.assignment]
    );
}

#[test]
fn departed_member_is_revoked_and_query_scope_is_reauthorized() {
    let fixture = Fixture::new("departure");
    let service = fixture.service();
    let target = review(1);
    service
        .apply(&command(
            &fixture,
            20,
            0,
            actor(&fixture.manager),
            target.clone(),
            ResponsibilityRole::Reviewer,
            ResponsibilityAssignmentAction::Assign {
                principal_user_id: fixture.first.clone(),
                expires_at_millis: None,
            },
        ))
        .expect("assign reviewer");
    fixture.set_authority(|mode| mode.principal_status = MemberStatus::Inactive);
    assert_eq!(
        service
            .apply(&command(
                &fixture,
                21,
                1,
                actor(&fixture.first),
                target.clone(),
                ResponsibilityRole::Reviewer,
                ResponsibilityAssignmentAction::Accept,
            ))
            .expect_err("departed member cannot accept")
            .kind(),
        ResponsibilityAssignmentErrorKind::MemberInactive
    );
    let revoked = service
        .apply(&command(
            &fixture,
            22,
            1,
            actor(&fixture.manager),
            target,
            ResponsibilityRole::Reviewer,
            ResponsibilityAssignmentAction::RevokeDeparted,
        ))
        .expect("revoke departed member");
    assert_eq!(
        revoked.assignment.state,
        ResponsibilityAssignmentState::Revoked
    );

    fixture.set_authority(|mode| {
        mode.principal_status = MemberStatus::Active;
        mode.scope_mode = ScopeMode::Foreign;
    });
    assert_eq!(
        service
            .list(&list_request(&fixture))
            .expect_err("foreign list")
            .kind(),
        ResponsibilityAssignmentErrorKind::ScopeDenied
    );
}

#[test]
fn concurrent_roles_admit_only_one_principal_under_separation_of_duties() {
    let fixture = Fixture::new("concurrent-separation");
    let barrier = Arc::new(Barrier::new(3));
    let attempts = [ResponsibilityRole::Assignee, ResponsibilityRole::Approver]
        .into_iter()
        .enumerate()
        .map(|(index, role)| {
            let fixture = fixture.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let service = fixture.service();
                let command = command(
                    &fixture,
                    30 + u64::try_from(index).expect("index"),
                    0,
                    actor(&fixture.manager),
                    delivery_stage(1),
                    role,
                    ResponsibilityAssignmentAction::Assign {
                        principal_user_id: fixture.first.clone(),
                        expires_at_millis: None,
                    },
                );
                barrier.wait();
                service.apply(&command)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = attempts
        .into_iter()
        .map(|attempt| attempt.join().expect("assignment thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("separation loser")
            .kind(),
        ResponsibilityAssignmentErrorKind::SeparationViolation
    );
    assert_eq!(
        fixture
            .service()
            .list(&list_request(&fixture))
            .expect("one assignment")
            .len(),
        1
    );
}

#[test]
fn changed_target_or_rbac_guard_fails_closed_before_assignment_write() {
    let fixture = Fixture::new("authority-guard");
    fixture.set_authority(|mode| mode.target_revision = 2);
    let service = fixture.service();
    let error = service
        .apply(&command(
            &fixture,
            40,
            0,
            actor(&fixture.manager),
            product_session(2),
            ResponsibilityRole::Approver,
            ResponsibilityAssignmentAction::Assign {
                principal_user_id: fixture.first.clone(),
                expires_at_millis: None,
            },
        ))
        .expect_err("missing authority revision");
    assert_eq!(
        error.kind(),
        ResponsibilityAssignmentErrorKind::AuthorityChanged
    );
    fixture.set_authority(|mode| mode.target_revision = 1);
    assert!(
        fixture
            .service()
            .list(&list_request(&fixture))
            .expect("empty after guard failure")
            .is_empty()
    );
}

#[test]
fn role_ineligible_principal_is_rejected_before_assignment_write() {
    let fixture = Fixture::new("role-ineligible");
    fixture.set_authority(|mode| mode.eligibility = Eligibility::Ineligible);
    let error = fixture
        .service()
        .apply(&command(
            &fixture,
            50,
            0,
            actor(&fixture.manager),
            delivery(2),
            ResponsibilityRole::Assignee,
            ResponsibilityAssignmentAction::Assign {
                principal_user_id: fixture.first.clone(),
                expires_at_millis: None,
            },
        ))
        .expect_err("ineligible principal");
    assert_eq!(
        error.kind(),
        ResponsibilityAssignmentErrorKind::RoleIneligible
    );
}

#[test]
fn query_rechecks_rbac_seal_and_fails_closed_when_authority_changes() {
    let fixture = Fixture::new("list-authority-race");
    fixture.set_authority(|mode| {
        mode.list_stability = ListStability::ChangeOnConfirmation;
        mode.list_calls = 0;
    });
    let error = fixture
        .service()
        .list(&list_request(&fixture))
        .expect_err("RBAC changed during list");
    assert_eq!(
        error.kind(),
        ResponsibilityAssignmentErrorKind::AuthorityChanged
    );
}

fn seed_authority_states(root: &Path, scope: &RepositoryScope, manager: &UserId) {
    let mut storage = SqliteStorage::open(root).expect("open authority seed storage");
    for (index, stream_id) in [RBAC_GUARD_STREAM, TARGET_GUARD_STREAM]
        .into_iter()
        .enumerate()
    {
        let seed = u64::try_from(index).expect("seed index") + 900;
        let identity = winwincode_control_plane::command_receipt_identity(
            &actor(manager),
            &Scope::RepositoryScope(scope.clone()),
            request(seed),
        )
        .expect("seed receipt identity");
        storage
            .commit(&StateCommit::new(
                identity,
                digest(if index == 0 { 'd' } else { 'e' }),
                stream_id,
                0,
                format!("authority-state-{index}").into_bytes(),
                vec![NewOutboxEvent::internal(
                    format!("evt_responsibility_authority_{index:016}"),
                    "responsibility.authority.seed.v1",
                    format!("authority-event-{index}").into_bytes(),
                )],
            ))
            .expect("seed authority state");
    }
    Box::new(storage).close().expect("close authority seed");
}

fn command(
    fixture: &Fixture,
    request_number: u64,
    expected_revision: u64,
    actor: Actor,
    target: ResponsibilityTarget,
    role: ResponsibilityRole,
    action: ResponsibilityAssignmentAction,
) -> ResponsibilityAssignmentCommand {
    ResponsibilityAssignmentCommand {
        context: ResponsibilityAssignmentContext {
            actor,
            authenticated_scopes: vec![Scope::RepositoryScope(fixture.scope.clone())],
            scope: fixture.scope.clone(),
            request_id: request(request_number),
            expected_revision,
        },
        target,
        role,
        action,
    }
}

fn list_request(fixture: &Fixture) -> ResponsibilityAssignmentListRequest {
    ResponsibilityAssignmentListRequest {
        actor: actor(&fixture.manager),
        authenticated_scopes: vec![Scope::RepositoryScope(fixture.scope.clone())],
        scope: fixture.scope.clone(),
        target: None,
        role: None,
        principal_user_id: None,
        include_ended: true,
    }
}

fn product_session(seed: u64) -> ResponsibilityTarget {
    ResponsibilityTarget::ProductSession {
        product_session_id: ProductSessionId(canonical("psn", seed)),
    }
}

fn delivery(seed: u64) -> ResponsibilityTarget {
    ResponsibilityTarget::Delivery {
        delivery_id: DeliveryId(canonical("dlv", seed)),
    }
}

fn delivery_stage(seed: u64) -> ResponsibilityTarget {
    ResponsibilityTarget::DeliveryStage {
        delivery_id: DeliveryId(canonical("dlv", seed)),
        stage: winwincode_delivery::domain::DeliveryStage::DeliveryReview,
    }
}

fn review(seed: u64) -> ResponsibilityTarget {
    ResponsibilityTarget::Review {
        delivery_id: DeliveryId(canonical("dlv", seed)),
        review: ResponsibilityReviewKind::Solution,
    }
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical("org", seed)),
        workspace_id: WorkspaceId(canonical("wsp", seed)),
        project_id: ProjectId(canonical("prj", seed)),
        repository_id: repository(seed),
    }
}

fn actor(user_id: &UserId) -> Actor {
    Actor::UserActor(UserActor {
        id: user_id.clone(),
        kind: UserActorKind::User,
    })
}

fn request(seed: u64) -> RequestId {
    RequestId(canonical("req", seed))
}

fn user(seed: u64) -> UserId {
    UserId(canonical("usr", seed))
}

fn repository(seed: u64) -> RepositoryId {
    RepositoryId(canonical("rep", seed))
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn canonical(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn temporary_directory(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "winwincode-responsibility-assignment-{label}-{}-{}",
        std::process::id(),
        NOW
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp directory");
    path
}
