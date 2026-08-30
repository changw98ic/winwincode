use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_control_plane::{
    EnterpriseHierarchyCommand, EnterpriseHierarchyService, EnterpriseScopeBindingCommand,
    EnterpriseScopeBindingErrorKind, EnterpriseScopeBindingMutation, EnterpriseScopeBindingService,
    EnvironmentId, HierarchyMutation, HierarchyResourceId, HierarchyScope,
    LocalScopeMigrationCommand, ScopeBindingSource, ScopeBindingSubject,
    local_scope_inventory_digest,
};
use winwincode_domain::{
    CredentialReferenceId, DeliveryId, EnterpriseIntegrationId, EnterprisePolicyId,
    EnterpriseWorkerPoolId, Instant, OrganizationId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{PublicEventActor, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct HierarchyFixture {
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    environment_id: EnvironmentId,
    repository_id: RepositoryId,
    revision: u64,
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-enterprise-scope-binding-{name}-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temporary directory");
    root
}

fn canonical(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn actor() -> PublicEventActor {
    PublicEventActor::User {
        id: UserId(canonical("usr", 1)),
    }
}

fn hierarchy_service(root: &Path) -> Arc<EnterpriseHierarchyService> {
    Arc::new(EnterpriseHierarchyService::new(Box::new(
        SqliteStorage::open(root).expect("hierarchy storage"),
    )))
}

fn binding_service(
    root: &Path,
    hierarchy: &Arc<EnterpriseHierarchyService>,
) -> EnterpriseScopeBindingService {
    EnterpriseScopeBindingService::new(
        Box::new(SqliteStorage::open(root).expect("binding storage")),
        Arc::clone(hierarchy),
    )
}

fn hierarchy_command(
    organization_id: &OrganizationId,
    request_seed: u64,
    expected_revision: u64,
    mutation: HierarchyMutation,
) -> EnterpriseHierarchyCommand {
    EnterpriseHierarchyCommand {
        actor: actor(),
        organization_id: organization_id.clone(),
        request_id: RequestId(canonical("req", request_seed)),
        expected_revision,
        occurred_at: Instant(format!("2028-01-15T08:{:02}:00.000Z", request_seed % 60)),
        mutation,
    }
}

fn create_hierarchy_resource(
    hierarchy: &EnterpriseHierarchyService,
    organization_id: &OrganizationId,
    request_seed: u64,
    expected_revision: u64,
    id: HierarchyResourceId,
    parent: Option<HierarchyResourceId>,
) -> u64 {
    hierarchy
        .mutate(&hierarchy_command(
            organization_id,
            request_seed,
            expected_revision,
            HierarchyMutation::Create {
                display_name: "Fixture".to_owned(),
                id,
                parent,
            },
        ))
        .expect("create hierarchy resource")
        .current_revision
}

fn seed_hierarchy(hierarchy: &EnterpriseHierarchyService, seed: u64) -> HierarchyFixture {
    let organization_id = OrganizationId(canonical("org", seed));
    let workspace_id = WorkspaceId(canonical("wsp", seed));
    let project_id = ProjectId(canonical("prj", seed));
    let environment_id = EnvironmentId::try_new(canonical("env", seed)).expect("Environment id");
    let repository_id = RepositoryId(canonical("rep", seed));
    let base = seed * 1_000;
    let mut revision = create_hierarchy_resource(
        hierarchy,
        &organization_id,
        base,
        0,
        HierarchyResourceId::Organization(organization_id.clone()),
        None,
    );
    revision = create_hierarchy_resource(
        hierarchy,
        &organization_id,
        base + 1,
        revision,
        HierarchyResourceId::Workspace(workspace_id.clone()),
        Some(HierarchyResourceId::Organization(organization_id.clone())),
    );
    revision = create_hierarchy_resource(
        hierarchy,
        &organization_id,
        base + 2,
        revision,
        HierarchyResourceId::Project(project_id.clone()),
        Some(HierarchyResourceId::Workspace(workspace_id.clone())),
    );
    revision = create_hierarchy_resource(
        hierarchy,
        &organization_id,
        base + 3,
        revision,
        HierarchyResourceId::Repository(repository_id.clone()),
        Some(HierarchyResourceId::Project(project_id.clone())),
    );
    revision = create_hierarchy_resource(
        hierarchy,
        &organization_id,
        base + 4,
        revision,
        HierarchyResourceId::Environment(environment_id.clone()),
        Some(HierarchyResourceId::Project(project_id.clone())),
    );
    HierarchyFixture {
        organization_id,
        workspace_id,
        project_id,
        environment_id,
        repository_id,
        revision,
    }
}

fn binding_command(
    fixture: &HierarchyFixture,
    request_seed: u64,
    expected_revision: u64,
    mutation: EnterpriseScopeBindingMutation,
) -> EnterpriseScopeBindingCommand {
    EnterpriseScopeBindingCommand {
        actor: actor(),
        organization_id: fixture.organization_id.clone(),
        request_id: RequestId(canonical("req", request_seed)),
        expected_revision,
        occurred_at: Instant(format!("2028-02-15T08:{:02}:00.000Z", request_seed % 60)),
        mutation,
    }
}

fn migration_command(
    request_seed: u64,
    expected_revision: u64,
    subjects: Vec<ScopeBindingSubject>,
) -> LocalScopeMigrationCommand {
    LocalScopeMigrationCommand {
        actor: actor(),
        request_id: RequestId(canonical("req", request_seed)),
        expected_revision,
        occurred_at: Instant("2028-03-15T08:00:00.000Z".to_owned()),
        inventory_digest: local_scope_inventory_digest(&subjects).expect("inventory digest"),
        subjects,
    }
}

fn all_subjects() -> Vec<ScopeBindingSubject> {
    vec![
        ScopeBindingSubject::Delivery(DeliveryId(canonical("dlv", 1))),
        ScopeBindingSubject::ProviderSettings(digest('a')),
        ScopeBindingSubject::CredentialReference(CredentialReferenceId(canonical("crd", 1))),
        ScopeBindingSubject::Policy(EnterprisePolicyId(canonical("pol", 1))),
        ScopeBindingSubject::Integration(EnterpriseIntegrationId(canonical("int", 1))),
        ScopeBindingSubject::WorkerPool(EnterpriseWorkerPoolId(canonical("wpl", 1))),
        ScopeBindingSubject::Usage(digest('b')),
    ]
}

#[test]
fn every_resource_class_has_one_binding_and_a_canonical_inheritance_chain() {
    let root = temporary_directory("all-kinds");
    let hierarchy = hierarchy_service(&root);
    let fixture = seed_hierarchy(&hierarchy, 1);
    let bindings = binding_service(&root, &hierarchy);
    let subjects = all_subjects();
    let targets = [
        HierarchyResourceId::Repository(fixture.repository_id.clone()),
        HierarchyResourceId::Organization(fixture.organization_id.clone()),
        HierarchyResourceId::Workspace(fixture.workspace_id.clone()),
        HierarchyResourceId::Project(fixture.project_id.clone()),
        HierarchyResourceId::Environment(fixture.environment_id.clone()),
        HierarchyResourceId::Repository(fixture.repository_id.clone()),
        HierarchyResourceId::Repository(fixture.repository_id.clone()),
    ];
    let expected_chain_lengths = [4, 1, 2, 3, 4, 4, 4];
    for (index, ((subject, target), expected_chain_length)) in subjects
        .iter()
        .zip(targets)
        .zip(expected_chain_lengths)
        .enumerate()
    {
        let receipt = bindings
            .mutate(&binding_command(
                &fixture,
                10_000 + u64::try_from(index).expect("index"),
                u64::try_from(index).expect("revision"),
                EnterpriseScopeBindingMutation::Bind {
                    subject: subject.clone(),
                    target,
                },
            ))
            .expect("bind subject");
        assert_eq!(receipt.current_revision, u64::try_from(index + 1).unwrap());
        let resolved = bindings.resolve(subject).expect("resolve binding");
        assert_eq!(resolved.binding.source, ScopeBindingSource::Explicit);
        assert_eq!(resolved.inheritance_chain.len(), expected_chain_length);
        assert_eq!(resolved.inheritance_chain.last(), Some(&resolved.scope));
    }

    let policy = subjects[3].clone();
    let rebound = bindings
        .mutate(&binding_command(
            &fixture,
            10_100,
            7,
            EnterpriseScopeBindingMutation::Rebind {
                subject: policy.clone(),
                new_target: HierarchyResourceId::Repository(fixture.repository_id.clone()),
            },
        ))
        .expect("rebind mutable subject");
    assert_eq!(rebound.binding.revision, 2);
    assert_eq!(
        bindings.resolve(&policy).unwrap().inheritance_chain.len(),
        4
    );

    let immutable = bindings
        .mutate(&binding_command(
            &fixture,
            10_101,
            8,
            EnterpriseScopeBindingMutation::Rebind {
                subject: subjects[0].clone(),
                new_target: HierarchyResourceId::Project(fixture.project_id.clone()),
            },
        ))
        .expect_err("Delivery attribution must stay immutable");
    assert_eq!(
        immutable.kind(),
        EnterpriseScopeBindingErrorKind::ImmutableBinding
    );
}

#[test]
fn binding_scope_tracks_hierarchy_moves_while_exact_replay_keeps_the_original_result() {
    let root = temporary_directory("hierarchy-move");
    let hierarchy = hierarchy_service(&root);
    let mut fixture = seed_hierarchy(&hierarchy, 2);
    let bindings = binding_service(&root, &hierarchy);
    let subject =
        ScopeBindingSubject::CredentialReference(CredentialReferenceId(canonical("crd", 2)));
    let command = binding_command(
        &fixture,
        20_000,
        0,
        EnterpriseScopeBindingMutation::Bind {
            subject: subject.clone(),
            target: HierarchyResourceId::Repository(fixture.repository_id.clone()),
        },
    );
    let first = bindings.mutate(&command).expect("initial binding");
    let original_scope = first.scope.clone();

    let replacement_workspace = WorkspaceId(canonical("wsp", 22));
    fixture.revision = create_hierarchy_resource(
        &hierarchy,
        &fixture.organization_id,
        20_001,
        fixture.revision,
        HierarchyResourceId::Workspace(replacement_workspace.clone()),
        Some(HierarchyResourceId::Organization(
            fixture.organization_id.clone(),
        )),
    );
    hierarchy
        .mutate(&hierarchy_command(
            &fixture.organization_id,
            20_002,
            fixture.revision,
            HierarchyMutation::Move {
                id: HierarchyResourceId::Project(fixture.project_id.clone()),
                new_parent: HierarchyResourceId::Workspace(replacement_workspace.clone()),
            },
        ))
        .expect("move Project");

    let resolved = bindings.resolve(&subject).expect("moved binding scope");
    assert!(matches!(
        resolved.scope,
        HierarchyScope::Repository { workspace_id, .. } if workspace_id == replacement_workspace
    ));
    let replay = bindings.mutate(&command).expect("exact replay after move");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.scope, original_scope);
}

#[test]
fn hierarchy_tombstones_keep_historical_bindings_traceable_and_reject_new_bindings() {
    let root = temporary_directory("tombstone");
    let hierarchy = hierarchy_service(&root);
    let mut fixture = seed_hierarchy(&hierarchy, 3);
    let bindings = binding_service(&root, &hierarchy);
    let delivery = ScopeBindingSubject::Delivery(DeliveryId(canonical("dlv", 33)));
    bindings
        .mutate(&binding_command(
            &fixture,
            25_000,
            0,
            EnterpriseScopeBindingMutation::Bind {
                subject: delivery.clone(),
                target: HierarchyResourceId::Repository(fixture.repository_id.clone()),
            },
        ))
        .expect("historical Delivery binding");
    fixture.revision = hierarchy
        .mutate(&hierarchy_command(
            &fixture.organization_id,
            25_001,
            fixture.revision,
            HierarchyMutation::Archive {
                id: HierarchyResourceId::Repository(fixture.repository_id.clone()),
            },
        ))
        .expect("archive Repository")
        .current_revision;
    hierarchy
        .mutate(&hierarchy_command(
            &fixture.organization_id,
            25_002,
            fixture.revision,
            HierarchyMutation::Delete {
                id: HierarchyResourceId::Repository(fixture.repository_id.clone()),
            },
        ))
        .expect("delete Repository tombstone");
    assert!(matches!(
        bindings.resolve(&delivery).expect("historical trace").scope,
        HierarchyScope::Repository { repository_id, .. }
            if repository_id == fixture.repository_id
    ));
    let new_policy = binding_command(
        &fixture,
        25_003,
        1,
        EnterpriseScopeBindingMutation::Bind {
            subject: ScopeBindingSubject::Policy(EnterprisePolicyId(canonical("pol", 33))),
            target: HierarchyResourceId::Repository(fixture.repository_id.clone()),
        },
    );
    assert_eq!(
        bindings
            .mutate(&new_policy)
            .expect_err("deleted target rejects new binding")
            .kind(),
        EnterpriseScopeBindingErrorKind::TargetUnavailable
    );
}

#[test]
fn local_migration_handles_more_than_sixteen_records_and_replays_after_restart() {
    let root = temporary_directory("local-migration");
    let hierarchy = hierarchy_service(&root);
    let default_fixture = seed_hierarchy(&hierarchy, 0);
    let mut subjects = all_subjects();
    subjects.extend(
        (10..=40).map(|value| ScopeBindingSubject::Delivery(DeliveryId(canonical("dlv", value)))),
    );
    subjects.reverse();
    let command = migration_command(30_000, 0, subjects.clone());
    let bindings = binding_service(&root, &hierarchy);
    let migrated = bindings
        .migrate_local_once(&command)
        .expect("local migration");
    assert_eq!(migrated.current_revision, 1);
    assert_eq!(
        migrated.migrated_subject_count,
        u64::try_from(subjects.len()).unwrap()
    );
    assert!(matches!(
        migrated.scope,
        HierarchyScope::Repository { repository_id, .. }
            if repository_id == default_fixture.repository_id
    ));
    for subject in subjects
        .iter()
        .filter(|subject| matches!(subject, ScopeBindingSubject::Delivery(_)))
    {
        let resolved = bindings
            .resolve(subject)
            .expect("historical Delivery scope");
        assert_eq!(resolved.binding.source, ScopeBindingSource::LocalMigration);
        assert_eq!(resolved.inheritance_chain.len(), 4);
    }
    drop(bindings);

    let restarted = binding_service(&root, &hierarchy);
    let status = restarted
        .local_migration_status()
        .expect("migration status")
        .expect("completed marker");
    assert_eq!(status.inventory_digest, command.inventory_digest);
    assert_eq!(status.registry_revision, 1);
    let replay = restarted
        .migrate_local_once(&command)
        .expect("migration replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.inventory_digest, migrated.inventory_digest);

    let mut changed = command.clone();
    changed
        .subjects
        .push(ScopeBindingSubject::Delivery(DeliveryId(canonical(
            "dlv", 99,
        ))));
    changed.inventory_digest = local_scope_inventory_digest(&changed.subjects).unwrap();
    assert_eq!(
        restarted
            .migrate_local_once(&changed)
            .expect_err("changed request reuse")
            .kind(),
        EnterpriseScopeBindingErrorKind::RequestConflict
    );
    let mut second = command;
    second.request_id = RequestId(canonical("req", 30_001));
    second.expected_revision = 1;
    assert_eq!(
        restarted
            .migrate_local_once(&second)
            .expect_err("migration only runs once")
            .kind(),
        EnterpriseScopeBindingErrorKind::AlreadyMigrated
    );
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("SQLite fixture");
    connection
        .execute(
            "DELETE FROM product_state WHERE stream_id = (
                 SELECT stream_id FROM product_state
                 WHERE stream_id LIKE 'enterprise-scope-binding-index:%'
                 ORDER BY stream_id LIMIT 1
             )",
            [],
        )
        .expect("remove one migration index shard");
    assert_eq!(
        restarted
            .local_migration_status()
            .expect_err("marker without every index is corrupt")
            .kind(),
        EnterpriseScopeBindingErrorKind::CorruptState
    );
}

#[test]
fn migrated_mutable_resources_can_rebind_without_losing_the_once_only_marker() {
    let root = temporary_directory("migrated-rebind");
    let hierarchy = hierarchy_service(&root);
    let default_fixture = seed_hierarchy(&hierarchy, 0);
    let bindings = binding_service(&root, &hierarchy);
    let subject = ScopeBindingSubject::Policy(EnterprisePolicyId(canonical("pol", 77)));
    let migration = migration_command(35_000, 0, vec![subject.clone()]);
    bindings
        .migrate_local_once(&migration)
        .expect("local migration");
    let rebound = bindings
        .mutate(&binding_command(
            &default_fixture,
            35_001,
            1,
            EnterpriseScopeBindingMutation::Rebind {
                subject: subject.clone(),
                new_target: HierarchyResourceId::Project(default_fixture.project_id.clone()),
            },
        ))
        .expect("rebind migrated Policy");
    assert_eq!(rebound.binding.source, ScopeBindingSource::LocalMigration);
    assert_eq!(rebound.current_revision, 2);
    assert_eq!(
        bindings.resolve(&subject).unwrap().inheritance_chain.len(),
        3
    );
    let marker = bindings.local_migration_status().unwrap().unwrap();
    assert_eq!(marker.migrated_subject_count, 1);
    assert_eq!(marker.registry_revision, 2);
}

#[test]
fn cross_tenant_collision_rejects_the_whole_migration_without_a_marker() {
    let root = temporary_directory("cross-tenant");
    let hierarchy = hierarchy_service(&root);
    let default_fixture = seed_hierarchy(&hierarchy, 0);
    let foreign_fixture = seed_hierarchy(&hierarchy, 4);
    let bindings = binding_service(&root, &hierarchy);
    let collision = ScopeBindingSubject::WorkerPool(EnterpriseWorkerPoolId(canonical("wpl", 44)));
    bindings
        .mutate(&binding_command(
            &foreign_fixture,
            40_000,
            0,
            EnterpriseScopeBindingMutation::Bind {
                subject: collision.clone(),
                target: HierarchyResourceId::Repository(foreign_fixture.repository_id.clone()),
            },
        ))
        .expect("foreign binding");

    let other = ScopeBindingSubject::Delivery(DeliveryId(canonical("dlv", 44)));
    let migration = migration_command(40_001, 0, vec![other.clone(), collision]);
    assert_eq!(
        bindings
            .migrate_local_once(&migration)
            .expect_err("cross-tenant collision")
            .kind(),
        EnterpriseScopeBindingErrorKind::CrossTenantReference
    );
    assert!(bindings.local_migration_status().unwrap().is_none());
    assert_eq!(
        bindings
            .resolve(&other)
            .expect_err("no partial index")
            .kind(),
        EnterpriseScopeBindingErrorKind::NotFound
    );

    let wrong_target = EnterpriseScopeBindingCommand {
        actor: actor(),
        organization_id: default_fixture.organization_id,
        request_id: RequestId(canonical("req", 40_002)),
        expected_revision: 0,
        occurred_at: Instant("2028-04-15T08:00:00.000Z".to_owned()),
        mutation: EnterpriseScopeBindingMutation::Bind {
            subject: ScopeBindingSubject::Policy(EnterprisePolicyId(canonical("pol", 44))),
            target: HierarchyResourceId::Repository(foreign_fixture.repository_id),
        },
    };
    assert_eq!(
        bindings
            .mutate(&wrong_target)
            .expect_err("foreign hierarchy target")
            .kind(),
        EnterpriseScopeBindingErrorKind::CrossTenantReference
    );
}

#[test]
fn a_secondary_index_failure_rolls_back_marker_bindings_receipt_and_event() {
    let root = temporary_directory("atomic-rollback");
    let hierarchy = hierarchy_service(&root);
    seed_hierarchy(&hierarchy, 0);
    let bindings = binding_service(&root, &hierarchy);
    let subject = ScopeBindingSubject::Delivery(DeliveryId(canonical("dlv", 55)));
    let command = migration_command(50_000, 0, vec![subject.clone()]);
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("SQLite fixture");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_scope_binding_index BEFORE INSERT ON product_state
             WHEN NEW.stream_id LIKE 'enterprise-scope-binding-index:%'
             BEGIN SELECT RAISE(FAIL, 'scope binding index fixture'); END;",
        )
        .expect("failure trigger");
    assert_eq!(
        bindings
            .migrate_local_once(&command)
            .expect_err("atomic migration failure")
            .kind(),
        EnterpriseScopeBindingErrorKind::Storage
    );
    assert!(bindings.local_migration_status().unwrap().is_none());
    assert_eq!(
        bindings
            .resolve(&subject)
            .expect_err("rolled-back subject")
            .kind(),
        EnterpriseScopeBindingErrorKind::NotFound
    );
    let durable_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts
             WHERE stream_id LIKE 'enterprise-scope-bindings:%'",
            [],
            |row| row.get(0),
        )
        .expect("receipt count");
    assert_eq!(durable_rows, 0);
    connection
        .execute_batch("DROP TRIGGER fail_scope_binding_index;")
        .expect("drop failure trigger");
    bindings
        .migrate_local_once(&command)
        .expect("retry after rollback");
}

