use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, EnterprisePolicyDefinition, EnterprisePolicyListParameters, EnterprisePolicyListQuery,
    EnterprisePolicyListQueryQuery, EnterprisePolicyRule, EnterprisePolicyUpdateCommand,
    EnterprisePolicyUpdateCommandCommand, EnterprisePolicyUpdatePayload,
    EnterprisePolicyVersionReference, OrganizationScope, OrganizationScopeKind, PageRequest, Scope,
};
use winwincode_control_plane::{
    EnterprisePolicyApiErrorKind, EnterprisePolicyApiService, EnterprisePolicyClock,
};
use winwincode_domain::{
    EnterprisePolicyId, Instant, OpaqueCursor, OrganizationId, RequestId, Revision, SchemaVersion,
    Sha256Digest, UserId,
};
use winwincode_domain::{UserActor, UserActorKind};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const POLICY_KINDS: [&str; 11] = [
    "repository",
    "model",
    "provider",
    "tool",
    "network",
    "approval",
    "verifier",
    "worker_placement",
    "publication",
    "retention",
    "integration",
];

struct FixedClock(Instant);

impl EnterprisePolicyClock for FixedClock {
    fn now(&mut self) -> Instant {
        self.0.clone()
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-cp-enterprise-policy-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn scope(organization: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", organization)),
    })
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: UserId(id("usr", 7)),
    })
}

fn definition(kind: &str) -> EnterprisePolicyDefinition {
    EnterprisePolicyDefinition {
        child_override_mode: "tighten_only".to_owned(),
        default_effect: "allow".to_owned(),
        rules: vec![EnterprisePolicyRule {
            condition_sha256: Sha256Digest(format!("sha256:{:064x}", 1)),
            effect: "deny".to_owned(),
            kind: kind.to_owned(),
            resource_pattern: "resource/restricted".to_owned(),
        }],
    }
}

fn canonical_digest<T: serde::Serialize>(value: &T) -> Sha256Digest {
    let canonical = serde_json::to_value(value).expect("serialize canonical value");
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).expect("encode canonical value"))
    ))
}

