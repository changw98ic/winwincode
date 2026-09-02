use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    EnterprisePolicyDecisionClock, EnterprisePolicyEvaluationService,
    EnterprisePolicyEvaluationTarget, EnterprisePolicyExceptionDecisionRequest,
    EnterprisePolicyExceptionOpenRequest,
};
use winwincode_domain::{
    EnterprisePolicyId, Instant, OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest,
    UserId, WorkspaceId,
};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyDefinition,
    EnterprisePolicyEffect, EnterprisePolicyEvaluationOutcome, EnterprisePolicyEvaluationReason,
    EnterprisePolicyExceptionDecision, EnterprisePolicyExceptionId,
    EnterprisePolicyInheritanceMode, EnterprisePolicyKind, EnterprisePolicyMode,
    EnterprisePolicyScope, EnterprisePolicyState, EnterprisePolicyVersionSource,
    EnterprisePolicyWrite, ProductStateStorage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct SequenceClock(VecDeque<Instant>);

impl SequenceClock {
    fn new(values: impl IntoIterator<Item = Instant>) -> Self {
        Self(values.into_iter().collect())
    }
}

impl EnterprisePolicyDecisionClock for SequenceClock {
    fn now(&mut self) -> Instant {
        self.0.pop_front().expect("trusted time fixture")
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-policy-evaluation-app-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u8) -> Instant {
    Instant(format!("2027-07-01T08:00:{second:02}.000Z"))
}

fn sha(seed: u64) -> Sha256Digest {
    Sha256Digest(format!("sha256:{seed:064x}"))
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

fn repository_scope() -> EnterprisePolicyScope {
    EnterprisePolicyScope::Repository {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn target(resource: &str) -> EnterprisePolicyEvaluationTarget {
    EnterprisePolicyEvaluationTarget {
        scope: repository_scope(),
        policy_kind: EnterprisePolicyKind::Model,
        resource: resource.into(),
        subject_sha256: sha(80),
        matched_condition_sha256: vec![sha(81)],
    }
}

fn seed_default_deny(storage: &mut SqliteStorage) {
    let definition = EnterprisePolicyDefinition {
        default_effect: EnterprisePolicyEffect::Deny,
        child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
        rules: Vec::new(),
    };
    storage
        .enterprise_policy_ledger()
        .expect("open Policy ledger")
        .write(&EnterprisePolicyWrite {
            policy_id: EnterprisePolicyId(id("pol", 1)),
            policy_kind: EnterprisePolicyKind::Model,
            scope: organization_scope(),
            mode: EnterprisePolicyMode::Enforce,
            state: EnterprisePolicyState::Active,
            definition_sha256: digest(&definition),
            definition,
            effective_at: instant(1),
            inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
            base_version: None,
            expected_revision: 0,
            source: EnterprisePolicyVersionSource {
                actor: actor(1),
                request_id: RequestId(id("req", 1)),
            },
            updated_at: instant(1),
        })
        .expect("write Policy");
}

#[test]
fn trusted_clock_drives_dry_run_and_exact_audit_replay() {
    let directory = temporary_directory("audit");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    seed_default_deny(&mut storage);
    let mut clock = SequenceClock::new([instant(2), instant(3), instant(7)]);
    let request_id = RequestId(id("req", 2));
    let evaluation_target = target("model/restricted");
    {
        let mut service = EnterprisePolicyEvaluationService::new(&mut storage, &mut clock);
        let dry = service
            .dry_run(&evaluation_target, None)
            .expect("dry run Policy");
        assert_eq!(dry.outcome, EnterprisePolicyEvaluationOutcome::Deny);
        assert_eq!(dry.evaluated_at, instant(2));

        let first = service
            .evaluate(actor(2), request_id.clone(), &evaluation_target, None)
            .expect("evaluate Policy");
        let replay = service
            .evaluate(actor(2), request_id, &evaluation_target, None)
            .expect("replay Policy evaluation");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.audit, first.audit);
        assert_eq!(first.audit.decision.evaluated_at, instant(3));
    }
    let audit = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open evaluation ledger")
        .scan_audit(None, 10)
        .expect("scan audit");
    assert_eq!(audit.entries.len(), 1);
    fs::remove_dir_all(directory).expect("remove temp directory");
}

#[test]
fn approved_exception_survives_restart_and_expires_on_the_exact_clock_cut() {
    let directory = temporary_directory("exception");
    fs::create_dir_all(&directory).expect("create temp directory");
    let mut storage = SqliteStorage::open(&directory).expect("open storage");
    seed_default_deny(&mut storage);
    let exception_id = EnterprisePolicyExceptionId(id("pex", 3));
    let mut clock = SequenceClock::new([instant(2), instant(3)]);
    let decision = EnterprisePolicyExceptionDecisionRequest {
        exception_id: exception_id.clone(),
        scope: repository_scope(),
        expected_revision: 1,
        decision: EnterprisePolicyExceptionDecision::Approve,
        actor: actor(4),
        request_id: RequestId(id("req", 4)),
    };
    {
        let mut service = EnterprisePolicyEvaluationService::new(&mut storage, &mut clock);
        service
            .request_exception(EnterprisePolicyExceptionOpenRequest {
                exception_id: exception_id.clone(),
                target: target("model/exception"),
                justification_sha256: sha(90),
                expires_at: instant(8),
                actor: actor(3),
                request_id: RequestId(id("req", 3)),
            })
            .expect("request exception");
        service
            .decide_exception(decision.clone())
            .expect("approve exception");
    }
    Box::new(storage).close().expect("close storage");

    let mut reopened = SqliteStorage::open(&directory).expect("reopen storage");
    let mut restart_clock = SequenceClock::new([instant(6), instant(8), instant(9)]);
    let mut service = EnterprisePolicyEvaluationService::new(&mut reopened, &mut restart_clock);
    assert_eq!(
        service
            .dry_run(&target("model/exception"), Some(exception_id.clone()))
            .expect("evaluate approved exception")
            .outcome,
        EnterprisePolicyEvaluationOutcome::Allow
    );
    let expired = service
        .dry_run(&target("model/exception"), Some(exception_id))
        .expect("evaluate expired exception");
    assert_eq!(expired.outcome, EnterprisePolicyEvaluationOutcome::Deny);
    assert_eq!(
        expired.reason,
        EnterprisePolicyEvaluationReason::ExceptionExpired
    );
    assert!(
        service
            .decide_exception(decision)
            .expect("terminal decision exact replay")
            .idempotent_replay
    );
    fs::remove_dir_all(directory).expect("remove temp directory");
}
