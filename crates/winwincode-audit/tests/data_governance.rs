// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_audit::{
    AuditActor, AuditErrorKind, AuditOrigin, AuditOutcome, AuditScope, AuditState, AuditStore,
    ClassificationRule, DataClassification, DataGovernanceAuthority, DeletionDecision,
    DeletionPermit, DeletionPortError, DeletionPortOutcome, GovernanceAuditContext,
    GovernanceDenial, GovernanceErrorKind, GovernedDataFact, GovernedDeletionPort,
    GovernedDeletionResult, LegalHold, LegalHoldId, PlacementDecision, RedactionStrategy,
    ResidencyRegion, RetentionRequirement,
};
use winwincode_domain::{
    OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, SystemActorId, WorkspaceId,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-data-governance-{name}-{}-{serial}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("create governance fixture directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn digest(tail: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", tail.to_string().repeat(64)))
}

fn cn() -> ResidencyRegion {
    ResidencyRegion::try_new("cn-north-1").expect("canonical CN region")
}

fn eu() -> ResidencyRegion {
    ResidencyRegion::try_new("eu-central-1").expect("canonical EU region")
}

fn repository_scope(organization_tail: char, repository_tail: char) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", organization_tail)),
        WorkspaceId(id("wsp", '2')),
        ProjectId(id("prj", '3')),
        RepositoryId(id("rep", repository_tail)),
    )
    .expect("canonical governance repository scope")
}

fn rule(
    classification: DataClassification,
    regions: Vec<ResidencyRegion>,
    retention: RetentionRequirement,
    redaction: RedactionStrategy,
) -> ClassificationRule {
    ClassificationRule::try_new(classification, regions, retention, redaction)
        .expect("valid governance classification rule")
}

fn complete_rules() -> Vec<ClassificationRule> {
    vec![
        rule(
            DataClassification::Public,
            vec![cn(), eu()],
            RetentionRequirement::MinimumDuration(0),
            RedactionStrategy::Reveal,
        ),
        rule(
            DataClassification::Internal,
            vec![cn(), eu()],
            RetentionRequirement::MinimumDuration(100),
            RedactionStrategy::Mask,
        ),
        rule(
            DataClassification::Confidential,
            vec![cn()],
            RetentionRequirement::MinimumDuration(150),
            RedactionStrategy::Hash,
        ),
        rule(
            DataClassification::Restricted,
            vec![cn()],
            RetentionRequirement::MinimumDuration(200),
            RedactionStrategy::Mask,
        ),
        rule(
            DataClassification::Secret,
            vec![cn()],
            RetentionRequirement::Indefinite,
            RedactionStrategy::Remove,
        ),
    ]
}

fn authority(holds: Vec<LegalHold>) -> DataGovernanceAuthority {
    DataGovernanceAuthority::try_new("enterprise.data-governance", 7, complete_rules(), holds)
        .expect("complete governance authority")
}

fn data(
    organization_tail: char,
    repository_tail: char,
    source_tail: char,
    classification: DataClassification,
    region: ResidencyRegion,
) -> GovernedDataFact {
    GovernedDataFact::try_new(
        repository_scope(organization_tail, repository_tail),
        digest(source_tail),
        classification,
        region,
        1_000,
    )
    .expect("valid governed data fact")
}

fn hold(
    tail: char,
    scope: AuditScope,
    source_digest: Option<Sha256Digest>,
    effective: u64,
    released: Option<u64>,
) -> LegalHold {
    LegalHold::try_new(
        LegalHoldId::try_new(id("lgh", tail)).expect("canonical legal hold id"),
        scope,
        source_digest,
        effective,
        released,
    )
    .expect("valid legal hold")
}

#[derive(Default)]
struct RecordingDeletionPort {
    calls: Vec<Sha256Digest>,
    deleted: BTreeSet<String>,
}

impl GovernedDeletionPort for RecordingDeletionPort {
    fn delete(
        &mut self,
        permit: &DeletionPermit,
    ) -> Result<DeletionPortOutcome, DeletionPortError> {
        self.calls.push(permit.decision_digest().clone());
        if self.deleted.insert(permit.source_digest().0.clone()) {
            Ok(DeletionPortOutcome::Deleted)
        } else {
            Ok(DeletionPortOutcome::AlreadyDeleted)
        }
    }
}

