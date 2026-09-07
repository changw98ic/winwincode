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
    Actor, ActorId, CollaborationActivityCategory, CollaborationActivityListParameters,
    CollaborationActivityListQuery, CollaborationActivityListQueryQuery,
    CollaborationNotificationAckCommand, CollaborationNotificationAckCommandCommand,
    CollaborationNotificationAckPayload, CollaborationNotificationListParameters,
    CollaborationNotificationListQuery, CollaborationNotificationListQueryQuery,
    CollaborationNotificationState, CollaborationPresenceListParameters,
    CollaborationPresenceListQuery, CollaborationPresenceListQueryQuery,
    CollaborationPresenceState, CollaborationPresenceUpdateCommand,
    CollaborationPresenceUpdateCommandCommand, CollaborationPresenceUpdatePayload,
    EnterpriseMembershipUpdateCommand, EnterpriseMembershipUpdateCommandCommand,
    EnterpriseMembershipUpdatePayload, EnterpriseOrganizationUpdateCommand,
    EnterpriseOrganizationUpdateCommandCommand, EnterpriseOrganizationUpdatePayload,
    EnterprisePermission, EnterpriseRoleAssignment, EnterpriseRolePermissionRule,
    EnterpriseRoleUpdateCommand, EnterpriseRoleUpdateCommandCommand, EnterpriseRoleUpdatePayload,
    OrganizationScope, OrganizationScopeKind, PageRequest, Scope,
};
use winwincode_control_plane::{
    CollaborationActivityRecordRequest, CollaborationClock, CollaborationClockError,
    CollaborationErrorKind, CollaborationService, EnterpriseRbacClock, EnterpriseRbacClockError,
    EnterpriseRbacService,
};
use winwincode_domain::{
    DeliveryId, EnterpriseMembershipId, EnterpriseRoleId, EnterpriseRoleVersion, Instant,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

const NOW_MILLIS: u64 = 1_700_000_000_000;
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct SharedClock(Arc<AtomicU64>);

impl CollaborationClock for SharedClock {
    fn now_millis(&mut self) -> Result<u64, CollaborationClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

impl EnterpriseRbacClock for SharedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseRbacClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[test]
fn activity_deduplicates_out_of_order_sources_and_keeps_a_fixed_restart_page() {
    let fixture = Fixture::new("activity");
    let service = fixture.collaboration();
    let second_source = activity(&fixture, 20, 2, 'a', "second source");
    let first_source = activity(&fixture, 21, 1, 'b', "first source");
    assert_eq!(
        service
            .record_activity(&fixture.authenticated_scopes(), &second_source)
            .expect("record source sequence two")
            .sequence,
        1
    );
    assert_eq!(
        service
            .record_activity(&fixture.authenticated_scopes(), &first_source)
            .expect("record out-of-order source sequence one")
            .sequence,
        2
    );

    let mut duplicate = second_source.clone();
    duplicate.request_id = request(22);
    assert_eq!(
        service
            .record_activity(&fixture.authenticated_scopes(), &duplicate)
            .expect("deduplicate another request receipt")
            .sequence,
        1
    );
    duplicate.source_digest = digest('c');
    assert_eq!(
        service
            .record_activity(&fixture.authenticated_scopes(), &duplicate)
            .expect_err("changed source receipt must conflict")
            .kind(),
        CollaborationErrorKind::RequestConflict
    );

    let first_page = service
        .activity_list(
            &fixture.authenticated_scopes(),
            &activity_query(&fixture, 23, 1, None),
        )
        .expect("first fixed page");
    assert!(first_page.page.has_more);
    let cursor = first_page.page.next_cursor.expect("fixed cursor");
    service
        .record_activity(
            &fixture.authenticated_scopes(),
            &activity(&fixture, 24, 3, 'd', "after snapshot"),
        )
        .expect("append after snapshot");

    let restarted = fixture.collaboration();
    let second_page = restarted
        .activity_list(
            &fixture.authenticated_scopes(),
            &activity_query(&fixture, 25, 10, Some(cursor)),
        )
        .expect("restart resumes the fixed upper bound");
    assert_eq!(second_page.result.items.len(), 1);
    assert_eq!(second_page.result.items[0].summary, "first source");
    assert!(!second_page.page.has_more);

    let foreign = foreign_scope(&fixture);
    let mut foreign_query = activity_query(&fixture, 26, 10, None);
    foreign_query.scope.clone_from(&foreign);
    assert_eq!(
        restarted
            .activity_list(&[foreign], &foreign_query)
            .expect_err("cross-tenant read must be denied")
            .kind(),
        CollaborationErrorKind::PermissionDenied
    );

    let storage = SqliteStorage::open(&fixture.root).expect("inspect durable outbox");
    assert_eq!(
        storage
            .pending_events()
            .expect("pending events")
            .iter()
            .filter(|event| event.topic == "activity.recorded.v1")
            .count(),
        3
    );
}

#[test]
fn notification_receipts_are_monotonic_exact_and_do_not_mutate_activity() {
    let fixture = Fixture::new("notifications");
    let service = fixture.collaboration();
    for (request_number, source_sequence, character) in [(30, 1, 'a'), (31, 2, 'b')] {
        service
            .record_activity(
                &fixture.authenticated_scopes(),
                &activity(
                    &fixture,
                    request_number,
                    source_sequence,
                    character,
                    "notification",
                ),
            )
            .expect("record activity");
    }
    let unread = service
        .notification_list(
            &fixture.authenticated_scopes(),
            &notification_query(&fixture, 32, Vec::new()),
        )
        .expect("unread notification page");
    assert_eq!(unread.result.items.len(), 2);
    assert!(
        unread
            .result
            .items
            .iter()
            .all(|item| item.state == CollaborationNotificationState::Unread)
    );

    let first = notification_ack(&fixture, 33, 0, 1);
    let first_response = service
        .notification_ack(&fixture.authenticated_scopes(), &first)
        .expect("ack first activity");
    fixture.clock.fetch_add(1_000, Ordering::SeqCst);
    service
        .notification_ack(
            &fixture.authenticated_scopes(),
            &notification_ack(&fixture, 34, 1, 2),
        )
        .expect("ack second activity");
    assert_eq!(
        service
            .notification_ack(&fixture.authenticated_scopes(), &first)
            .expect("exact replay after later mutation"),
        first_response
    );

    let read = service
        .notification_list(
            &fixture.authenticated_scopes(),
            &notification_query(&fixture, 35, vec![CollaborationNotificationState::Read]),
        )
        .expect("read notification page");
    assert_eq!(read.result.items.len(), 2);
    assert!(
        read.result
            .items
            .iter()
            .all(|item| item.acknowledged_at.is_some())
    );

    assert_eq!(
        service
            .notification_ack(
                &fixture.authenticated_scopes(),
                &notification_ack(&fixture, 36, 2, 99),
            )
            .expect_err("ack beyond Activity must fail")
            .kind(),
        CollaborationErrorKind::InvalidRequest
    );
    assert_eq!(
        service
            .activity_list(
                &fixture.authenticated_scopes(),
                &activity_query(&fixture, 37, 10, None),
            )
            .expect("business Activity remains unchanged")
            .result
            .items
            .len(),
        2
    );
}

#[test]
fn presence_expires_reconnects_replays_and_revalidates_revocation() {
    let fixture = Fixture::new("presence");
    let service = fixture.collaboration();
    let online = presence_update(
        &fixture,
        40,
        0,
        CollaborationPresenceState::Online,
        Some(5_000),
    );
    let first = service
        .presence_update(&fixture.authenticated_scopes(), &online)
        .expect("publish online lease");
    assert_eq!(first.current_revision, Revision(1));
    assert_eq!(
        service
            .presence_list(
                &fixture.authenticated_scopes(),
                &presence_query(&fixture, 41, vec![CollaborationPresenceState::Online]),
            )
            .expect("online presence")
            .result
            .items
            .len(),
        1
    );

    fixture.clock.fetch_add(5_001, Ordering::SeqCst);
    let stale = service
        .presence_list(
            &fixture.authenticated_scopes(),
            &presence_query(&fixture, 42, vec![CollaborationPresenceState::Offline]),
        )
        .expect("stale lease derives offline");
    assert_eq!(
        stale.result.items[0].state,
        CollaborationPresenceState::Offline
    );
    assert!(stale.result.items[0].expires_at.is_none());

    let reconnect = presence_update(
        &fixture,
        43,
        1,
        CollaborationPresenceState::Online,
        Some(10_000),
    );
    assert_eq!(
        service
            .presence_update(&fixture.authenticated_scopes(), &reconnect)
            .expect("reconnect the same Presence identity")
            .current_revision,
        Revision(2)
    );
    assert_eq!(
        service
            .presence_update(&fixture.authenticated_scopes(), &online)
            .expect("original Presence command replays exact response"),
        first
    );

    let restarted = fixture.collaboration();
    assert_eq!(
        restarted
            .presence_list(
                &fixture.authenticated_scopes(),
                &presence_query(&fixture, 44, vec![CollaborationPresenceState::Online]),
            )
            .expect("restart restores live lease")
            .result
            .items
            .len(),
        1
    );
    fixture
        .rbac
        .update_membership(&membership_command(&fixture, 45, 3, "disabled"))
        .expect("revoke membership");
    assert_eq!(
        restarted
            .presence_list(
                &fixture.authenticated_scopes(),
                &presence_query(&fixture, 46, Vec::new()),
            )
            .expect_err("revocation is checked on every read")
            .kind(),
        CollaborationErrorKind::PermissionDenied
    );
}

struct Fixture {
    root: PathBuf,
    clock: Arc<AtomicU64>,
    rbac: Arc<EnterpriseRbacService>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    repository_id: RepositoryId,
    user_id: UserId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-collaboration-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::remove_dir_all(&root).ok();
        let clock = Arc::new(AtomicU64::new(NOW_MILLIS));
        let organization_id = OrganizationId(format!("org_{}", suffix(1)));
        let workspace_id = WorkspaceId(format!("wsp_{}", suffix(2)));
        let project_id = ProjectId(format!("prj_{}", suffix(3)));
        let repository_id = RepositoryId(format!("rep_{}", suffix(4)));
        let user_id = UserId(format!("usr_{}", suffix(5)));
        let rbac = Arc::new(EnterpriseRbacService::with_clock(
            Box::new(SqliteStorage::open(&root).expect("open RBAC storage")),
            Box::new(SharedClock(Arc::clone(&clock))),
        ));
        let fixture = Self {
            root,
            clock,
            rbac,
            organization_id,
            workspace_id,
            project_id,
            repository_id,
            user_id,
        };
        fixture.seed_rbac();
        fixture
    }

    fn seed_rbac(&self) {
        self.rbac
            .update_organization(&organization_command(self, 1, 0))
            .expect("create Organization");
        self.rbac
            .update_role(&role_command(self, 2, 1))
            .expect("create collaboration Role");
        self.rbac
            .update_membership(&membership_command(self, 3, 2, "active"))
            .expect("create active membership");
    }

    fn collaboration(&self) -> CollaborationService {
        CollaborationService::with_clock(
            SqliteStorage::open(&self.root).expect("open collaboration storage"),
            Arc::clone(&self.rbac),
            Box::new(SharedClock(Arc::clone(&self.clock))),
        )
    }

    fn actor(&self) -> Actor {
        Actor::UserActor(UserActor {
            id: self.user_id.clone(),
            kind: UserActorKind::User,
        })
    }

    fn scope(&self) -> Scope {
        Scope::RepositoryScope(RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            project_id: self.project_id.clone(),
            repository_id: self.repository_id.clone(),
        })
    }

    fn organization_scope(&self) -> OrganizationScope {
        OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: self.organization_id.clone(),
        }
    }

    fn authenticated_scopes(&self) -> Vec<Scope> {
        vec![self.scope()]
    }
}

