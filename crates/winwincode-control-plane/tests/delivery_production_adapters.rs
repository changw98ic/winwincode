// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use rusqlite::Connection;
use winwincode_api::generated::{
    AcceptanceCriterionInput, Actor, DeliveryAdvanceCommand, DeliveryAdvanceCommandCommand,
    DeliveryAdvancePayload, DeliveryCreateCommand, DeliveryCreateCommandCommand,
    DeliveryCreatePayload, DeliverySpecInput, RepositoryScope, RepositoryScopeKind, UserActor,
    UserActorKind,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, OutboxEvent,
};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    UserId, WorkspaceId,
};

struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[test]
fn local_authority_dispatches_once_and_restart_replays_exact_command() {
    let root = unique_root("delivery-production-replay");
    let repository = root.join("repository");
    let data = root.join("data");
    let baseline = initialize_repository(&repository);
    let scope = scope(1);
    let delivery_id = DeliveryId(canonical_id("dlv", 1));
    let create = create_command(scope.clone(), delivery_id.clone(), baseline, 1);
    let advance = advance_command(scope.clone(), delivery_id, 1, 2);

    let mut first = start(&data, &repository, scope.clone());
    let canonical_repository_source_root = std::fs::canonicalize(
        repository
            .parent()
            .expect("configured repository has a source root"),
    )
    .expect("canonical source root");
    assert_eq!(
        first.git_repository_root(),
        Some(canonical_repository_source_root.as_path())
    );
    let created = first.delivery_create(&create).expect("create Delivery");
    assert_eq!(created.current_revision, Revision(1));
    let advanced = first.delivery_advance(&advance).expect("advance Delivery");
    assert_eq!(advanced.current_revision, Revision(2));
    first.shutdown().expect("shutdown first host");

    assert_eq!(queued_jobs(&data), 1);
    let mut restarted = start(&data, &repository, scope);
    assert_eq!(
        restarted
            .delivery_advance(&advance)
            .expect("receipt-first restart replay"),
        advanced
    );
    restarted.shutdown().expect("shutdown restarted host");
    assert_eq!(queued_jobs(&data), 1);
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn local_authority_rejects_foreign_scope_and_missing_baseline_without_writes() {
    let root = unique_root("delivery-production-stale");
    let repository = root.join("repository");
    let data = root.join("data");
    initialize_repository(&repository);
    let configured_scope = scope(2);
    let mut control_plane = start(&data, &repository, configured_scope.clone());

    let stale = create_command(
        configured_scope,
        DeliveryId(canonical_id("dlv", 2)),
        "0000000000000000000000000000000000000000".to_owned(),
        3,
    );
    let stale_error = control_plane
        .delivery_create(&stale)
        .expect_err("missing exact baseline fails closed");
    assert_eq!(
        stale_error.code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );

    let foreign = create_command(
        scope(99),
        DeliveryId(canonical_id("dlv", 3)),
        "0000000000000000000000000000000000000000".to_owned(),
        4,
    );
    let foreign_error = control_plane
        .delivery_create(&foreign)
        .expect_err("foreign scope fails closed");
    assert_eq!(
        foreign_error.code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
    control_plane.shutdown().expect("shutdown host");
    assert_eq!(queued_jobs(&data), 0);
    std::fs::remove_dir_all(root).expect("remove fixture");
}

fn start(data: &PathBuf, repository: &PathBuf, scope: RepositoryScope) -> ControlPlane {
    ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(data),
        Box::new(NoopPublisher),
        LocalDeliveryAdapterConfig::new(repository, scope),
    )
    .expect("start production-local Delivery host")
}

fn initialize_repository(repository: &PathBuf) -> String {
    std::fs::create_dir_all(repository.join("src")).expect("create repository");
    std::fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");
    std::fs::write(repository.join("src/lib.rs"), "pub fn fixture() {}\n").expect("write source");
    git(repository, &["init", "-q"]);
    git(
        repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(repository, &["config", "user.name", "Fixture"]);
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "fixture"]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repository)
        .output()
        .expect("read baseline");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("UTF-8 baseline")
        .trim()
        .to_owned()
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {arguments:?}");
}

fn queued_jobs(data: &Path) -> i64 {
    Connection::open(data.join("control-plane.sqlite3"))
        .expect("open queue database")
        .query_row("SELECT COUNT(*) FROM scheduler_execution_jobs", [], |row| {
            row.get(0)
        })
        .expect("count queued jobs")
}

fn create_command(
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    baseline: String,
    seed: u64,
) -> DeliveryCreateCommand {
    DeliveryCreateCommand {
        actor: actor(seed),
        command: DeliveryCreateCommandCommand::DeliveryCreate,
        expected_revision: Revision(0),
        payload: DeliveryCreatePayload {
            delivery_id,
            spec: DeliverySpecInput {
                acceptance_criteria: vec![AcceptanceCriterionInput {
                    id: "criterion-1".to_owned(),
                    required: true,
                    title: "Repository tests pass".to_owned(),
                }],
                base_revision: baseline,
                goal: "Ship the exact repository change".to_owned(),
                publication_target: None,
                repository_id: scope.repository_id.clone(),
                title: "Production Delivery".to_owned(),
            },
            tasks: Vec::new(),
        },
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn advance_command(
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    revision: i64,
    seed: u64,
) -> DeliveryAdvanceCommand {
    DeliveryAdvanceCommand {
        actor: actor(seed),
        command: DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(revision),
        payload: DeliveryAdvancePayload { delivery_id },
        request_id: RequestId(canonical_id("req", seed)),
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