fn audit_context(request_tail: char) -> GovernanceAuditContext {
    GovernanceAuditContext::new(
        AuditActor::System(SystemActorId(id("sys", '4'))),
        RequestId(id("req", request_tail)),
        AuditOrigin::local("data-governance").expect("canonical governance audit origin"),
    )
}

#[test]
fn authority_requires_complete_closed_rules_and_safe_redaction() {
    let incomplete = DataGovernanceAuthority::try_new(
        "enterprise.data-governance",
        1,
        complete_rules().into_iter().take(4),
        [],
    )
    .expect_err("every classification requires one rule");
    assert_eq!(incomplete.kind(), GovernanceErrorKind::InvalidInput);

    let unsafe_rule = ClassificationRule::try_new(
        DataClassification::Secret,
        [cn()],
        RetentionRequirement::Indefinite,
        RedactionStrategy::Reveal,
    )
    .expect_err("secret content cannot be revealed");
    assert_eq!(unsafe_rule.kind(), GovernanceErrorKind::InvalidInput);

    let unsafe_region =
        ResidencyRegion::try_new("CN North secret").expect_err("free-form region text is rejected");
    assert_eq!(unsafe_region.kind(), GovernanceErrorKind::InvalidInput);
}

#[test]
fn restricted_content_never_receives_an_out_of_region_permit() {
    let authority = authority(vec![]);
    let restricted = data('1', '5', 'a', DataClassification::Restricted, cn());
    let allowed = authority
        .evaluate_placement(&restricted, &cn(), 1_100)
        .expect("evaluate in-region placement");
    let PlacementDecision::Allowed(permit) = allowed else {
        panic!("in-region restricted placement must be allowed");
    };
    assert_eq!(permit.destination_region(), &cn());
    assert_eq!(permit.source_digest(), restricted.source_digest());
    assert_eq!(permit.rule_version(), 7);
    assert_eq!(permit.rule_digest(), authority.rule_digest());

    let denied = authority
        .evaluate_placement(&restricted, &eu(), 1_100)
        .expect("evaluate out-of-region placement");
    assert!(matches!(
        denied,
        PlacementDecision::Denied {
            denial: GovernanceDenial::ResidencyDenied,
            ..
        }
    ));

    let already_outside = data('1', '5', 'b', DataClassification::Restricted, eu());
    let error = authority
        .redaction_plan(&already_outside)
        .expect_err("an invalid current residency must fail closed");
    assert_eq!(error.kind(), GovernanceErrorKind::ResidencyDenied);
}

#[test]
fn redaction_keeps_source_and_rule_provenance_without_raw_content() {
    let directory = TestDirectory::new("audited-redaction");
    let authority = authority(vec![]);
    let confidential = data('1', '5', 'c', DataClassification::Confidential, cn());
    let mut audit = AuditStore::open(directory.path()).expect("open canonical Audit Ledger");
    let first = authority
        .record_redaction(&confidential, 1_100, &audit_context('H'), &mut audit)
        .expect("record confidential redaction");
    let replay = authority
        .record_redaction(&confidential, 1_100, &audit_context('H'), &mut audit)
        .expect("replay confidential redaction");
    assert_eq!(first, replay);
    assert_eq!(first.source_digest(), confidential.source_digest());
    assert_eq!(first.classification(), DataClassification::Confidential);
    assert_eq!(first.strategy(), RedactionStrategy::Hash);
    assert_eq!(first.rule_id(), "enterprise.data-governance");
    assert_eq!(first.rule_version(), 7);
    assert_eq!(first.rule_digest(), authority.rule_digest());
    assert_ne!(first.decision_digest(), first.source_digest());

    let page = audit
        .read(&repository_scope('1', '5').into_access(), 0, 100, 1_200)
        .expect("read redaction audit decision");
    assert_eq!(page.records().len(), 1);
    let event = page.records()[0].event().expect("retained redaction event");
    assert_eq!(event.occurred_at_millis(), 1_100);
    assert_eq!(event.outcome(), AuditOutcome::Succeeded);
    assert_eq!(event.result_code(), "redaction-planned");
    assert_eq!(event.action().name(), "enterprise.data-governance.v7");
    assert!(matches!(
        event.state(),
        AuditState::Unchanged {
            current: Some(digest)
        } if digest == first.decision_digest()
    ));
}

