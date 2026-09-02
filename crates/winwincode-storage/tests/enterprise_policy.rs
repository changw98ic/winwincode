use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    EnterprisePolicyId, Instant, OrganizationId, ProjectId, RequestId, Sha256Digest, UserId,
    WorkspaceId,
};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyDefinition,
    EnterprisePolicyEffect, EnterprisePolicyErrorKind, EnterprisePolicyFilter,
    EnterprisePolicyInheritanceMode, EnterprisePolicyKind, EnterprisePolicyMode,
    EnterprisePolicyRule, EnterprisePolicyScope, EnterprisePolicyState, EnterprisePolicyWrite,
    ProductStateStorage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-policy-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u8) -> Instant {
    Instant(format!("2027-05-01T08:00:{second:02}.000Z"))
}

fn organization_scope(organization: u64) -> EnterprisePolicyScope {
    EnterprisePolicyScope::Organization {
        organization_id: OrganizationId(id("org", organization)),
    }
}

fn workspace_scope(organization: u64) -> EnterprisePolicyScope {
    EnterprisePolicyScope::Workspace {
        organization_id: OrganizationId(id("org", organization)),
        workspace_id: WorkspaceId(id("wsp", 2)),
    }
}

fn project_scope(organization: u64) -> EnterprisePolicyScope {
    EnterprisePolicyScope::Project {
        organization_id: OrganizationId(id("org", organization)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
    }
}

fn rule(effect: EnterprisePolicyEffect, seed: u64) -> EnterprisePolicyRule {
    EnterprisePolicyRule {
        kind: EnterprisePolicyKind::Model,
        effect,
        resource_pattern: format!("model/{seed}"),
        condition_sha256: Sha256Digest(format!("sha256:{seed:064x}")),
    }
}

fn definition(
    default_effect: EnterprisePolicyEffect,
    child_override_mode: EnterprisePolicyChildOverrideMode,
    rules: Vec<EnterprisePolicyRule>,
) -> EnterprisePolicyDefinition {
    EnterprisePolicyDefinition {
        default_effect,
        child_override_mode,
        rules,
    }
}

fn digest<T: serde::Serialize>(value: &T) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(
            serde_json::to_vec(&serde_json::to_value(value).expect("value fixture"))
                .expect("serialize fixture")
        )
    ))
}

fn write(
    seed: u64,
    policy_id: u64,
    scope: EnterprisePolicyScope,
    definition: EnterprisePolicyDefinition,
) -> EnterprisePolicyWrite {
    EnterprisePolicyWrite {
        policy_id: EnterprisePolicyId(id("pol", policy_id)),
        policy_kind: EnterprisePolicyKind::Model,
        scope,
        mode: EnterprisePolicyMode::Enforce,
        state: EnterprisePolicyState::Active,
        definition_sha256: digest(&definition),
        definition,
        effective_at: instant(1),
        inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
        base_version: None,
        expected_revision: 0,
        source: EnterprisePolicyVersionSourceFixture::source(seed),
        updated_at: instant(1),
    }
}

struct EnterprisePolicyVersionSourceFixture;

impl EnterprisePolicyVersionSourceFixture {
    fn source(seed: u64) -> winwincode_storage::EnterprisePolicyVersionSource {
        winwincode_storage::EnterprisePolicyVersionSource {
            actor: EnterprisePolicyActor::User {
                id: UserId(id("usr", 9)),
            },
            request_id: RequestId(id("req", seed)),
        }
    }
}

