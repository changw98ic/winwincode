// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    EnterprisePolicyEnforcement, EnterprisePolicyEnforcementRequest, enforce_enterprise_policy,
    enterprise_policy_condition_sha256, enterprise_policy_subject_sha256,
};
use winwincode_domain::{
    EnterprisePolicyId, Instant, OrganizationId, ProjectId, RepositoryId, RequestId, UserId,
    WorkspaceId,
};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyDefinition,
    EnterprisePolicyEffect, EnterprisePolicyInheritanceMode, EnterprisePolicyKind,
    EnterprisePolicyMode, EnterprisePolicyRule, EnterprisePolicyScope, EnterprisePolicyState,
    EnterprisePolicyVersionSource, EnterprisePolicyWrite, ProductStateStorage, SqliteStorage,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-policy-enforcement-{name}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-09-01T08:00:{second:02}.000Z"))
}

fn scope() -> EnterprisePolicyScope {
    EnterprisePolicyScope::Repository {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn actor() -> EnterprisePolicyActor {
    EnterprisePolicyActor::User {
        id: UserId(id("usr", 5)),
    }
}

fn policy_digest(value: &impl serde::Serialize) -> winwincode_domain::Sha256Digest {
    let canonical = serde_json::to_value(value).expect("Policy value fixture");
    winwincode_domain::Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).expect("serialize Policy fixture"))
    ))
}

fn seed_policy(
    storage: &mut SqliteStorage,
    mode: EnterprisePolicyMode,
    definition: EnterprisePolicyDefinition,
) {
    storage
        .enterprise_policy_ledger()
        .expect("open Policy ledger")
        .write(&EnterprisePolicyWrite {
            policy_id: EnterprisePolicyId(id("pol", 6)),
            policy_kind: EnterprisePolicyKind::Model,
            scope: EnterprisePolicyScope::Organization {
                organization_id: OrganizationId(id("org", 1)),
            },
            mode,
            state: EnterprisePolicyState::Active,
            definition_sha256: policy_digest(&definition),
            definition,
            effective_at: at(1),
            inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
            base_version: None,
            expected_revision: 0,
            source: EnterprisePolicyVersionSource {
                actor: actor(),
                request_id: RequestId(id("req", 6)),
            },
            updated_at: at(1),
        })
        .expect("write Policy");
}

fn request(resource: &str) -> EnterprisePolicyEnforcementRequest {
    EnterprisePolicyEnforcementRequest {
        actor: actor(),
        base_request_id: RequestId(id("req", 7)),
        scope: scope(),
        policy_kind: EnterprisePolicyKind::Model,
        resource: resource.to_owned(),
        subject_sha256: enterprise_policy_subject_sha256(&(resource, "sealed-authority"))
            .expect("subject digest"),
        matched_condition_sha256: Vec::new(),
        evaluated_at: at(2),
        exception_id: None,
    }
}

#[test]
fn audit_mode_default_denial_permits_but_retains_the_version_bound_decision() {
    let root = root("audit");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    seed_policy(
        &mut storage,
        EnterprisePolicyMode::Audit,
        EnterprisePolicyDefinition {
            default_effect: EnterprisePolicyEffect::Deny,
            child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
            rules: Vec::new(),
        },
    );
    let first = enforce_enterprise_policy(&mut storage, &request("model:provider/model"))
        .expect("audit Policy evaluation");
    let EnterprisePolicyEnforcement::Permit(first) = first else {
        panic!("audit-only default denial must permit")
    };
    assert_eq!(
        first.audit.decision.policy_mode,
        Some(EnterprisePolicyMode::Audit)
    );
    assert!(first.audit.decision.policy_version.is_some());
    assert!(!first.audit.decision.hard_invariant);

    let replay = enforce_enterprise_policy(&mut storage, &request("model:provider/model"))
        .expect("exact Policy replay");
    assert!(replay.receipt().idempotent_replay);
    assert_eq!(replay.receipt().audit, first.audit);
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("audit ledger")
            .scan_audit(None, 10)
            .expect("audit page")
            .entries
            .len(),
        1
    );
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn explicit_critical_rule_denies_in_audit_mode_and_changed_reuse_fails_closed() {
    let root = root("critical");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let condition = enterprise_policy_condition_sha256("all");
    seed_policy(
        &mut storage,
        EnterprisePolicyMode::Audit,
        EnterprisePolicyDefinition {
            default_effect: EnterprisePolicyEffect::Allow,
            child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
            rules: vec![EnterprisePolicyRule {
                kind: EnterprisePolicyKind::Model,
                effect: EnterprisePolicyEffect::Deny,
                resource_pattern: "model:*".to_owned(),
                condition_sha256: condition,
            }],
        },
    );
    let denied = enforce_enterprise_policy(&mut storage, &request("model:provider/model"))
        .expect("critical Policy evaluation");
    let EnterprisePolicyEnforcement::Reject(receipt) = denied else {
        panic!("explicit deny remains fail-closed in audit mode")
    };
    assert!(receipt.audit.decision.hard_invariant);
    assert!(receipt.audit.decision.matched_rule.is_some());
    assert!(
        enforce_enterprise_policy(&mut storage, &request("model:provider/changed")).is_err(),
        "changed resource reuse of one durable boundary request must conflict"
    );
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("audit ledger")
            .scan_audit(None, 10)
            .expect("audit page")
            .entries
            .len(),
        1
    );
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove fixture");
}