#[test]
fn rule_digest_commits_version_rules_and_order_independent_legal_holds() {
    let protected = data('1', '5', '3', DataClassification::Restricted, cn());
    let first_hold = hold(
        'J',
        repository_scope('1', '5'),
        Some(protected.source_digest().clone()),
        1_100,
        None,
    );
    let second_hold = hold(
        'K',
        AuditScope::organization(OrganizationId(id("org", '1'))).expect("organization hold scope"),
        None,
        1_200,
        Some(1_900),
    );
    let ordered = authority(vec![first_hold.clone(), second_hold.clone()]);
    let reversed = authority(vec![second_hold, first_hold]);
    let no_holds = authority(vec![]);
    let next_version =
        DataGovernanceAuthority::try_new("enterprise.data-governance", 8, complete_rules(), [])
            .expect("next governance policy version");

    assert_eq!(ordered.rule_digest(), reversed.rule_digest());
    assert_ne!(ordered.rule_digest(), no_holds.rule_digest());
    assert_ne!(no_holds.rule_digest(), next_version.rule_digest());
}

#[test]
fn retention_blocks_early_or_indefinite_deletion() {
    let authority = authority(vec![]);
    let restricted = data('1', '5', 'd', DataClassification::Restricted, cn());
    let plan = authority
        .retention_plan(&restricted, 1_100)
        .expect("compute restricted retention");
    assert_eq!(plan.delete_not_before_millis(), Some(1_200));
    assert_eq!(plan.source_digest(), restricted.source_digest());
    assert_eq!(plan.rule_version(), 7);
    assert!(matches!(
        authority
            .evaluate_deletion(&restricted, 1_199)
            .expect("evaluate early deletion"),
        DeletionDecision::Denied {
            denial: GovernanceDenial::RetentionActive {
                delete_not_before_millis: 1_200
            },
            requested_at_millis: 1_199,
            ..
        }
    ));
    assert!(matches!(
        authority
            .evaluate_deletion(&restricted, 1_200)
            .expect("evaluate deletion at deadline"),
        DeletionDecision::Allowed(_)
    ));

    let secret = data('1', '5', 'e', DataClassification::Secret, cn());
    assert!(matches!(
        authority
            .evaluate_deletion(&secret, 9_000)
            .expect("evaluate indefinite retention"),
        DeletionDecision::Denied {
            denial: GovernanceDenial::IndefiniteRetention,
            ..
        }
    ));
}

#[test]
fn legal_hold_scope_and_source_cannot_be_bypassed() {
    let protected = data('1', '5', 'f', DataClassification::Restricted, cn());
    let organization_hold = hold(
        'A',
        AuditScope::organization(OrganizationId(id("org", '1'))).expect("organization hold scope"),
        None,
        1_100,
        Some(1_500),
    );
    let source_hold = hold(
        'B',
        repository_scope('1', '5'),
        Some(protected.source_digest().clone()),
        1_050,
        Some(1_400),
    );
    let authority = authority(vec![source_hold, organization_hold]);
    let denied = authority
        .evaluate_deletion(&protected, 1_300)
        .expect("evaluate held deletion");
    assert!(matches!(
        denied,
        DeletionDecision::Denied {
            denial: GovernanceDenial::LegalHoldActive { .. },
            ..
        }
    ));

    assert!(matches!(
        authority
            .evaluate_deletion(&protected, 1_500)
            .expect("released holds no longer block"),
        DeletionDecision::Allowed(_)
    ));
    let other_tenant = data('8', '5', 'f', DataClassification::Restricted, cn());
    assert!(matches!(
        authority
            .evaluate_deletion(&other_tenant, 1_300)
            .expect("hold does not cross organizations"),
        DeletionDecision::Allowed(_)
    ));
}