#[test]
fn version_chain_freezes_parent_digest_and_restarts_without_changing_bytes() {
    let directory = temporary_directory("restart");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let deny = rule(EnterprisePolicyEffect::Deny, 1);
    let org_definition = definition(
        EnterprisePolicyEffect::Allow,
        EnterprisePolicyChildOverrideMode::TightenOnly,
        vec![deny.clone()],
    );
    let org = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&write(1, 1, organization_scope(1), org_definition))
        .expect("write organization Policy")
        .version;

    let child_definition = definition(
        EnterprisePolicyEffect::Deny,
        EnterprisePolicyChildOverrideMode::TightenOnly,
        vec![deny, rule(EnterprisePolicyEffect::Deny, 2)],
    );
    let mut child_write = write(2, 2, workspace_scope(1), child_definition);
    child_write.base_version = Some(org.reference());
    child_write.effective_at = instant(2);
    child_write.updated_at = instant(2);
    let child = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&child_write)
        .expect("write child Policy")
        .version;
    assert_eq!(child.version, 1);
    assert_eq!(child.revision, 1);
    assert_eq!(child.base_version, Some(org.reference()));
    assert_ne!(child.definition_sha256, child.effective_definition_sha256);
    assert_eq!(child.relaxation_authority, None);

    Box::new(storage).close().expect("close storage");
    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    let loaded = reopened
        .enterprise_policy_ledger()
        .expect("reopen ledger")
        .load_version(&child.policy_id, 1)
        .expect("load immutable version");
    assert_eq!(loaded, child);
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn child_relaxation_requires_exact_active_organization_authority() {
    let directory = temporary_directory("override");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let org_definition = definition(
        EnterprisePolicyEffect::Deny,
        EnterprisePolicyChildOverrideMode::TightenOnly,
        Vec::new(),
    );
    let org = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&write(10, 10, organization_scope(1), org_definition))
        .expect("write restrictive organization Policy")
        .version;
    let child_definition = definition(
        EnterprisePolicyEffect::Allow,
        EnterprisePolicyChildOverrideMode::TightenOnly,
        Vec::new(),
    );
    let mut child = write(11, 11, workspace_scope(1), child_definition.clone());
    child.base_version = Some(org.reference());
    child.effective_at = instant(2);
    child.updated_at = instant(2);
    assert_eq!(
        storage
            .enterprise_policy_ledger()
            .expect("open ledger")
            .write(&child)
            .expect_err("implicit relaxation must fail")
            .kind(),
        EnterprisePolicyErrorKind::AuthorityMismatch
    );

    let enabled_definition = definition(
        EnterprisePolicyEffect::Deny,
        EnterprisePolicyChildOverrideMode::AllowExplicitRelaxation,
        Vec::new(),
    );
    let mut enable = write(12, 10, organization_scope(1), enabled_definition);
    enable.expected_revision = 1;
    enable.effective_at = instant(3);
    enable.updated_at = instant(3);
    let authority = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&enable)
        .expect("enable explicit child override")
        .version;

    let mut explicit = write(13, 11, workspace_scope(1), child_definition);
    explicit.base_version = Some(authority.reference());
    explicit.inheritance_mode = EnterprisePolicyInheritanceMode::Override;
    explicit.effective_at = instant(4);
    explicit.updated_at = instant(4);
    let accepted = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&explicit)
        .expect("accept explicit organization-authorized override")
        .version;
    assert_eq!(accepted.relaxation_authority, Some(authority.reference()));

    let mut retirement = explicit;
    retirement.source = EnterprisePolicyVersionSourceFixture::source(14);
    retirement.expected_revision = 1;
    retirement.state = EnterprisePolicyState::Retired;
    retirement.effective_at = instant(5);
    retirement.updated_at = instant(5);
    retirement.inheritance_mode = EnterprisePolicyInheritanceMode::Tighten;
    assert_eq!(
        storage
            .enterprise_policy_ledger()
            .expect("open ledger")
            .write(&retirement)
            .expect_err("retiring an active child is a relaxation")
            .kind(),
        EnterprisePolicyErrorKind::AuthorityMismatch
    );
    retirement.source = EnterprisePolicyVersionSourceFixture::source(15);
    retirement.inheritance_mode = EnterprisePolicyInheritanceMode::Override;
    storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&retirement)
        .expect("organization-authorized retirement succeeds");
    let organization_history = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .scan_versions(&authority.policy_id, 0, 10)
        .expect("audit organization history");
    assert_eq!(organization_history.len(), 2);
    assert_eq!(organization_history[0].version, 1);
    assert_eq!(organization_history[1], authority);
    let child_history = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .scan_versions(&accepted.policy_id, 0, 10)
        .expect("audit child history");
    assert_eq!(child_history.len(), 2);
    assert_eq!(child_history[0], accepted);
    assert_eq!(child_history[1].state, EnterprisePolicyState::Retired);
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn stale_or_cross_tenant_parent_reference_is_rejected() {
    let directory = temporary_directory("foreign");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let base_definition = definition(
        EnterprisePolicyEffect::Allow,
        EnterprisePolicyChildOverrideMode::TightenOnly,
        Vec::new(),
    );
    let foreign = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&write(
            20,
            20,
            organization_scope(2),
            base_definition.clone(),
        ))
        .expect("write foreign organization Policy")
        .version;
    let local = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&write(
            21,
            21,
            organization_scope(1),
            base_definition.clone(),
        ))
        .expect("write local organization Policy")
        .version;
    let mut child = write(22, 22, project_scope(1), base_definition);
    child.base_version = Some(foreign.reference());
    child.effective_at = instant(2);
    child.updated_at = instant(2);
    assert_eq!(
        storage
            .enterprise_policy_ledger()
            .expect("open ledger")
            .write(&child)
            .expect_err("foreign base must fail")
            .kind(),
        EnterprisePolicyErrorKind::AuthorityMismatch
    );
    child.base_version = Some(local.reference());
    storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&child)
        .expect("exact local base succeeds");

    let reused_identity = write(
        23,
        21,
        workspace_scope(1),
        definition(
            EnterprisePolicyEffect::Allow,
            EnterprisePolicyChildOverrideMode::TightenOnly,
            Vec::new(),
        ),
    );
    assert_eq!(
        storage
            .enterprise_policy_ledger()
            .expect("open ledger")
            .write(&reused_identity)
            .expect_err("one Policy id cannot cross scopes")
            .kind(),
        EnterprisePolicyErrorKind::AuthorityMismatch
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn exact_request_replays_once_and_changed_reuse_is_rejected() {
    let directory = temporary_directory("replay");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let command = write(
        30,
        30,
        organization_scope(1),
        definition(
            EnterprisePolicyEffect::Allow,
            EnterprisePolicyChildOverrideMode::TightenOnly,
            Vec::new(),
        ),
    );
    let first = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&command)
        .expect("first write");
    let replay = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&command)
        .expect("exact replay");
    assert!(!first.idempotent_replay);
    assert!(replay.idempotent_replay);
    assert_eq!(first.version, replay.version);

    let mut changed = command;
    changed.mode = EnterprisePolicyMode::Audit;
    assert_eq!(
        storage
            .enterprise_policy_ledger()
            .expect("open ledger")
            .write(&changed)
            .expect_err("changed reuse must fail")
            .kind(),
        EnterprisePolicyErrorKind::RequestConflict
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn concurrent_first_versions_admit_one_canonical_chain() {
    let directory = temporary_directory("concurrent");
    fs::create_dir_all(&directory).expect("create temp directory");
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for seed in [40, 41] {
        let directory = directory.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut storage = SqliteStorage::open(&directory).expect("open thread storage");
            let command = write(
                seed,
                seed,
                organization_scope(1),
                definition(
                    EnterprisePolicyEffect::Allow,
                    EnterprisePolicyChildOverrideMode::TightenOnly,
                    Vec::new(),
                ),
            );
            barrier.wait();
            storage
                .enterprise_policy_ledger()
                .expect("open ledger")
                .write(&command)
                .map(|receipt| receipt.version.policy_id)
                .map_err(|error| error.kind())
        }));
    }
    barrier.wait();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().expect("join writer"))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_err()).count(),
        1
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn historical_versions_are_immutable_and_head_scan_uses_stable_snapshot() {
    let directory = temporary_directory("history");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    for (seed, kind) in [
        (50, EnterprisePolicyKind::Model),
        (51, EnterprisePolicyKind::Tool),
    ] {
        let mut command = write(
            seed,
            seed,
            organization_scope(1),
            definition(
                EnterprisePolicyEffect::Allow,
                EnterprisePolicyChildOverrideMode::TightenOnly,
                Vec::new(),
            ),
        );
        command.policy_kind = kind;
        command.definition_sha256 = digest(&command.definition);
        storage
            .enterprise_policy_ledger()
            .expect("open ledger")
            .write(&command)
            .expect("write Policy kind");
    }
    let page = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .scan_heads(
            &organization_scope(1),
            &EnterprisePolicyFilter::default(),
            None,
            1,
        )
        .expect("scan first page");
    assert_eq!(page.versions.len(), 1);
    let cursor = page.next.expect("bounded cursor");
    let next = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .scan_heads(
            &organization_scope(1),
            &EnterprisePolicyFilter::default(),
            Some(&cursor),
            1,
        )
        .expect("scan second page");
    assert_eq!(next.snapshot_sequence, page.snapshot_sequence);
    assert_eq!(next.versions.len(), 1);

    Box::new(storage).close().expect("close storage");
    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("open raw database");
    assert!(
        connection
            .execute(
                "UPDATE enterprise_policy_versions SET state = 'retired' WHERE version = 1",
                [],
            )
            .is_err()
    );
    drop(connection);
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn noncanonical_durable_version_bytes_fail_closed_after_restart() {
    let directory = temporary_directory("corrupt-bytes");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let policy = storage
        .enterprise_policy_ledger()
        .expect("open ledger")
        .write(&write(
            60,
            60,
            organization_scope(1),
            definition(
                EnterprisePolicyEffect::Allow,
                EnterprisePolicyChildOverrideMode::TightenOnly,
                Vec::new(),
            ),
        ))
        .expect("write Policy")
        .version;
    Box::new(storage).close().expect("close storage");

    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("open raw database");
    connection
        .execute_batch("DROP TRIGGER enterprise_policy_versions_no_update;")
        .expect("drop immutability trigger for corruption fixture");
    connection
        .execute(
            "UPDATE enterprise_policy_versions
             SET record_json = record_json || ' '
             WHERE policy_id = ?1 AND version = 1",
            [&policy.policy_id.0],
        )
        .expect("inject noncanonical JSON bytes");
    drop(connection);

    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    assert_eq!(
        reopened
            .enterprise_policy_ledger()
            .expect("reopen ledger")
            .load_version(&policy.policy_id, 1)
            .expect_err("noncanonical durable bytes must fail")
            .kind(),
        EnterprisePolicyErrorKind::CorruptState
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}