fn command(seed: u64, kind: &str, organization: u64) -> EnterprisePolicyUpdateCommand {
    let definition = definition(kind);
    EnterprisePolicyUpdateCommand {
        actor: actor(),
        command: EnterprisePolicyUpdateCommandCommand::EnterprisePolicyUpdate,
        expected_revision: Revision(0),
        payload: EnterprisePolicyUpdatePayload {
            base_version: None,
            definition_sha256: canonical_digest(&definition),
            definition,
            effective_at: Instant("2027-05-01T08:00:00.000Z".to_owned()),
            inheritance_mode: "tighten".to_owned(),
            mode: "enforce".to_owned(),
            policy_id: EnterprisePolicyId(id("pol", seed)),
            policy_kind: kind.to_owned(),
            state: "active".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(organization),
    }
}

fn list_query(seed: u64, cursor: Option<OpaqueCursor>, limit: i64) -> EnterprisePolicyListQuery {
    EnterprisePolicyListQuery {
        actor: actor(),
        page: PageRequest { cursor, limit },
        parameters: EnterprisePolicyListParameters {
            policy_kinds: Vec::new(),
            states: Vec::new(),
        },
        query: EnterprisePolicyListQueryQuery::EnterprisePolicyList,
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(1),
    }
}

fn version_reference(
    projection: &winwincode_api::generated::EnterprisePolicyProjection,
) -> EnterprisePolicyVersionReference {
    EnterprisePolicyVersionReference {
        definition_sha256: projection.definition_sha256.clone(),
        effective_definition_sha256: projection.effective_definition_sha256.clone(),
        policy_id: projection.id.clone(),
        policy_kind: projection.policy_kind.clone(),
        scope: projection.scope.clone(),
        version: projection.version,
        version_digest: projection.version_digest.clone(),
    }
}

#[test]
fn generated_update_and_bounded_list_restart_with_exact_projection_bytes() {
    let directory = temporary_directory("restart");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let mut clock = FixedClock(Instant("2027-05-01T08:00:01.000Z".to_owned()));
    let mut expected = Vec::new();
    for (offset, kind) in POLICY_KINDS.iter().enumerate() {
        let response = EnterprisePolicyApiService::new(&mut storage, &mut clock)
            .update(&command(100 + offset as u64, kind, 1))
            .expect("apply generated Policy update");
        assert_eq!(response.result.scope, scope(1));
        assert_eq!(response.result.source.actor, actor());
        assert_eq!(response.result.base_version, None);
        assert_eq!(response.result.relaxation_authority, None);
        assert_eq!(response.result.effective_at.0, "2027-05-01T08:00:00.000Z");
        assert_eq!(response.result.updated_at.0, "2027-05-01T08:00:01.000Z");
        expected.push(response.result);
    }

    let first = EnterprisePolicyApiService::new(&mut storage, &mut clock)
        .list(&list_query(200, None, 5))
        .expect("list first page");
    assert_eq!(first.result.items.len(), 5);
    assert!(first.page.has_more);
    let cursor = first.page.next_cursor.clone().expect("next cursor");
    let snapshot = first.result.snapshot_revision.clone();

    Box::new(storage).close().expect("close storage");
    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    let second = EnterprisePolicyApiService::new(&mut reopened, &mut clock)
        .list(&list_query(201, Some(cursor), 20))
        .expect("resume stable list after restart");
    assert_eq!(second.result.snapshot_revision, snapshot);
    assert_eq!(second.result.items.len(), 6);
    let mut listed = first.result.items;
    listed.extend(second.result.items);
    listed.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    expected.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    assert_eq!(
        serde_json::to_vec(&listed).expect("encode listed projections"),
        serde_json::to_vec(&expected).expect("encode written projections")
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn generated_exact_replay_is_stable_and_changed_body_conflicts() {
    let directory = temporary_directory("replay");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let mut clock = FixedClock(Instant("2027-05-01T08:00:01.000Z".to_owned()));
    let request = command(300, "model", 1);
    let first = EnterprisePolicyApiService::new(&mut storage, &mut clock)
        .update(&request)
        .expect("first update");
    clock.0 = Instant("2027-05-01T09:00:00.000Z".to_owned());
    let replay = EnterprisePolicyApiService::new(&mut storage, &mut clock)
        .update(&request)
        .expect("exact update replay");
    assert_eq!(
        serde_json::to_vec(&first).expect("encode first response"),
        serde_json::to_vec(&replay).expect("encode replay response")
    );

    let mut changed = request;
    changed.payload.mode = "audit".to_owned();
    assert_eq!(
        EnterprisePolicyApiService::new(&mut storage, &mut clock)
            .update(&changed)
            .expect_err("changed request reuse must conflict")
            .kind(),
        EnterprisePolicyApiErrorKind::RequestConflict
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn generated_cross_tenant_base_reference_is_rejected() {
    let directory = temporary_directory("foreign-base");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let mut clock = FixedClock(Instant("2027-05-01T08:00:01.000Z".to_owned()));
    let foreign = EnterprisePolicyApiService::new(&mut storage, &mut clock)
        .update(&command(400, "model", 2))
        .expect("write foreign Policy");
    let mut local = command(401, "model", 1);
    local.scope = Scope::WorkspaceScope(winwincode_api::generated::WorkspaceScope {
        kind: winwincode_api::generated::WorkspaceScopeKind::Workspace,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: winwincode_domain::WorkspaceId(id("wsp", 2)),
    });
    local.payload.base_version = Some(version_reference(&foreign.result));
    local.payload.effective_at = Instant("2027-05-01T08:00:02.000Z".to_owned());
    assert_eq!(
        EnterprisePolicyApiService::new(&mut storage, &mut clock)
            .update(&local)
            .expect_err("foreign base must fail")
            .kind(),
        EnterprisePolicyApiErrorKind::AuthorityMismatch
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}