#[test]
fn audit_is_durable_before_the_deletion_port_and_replay_is_idempotent() {
    let directory = TestDirectory::new("audited-delete");
    let protected = data('1', '5', '9', DataClassification::Restricted, cn());
    let legal_hold = hold(
        'C',
        repository_scope('1', '5'),
        Some(protected.source_digest().clone()),
        1_100,
        Some(1_500),
    );
    let authority = authority(vec![legal_hold]);
    let mut audit = AuditStore::open(directory.path()).expect("open canonical Audit Ledger");
    let mut port = RecordingDeletionPort::default();

    let denied = authority
        .execute_deletion(
            &protected,
            1_300,
            &audit_context('D'),
            &mut audit,
            &mut port,
        )
        .expect("record legal-hold denial");
    assert!(matches!(
        denied,
        GovernedDeletionResult::Denied(GovernanceDenial::LegalHoldActive { .. })
    ));
    assert!(port.calls.is_empty());

    let replay = authority
        .execute_deletion(
            &protected,
            1_300,
            &audit_context('D'),
            &mut audit,
            &mut port,
        )
        .expect("replay legal-hold denial");
    assert_eq!(replay, denied);
    assert!(port.calls.is_empty());

    let applied = authority
        .execute_deletion(
            &protected,
            1_600,
            &audit_context('E'),
            &mut audit,
            &mut port,
        )
        .expect("delete after legal hold release");
    assert!(matches!(
        applied,
        GovernedDeletionResult::Applied {
            outcome: DeletionPortOutcome::Deleted,
            ..
        }
    ));
    let applied_replay = authority
        .execute_deletion(
            &protected,
            1_600,
            &audit_context('E'),
            &mut audit,
            &mut port,
        )
        .expect("idempotently replay deletion");
    assert!(matches!(
        applied_replay,
        GovernedDeletionResult::Applied {
            outcome: DeletionPortOutcome::AlreadyDeleted,
            ..
        }
    ));
    assert_eq!(port.calls.len(), 2);

    let page = audit
        .read(&repository_scope('1', '5').into_access(), 0, 100, 2_000)
        .expect("read governance audit decisions");
    assert_eq!(page.records().len(), 2);
    let denied_event = page.records()[0].event().expect("retained denial event");
    assert_eq!(denied_event.occurred_at_millis(), 1_300);
    assert_eq!(denied_event.outcome(), AuditOutcome::Rejected);
    assert_eq!(denied_event.result_code(), "legal-hold-active");
    let allowed_event = page.records()[1].event().expect("retained permit event");
    assert_eq!(allowed_event.occurred_at_millis(), 1_600);
    assert_eq!(allowed_event.outcome(), AuditOutcome::Succeeded);
    assert_eq!(allowed_event.result_code(), "deletion-authorized");
    assert_eq!(
        allowed_event.action().name(),
        "enterprise.data-governance.v7"
    );

    drop(audit);
    let reopened = AuditStore::open(directory.path()).expect("reopen canonical Audit Ledger");
    let checkpoint = reopened
        .verify_organization(protected.scope().organization_id())
        .expect("verify governance audit chain after restart");
    assert_eq!(checkpoint.last_sequence(), 2);
}

#[test]
fn audit_corruption_prevents_any_new_storage_deletion() {
    let directory = TestDirectory::new("audit-first");
    let authority = authority(vec![]);
    let first = data('1', '5', '1', DataClassification::Restricted, cn());
    let second = data('1', '5', '2', DataClassification::Restricted, cn());
    let mut audit = AuditStore::open(directory.path()).expect("open canonical Audit Ledger");
    let mut port = RecordingDeletionPort::default();
    authority
        .execute_deletion(&first, 1_300, &audit_context('F'), &mut audit, &mut port)
        .expect("establish first governed deletion");
    assert_eq!(port.calls.len(), 1);

    let connection = rusqlite::Connection::open(audit.database_path()).expect("inspect Audit DB");
    connection
        .execute(
            "UPDATE audit_chain_heads SET last_digest = \
             'sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
            [],
        )
        .expect("simulate out-of-band chain-head tampering");
    drop(connection);

    let error = authority
        .execute_deletion(&second, 1_300, &audit_context('G'), &mut audit, &mut port)
        .expect_err("corrupt Audit Ledger must block the deletion port");
    assert_eq!(error.kind(), GovernanceErrorKind::Audit);
    assert_eq!(port.calls.len(), 1);
    assert_eq!(
        audit
            .verify_organization(second.scope().organization_id())
            .expect_err("tampered chain remains corrupt")
            .kind(),
        AuditErrorKind::Corrupt
    );
}
