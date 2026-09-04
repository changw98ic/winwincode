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
    Actor, ActorId, CollaborationActivityCategory, CommandRequest,
    EnterpriseMembershipUpdateCommand, EnterpriseMembershipUpdateCommandCommand,
    EnterpriseMembershipUpdatePayload, EnterpriseOrganizationUpdateCommand,
    EnterpriseOrganizationUpdateCommandCommand, EnterpriseOrganizationUpdatePayload,
    EnterprisePermission, EnterpriseRoleAssignment, EnterpriseRolePermissionRule,
    EnterpriseRoleUpdateCommand, EnterpriseRoleUpdateCommandCommand, EnterpriseRoleUpdatePayload,
    OrganizationScope, OrganizationScopeKind, QueryRequest, Scope,
};
use winwincode_control_plane::{
    CollaborationActivityRecordRequest, CollaborationService, ControlPlane, ControlPlaneConfig,
    DurableWorkerInteractionOutbound, EnterpriseRbacService, EventPublishError, EventPublisher,
    OutboxEvent, ProductSessionExecutionConfig,
};
use winwincode_domain::{
    EnterpriseMembershipId, EnterpriseRoleId, EnterpriseRoleVersion, Instant, OrganizationId,
    ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_server::{
    AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    DurableEventHubConfig, DurableEventPublisher, QueryFamily, StandaloneControlPlaneApplication,
    TypedControlPlaneApiPort, UnavailableEnterpriseManagementApplication,
};
use winwincode_storage::{SqliteStorage, WorkerOutboundQueueConfig};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[test]
fn generated_collaboration_routes_use_rbac_storage_and_durable_hub() {
    let fixture = Fixture::new("routes");
    fixture.seed_rbac();
    let rbac = Arc::new(EnterpriseRbacService::new(Box::new(
        SqliteStorage::open(&fixture.root).expect("open RBAC service"),
    )));
    let collaboration = Arc::new(CollaborationService::new(
        SqliteStorage::open(&fixture.root).expect("open Collaboration service"),
        rbac,
    ));
    collaboration
        .record_activity(
            &[fixture.scope()],
            &CollaborationActivityRecordRequest {
                actor: fixture.actor(),
                scope: fixture.scope(),
                request_id: request(10),
                source: "server-vertical-fixture".to_owned(),
                source_sequence: 1,
                source_digest: digest('a'),
                category: CollaborationActivityCategory::Collaboration,
                summary: "Review requested".to_owned(),
                delivery_id: None,
                product_session_id: None,
                occurred_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
            },
        )
        .expect("record Activity before HTTP read");
    let application = fixture.application(Arc::clone(&collaboration));
    let principal = fixture.principal();

    let activity = application
        .query(
            &principal,
            QueryFamily::Collaboration,
            query_request(&fixture, "collaboration.activity.list", 11),
        )
        .expect("Activity route");
    let activity = serde_json::to_value(activity).expect("encode Activity response");
    assert_eq!(
        activity["result"]["items"][0]["summary"],
        "Review requested"
    );

    let notifications = application
        .query(
            &principal,
            QueryFamily::Collaboration,
            query_request(&fixture, "collaboration.notification.list", 12),
        )
        .expect("notification route");
    let notifications = serde_json::to_value(notifications).expect("encode notification response");
    assert_eq!(notifications["result"]["items"][0]["state"], "unread");
    let acknowledgement = application
        .command(
            &principal,
            CommandFamily::Collaboration,
            notification_ack(&fixture, 13),
        )
        .expect("notification acknowledgement route");
    assert_completed_revision(acknowledgement, 1);

    let presence = application
        .command(
            &principal,
            CommandFamily::Collaboration,
            presence_update(&fixture, 14),
        )
        .expect("Presence update route");
    assert_completed_revision(presence, 1);
    let presence = application
        .query(
            &principal,
            QueryFamily::Collaboration,
            query_request(&fixture, "collaboration.presence.list", 15),
        )
        .expect("Presence list route");
    let presence = serde_json::to_value(presence).expect("encode Presence response");
    assert_eq!(presence["result"]["items"][0]["state"], "online");

    application.shutdown().expect("shutdown application");
    fs::remove_dir_all(&fixture.root).expect("remove fixture root");
}

#[test]
fn composition_rejects_a_foreign_collaboration_database() {
    let application_root = temporary_root("application-authority");
    let foreign_root = temporary_root("foreign-collaboration");
    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&application_root),
        Box::new(RecordingPublisher),
    )
    .expect("open Control Plane");
    let storage = SqliteStorage::open(&application_root).expect("open application storage");
    let outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&application_root).expect("open outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("open outbound authority");
    let hub = Arc::new(
        DurableEventHub::open(
            application_root.join("events"),
            DurableEventHubConfig::default(),
        )
        .expect("open hub"),
    );
    let foreign_rbac = Arc::new(EnterpriseRbacService::new(Box::new(
        SqliteStorage::open(&foreign_root).expect("open foreign RBAC"),
    )));
    let collaboration = Arc::new(CollaborationService::new(
        SqliteStorage::open(&foreign_root).expect("open foreign Collaboration"),
        foreign_rbac,
    ));
    let error = StandaloneControlPlaneApplication::new_with_enterprise_and_collaboration(
        control_plane,
        storage,
        outbound,
        hub,
        Arc::new(UnavailableEnterpriseManagementApplication),
        collaboration,
        fixed_execution_config(),
    )
    .err()
    .expect("foreign Collaboration authority must fail");
    assert_eq!(error.code(), "APPLICATION_CONFIGURATION_INVALID");
    fs::remove_dir_all(application_root).expect("remove application root");
    fs::remove_dir_all(foreign_root).expect("remove foreign root");
}