#[test]
fn corrupt_current_state_is_fail_closed_while_the_original_receipt_still_replays() {
    let root = temporary_directory("corrupt-state");
    let hierarchy = hierarchy_service(&root);
    let fixture = seed_hierarchy(&hierarchy, 6);
    let bindings = binding_service(&root, &hierarchy);
    let subject = ScopeBindingSubject::Policy(EnterprisePolicyId(canonical("pol", 66)));
    let command = binding_command(
        &fixture,
        60_000,
        0,
        EnterpriseScopeBindingMutation::Bind {
            subject: subject.clone(),
            target: HierarchyResourceId::Project(fixture.project_id.clone()),
        },
    );
    let first = bindings.mutate(&command).expect("binding");
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("SQLite fixture");
    connection
        .execute(
            "UPDATE product_state SET payload = X'00'
             WHERE stream_id = ?1",
            [format!(
                "enterprise-scope-bindings:{}",
                fixture.organization_id.0
            )],
        )
        .expect("corrupt binding registry");

    let replay = bindings.mutate(&command).expect("receipt-first replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.scope, first.scope);
    drop(bindings);
    let restarted = binding_service(&root, &hierarchy);
    assert_eq!(
        restarted
            .resolve(&subject)
            .expect_err("corrupt state fails closed")
            .kind(),
        EnterpriseScopeBindingErrorKind::CorruptState
    );
}
