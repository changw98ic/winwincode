use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    EnterprisePolicyId, Instant, OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest,
    UserId, WorkspaceId,
};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyDefinition,
    EnterprisePolicyEffect, EnterprisePolicyEvaluationCommand, EnterprisePolicyEvaluationErrorKind,
    EnterprisePolicyEvaluationInput, EnterprisePolicyEvaluationOutcome,
    EnterprisePolicyEvaluationReason, EnterprisePolicyEvaluationRequest,
    EnterprisePolicyExceptionDecision, EnterprisePolicyExceptionDecisionCommand,
    EnterprisePolicyExceptionId, EnterprisePolicyExceptionRequest, EnterprisePolicyExceptionState,
    EnterprisePolicyInheritanceMode, EnterprisePolicyKind, EnterprisePolicyMode,
    EnterprisePolicyRule, EnterprisePolicyScope, EnterprisePolicyState,
    EnterprisePolicyVersionReference, EnterprisePolicyVersionSource, EnterprisePolicyWrite,
    ProductStateStorage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-policy-evaluation-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u8) -> Instant {
    Instant(format!("2027-06-01T08:00:{second:02}.000Z"))
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

fn sha(seed: u64) -> Sha256Digest {
    Sha256Digest(format!("sha256:{seed:064x}"))
}

fn actor(seed: u64) -> EnterprisePolicyActor {
    EnterprisePolicyActor::User {
        id: UserId(id("usr", seed)),
    }
}

fn organization_scope() -> EnterprisePolicyScope {
    EnterprisePolicyScope::Organization {
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn workspace_scope() -> EnterprisePolicyScope {
    EnterprisePolicyScope::Workspace {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
    }
}

fn repository_scope() -> EnterprisePolicyScope {
    EnterprisePolicyScope::Repository {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn rule(effect: EnterprisePolicyEffect, pattern: &str, condition: u64) -> EnterprisePolicyRule {
    EnterprisePolicyRule {
        kind: EnterprisePolicyKind::Model,
        effect,
        resource_pattern: pattern.into(),
        condition_sha256: sha(condition),
    }
}

fn definition(
    default_effect: EnterprisePolicyEffect,
    rules: Vec<EnterprisePolicyRule>,
) -> EnterprisePolicyDefinition {
    EnterprisePolicyDefinition {
        default_effect,
        child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
        rules,
    }
}

struct PolicyWriteFixture {
    seed: u64,
    policy_id: u64,
    scope: EnterprisePolicyScope,
    definition: EnterprisePolicyDefinition,
    effective_at: Instant,
    base_version: Option<EnterprisePolicyVersionReference>,
    expected_revision: u64,
}

fn write_policy(
    storage: &mut SqliteStorage,
    fixture: PolicyWriteFixture,
) -> EnterprisePolicyVersionReference {
    let write = EnterprisePolicyWrite {
        policy_id: EnterprisePolicyId(id("pol", fixture.policy_id)),
        policy_kind: EnterprisePolicyKind::Model,
        scope: fixture.scope,
        mode: EnterprisePolicyMode::Enforce,
        state: EnterprisePolicyState::Active,
        definition_sha256: digest(&fixture.definition),
        definition: fixture.definition,
        effective_at: fixture.effective_at.clone(),
        inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
        base_version: fixture.base_version,
        expected_revision: fixture.expected_revision,
        source: EnterprisePolicyVersionSource {
            actor: actor(9),
            request_id: RequestId(id("req", fixture.seed)),
        },
        updated_at: fixture.effective_at,
    };
    storage
        .enterprise_policy_ledger()
        .expect("open Policy ledger")
        .write(&write)
        .expect("write Policy")
        .version
        .reference()
}

fn write_default_deny_policy(storage: &mut SqliteStorage, seed: u64) {
    write_policy(
        storage,
        PolicyWriteFixture {
            seed,
            policy_id: seed,
            scope: organization_scope(),
            definition: definition(EnterprisePolicyEffect::Deny, Vec::new()),
            effective_at: instant(1),
            base_version: None,
            expected_revision: 0,
        },
    );
}

fn input(
    scope: EnterprisePolicyScope,
    resource: &str,
    condition: u64,
    at: u8,
) -> EnterprisePolicyEvaluationInput {
    EnterprisePolicyEvaluationInput {
        scope,
        policy_kind: EnterprisePolicyKind::Model,
        resource: resource.into(),
        subject_sha256: sha(90),
        matched_condition_sha256: vec![sha(condition)],
        evaluated_at: instant(at),
    }
}

fn exception_request(
    seed: u64,
    input: EnterprisePolicyEvaluationInput,
) -> EnterprisePolicyExceptionRequest {
    EnterprisePolicyExceptionRequest {
        exception_id: EnterprisePolicyExceptionId(id("pex", seed)),
        input,
        justification_sha256: sha(70),
        expires_at: instant(8),
        actor: actor(20),
        request_id: RequestId(id("req", seed)),
    }
}

#[test]
fn effective_at_and_nearest_inheritance_are_deterministic_and_explainable() {
    let directory = temporary_directory("effective-cut");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let deny_private = rule(EnterprisePolicyEffect::Deny, "model/private/*", 1);
    let organization = write_policy(
        &mut storage,
        PolicyWriteFixture {
            seed: 1,
            policy_id: 1,
            scope: organization_scope(),
            definition: definition(EnterprisePolicyEffect::Allow, vec![deny_private.clone()]),
            effective_at: instant(1),
            base_version: None,
            expected_revision: 0,
        },
    );
    write_policy(
        &mut storage,
        PolicyWriteFixture {
            seed: 2,
            policy_id: 2,
            scope: workspace_scope(),
            definition: definition(
                EnterprisePolicyEffect::Deny,
                vec![
                    deny_private,
                    rule(EnterprisePolicyEffect::Allow, "model/public/*", 2),
                ],
            ),
            effective_at: instant(5),
            base_version: Some(organization.clone()),
            expected_revision: 0,
        },
    );

    let before = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger")
        .dry_run(&EnterprisePolicyEvaluationRequest {
            input: input(repository_scope(), "model/public/a", 2, 4),
            exception_id: None,
        })
        .expect("evaluate before child effectiveAt");
    assert_eq!(before.outcome, EnterprisePolicyEvaluationOutcome::Allow);
    assert_eq!(
        before.reason,
        EnterprisePolicyEvaluationReason::DefaultAllow
    );
    assert_eq!(before.policy_version, Some(organization));

    let after = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger")
        .dry_run(&EnterprisePolicyEvaluationRequest {
            input: input(repository_scope(), "model/public/a", 2, 6),
            exception_id: None,
        })
        .expect("evaluate after child effectiveAt");
    assert_eq!(after.outcome, EnterprisePolicyEvaluationOutcome::Allow);
    assert_eq!(
        after.reason,
        EnterprisePolicyEvaluationReason::ExplicitAllow
    );
    assert_eq!(
        after.policy_version.expect("child version").policy_id.0,
        id("pol", 2)
    );

    let hard = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger")
        .dry_run(&EnterprisePolicyEvaluationRequest {
            input: input(repository_scope(), "model/private/a", 1, 6),
            exception_id: None,
        })
        .expect("evaluate inherited explicit deny");
    assert_eq!(hard.outcome, EnterprisePolicyEvaluationOutcome::Deny);
    assert_eq!(hard.reason, EnterprisePolicyEvaluationReason::ExplicitDeny);
    assert!(hard.hard_invariant);
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn dry_run_writes_nothing_while_enforced_audit_replays_original_bytes_after_restart() {
    let directory = temporary_directory("dry-run-audit");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    write_policy(
        &mut storage,
        PolicyWriteFixture {
            seed: 10,
            policy_id: 10,
            scope: organization_scope(),
            definition: definition(EnterprisePolicyEffect::Deny, Vec::new()),
            effective_at: instant(1),
            base_version: None,
            expected_revision: 0,
        },
    );
    let request = EnterprisePolicyEvaluationRequest {
        input: input(repository_scope(), "model/other", 3, 2),
        exception_id: None,
    };
    let mut ledger = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger");
    let dry = ledger.dry_run(&request).expect("dry run");
    assert_eq!(dry.outcome, EnterprisePolicyEvaluationOutcome::Deny);
    assert!(
        ledger
            .scan_audit(None, 10)
            .expect("scan empty audit")
            .entries
            .is_empty()
    );

    let command = EnterprisePolicyEvaluationCommand {
        request: request.clone(),
        actor: actor(30),
        request_id: RequestId(id("req", 30)),
    };
    let first = ledger.evaluate(&command).expect("audit evaluation");
    assert!(!first.idempotent_replay);
    let mut retry = command.clone();
    retry.request.input.evaluated_at = instant(7);
    let replay = ledger
        .evaluate(&retry)
        .expect("replay with later trusted clock");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.audit, first.audit);
    let mut changed = retry;
    changed.request.input.resource = "model/changed".into();
    assert_eq!(
        ledger
            .evaluate(&changed)
            .expect_err("changed reuse must conflict")
            .kind(),
        EnterprisePolicyEvaluationErrorKind::RequestConflict
    );
    assert_eq!(
        ledger.scan_audit(None, 10).expect("scan audit").entries,
        vec![first.audit.clone()]
    );

    Box::new(storage).close().expect("close storage");
    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    let page = reopened
        .enterprise_policy_evaluation_ledger()
        .expect("reopen evaluation ledger")
        .scan_audit(None, 10)
        .expect("scan restarted audit");
    assert_eq!(page.entries, vec![first.audit]);
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn exception_approval_escalation_and_expiry_use_one_immutable_seal() {
    let directory = temporary_directory("exception-lifecycle");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    write_default_deny_policy(&mut storage, 40);
    let request = exception_request(41, input(repository_scope(), "model/restricted", 4, 2));
    let mut ledger = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger");
    let pending = ledger
        .request_exception(&request)
        .expect("request exception");
    assert_eq!(
        pending.version.state,
        EnterprisePolicyExceptionState::Pending
    );
    let exception_evaluation = |at| EnterprisePolicyEvaluationRequest {
        input: input(repository_scope(), "model/restricted", 4, at),
        exception_id: Some(request.exception_id.clone()),
    };
    let approval = ledger
        .dry_run(&exception_evaluation(3))
        .expect("pending evaluation");
    assert_eq!(
        approval.outcome,
        EnterprisePolicyEvaluationOutcome::RequireApproval
    );

    let escalate = EnterprisePolicyExceptionDecisionCommand {
        exception_id: request.exception_id.clone(),
        scope: repository_scope(),
        expected_revision: 1,
        decision: EnterprisePolicyExceptionDecision::Escalate,
        actor: actor(50),
        request_id: RequestId(id("req", 50)),
        decided_at: instant(3),
    };
    let escalated = ledger
        .decide_exception(&escalate)
        .expect("escalate exception");
    assert_eq!(
        escalated.version.state,
        EnterprisePolicyExceptionState::Escalated
    );
    assert_eq!(
        ledger
            .dry_run(&exception_evaluation(4))
            .expect("escalated evaluation")
            .outcome,
        EnterprisePolicyEvaluationOutcome::Escalate
    );

    let approve = EnterprisePolicyExceptionDecisionCommand {
        exception_id: request.exception_id.clone(),
        scope: repository_scope(),
        expected_revision: 2,
        decision: EnterprisePolicyExceptionDecision::Approve,
        actor: actor(51),
        request_id: RequestId(id("req", 51)),
        decided_at: instant(5),
    };
    let approved = ledger
        .decide_exception(&approve)
        .expect("approve exception");
    assert_eq!(
        approved.version.state,
        EnterprisePolicyExceptionState::Approved
    );
    assert_eq!(
        ledger
            .dry_run(&exception_evaluation(6))
            .expect("approved evaluation")
            .outcome,
        EnterprisePolicyEvaluationOutcome::Allow
    );
    let expired = ledger
        .dry_run(&exception_evaluation(8))
        .expect("expired evaluation");
    assert_eq!(expired.outcome, EnterprisePolicyEvaluationOutcome::Deny);
    assert_eq!(
        expired.reason,
        EnterprisePolicyEvaluationReason::ExceptionExpired
    );

    let mut replay_after_expiry = approve;
    replay_after_expiry.decided_at = instant(9);
    let replay = ledger
        .decide_exception(&replay_after_expiry)
        .expect("terminal receipt replays after expiry");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.version, approved.version);
    assert_eq!(
        ledger
            .scan_exception_versions(&request.exception_id, 0, 10)
            .expect("scan exception history")
            .len(),
        3
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn explicit_deny_is_never_exception_eligible_and_policy_change_invalidates_approval() {
    let directory = temporary_directory("hard-invariant");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    let hard_rule = rule(EnterprisePolicyEffect::Deny, "model/hard/*", 5);
    write_policy(
        &mut storage,
        PolicyWriteFixture {
            seed: 60,
            policy_id: 60,
            scope: organization_scope(),
            definition: definition(EnterprisePolicyEffect::Deny, vec![hard_rule.clone()]),
            effective_at: instant(1),
            base_version: None,
            expected_revision: 0,
        },
    );
    let hard = exception_request(61, input(repository_scope(), "model/hard/a", 5, 2));
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("open evaluation ledger")
            .request_exception(&hard)
            .expect_err("hard deny cannot open exception")
            .kind(),
        EnterprisePolicyEvaluationErrorKind::HardInvariant
    );

    let ordinary = exception_request(62, input(repository_scope(), "model/ordinary", 6, 2));
    {
        let mut ledger = storage
            .enterprise_policy_evaluation_ledger()
            .expect("open evaluation ledger");
        ledger
            .request_exception(&ordinary)
            .expect("request default-deny exception");
        ledger
            .decide_exception(&EnterprisePolicyExceptionDecisionCommand {
                exception_id: ordinary.exception_id.clone(),
                scope: repository_scope(),
                expected_revision: 1,
                decision: EnterprisePolicyExceptionDecision::Approve,
                actor: actor(63),
                request_id: RequestId(id("req", 63)),
                decided_at: instant(3),
            })
            .expect("approve default-deny exception");
    }

    write_policy(
        &mut storage,
        PolicyWriteFixture {
            seed: 64,
            policy_id: 60,
            scope: organization_scope(),
            definition: definition(EnterprisePolicyEffect::Deny, vec![hard_rule]),
            effective_at: instant(5),
            base_version: None,
            expected_revision: 1,
        },
    );
    let stale = EnterprisePolicyEvaluationRequest {
        input: input(repository_scope(), "model/ordinary", 6, 6),
        exception_id: Some(ordinary.exception_id),
    };
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("open evaluation ledger")
            .dry_run(&stale)
            .expect_err("old exception cannot bind a new Policy version")
            .kind(),
        EnterprisePolicyEvaluationErrorKind::AuthorityMismatch
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn durable_exception_and_audit_bytes_must_remain_canonical() {
    let directory = temporary_directory("canonical-bytes");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    write_default_deny_policy(&mut storage, 65);
    let exception = exception_request(66, input(repository_scope(), "model/canonical", 6, 2));
    let evaluation = EnterprisePolicyEvaluationCommand {
        request: EnterprisePolicyEvaluationRequest {
            input: input(repository_scope(), "model/canonical", 6, 2),
            exception_id: None,
        },
        actor: actor(67),
        request_id: RequestId(id("req", 67)),
    };
    {
        let mut ledger = storage
            .enterprise_policy_evaluation_ledger()
            .expect("open evaluation ledger");
        ledger
            .request_exception(&exception)
            .expect("write exception bytes");
        ledger.evaluate(&evaluation).expect("write audit bytes");
    }
    Box::new(storage).close().expect("close storage");

    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("open raw database");
    connection
        .execute_batch(
            "DROP TRIGGER enterprise_policy_exception_versions_no_update;
             DROP TRIGGER enterprise_policy_evaluation_audit_no_update;
             UPDATE enterprise_policy_exception_versions
                SET record_json = ' ' || record_json;
             UPDATE enterprise_policy_evaluation_audit
                SET record_json = ' ' || record_json;",
        )
        .expect("inject noncanonical bytes");
    drop(connection);

    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    let ledger = reopened
        .enterprise_policy_evaluation_ledger()
        .expect("reopen evaluation ledger");
    assert_eq!(
        ledger
            .load_exception(&exception.exception_id)
            .expect_err("noncanonical exception bytes must fail")
            .kind(),
        EnterprisePolicyEvaluationErrorKind::CorruptState
    );
    assert_eq!(
        ledger
            .scan_audit(None, 10)
            .expect_err("noncanonical audit bytes must fail")
            .kind(),
        EnterprisePolicyEvaluationErrorKind::CorruptState
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn concurrent_exception_decisions_commit_one_terminal_version() {
    let directory = temporary_directory("concurrent-decision");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    write_policy(
        &mut storage,
        PolicyWriteFixture {
            seed: 70,
            policy_id: 70,
            scope: organization_scope(),
            definition: definition(EnterprisePolicyEffect::Deny, Vec::new()),
            effective_at: instant(1),
            base_version: None,
            expected_revision: 0,
        },
    );
    let request = exception_request(71, input(repository_scope(), "model/concurrent", 7, 2));
    storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger")
        .request_exception(&request)
        .expect("request exception");
    Box::new(storage).close().expect("close storage");

    let barrier = Arc::new(Barrier::new(2));
    let handles = [
        EnterprisePolicyExceptionDecision::Approve,
        EnterprisePolicyExceptionDecision::Reject,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, decision)| {
        let directory = directory.clone();
        let barrier = Arc::clone(&barrier);
        let exception_id = request.exception_id.clone();
        thread::spawn(move || {
            let mut storage = SqliteStorage::open(&directory).expect("open concurrent storage");
            barrier.wait();
            storage
                .enterprise_policy_evaluation_ledger()
                .expect("open concurrent ledger")
                .decide_exception(&EnterprisePolicyExceptionDecisionCommand {
                    exception_id,
                    scope: repository_scope(),
                    expected_revision: 1,
                    decision,
                    actor: actor(80 + index as u64),
                    request_id: RequestId(id("req", 80 + index as u64)),
                    decided_at: instant(3),
                })
        })
    })
    .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("join decision"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .map(winwincode_storage::EnterprisePolicyEvaluationError::kind)
            .collect::<Vec<_>>(),
        vec![EnterprisePolicyEvaluationErrorKind::RevisionConflict]
    );
    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    let head = reopened
        .enterprise_policy_evaluation_ledger()
        .expect("reopen evaluation ledger")
        .load_exception(&request.exception_id)
        .expect("load exception")
        .expect("exception head");
    assert_eq!(head.revision, 2);
    fs::remove_dir_all(directory).expect("remove temp directory");
}
