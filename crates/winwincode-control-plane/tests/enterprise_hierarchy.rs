use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_control_plane::{
    EnterpriseHierarchyCommand, EnterpriseHierarchyErrorKind, EnterpriseHierarchyService,
    EnvironmentId, HierarchyMutation, HierarchyResourceId, HierarchyResourceState, HierarchyScope,
};
use winwincode_domain::{
    Instant, OrganizationId, ProjectId, RepositoryId, RequestId, UserId, WorkspaceId,
};
use winwincode_storage::{PublicEventActor, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-hierarchy-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn organization(value: u64) -> OrganizationId {
    OrganizationId(canonical("org", value))
}

fn workspace(value: u64) -> WorkspaceId {
    WorkspaceId(canonical("wsp", value))
}

fn project(value: u64) -> ProjectId {
    ProjectId(canonical("prj", value))
}

fn environment(value: u64) -> EnvironmentId {
    EnvironmentId::try_new(canonical("env", value)).expect("Environment id")
}

fn repository(value: u64) -> RepositoryId {
    RepositoryId(canonical("rep", value))
}

fn service(root: &Path) -> EnterpriseHierarchyService {
    EnterpriseHierarchyService::new(Box::new(
        SqliteStorage::open(root).expect("hierarchy storage"),
    ))
}

fn command(
    organization_id: &OrganizationId,
    request_seed: u64,
    expected_revision: u64,
    mutation: HierarchyMutation,
) -> EnterpriseHierarchyCommand {
    EnterpriseHierarchyCommand {
        actor: PublicEventActor::User {
            id: UserId(canonical("usr", 1)),
        },
        organization_id: organization_id.clone(),
        request_id: RequestId(canonical("req", request_seed)),
        expected_revision,
        occurred_at: Instant(format!("2027-01-15T08:{:02}:00.000Z", request_seed % 60)),
        mutation,
    }
}

fn create(
    service: &EnterpriseHierarchyService,
    organization_id: &OrganizationId,
    request_seed: u64,
    expected_revision: u64,
    id: HierarchyResourceId,
    parent: Option<HierarchyResourceId>,
    display_name: &str,
) -> u64 {
    service
        .mutate(&command(
            organization_id,
            request_seed,
            expected_revision,
            HierarchyMutation::Create {
                id,
                parent,
                display_name: display_name.to_owned(),
            },
        ))
        .expect("hierarchy create")
        .current_revision
}

fn seed_repository_hierarchy(
    service: &EnterpriseHierarchyService,
    seed: u64,
) -> (OrganizationId, WorkspaceId, ProjectId, RepositoryId, u64) {
    let organization_id = organization(seed);
    let workspace_id = workspace(seed);
    let project_id = project(seed);
    let repository_id = repository(seed);
    let mut revision = create(
        service,
        &organization_id,
        seed * 100,
        0,
        HierarchyResourceId::Organization(organization_id.clone()),
        None,
        "Organization",
    );
    revision = create(
        service,
        &organization_id,
        seed * 100 + 1,
        revision,
        HierarchyResourceId::Workspace(workspace_id.clone()),
        Some(HierarchyResourceId::Organization(organization_id.clone())),
        "Workspace",
    );
    revision = create(
        service,
        &organization_id,
        seed * 100 + 2,
        revision,
        HierarchyResourceId::Project(project_id.clone()),
        Some(HierarchyResourceId::Workspace(workspace_id.clone())),
        "Project",
    );
    revision = create(
        service,
        &organization_id,
        seed * 100 + 3,
        revision,
        HierarchyResourceId::Repository(repository_id.clone()),
        Some(HierarchyResourceId::Project(project_id.clone())),
        "Repository",
    );
    (
        organization_id,
        workspace_id,
        project_id,
        repository_id,
        revision,
    )
}

#[test]
fn all_levels_resolve_to_one_canonical_scope_and_generated_repository_locator() {
    let root = temporary_directory("canonical-scope");
    let service = service(&root);
    let (organization_id, workspace_id, project_id, repository_id, revision) =
        seed_repository_hierarchy(&service, 1);
    let environment_id = environment(1);
    let environment_receipt = service
        .mutate(&command(
            &organization_id,
            104,
            revision,
            HierarchyMutation::Create {
                id: HierarchyResourceId::Environment(environment_id.clone()),
                parent: Some(HierarchyResourceId::Project(project_id.clone())),
                display_name: "Production".to_owned(),
            },
        ))
        .expect("Environment create");
    assert_eq!(environment_receipt.current_revision, 5);
    assert_eq!(
        service
            .resolve(&HierarchyResourceId::Environment(environment_id.clone()))
            .expect("Environment resolve")
            .scope,
        HierarchyScope::Environment {
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            environment_id,
        }
    );
    let locator = service
        .repository_locator(&repository_id)
        .expect("Repository locator");
    assert_eq!(locator.organization_id, organization_id);
    assert_eq!(locator.workspace_id, workspace_id);
    assert_eq!(locator.project_id, project_id);
    assert_eq!(locator.repository_id, repository_id);
    drop(service);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn exact_replay_precedes_newer_state_and_changed_request_reuse_is_rejected() {
    let root = temporary_directory("replay");
    let service = service(&root);
    let organization_id = organization(2);
    let create_organization = command(
        &organization_id,
        200,
        0,
        HierarchyMutation::Create {
            id: HierarchyResourceId::Organization(organization_id.clone()),
            parent: None,
            display_name: "Original Organization".to_owned(),
        },
    );
    let original = service
        .mutate(&create_organization)
        .expect("Organization create");
    create(
        &service,
        &organization_id,
        201,
        1,
        HierarchyResourceId::Workspace(workspace(2)),
        Some(HierarchyResourceId::Organization(organization_id.clone())),
        "Workspace",
    );
    let replay = service
        .mutate(&create_organization)
        .expect("exact replay after newer state");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.current_revision, original.current_revision);
    assert_eq!(replay.resource, original.resource);
    let mut changed = create_organization;
    changed.mutation = HierarchyMutation::Create {
        id: HierarchyResourceId::Organization(organization_id),
        parent: None,
        display_name: "Changed Organization".to_owned(),
    };
    assert_eq!(
        service
            .mutate(&changed)
            .expect_err("changed request reuse")
            .kind(),
        EnterpriseHierarchyErrorKind::RequestConflict
    );
    drop(service);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn moves_update_the_canonical_locator_and_reject_cycles_or_wrong_levels() {
    let root = temporary_directory("move");
    let service = service(&root);
    let (organization_id, _workspace_id, project_id, repository_id, mut revision) =
        seed_repository_hierarchy(&service, 3);
    let new_workspace = workspace(30);
    revision = create(
        &service,
        &organization_id,
        304,
        revision,
        HierarchyResourceId::Workspace(new_workspace.clone()),
        Some(HierarchyResourceId::Organization(organization_id.clone())),
        "Second Workspace",
    );
    let moved = service
        .mutate(&command(
            &organization_id,
            305,
            revision,
            HierarchyMutation::Move {
                id: HierarchyResourceId::Project(project_id.clone()),
                new_parent: HierarchyResourceId::Workspace(new_workspace.clone()),
            },
        ))
        .expect("Project move");
    let locator = service
        .repository_locator(&repository_id)
        .expect("moved locator");
    assert_eq!(locator.workspace_id, new_workspace);
    let cycle = service
        .mutate(&command(
            &organization_id,
            306,
            moved.current_revision,
            HierarchyMutation::Move {
                id: HierarchyResourceId::Project(project_id.clone()),
                new_parent: HierarchyResourceId::Project(project_id),
            },
        ))
        .expect_err("self-parent cycle");
    assert_eq!(cycle.kind(), EnterpriseHierarchyErrorKind::Cycle);
    let wrong_level = service
        .mutate(&command(
            &organization_id,
            307,
            moved.current_revision,
            HierarchyMutation::Move {
                id: HierarchyResourceId::Repository(repository_id),
                new_parent: HierarchyResourceId::Workspace(workspace(30)),
            },
        ))
        .expect_err("wrong parent level");
    assert_eq!(
        wrong_level.kind(),
        EnterpriseHierarchyErrorKind::InvalidParent
    );
    drop(service);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn cross_tenant_parents_moves_and_duplicate_ids_are_rejected() {
    let root = temporary_directory("cross-tenant");
    let service = service(&root);
    let first_organization = organization(4);
    let second_organization = organization(5);
    let first_workspace = workspace(4);
    let second_workspace = workspace(5);
    let first_project = project(4);
    create(
        &service,
        &first_organization,
        400,
        0,
        HierarchyResourceId::Organization(first_organization.clone()),
        None,
        "First Organization",
    );
    let first_revision = create(
        &service,
        &first_organization,
        401,
        1,
        HierarchyResourceId::Workspace(first_workspace.clone()),
        Some(HierarchyResourceId::Organization(
            first_organization.clone(),
        )),
        "First Workspace",
    );
    let first_revision = create(
        &service,
        &first_organization,
        402,
        first_revision,
        HierarchyResourceId::Project(first_project.clone()),
        Some(HierarchyResourceId::Workspace(first_workspace)),
        "First Project",
    );
    create(
        &service,
        &second_organization,
        500,
        0,
        HierarchyResourceId::Organization(second_organization.clone()),
        None,
        "Second Organization",
    );
    let second_revision = create(
        &service,
        &second_organization,
        501,
        1,
        HierarchyResourceId::Workspace(second_workspace.clone()),
        Some(HierarchyResourceId::Organization(
            second_organization.clone(),
        )),
        "Second Workspace",
    );
    let foreign_move = service
        .mutate(&command(
            &first_organization,
            403,
            first_revision,
            HierarchyMutation::Move {
                id: HierarchyResourceId::Project(first_project),
                new_parent: HierarchyResourceId::Workspace(second_workspace),
            },
        ))
        .expect_err("foreign parent move");
    assert_eq!(
        foreign_move.kind(),
        EnterpriseHierarchyErrorKind::CrossTenantReference
    );
    let duplicate = service
        .mutate(&command(
            &second_organization,
            502,
            second_revision,
            HierarchyMutation::Create {
                id: HierarchyResourceId::Workspace(workspace(4)),
                parent: Some(HierarchyResourceId::Organization(
                    second_organization.clone(),
                )),
                display_name: "Foreign Duplicate".to_owned(),
            },
        ))
        .expect_err("foreign duplicate id");
    assert_eq!(
        duplicate.kind(),
        EnterpriseHierarchyErrorKind::CrossTenantReference
    );
    drop(service);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn archive_and_delete_require_children_to_finish_bottom_up() {
    let root = temporary_directory("archive-delete");
    let service = service(&root);
    let (organization_id, _workspace_id, project_id, repository_id, mut revision) =
        seed_repository_hierarchy(&service, 6);
    let active_parent = service
        .mutate(&command(
            &organization_id,
            604,
            revision,
            HierarchyMutation::Archive {
                id: HierarchyResourceId::Project(project_id.clone()),
            },
        ))
        .expect_err("active child blocks archive");
    assert_eq!(
        active_parent.kind(),
        EnterpriseHierarchyErrorKind::DescendantsExist
    );
    let archived_repository = service
        .mutate(&command(
            &organization_id,
            605,
            revision,
            HierarchyMutation::Archive {
                id: HierarchyResourceId::Repository(repository_id.clone()),
            },
        ))
        .expect("Repository archive");
    revision = archived_repository.current_revision;
    let archived_project = service
        .mutate(&command(
            &organization_id,
            606,
            revision,
            HierarchyMutation::Archive {
                id: HierarchyResourceId::Project(project_id.clone()),
            },
        ))
        .expect("Project archive");
    revision = archived_project.current_revision;
    assert_eq!(
        service
            .mutate(&command(
                &organization_id,
                607,
                revision,
                HierarchyMutation::Delete {
                    id: HierarchyResourceId::Project(project_id.clone()),
                },
            ))
            .expect_err("retained child blocks delete")
            .kind(),
        EnterpriseHierarchyErrorKind::DescendantsExist
    );
    let deleted_repository = service
        .mutate(&command(
            &organization_id,
            608,
            revision,
            HierarchyMutation::Delete {
                id: HierarchyResourceId::Repository(repository_id.clone()),
            },
        ))
        .expect("Repository delete");
    let deleted_project = service
        .mutate(&command(
            &organization_id,
            609,
            deleted_repository.current_revision,
            HierarchyMutation::Delete {
                id: HierarchyResourceId::Project(project_id),
            },
        ))
        .expect("Project delete");
    assert_eq!(
        deleted_project.resource.state,
        HierarchyResourceState::Deleted
    );
    assert_eq!(
        service
            .repository_locator(&repository_id)
            .expect_err("deleted Repository has no active locator")
            .kind(),
        EnterpriseHierarchyErrorKind::Deleted
    );
    drop(service);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn restart_preserves_scope_revision_and_exact_replay() {
    let root = temporary_directory("restart");
    let original_service = service(&root);
    let (organization_id, workspace_id, project_id, repository_id, revision) =
        seed_repository_hierarchy(&original_service, 7);
    let archive = command(
        &organization_id,
        704,
        revision,
        HierarchyMutation::Archive {
            id: HierarchyResourceId::Repository(repository_id.clone()),
        },
    );
    let original_receipt = original_service.mutate(&archive).expect("archive");
    drop(original_service);
    let restarted = service(&root);
    let locator = restarted
        .repository_locator(&repository_id)
        .expect("restart locator");
    assert_eq!(locator.organization_id, organization_id);
    assert_eq!(locator.workspace_id, workspace_id);
    assert_eq!(locator.project_id, project_id);
    let replay = restarted.mutate(&archive).expect("restart replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.current_revision, original_receipt.current_revision);
    assert_eq!(replay.resource, original_receipt.resource);
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn concurrent_same_revision_writes_have_one_winner_and_no_partial_index() {
    let root = temporary_directory("concurrency");
    let organization_id = organization(8);
    let setup = service(&root);
    create(
        &setup,
        &organization_id,
        800,
        0,
        HierarchyResourceId::Organization(organization_id.clone()),
        None,
        "Organization",
    );
    drop(setup);
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for seed in [81, 82] {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let organization_id = organization_id.clone();
        threads.push(thread::spawn(move || {
            let service = service(&root);
            barrier.wait();
            let result = service.mutate(&command(
                &organization_id,
                800 + seed,
                1,
                HierarchyMutation::Create {
                    id: HierarchyResourceId::Workspace(workspace(seed)),
                    parent: Some(HierarchyResourceId::Organization(organization_id.clone())),
                    display_name: format!("Workspace {seed}"),
                },
            ));
            (seed, result)
        }));
    }
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| matches!(
                result,
                Err(error) if error.kind() == EnterpriseHierarchyErrorKind::RevisionConflict
            ))
            .count(),
        1
    );
    let inspector = service(&root);
    for (seed, result) in results {
        let resolved = inspector.resolve(&HierarchyResourceId::Workspace(workspace(seed)));
        assert_eq!(resolved.is_ok(), result.is_ok());
    }
    drop(inspector);
    fs::remove_dir_all(root).expect("cleanup");
}
