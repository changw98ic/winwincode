// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_audit::{
    AuditAction, AuditActor, AuditArtifactDigestKind, AuditExportContent, AuditExportCursor,
    AuditExportErrorKind, AuditExportLimits, AuditExportQuery, AuditExportTimeRange,
    AuditExportVerifier, AuditOrigin, AuditRetention, AuditScope, AuditState, AuditStore,
    AuditSubject, AuditSubjectFilter, ClassificationRule, DataClassification,
    DataGovernanceAuthority, GovernanceAuditContext, GovernedDataFact, RedactionPlan,
    RedactionStrategy, ResidencyRegion, RetentionRequirement,
};
use winwincode_audit::{AuditEvent, AuditEventId};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest,
    SystemActorId, WorkspaceId,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-audit-export-{name}-{}-{serial}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("create audit export fixture directory");
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

fn organization_scope() -> AuditScope {
    AuditScope::organization(OrganizationId(id("org", '1'))).expect("canonical organization scope")
}

fn repository_scope(repository_tail: char) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", '1')),
        WorkspaceId(id("wsp", '2')),
        ProjectId(id("prj", '3')),
        RepositoryId(id("rep", repository_tail)),
    )
    .expect("canonical repository scope")
}

fn event(
    tail: char,
    occurred_at_millis: u64,
    repository_tail: char,
    delivery_tail: char,
    retention: AuditRetention,
) -> AuditEvent {
    AuditEvent::state_change(
        AuditEventId::try_new(id("aud", tail)).expect("canonical audit event id"),
        occurred_at_millis,
        AuditActor::System(SystemActorId(id("sys", '4'))),
        repository_scope(repository_tail),
        RequestId(id("req", tail)),
        AuditAction::command("delivery.advance").expect("canonical audit action"),
        AuditState::changed(Some(digest('0')), digest(tail.to_ascii_lowercase()))
            .expect("canonical audit state change"),
        AuditOrigin::local("control-plane").expect("canonical audit origin"),
        AuditSubject::new()
            .with_delivery(DeliveryId(id("dlv", delivery_tail)))
            .with_product_session(ProductSessionId(id("psn", tail))),
        "completed",
        retention,
    )
    .expect("valid audit export fixture event")
}

fn governance_policy(store: &mut AuditStore, scope: AuditScope) -> (RedactionPlan, AuditEventId) {
    let cn = ResidencyRegion::try_new("cn-north-1").expect("canonical residency region");
    let rules = [
        ClassificationRule::try_new(
            DataClassification::Public,
            [cn.clone()],
            RetentionRequirement::MinimumDuration(0),
            RedactionStrategy::Reveal,
        ),
        ClassificationRule::try_new(
            DataClassification::Internal,
            [cn.clone()],
            RetentionRequirement::MinimumDuration(100),
            RedactionStrategy::Mask,
        ),
        ClassificationRule::try_new(
            DataClassification::Confidential,
            [cn.clone()],
            RetentionRequirement::MinimumDuration(200),
            RedactionStrategy::Hash,
        ),
        ClassificationRule::try_new(
            DataClassification::Restricted,
            [cn.clone()],
            RetentionRequirement::MinimumDuration(300),
            RedactionStrategy::Mask,
        ),
        ClassificationRule::try_new(
            DataClassification::Secret,
            [cn.clone()],
            RetentionRequirement::Indefinite,
            RedactionStrategy::Remove,
        ),
    ]
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("complete safe governance rules");
    let authority = DataGovernanceAuthority::try_new("enterprise.audit-export", 3, rules, [])
        .expect("complete audit export governance authority");
    let source = GovernedDataFact::try_new(
        scope.clone(),
        digest('f'),
        DataClassification::Secret,
        cn,
        900,
    )
    .expect("secret audit export source fact");
    let plan = authority
        .record_redaction(
            &source,
            800,
            &GovernanceAuditContext::new(
                AuditActor::System(SystemActorId(id("sys", 'P'))),
                RequestId(id("req", 'P')),
                AuditOrigin::local("audit-export").expect("canonical governance audit origin"),
            ),
            store,
        )
        .expect("record audit export redaction decision");
    let page = store
        .read(&scope.into_access(), 0, 200, 10_000)
        .expect("read governance decision event");
    let event_id = page
        .records()
        .iter()
        .find_map(|record| {
            record.event().and_then(|event| {
                (event.result_code() == "redaction-planned").then(|| event.event_id().clone())
            })
        })
        .expect("governance decision event identity");
    (plan, event_id)
}