struct Fixture {
    root: PathBuf,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    repository_id: RepositoryId,
    user_id: UserId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        Self {
            root: temporary_root(label),
            organization_id: OrganizationId(id("org", 1)),
            workspace_id: WorkspaceId(id("wsp", 2)),
            project_id: ProjectId(id("prj", 3)),
            repository_id: RepositoryId(id("rep", 4)),
            user_id: UserId(id("usr", 5)),
        }
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

    fn repository_scope(&self) -> RepositoryScope {
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: self.organization_id.clone(),
            workspace_id: self.workspace_id.clone(),
            project_id: self.project_id.clone(),
            repository_id: self.repository_id.clone(),
        }
    }

    fn execution_config(&self) -> ProductSessionExecutionConfig {
        ProductSessionExecutionConfig::try_new(
            self.repository_scope(),
            "fixture-checkout-revision",
            "codex-chat",
            3_600,
            1_073_741_824,
        )
        .expect("execution config")
    }

    fn organization_scope(&self) -> OrganizationScope {
        OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: self.organization_id.clone(),
        }
    }

    fn principal(&self) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::new(self.actor(), vec![self.scope()]).expect("principal")
    }

    fn seed_rbac(&self) {
        let service = EnterpriseRbacService::new(Box::new(
            SqliteStorage::open(&self.root).expect("open RBAC seed storage"),
        ));
        service
            .update_organization(&organization_command(self))
            .expect("create Organization");
        service
            .update_role(&role_command(self))
            .expect("create collaboration Role");
        service
            .update_membership(&membership_command(self))
            .expect("create membership");
    }

    fn application(
        &self,
        collaboration: Arc<CollaborationService>,
    ) -> StandaloneControlPlaneApplication {
        let hub = Arc::new(
            DurableEventHub::open(self.root.join("events"), DurableEventHubConfig::default())
                .expect("open hub"),
        );
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&self.root),
            Box::new(DurableEventPublisher::new(Arc::clone(&hub))),
        )
        .expect("open Control Plane");
        let outbound = DurableWorkerInteractionOutbound::new(
            SqliteStorage::open(&self.root).expect("open outbound storage"),
            WorkerOutboundQueueConfig::default(),
        )
        .expect("open outbound authority");
        StandaloneControlPlaneApplication::new_with_enterprise_and_collaboration(
            control_plane,
            SqliteStorage::open(&self.root).expect("open application storage"),
            outbound,
            hub,
            Arc::new(UnavailableEnterpriseManagementApplication),
            collaboration,
            self.execution_config(),
        )
        .expect("compose application")
    }
}

