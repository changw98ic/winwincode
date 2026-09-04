// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::{Connection, params};
use winwincode_api::generated::{
    AcceptanceCriterionInput, Actor, DeliveryAdvanceCommand, DeliveryAdvanceCommandCommand,
    DeliveryAdvancePayload, DeliveryCreateCommand, DeliveryCreateCommandCommand,
    DeliveryCreatePayload, DeliveryGetParameters, DeliveryGetQuery, DeliveryGetQueryQuery,
    DeliverySpecInput, PageRequest, QueryResultResponse,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, OutboxEvent,
    strongflow_projection::{StrongFlowProjectionError, StrongFlowProjectionQueryPort},
};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[test]
fn startup_installs_restart_stable_empty_publication_and_rejects_corrupt_facts() {
    let root = unique_root();
    let repository = root.join("repository");
    let data = root.join("data");
    let baseline = initialize_repository(&repository);
    let scope = repository_scope(41);
    let delivery_id = DeliveryId(canonical_id("dlv", 41));
    let create = create_command(&scope, &delivery_id, baseline);
    let advance = advance_command(&scope, &delivery_id);

    let mut first = start(&data, &repository, &scope);
    first.delivery_create(&create).expect("create Delivery");
    first.delivery_advance(&advance).expect("advance Delivery");
    let current_query = delivery_query(&scope, &delivery_id, None);
    let current = StrongFlowProjectionQueryPort::delivery_get(&first, &current_query)
        .expect("production StrongFlow read");
    let QueryResultResponse::DeliveryGetResultResponse(current_response) = &current else {
        panic!("delivery.get returned another response kind");
    };
    assert_eq!(current_response.result.delivery_revision, Revision(2));
    assert!(current_response.result.current_candidate.is_none());
    assert!(current_response.result.publication.is_none());
    let cursor = current_response.result.read_cursor.clone();
    let expected_bytes = serde_json::to_vec(&current).expect("current response JSON");
    first.shutdown().expect("first shutdown");

    let restarted = start(&data, &repository, &scope);
    let replay = StrongFlowProjectionQueryPort::delivery_get(
        &restarted,
        &delivery_query(&scope, &delivery_id, Some(cursor)),
    )
    .expect("restart exact read replay");
    assert_eq!(
        serde_json::to_vec(&replay).expect("replay response JSON"),
        expected_bytes
    );
    restarted.shutdown().expect("restart shutdown");

    insert_corrupt_publication(&data);
    let corrupt = start(&data, &repository, &scope);
    assert!(matches!(
        StrongFlowProjectionQueryPort::delivery_get(&corrupt, &current_query),
        Err(StrongFlowProjectionError::TrustedFactsUnavailable(_))
    ));
    corrupt.shutdown().expect("corrupt host shutdown");
    std::fs::remove_dir_all(root).expect("fixture cleanup");
}

fn delivery_query(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    at_cursor: Option<winwincode_api::generated::StrongFlowReadCursor>,
) -> DeliveryGetQuery {
    DeliveryGetQuery {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", 41)),
            kind: UserActorKind::User,
        }),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: DeliveryGetParameters {
            at_cursor,
            delivery_id: delivery_id.clone(),
        },
        query: DeliveryGetQueryQuery::DeliveryGet,
        request_id: RequestId(canonical_id("req", 44)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn start(data: &Path, repository: &Path, scope: &RepositoryScope) -> ControlPlane {
    ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(data),
        Box::new(NoopPublisher),
        LocalDeliveryAdapterConfig::new(repository, scope.clone()),
    )
    .expect("start production Control Plane")
}

fn create_command(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    baseline: String,
) -> DeliveryCreateCommand {
    DeliveryCreateCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", 41)),
            kind: UserActorKind::User,
        }),
        command: DeliveryCreateCommandCommand::DeliveryCreate,
        expected_revision: Revision(0),
        payload: DeliveryCreatePayload {
            delivery_id: delivery_id.clone(),
            spec: DeliverySpecInput {
                acceptance_criteria: vec![AcceptanceCriterionInput {
                    id: "criterion-1".to_owned(),
                    required: true,
                    title: "StrongFlow production read remains exact".to_owned(),
                }],
                base_revision: baseline,
                goal: "Preserve durable StrongFlow authority".to_owned(),
                publication_target: None,
                repository_id: scope.repository_id.clone(),
                title: "Production StrongFlow sources".to_owned(),
            },
            tasks: Vec::new(),
        },
        request_id: RequestId(canonical_id("req", 41)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn advance_command(scope: &RepositoryScope, delivery_id: &DeliveryId) -> DeliveryAdvanceCommand {
    DeliveryAdvanceCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", 41)),
            kind: UserActorKind::User,
        }),
        command: DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(1),
        payload: DeliveryAdvancePayload {
            delivery_id: delivery_id.clone(),
        },
        request_id: RequestId(canonical_id("req", 42)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn initialize_repository(repository: &Path) -> String {
    std::fs::create_dir_all(repository.join("src")).expect("repository directory");
    std::fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"strongflow-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("fixture manifest");
    std::fs::write(repository.join("src/lib.rs"), "pub fn fixture() {}\n").expect("fixture source");
    git(repository, &["init", "-q", "-b", "main"]);
    git(
        repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(repository, &["config", "user.name", "Fixture"]);
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "fixture"]);
    git_text(repository, &["rev-parse", "HEAD"])
}

fn insert_corrupt_publication(data: &Path) {
    let connection = Connection::open(data.join("control-plane.sqlite3")).expect("open database");
    connection
        .execute(
            "INSERT INTO product_state (stream_id, revision, payload) VALUES (?1, 1, ?2)",
            params!["publication:pub_00000000000000000000000000", b"{}"],
        )
        .expect("insert corrupt Publication fact");
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {arguments:?}");
}

fn git_text(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run Git query");
    assert!(output.status.success(), "Git query failed: {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("Git output")
        .trim()
        .to_owned()
}

fn repository_scope(seed: u64) -> RepositoryScope {
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

fn unique_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-strongflow-production-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}