fn query(
    time_range: AuditExportTimeRange,
    scan_records: usize,
    max_record_bytes: usize,
    policy: &RedactionPlan,
    policy_event_id: AuditEventId,
) -> AuditExportQuery {
    query_for_scope(
        organization_scope(),
        time_range,
        scan_records,
        max_record_bytes,
        policy,
        policy_event_id,
    )
}

fn query_for_scope(
    scope: AuditScope,
    time_range: AuditExportTimeRange,
    scan_records: usize,
    max_record_bytes: usize,
    policy: &RedactionPlan,
    policy_event_id: AuditEventId,
) -> AuditExportQuery {
    AuditExportQuery::try_new(
        scope.into_access(),
        time_range,
        10_000,
        AuditExportLimits::try_new(scan_records, max_record_bytes).expect("bounded export limits"),
        policy,
        policy_event_id,
    )
    .expect("valid export query")
}

#[test]
fn fixed_snapshot_pages_filter_subject_and_verify_offline_after_new_appends() {
    let directory = TestDirectory::new("fixed-snapshot");
    let mut store = AuditStore::open(directory.path()).expect("open canonical Audit Ledger");
    let (policy, policy_event_id) = governance_policy(&mut store, repository_scope('5'));
    for fixture in [
        event('5', 1_000, '5', 'A', AuditRetention::Indefinite),
        event('6', 2_000, '5', 'B', AuditRetention::Indefinite),
        event('7', 3_000, '6', 'C', AuditRetention::Indefinite),
    ] {
        store.append(&fixture).expect("append audit export fixture");
    }

    let query = query_for_scope(
        repository_scope('5'),
        AuditExportTimeRange::try_new(1_500, 3_500).expect("bounded export time"),
        2,
        32_768,
        &policy,
        policy_event_id,
    )
    .with_subject(
        AuditSubjectFilter::delivery(DeliveryId(id("dlv", 'B')))
            .expect("canonical Delivery filter"),
    )
    .expect("bind subject filter");

    let first = store.export_page(&query, None).expect("export first page");
    assert_eq!(first.checkpoint().last_sequence(), 4);
    assert_eq!(first.records().len(), 2);
    assert!(matches!(
        first.records()[1].content(),
        AuditExportContent::Witness
    ));
    assert_eq!(first.included_records().count(), 0);
    let (_, first_state) = AuditExportVerifier::verify_json(
        &serde_json::to_vec(&first).expect("encode first export page"),
        None,
    )
    .expect("offline verify first page");

    store
        .append(&event('8', 4_000, '5', 'B', AuditRetention::Indefinite))
        .expect("append after fixed snapshot");

    let cursor: AuditExportCursor = serde_json::from_slice(
        &serde_json::to_vec(first.next_cursor().expect("first continuation cursor"))
            .expect("encode continuation cursor"),
    )
    .expect("decode continuation cursor after restart");
    drop(store);
    let store = AuditStore::open(directory.path()).expect("reopen canonical Audit Ledger");

    let second = store
        .export_page(&query, Some(&cursor))
        .expect("export second page from fixed snapshot");
    assert_eq!(second.checkpoint().last_sequence(), 4);
    assert_eq!(second.included_records().count(), 1);
    let included = second.included_records().next().expect("matching event");
    assert!(matches!(
        included.content(),
        AuditExportContent::Event { .. }
    ));
    assert_eq!(included.artifact_references().len(), 2);
    assert_eq!(
        included.artifact_references()[0].kind(),
        AuditArtifactDigestKind::StateBefore
    );
    assert!(second.next_cursor().is_none());
    let final_state = AuditExportVerifier::verify_page(&second, Some(&first_state))
        .expect("offline verify final page");
    assert!(final_state.complete());
    assert_eq!(final_state.after_sequence(), 4);
    assert_eq!(
        store
            .verify_organization(organization_scope().organization_id())
            .expect("verify current chain")
            .last_sequence(),
        5
    );

    let encoded = serde_json::to_string(&second).expect("encode secret-safe export page");
    assert_eq!(
        second.manifest().policy().strategy(),
        RedactionStrategy::Remove
    );
    assert!(!encoded.contains("RAW_SECRET_TOKEN"));

    let mut changed_policy =
        serde_json::to_value(&second).expect("encode governance-bound export page");
    changed_policy["manifest"]["policy"]["strategy"] =
        serde_json::Value::String("reveal".to_owned());
    assert_eq!(
        AuditExportVerifier::verify_json(
            &serde_json::to_vec(&changed_policy).expect("encode changed governance proof"),
            Some(&first_state),
        )
        .expect_err("changed redaction strategy must fail offline verification")
        .kind(),
        AuditExportErrorKind::Corrupt
    );
}