fn organization_command(fixture: &Fixture) -> EnterpriseOrganizationUpdateCommand {
    EnterpriseOrganizationUpdateCommand {
        actor: fixture.actor(),
        command: EnterpriseOrganizationUpdateCommandCommand::EnterpriseOrganizationUpdate,
        expected_revision: Revision(0),
        payload: EnterpriseOrganizationUpdatePayload {
            display_name: "Collaboration Organization".to_owned(),
            organization_id: fixture.organization_id.clone(),
            slug: "collaboration-organization".to_owned(),
            state: "active".to_owned(),
        },
        request_id: request(1),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::OrganizationScope(fixture.organization_scope()),
    }
}

fn role_command(fixture: &Fixture) -> EnterpriseRoleUpdateCommand {
    EnterpriseRoleUpdateCommand {
        actor: fixture.actor(),
        command: EnterpriseRoleUpdateCommandCommand::EnterpriseRoleUpdate,
        expected_revision: Revision(1),
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
        request_id: request(2),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn membership_command(fixture: &Fixture) -> EnterpriseMembershipUpdateCommand {
    EnterpriseMembershipUpdateCommand {
        actor: fixture.actor(),
        command: EnterpriseMembershipUpdateCommandCommand::EnterpriseMembershipUpdate,
        expected_revision: Revision(2),
        payload: EnterpriseMembershipUpdatePayload {
            actor_id: ActorId::UserId(fixture.user_id.clone()),
            display_name: "Collaborator".to_owned(),
            membership_id: EnterpriseMembershipId(id("mem", 7)),
            role_assignments: vec![EnterpriseRoleAssignment {
                expires_at: None,
                not_before: None,
                role_id: role(),
                role_version: EnterpriseRoleVersion(1),
                scope: fixture.scope(),
                scope_mode: "exact".to_owned(),
            }],
            state: "active".to_owned(),
            team_ids: Vec::new(),
        },
        request_id: request(3),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.organization_scope(),
    }
}

fn query_request(fixture: &Fixture, query: &str, request_number: u64) -> QueryRequest {
    let parameters = match query {
        "collaboration.activity.list" => serde_json::json!({
            "categories": [], "deliveryId": null, "productSessionId": null
        }),
        "collaboration.notification.list" => {
            serde_json::json!({"categories": [], "states": []})
        }
        "collaboration.presence.list" => {
            serde_json::json!({"productSessionId": null, "states": []})
        }
        _ => unreachable!("query fixture is closed"),
    };
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": request(request_number),
        "query": query,
        "actor": fixture.actor(),
        "scope": fixture.scope(),
        "parameters": parameters,
        "page": {"cursor": null, "limit": 20}
    }))
    .expect("generated collaboration query")
}

fn notification_ack(fixture: &Fixture, request_number: u64) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": request(request_number),
        "command": "collaboration.notification.ack",
        "actor": fixture.actor(),
        "scope": fixture.scope(),
        "expectedRevision": 0,
        "payload": {"throughSequence": 1}
    }))
    .expect("generated notification ack command")
}

fn presence_update(fixture: &Fixture, request_number: u64) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": request(request_number),
        "command": "collaboration.presence.update",
        "actor": fixture.actor(),
        "scope": fixture.scope(),
        "expectedRevision": 0,
        "payload": {
            "productSessionId": null,
            "state": "online",
            "leaseDurationMillis": 30000
        }
    }))
    .expect("generated Presence command")
}

fn assert_completed_revision(response: CommandDispatchResponse, revision: i64) {
    let CommandDispatchResponse::Completed(response) = response else {
        panic!("command must complete synchronously");
    };
    let value = serde_json::to_value(response).expect("encode completed response");
    assert_eq!(value["currentRevision"], revision);
}

fn temporary_root(label: &str) -> PathBuf {
    let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-collaboration-application-{label}-{}-{sequence}",
        std::process::id()
    ))
}

fn id(prefix: &str, number: u64) -> String {
    format!("{prefix}_{number:026}")
}

fn request(number: u64) -> RequestId {
    RequestId(id("req", number))
}

fn role() -> EnterpriseRoleId {
    EnterpriseRoleId(id("rol", 6))
}

fn fixed_execution_config() -> ProductSessionExecutionConfig {
    ProductSessionExecutionConfig::try_new(
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(id("org", 1)),
            workspace_id: WorkspaceId(id("wsp", 2)),
            project_id: ProjectId(id("prj", 3)),
            repository_id: RepositoryId(id("rep", 4)),
        },
        "fixture-checkout-revision",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config")
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}