fn organization_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
) -> EnterpriseOrganizationUpdateCommand {
    EnterpriseOrganizationUpdateCommand {
        actor: fixture.actor(),
        command: EnterpriseOrganizationUpdateCommandCommand::EnterpriseOrganizationUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseOrganizationUpdatePayload {
            display_name: "Collaboration Organization".to_owned(),
            organization_id: fixture.organization_id.clone(),
            slug: "collaboration-organization".to_owned(),
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
) -> EnterpriseRoleUpdateCommand {
    EnterpriseRoleUpdateCommand {
        actor: fixture.actor(),
        command: EnterpriseRoleUpdateCommandCommand::EnterpriseRoleUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseRoleUpdatePayload {
            conflicting_role_ids: Vec::new(),
            display_name: "Collaborator".to_owned(),
            inherited_roles: Vec::new(),
            role_id: role(),
            rules: vec![
                EnterpriseRolePermissionRule {
                    effect: "allow".to_owned(),
                    permission: EnterprisePermission::CollaborationRead,
                },
                EnterpriseRolePermissionRule {
                    effect: "allow".to_owned(),
                    permission: EnterprisePermission::CollaborationWrite,
                },
            ],
            state: "active".to_owned(),
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn membership_command(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    state: &str,
) -> EnterpriseMembershipUpdateCommand {
    EnterpriseMembershipUpdateCommand {
        actor: fixture.actor(),
        command: EnterpriseMembershipUpdateCommandCommand::EnterpriseMembershipUpdate,
        expected_revision: Revision(expected_revision),
        payload: EnterpriseMembershipUpdatePayload {
            actor_id: ActorId::UserId(fixture.user_id.clone()),
            display_name: "Collaborator".to_owned(),
            membership_id: EnterpriseMembershipId(format!("mem_{}", suffix(7))),
            role_assignments: vec![EnterpriseRoleAssignment {
                expires_at: None,
                not_before: None,
                role_id: role(),
                role_version: EnterpriseRoleVersion(1),
                scope: fixture.scope(),
                scope_mode: "exact".to_owned(),
            }],
            state: state.to_owned(),
            team_ids: Vec::new(),
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn activity(
    fixture: &Fixture,
    request_number: u8,
    source_sequence: u64,
    digest_character: char,
    summary: &str,
) -> CollaborationActivityRecordRequest {
    CollaborationActivityRecordRequest {
        actor: fixture.actor(),
        scope: fixture.scope(),
        request_id: request(request_number),
        source: "canonical-business-owner".to_owned(),
        source_sequence,
        source_digest: digest(digest_character),
        category: CollaborationActivityCategory::Collaboration,
        summary: summary.to_owned(),
        delivery_id: Some(DeliveryId(format!("dlv_{}", suffix(8)))),
        product_session_id: Some(ProductSessionId(format!("psn_{}", suffix(9)))),
        occurred_at: instant(NOW_MILLIS + source_sequence),
    }
}

fn activity_query(
    fixture: &Fixture,
    request_number: u8,
    limit: i64,
    cursor: Option<winwincode_domain::OpaqueCursor>,
) -> CollaborationActivityListQuery {
    CollaborationActivityListQuery {
        actor: fixture.actor(),
        page: PageRequest { cursor, limit },
        parameters: CollaborationActivityListParameters {
            categories: Vec::new(),
            delivery_id: None,
            product_session_id: None,
        },
        query: CollaborationActivityListQueryQuery::CollaborationActivityList,
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope(),
    }
}

fn notification_query(
    fixture: &Fixture,
    request_number: u8,
    states: Vec<CollaborationNotificationState>,
) -> CollaborationNotificationListQuery {
    CollaborationNotificationListQuery {
        actor: fixture.actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: CollaborationNotificationListParameters {
            categories: Vec::new(),
            states,
        },
        query: CollaborationNotificationListQueryQuery::CollaborationNotificationList,
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope(),
    }
}

fn notification_ack(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    through_sequence: i64,
) -> CollaborationNotificationAckCommand {
    CollaborationNotificationAckCommand {
        actor: fixture.actor(),
        command: CollaborationNotificationAckCommandCommand::CollaborationNotificationAck,
        expected_revision: Revision(expected_revision),
        payload: CollaborationNotificationAckPayload { through_sequence },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope(),
    }
}

fn presence_update(
    fixture: &Fixture,
    request_number: u8,
    expected_revision: i64,
    state: CollaborationPresenceState,
    lease_duration_millis: Option<i64>,
) -> CollaborationPresenceUpdateCommand {
    CollaborationPresenceUpdateCommand {
        actor: fixture.actor(),
        command: CollaborationPresenceUpdateCommandCommand::CollaborationPresenceUpdate,
        expected_revision: Revision(expected_revision),
        payload: CollaborationPresenceUpdatePayload {
            lease_duration_millis,
            product_session_id: Some(ProductSessionId(format!("psn_{}", suffix(9)))),
            state,
        },
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope(),
    }
}

fn presence_query(
    fixture: &Fixture,
    request_number: u8,
    states: Vec<CollaborationPresenceState>,
) -> CollaborationPresenceListQuery {
    CollaborationPresenceListQuery {
        actor: fixture.actor(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: CollaborationPresenceListParameters {
            product_session_id: None,
            states,
        },
        query: CollaborationPresenceListQueryQuery::CollaborationPresenceList,
        request_id: request(request_number),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope(),
    }
}

fn foreign_scope(fixture: &Fixture) -> Scope {
    Scope::RepositoryScope(RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(format!("org_{}", suffix(91))),
        workspace_id: fixture.workspace_id.clone(),
        project_id: fixture.project_id.clone(),
        repository_id: RepositoryId(format!("rep_{}", suffix(92))),
    })
}

fn instant(millis: u64) -> Instant {
    let offset = millis
        .checked_sub(NOW_MILLIS)
        .filter(|offset| *offset < 1_000)
        .expect("fixture millis stay within one second");
    Instant(format!("2023-11-14T22:13:20.{offset:03}Z"))
}

fn role() -> EnterpriseRoleId {
    EnterpriseRoleId(format!("rol_{}", suffix(6)))
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