#[test]
fn finite_payload_deletion_exports_a_sealed_proof_and_tampering_fails() {
    let directory = TestDirectory::new("deletion-proof");
    let mut store = AuditStore::open(directory.path()).expect("open canonical Audit Ledger");
    let (policy, policy_event_id) = governance_policy(&mut store, organization_scope());
    store
        .append(&event(
            '9',
            1_000,
            '5',
            'D',
            AuditRetention::UntilMillis(1_200),
        ))
        .expect("append finite audit payload");
    assert_eq!(
        store
            .prune_expired_payloads(1_300)
            .expect("prune finite payload after deadline"),
        1
    );
    let query = AuditExportQuery::try_new(
        organization_scope().into_access(),
        AuditExportTimeRange::try_new(900, 1_100).expect("deletion proof time range"),
        1_300,
        AuditExportLimits::try_new(10, 32_768).expect("bounded export limits"),
        &policy,
        policy_event_id,
    )
    .expect("valid deletion proof query");
    let page = store
        .export_page(&query, None)
        .expect("export deletion proof");
    assert_eq!(page.included_records().count(), 1);
    let deletion_record = page
        .included_records()
        .next()
        .expect("included deletion proof");
    let AuditExportContent::DeletionProof { proof } = deletion_record.content() else {
        panic!("pruned payload must export a deletion proof");
    };
    assert_eq!(proof.pruned_at_millis(), 1_300);
    assert_eq!(
        proof.tombstone_event_digest(),
        deletion_record.header().event_digest()
    );
    assert!(
        AuditExportVerifier::verify_page(&page, None)
            .expect("offline verify deletion proof")
            .complete()
    );

    let subject_query = query
        .clone()
        .with_subject(
            AuditSubjectFilter::delivery(DeliveryId(id("dlv", 'D')))
                .expect("canonical Delivery filter"),
        )
        .expect("bind subject filter");
    assert_eq!(
        store
            .export_page(&subject_query, None)
            .expect_err("a deleted payload cannot prove a subject match")
            .kind(),
        AuditExportErrorKind::SnapshotConflict
    );

    let mut encoded = serde_json::to_value(&page).expect("encode proof page");
    encoded["records"][1]["header"]["event_digest"] = serde_json::Value::String(digest('e').0);
    let changed = serde_json::to_vec(&encoded).expect("encode changed proof page");
    assert_eq!(
        AuditExportVerifier::verify_json(&changed, None)
            .expect_err("changed deletion proof must fail offline verification")
            .kind(),
        AuditExportErrorKind::Corrupt
    );
}

#[test]
fn page_and_byte_limits_are_closed_and_enforced() {
    assert_eq!(
        AuditExportLimits::try_new(0, 1)
            .expect_err("zero scan bound is invalid")
            .kind(),
        AuditExportErrorKind::InvalidInput
    );
    assert_eq!(
        AuditExportLimits::try_new(201, 1_048_576)
            .expect_err("unbounded scan count is invalid")
            .kind(),
        AuditExportErrorKind::InvalidInput
    );
    assert_eq!(
        AuditExportLimits::try_new(1, 1_048_577)
            .expect_err("unbounded record bytes are invalid")
            .kind(),
        AuditExportErrorKind::InvalidInput
    );

    let directory = TestDirectory::new("byte-bound");
    let mut store = AuditStore::open(directory.path()).expect("open canonical Audit Ledger");
    let (policy, policy_event_id) = governance_policy(&mut store, organization_scope());
    store
        .append(&event('A', 1_000, '5', 'A', AuditRetention::Indefinite))
        .expect("append bounded audit event");
    let query = query(
        AuditExportTimeRange::try_new(900, 1_100).expect("bounded export time"),
        1,
        1,
        &policy,
        policy_event_id,
    );
    assert_eq!(
        store
            .export_page(&query, None)
            .expect_err("an individual record cannot exceed the byte cap")
            .kind(),
        AuditExportErrorKind::InvalidInput
    );
}
