// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use winwincode_api::generated::{
    AcceptanceCriterionInput, Actor, DeliveryCreateCommand, DeliveryCreateCommandCommand,
    DeliveryCreatePayload, DeliveryListParameters, DeliveryListQuery, DeliveryListQueryQuery,
    DeliverySpecInput, DeliveryUpdateSpecCommand, DeliveryUpdateSpecCommandCommand,
    DeliveryUpdateSpecPayload, PageRequest, Scope,
};
use winwincode_control_plane::{
    CollaborationInboxItemId, CollaborationInboxSourcePort, ControlPlane, DeliveryAdvanceAuthority,
    DeliveryApplicationError, DeliveryAttentionAuthority, DeliveryAuthorityError,
    DeliveryAuthorityPort, DeliveryAuthorityRequest, DeliverySpecificationAuthority,
    DeliveryVerdictAuthority, DurableCollaborationInboxSource, EventPublishError, EventPublisher,
    FormalCollaborationCommandRoute, OutboxEvent, ResponsibilityReviewKind, ResponsibilityRole,
    ResponsibilityTarget, command_receipt_identity,
};
use winwincode_delivery::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, DELIVERY_SCHEMA_VERSION, Delivery,
    RepositoryKind, RepositoryRef,
};
use winwincode_domain::{
    AttentionItemId, DeliveryId, OrganizationId, ProjectId, RepositoryId, RequestId, Revision,
    SchemaVersion, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::{NewOutboxEvent, ProductStateStorage, SqliteStorage, StateCommit};

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
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliverySpecificationAuthority, DeliveryAuthorityError> {
        let now_millis = request.delivery().map_or(1_800_000_000_000, |delivery| {
            delivery.snapshot().updated_at_millis + 1
        });
        Ok(DeliverySpecificationAuthority {
            now_millis,
            repository: RepositoryRef {
                schema_version: DELIVERY_SCHEMA_VERSION,
                kind: RepositoryKind::LocalGit,
                locator: "file:///workspace/repository".to_owned(),
            },
            source_ref: None,
            scope: vec!["src".to_owned()],
            out_of_scope: vec!["target".to_owned()],
            constraints: vec!["tests pass".to_owned()],
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
        Err(DeliveryAuthorityError::new("advance authority is not used"))
    }

    fn resolve_attention(
        &mut self,
        _request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAttentionAuthority, DeliveryAuthorityError> {
        Err(DeliveryAuthorityError::new(
            "Attention authority is not used",
        ))
    }

    fn verdict(
        &mut self,
        _request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryVerdictAuthority, DeliveryAuthorityError> {
        Err(DeliveryAuthorityError::new("verdict authority is not used"))
    }
}

#[test]
fn generated_create_catalog_list_and_restart_replay_are_one_durable_path() {
    let root = unique_root("delivery-application-create-list-replay");
    let scope = scope(1);
    let delivery_id = DeliveryId(canonical_id("dlv", 1));
    let command = create_command(scope.clone(), delivery_id.clone(), 1, "First Delivery");

    let mut first = start(&root);
    first
        .install_delivery_authority_port(Box::new(RepositoryAuthority))
        .expect("install authority");
    let created = first.delivery_create(&command).expect("create Delivery");
    assert_eq!(created.current_revision, Revision(1));
    assert_eq!(created.result.delivery_id, delivery_id);

    let listed = first
        .delivery_list(&list_query(scope.clone(), 10, None, vec![]))
        .expect("list Delivery");
    assert_eq!(listed.result.items, vec![created.result.clone()]);
    assert!(!listed.page.has_more);
    first.shutdown().expect("close first Control Plane");

    let mut restarted = start(&root);
    let replay = restarted
        .delivery_create(&command)
        .expect("receipt-first replay does not require authority");
    assert_eq!(replay, created);
    let mut changed = command.clone();
    changed.payload.spec.title = "Changed body".to_owned();
    let error = restarted
        .delivery_create(&changed)
        .expect_err("changed-body request conflicts");
    assert_eq!(
        error.code(),
        winwincode_api::generated::ErrorCode::IdempotencyConflict
    );
    restarted.shutdown().expect("close restarted Control Plane");
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn collaboration_source_reads_current_attention_from_the_delivery_catalog_after_restart() {
    let root = unique_root("delivery-collaboration-source");
    let scope = scope(20);
    let delivery_id = DeliveryId(canonical_id("dlv", 20));
    let mut control_plane = start(&root);
    control_plane
        .install_delivery_authority_port(Box::new(RepositoryAuthority))
        .expect("install authority");
    control_plane
        .delivery_create(&create_command(
            scope.clone(),
            delivery_id.clone(),
            20,
            "Review candidate",
        ))
        .expect("create Delivery");
    control_plane.shutdown().expect("close Control Plane");

    let stream_id = format!("delivery:{}", delivery_id.0);
    let mut storage = SqliteStorage::open(&root).expect("open Delivery state");
    let stored = storage
        .load_state(&stream_id)
        .expect("load Delivery state")
        .expect("Delivery state");
    let delivery = Delivery::decode_json(&stored.payload).expect("decode Delivery");
    let mut snapshot = delivery.into_snapshot();
    let attention_id = AttentionItemId(canonical_id("att", 20));
    snapshot.attention_items.push(AttentionItem {
        schema_version: 3,
        id: attention_id.clone(),
        delivery_id: delivery_id.clone(),
        delivery_spec_id: snapshot.spec.id.clone(),
        stage_run_id: None,
        item_type: AttentionItemType::DecisionRequired,
        title: "Review the candidate".to_owned(),
        context: "canonical-review-context".to_owned(),
        options: Vec::new(),
        assigned_to: None,
        blocking: true,
        status: AttentionItemStatus::Open,
        resolution: None,
        resolved_by: None,
        created_at_millis: 1_800_000_000_020,
        resolved_at_millis: None,
    });
    snapshot.revision = stored.revision + 1;
    let delivery = Delivery::try_from_snapshot(snapshot).expect("validated Delivery Attention");
    storage
        .commit(&StateCommit::new(
            command_receipt_identity(
                &actor(20),
                &Scope::RepositoryScope(scope.clone()),
                RequestId(canonical_id("req", 120)),
            )
            .expect("receipt identity"),
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            stream_id,
            stored.revision,
            delivery.encode_json().expect("encode Delivery"),
            vec![NewOutboxEvent::internal(
                "evt_delivery_collaboration_source_0001",
                "delivery.collaboration-source.fixture.v1",
                b"{}".to_vec(),
            )],
        ))
        .expect("commit current Attention");
    Box::new(storage).close().expect("close state writer");

    let mut source = DurableCollaborationInboxSource::new(Box::new(
        SqliteStorage::open(&root).expect("restart collaboration source"),
    ));
    let cut = source.snapshot(&scope).expect("load collaboration cut");
    assert_eq!(cut.items.len(), 1);
    let item = &cut.items[0];
    assert_eq!(
        item.id,
        CollaborationInboxItemId::DeliveryAttention(attention_id.clone())
    );
    assert_eq!(item.responsibility_role, ResponsibilityRole::Reviewer);
    assert_eq!(
        item.target,
        ResponsibilityTarget::Review {
            delivery_id: delivery_id.clone(),
            review: ResponsibilityReviewKind::Solution,
        }
    );
    assert_eq!(
        item.command_route,
        FormalCollaborationCommandRoute::DeliveryResolveAttention {
            attention_item_id: attention_id,
            delivery_id,
        }
    );
    assert_eq!(cut.item_state_guards[&item.id].len(), 2);
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn scoped_catalog_cursor_expires_when_a_matching_delivery_changes() {
    let root = unique_root("delivery-application-stable-cursor");
    let foreign_scope = scope(99);
    let scope = scope(2);
    let first_id = DeliveryId(canonical_id("dlv", 2));
    let second_id = DeliveryId(canonical_id("dlv", 3));
    let nonmatching_id = DeliveryId(canonical_id("dlv", 4));
    let mut control_plane = start(&root);
    control_plane
        .install_delivery_authority_port(Box::new(RepositoryAuthority))
        .expect("install authority");
    control_plane
        .delivery_create(&create_command(scope.clone(), first_id.clone(), 2, "First"))
        .expect("create first");
    control_plane
        .delivery_create(&create_command(
            scope.clone(),
            second_id.clone(),
            3,
            "Second",
        ))
        .expect("create second");
    control_plane
        .delivery_create(&create_command(
            scope.clone(),
            nonmatching_id.clone(),
            4,
            "Nonmatching",
        ))
        .expect("create nonmatching");
    control_plane
        .delivery_update_spec(&update_command(
            scope.clone(),
            nonmatching_id.clone(),
            1,
            5,
            "Ready nonmatching",
        ))
        .expect("move nonmatching Delivery to ready");

    let first_page = control_plane
        .delivery_list(&list_query(
            scope.clone(),
            1,
            None,
            vec!["draft".to_owned()],
        ))
        .expect("first page");
    assert!(first_page.page.has_more);
    let cursor = first_page.page.next_cursor.clone().expect("next cursor");
    control_plane
        .delivery_update_spec(&update_command(
            scope.clone(),
            nonmatching_id,
            2,
            6,
            "Changed ready nonmatching",
        ))
        .expect("mutate a nonmatching Delivery");
    let unchanged = control_plane
        .delivery_list(&list_query(
            scope.clone(),
            1,
            Some(cursor.clone()),
            vec!["draft".to_owned()],
        ))
        .expect("nonmatching state change preserves filtered cursor");
    assert_eq!(unchanged.result.items.len(), 1);
    control_plane
        .delivery_update_spec(&update_command(
            scope.clone(),
            second_id,
            1,
            7,
            "Changed matching",
        ))
        .expect("move matching Delivery out of the filter");
    let error = control_plane
        .delivery_list(&list_query(
            scope.clone(),
            1,
            Some(cursor),
            vec!["draft".to_owned()],
        ))
        .expect_err("changed snapshot expires cursor");
    assert!(matches!(error, DeliveryApplicationError::ReadCursorExpired));

    let foreign = control_plane
        .delivery_list(&list_query(foreign_scope, 10, None, vec![]))
        .expect("foreign scope has its own empty catalog");
    assert!(foreign.result.items.is_empty());
    control_plane.shutdown().expect("close Control Plane");
    std::fs::remove_dir_all(root).expect("remove fixture");
}

fn start(root: &PathBuf) -> ControlPlane {
    let storage = SqliteStorage::open(root).expect("open SQLite storage");
    ControlPlane::start(Box::new(storage), Box::new(NoopPublisher)).expect("start Control Plane")
}

fn create_command(
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    seed: u64,
    title: &str,
) -> DeliveryCreateCommand {
    DeliveryCreateCommand {
        actor: actor(seed),
        command: DeliveryCreateCommandCommand::DeliveryCreate,
        expected_revision: Revision(0),
        payload: DeliveryCreatePayload {
            delivery_id,
            spec: spec(&scope, title),
            tasks: vec![],
        },
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn update_command(
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    expected_revision: i64,
    seed: u64,
    title: &str,
) -> DeliveryUpdateSpecCommand {
    DeliveryUpdateSpecCommand {
        actor: actor(seed),
        command: DeliveryUpdateSpecCommandCommand::DeliveryUpdateSpec,
        expected_revision: Revision(expected_revision),
        payload: DeliveryUpdateSpecPayload {
            delivery_id,
            spec: spec(&scope, title),
        },
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn spec(scope: &RepositoryScope, title: &str) -> DeliverySpecInput {
    DeliverySpecInput {
        acceptance_criteria: vec![AcceptanceCriterionInput {
            id: "criterion-1".to_owned(),
            required: true,
            title: "Tests pass".to_owned(),
        }],
        base_revision: "0123456789abcdef".to_owned(),
        goal: "Ship the exact implementation".to_owned(),
        publication_target: None,
        repository_id: scope.repository_id.clone(),
        title: title.to_owned(),
    }
}

fn list_query(
    scope: RepositoryScope,
    limit: i64,
    cursor: Option<winwincode_domain::OpaqueCursor>,
    states: Vec<String>,
) -> DeliveryListQuery {
    DeliveryListQuery {
        actor: actor(90),
        page: PageRequest { cursor, limit },
        parameters: DeliveryListParameters { states },
        query: DeliveryListQueryQuery::DeliveryList,
        request_id: RequestId(canonical_id("req", 90)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}
