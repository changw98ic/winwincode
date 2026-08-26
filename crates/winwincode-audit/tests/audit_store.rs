// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use winwincode_audit::{
    AuditAction, AuditActionKind, AuditActor, AuditErrorKind, AuditEvent, AuditEventId,
    AuditModelInvocation, AuditOrigin, AuditOutcome, AuditPage, AuditRetention, AuditScope,
    AuditState, AuditStore, AuditSubject,
};
use winwincode_domain::{
    DeliveryId, LeaseId, OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId,
    RequestId, ServiceAccountId, Sha256Digest, SystemActorId, UserId, WorkspaceId,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-audit-{name}-{}-{serial}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("create audit fixture directory");
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

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn repository_scope() -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", '1')),
        WorkspaceId(id("wsp", '2')),
        ProjectId(id("prj", '3')),
        RepositoryId(id("rep", '4')),
    )
    .expect("canonical repository audit scope")
}

fn failed_event(tail: char, occurred_at_millis: u64, scope: AuditScope) -> AuditEvent {
    AuditEvent::failed(
        AuditEventId::try_new(id("aud", tail)).expect("canonical fixture audit event id"),
        occurred_at_millis,
        AuditActor::System(SystemActorId(id("sys", '0'))),
        scope,
        RequestId(id("req", tail)),
        AuditAction::policy("policy.evaluate").expect("canonical fixture policy action"),
        AuditState::unchanged(None).expect("unchanged fixture state"),
        AuditOrigin::local("control-plane").expect("canonical fixture origin"),
        AuditSubject::new(),
        "policy-denied",
        AuditRetention::Indefinite,
    )
    .expect("valid failed fixture event")
}

#[derive(Clone, Copy)]
enum FixtureScope {
    Organization,
    WorkspaceA,
    ProjectA,
    RepositoryA,
    RepositoryB,
    ProjectB,
    OtherOrganization,
}

fn fixture_scope(scope: FixtureScope) -> AuditScope {
    let organization = OrganizationId(id("org", '1'));
    let workspace = WorkspaceId(id("wsp", '2'));
    let project = ProjectId(id("prj", '4'));
    match scope {
        FixtureScope::Organization => {
            AuditScope::organization(organization).expect("organization fixture scope")
        }
        FixtureScope::WorkspaceA => {
            AuditScope::workspace(organization, workspace).expect("workspace fixture scope")
        }
        FixtureScope::ProjectA => {
            AuditScope::project(organization, workspace, project).expect("project fixture scope")
        }
        FixtureScope::RepositoryA => AuditScope::repository(
            organization,
            workspace,
            project,
            RepositoryId(id("rep", '6')),
        )
        .expect("first repository fixture scope"),
        FixtureScope::RepositoryB => AuditScope::repository(
            organization,
            workspace,
            project,
            RepositoryId(id("rep", '7')),
        )
        .expect("second repository fixture scope"),
        FixtureScope::ProjectB => AuditScope::project(
            organization,
            WorkspaceId(id("wsp", '3')),
            ProjectId(id("prj", '5')),
        )
        .expect("other workspace project fixture scope"),
        FixtureScope::OtherOrganization => AuditScope::organization(OrganizationId(id("org", '8')))
            .expect("other organization fixture scope"),
    }
}

fn read_fixture_page(
    store: &AuditStore,
    scope: FixtureScope,
    after_sequence: u64,
    limit: usize,
) -> AuditPage {
    store
        .read(
            &fixture_scope(scope).into_access(),
            after_sequence,
            limit,
            1_800_000_002_000,
        )
        .expect("read scoped audit fixture page")
}

fn assert_retention_rows_are_immutable(database_path: &Path) {
    let connection = rusqlite::Connection::open(database_path).expect("inspect audit db");
    let header_count = connection
        .query_row("SELECT COUNT(*) FROM audit_events", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count immutable headers");
    assert_eq!(header_count, 2);
    let payload_count = connection
        .query_row("SELECT COUNT(*) FROM audit_payloads", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count retained payloads");
    assert_eq!(payload_count, 1);
    let tombstone_count = connection
        .query_row("SELECT COUNT(*) FROM audit_payload_tombstones", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count immutable retention tombstones");
    assert_eq!(tombstone_count, 1);
    let update = connection.execute(
        "UPDATE audit_events SET event_digest = 'sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff' \
         WHERE sequence = 1",
        [],
    );
    assert!(
        update.is_err(),
        "audit event header update must be rejected"
    );
    let delete = connection.execute("DELETE FROM audit_events WHERE sequence = 1", []);
    assert!(
        delete.is_err(),
        "audit event header delete must be rejected"
    );
    let tombstone_update = connection.execute(
        "UPDATE audit_payload_tombstones SET pruned_at_millis = pruned_at_millis + 1 \
         WHERE sequence = 1",
        [],
    );
    assert!(
        tombstone_update.is_err(),
        "audit retention tombstone update must be rejected"
    );
    let tombstone_delete = connection.execute(
        "DELETE FROM audit_payload_tombstones WHERE sequence = 1",
        [],
    );
    assert!(
        tombstone_delete.is_err(),
        "audit retention tombstone delete must be rejected"
    );
}

#[test]
fn state_change_survives_restart_with_one_verified_sequence() {
    let directory = TestDirectory::new("state-change");
    let event = AuditEvent::state_change(
        AuditEventId::try_new(id("aud", '5')).expect("canonical audit event id"),
        1_800_000_000_000,
        AuditActor::System(SystemActorId(id("sys", '6'))),
        repository_scope(),
        RequestId(id("req", '7')),
        AuditAction::command("delivery.advance").expect("canonical command action"),
        AuditState::changed(Some(digest('a')), digest('b')).expect("changed state digests"),
        AuditOrigin::local("control-plane").expect("canonical local origin"),
        AuditSubject::new()
            .with_delivery(DeliveryId(id("dlv", '8')))
            .with_product_session(ProductSessionId(id("psn", '9')))
            .with_lease(LeaseId(id("lse", 'A'))),
        "completed",
        AuditRetention::UntilMillis(1_900_000_000_000),
    )
    .expect("valid state-change audit event");

    let first = {
        let mut store = AuditStore::open(directory.path()).expect("open audit store");
        let record = store.append(&event).expect("append audit event");
        assert_eq!(record.sequence(), 1);
        assert_eq!(
            record.event_digest().0,
            "sha256:e2fbc7e3d1a0aacf5980f19afa468e960f652e3e5a530ac0455df0883956ccdd"
        );
        assert_eq!(record.event(), Some(&event));
        assert_eq!(record.previous_digest(), None);
        store
            .verify_organization(event.scope().organization_id())
            .expect("verify first audit chain");
        record
    };

    let store = AuditStore::open(directory.path()).expect("reopen audit store");
    let page = store
        .read(
            &event.scope().clone().into_access(),
            0,
            100,
            1_850_000_000_000,
        )
        .expect("read repository audit records");
    assert_eq!(page.records(), &[first]);
    assert_eq!(page.next_sequence(), None);
    store
        .verify_organization(event.scope().organization_id())
        .expect("verify audit chain after restart");
}

#[test]
fn rejected_and_failed_results_are_ordered_without_raw_sensitive_text() {
    let directory = TestDirectory::new("closed-results");
    let rejected = AuditEvent::rejected(
        AuditEventId::try_new(id("aud", 'B')).expect("canonical rejected event id"),
        1_800_000_000_010,
        AuditActor::System(SystemActorId(id("sys", 'C'))),
        repository_scope(),
        RequestId(id("req", 'D')),
        AuditAction::policy("repository.write").expect("canonical policy action"),
        AuditState::unchanged(Some(digest('c'))).expect("unchanged rejected state"),
        AuditOrigin::network(std::net::IpAddr::from_str("2001:db8::7").expect("fixed source IP")),
        AuditSubject::new().with_delivery(DeliveryId(id("dlv", 'E'))),
        "repository-write-denied",
        AuditRetention::Indefinite,
    )
    .expect("valid rejected audit event");
    let failed = AuditEvent::failed(
        AuditEventId::try_new(id("aud", 'F')).expect("canonical failed event id"),
        1_800_000_000_020,
        AuditActor::System(SystemActorId(id("sys", 'G'))),
        repository_scope(),
        RequestId(id("req", 'H')),
        AuditAction::publication("pull-request.create").expect("canonical publication action"),
        AuditState::unchanged(Some(digest('c'))).expect("unchanged failed state"),
        AuditOrigin::local("control-plane").expect("canonical local origin"),
        AuditSubject::new().with_publication(PublicationId(id("pub", 'J'))),
        "github-rate-limited",
        AuditRetention::Indefinite,
    )
    .expect("valid failed audit event");

    let mut store = AuditStore::open(directory.path()).expect("open audit store");
    let first = store.append(&rejected).expect("append rejected event");
    let second = store.append(&failed).expect("append failed event");
    assert_eq!(first.sequence(), 1);
    assert_eq!(second.sequence(), 2);
    assert_eq!(second.previous_digest(), Some(first.event_digest()));
    assert_eq!(rejected.outcome(), AuditOutcome::Rejected);
    assert_eq!(failed.outcome(), AuditOutcome::Failed);

    let unsafe_action = AuditAction::command("Bearer credential-secret")
        .expect_err("raw credential text is not a stable action token");
    assert_eq!(unsafe_action.kind(), AuditErrorKind::InvalidInput);
    let unsafe_origin = AuditOrigin::local("raw prompt text")
        .expect_err("raw prompt text is not a local component identity");
    assert_eq!(unsafe_origin.kind(), AuditErrorKind::InvalidInput);

    let database = fs::read(store.database_path()).expect("read audit database bytes");
    assert!(
        !database
            .windows(b"credential-secret".len())
            .any(|window| window == b"credential-secret")
    );
    assert!(
        !database
            .windows(b"raw prompt text".len())
            .any(|window| window == b"raw prompt text")
    );
    store
        .verify_organization(rejected.scope().organization_id())
        .expect("verify rejected and failed chain");
}

#[test]
fn retention_removes_only_expired_payloads_and_keeps_immutable_chain_headers() {
    let directory = TestDirectory::new("retention");
    let expiring = AuditEvent::failed(
        AuditEventId::try_new(id("aud", 'K')).expect("canonical expiring event id"),
        1_800_000_000_100,
        AuditActor::System(SystemActorId(id("sys", 'M'))),
        repository_scope(),
        RequestId(id("req", 'N')),
        AuditAction::worker_lease("lease.expired").expect("canonical lease action"),
        AuditState::unchanged(None).expect("unchanged lease state"),
        AuditOrigin::local("control-plane").expect("canonical local origin"),
        AuditSubject::new().with_lease(LeaseId(id("lse", 'P'))),
        "lease-expired",
        AuditRetention::UntilMillis(1_800_000_000_200),
    )
    .expect("valid expiring event");
    let indefinite = AuditEvent::state_change(
        AuditEventId::try_new(id("aud", 'Q')).expect("canonical indefinite event id"),
        1_800_000_000_110,
        AuditActor::System(SystemActorId(id("sys", 'R'))),
        repository_scope(),
        RequestId(id("req", 'S')),
        AuditAction::delivery_state("delivery.delivered").expect("canonical delivery action"),
        AuditState::changed(Some(digest('d')), digest('e')).expect("changed delivery state"),
        AuditOrigin::local("control-plane").expect("canonical local origin"),
        AuditSubject::new().with_delivery(DeliveryId(id("dlv", 'T'))),
        "completed",
        AuditRetention::Indefinite,
    )
    .expect("valid indefinite event");

    let mut store = AuditStore::open(directory.path()).expect("open audit store");
    store.append(&expiring).expect("append expiring event");
    store.append(&indefinite).expect("append indefinite event");
    assert_eq!(
        store
            .prune_expired_payloads(1_800_000_000_199)
            .expect("retention before deadline"),
        0
    );
    assert_eq!(
        store
            .prune_expired_payloads(1_800_000_000_200)
            .expect("retention at deadline"),
        1
    );

    let page = store
        .read(&repository_scope().into_access(), 0, 100, 1_800_000_000_200)
        .expect("read retained chain");
    assert_eq!(page.records().len(), 2);
    assert_eq!(page.records()[0].event(), None);
    assert_eq!(page.records()[1].event(), Some(&indefinite));
    let replay = store
        .append(&expiring)
        .expect("exact replay after retention remains the original record");
    assert_eq!(replay.sequence(), 1);
    assert_eq!(replay.event(), None);
    store
        .verify_organization(indefinite.scope().organization_id())
        .expect("expired payload does not break immutable header chain");
    assert_retention_rows_are_immutable(store.database_path());
}

#[test]
fn read_authority_filters_every_scope_level_without_cross_tenant_payloads() {
    let directory = TestDirectory::new("scope-filter");
    let organization = OrganizationId(id("org", '1'));
    let events = [
        failed_event(
            '0',
            1_800_000_001_000,
            fixture_scope(FixtureScope::Organization),
        ),
        failed_event(
            '1',
            1_800_000_001_001,
            fixture_scope(FixtureScope::WorkspaceA),
        ),
        failed_event(
            '2',
            1_800_000_001_002,
            fixture_scope(FixtureScope::ProjectA),
        ),
        failed_event(
            '3',
            1_800_000_001_003,
            fixture_scope(FixtureScope::RepositoryA),
        ),
        failed_event(
            '4',
            1_800_000_001_004,
            fixture_scope(FixtureScope::RepositoryB),
        ),
        failed_event(
            '5',
            1_800_000_001_005,
            fixture_scope(FixtureScope::ProjectB),
        ),
        failed_event(
            '6',
            1_800_000_001_006,
            fixture_scope(FixtureScope::OtherOrganization),
        ),
    ];
    let mut store = AuditStore::open(directory.path()).expect("open audit store");
    for event in &events {
        store.append(event).expect("append scoped event");
    }

    let repository_page = read_fixture_page(&store, FixtureScope::RepositoryA, 0, 100);
    assert_eq!(repository_page.records().len(), 1);
    assert_eq!(repository_page.records()[0].event(), Some(&events[3]));

    let project_page = read_fixture_page(&store, FixtureScope::ProjectA, 0, 2);
    assert_eq!(project_page.records().len(), 2);
    assert_eq!(project_page.next_sequence(), Some(4));
    let project_tail = read_fixture_page(
        &store,
        FixtureScope::ProjectA,
        project_page.next_sequence().expect("project page cursor"),
        100,
    );
    assert_eq!(project_tail.records().len(), 1);
    assert_eq!(project_tail.records()[0].event(), Some(&events[4]));

    let workspace_page = read_fixture_page(&store, FixtureScope::WorkspaceA, 0, 100);
    assert_eq!(workspace_page.records().len(), 4);

    let organization_page = read_fixture_page(&store, FixtureScope::Organization, 0, 100);
    assert_eq!(organization_page.records().len(), 6);
    assert!(organization_page.records().iter().all(|record| {
        record
            .event()
            .is_none_or(|event| event.scope().organization_id() == &organization)
    }));

    let other_page = read_fixture_page(&store, FixtureScope::OtherOrganization, 0, 100);
    assert_eq!(other_page.records().len(), 1);
    assert_eq!(other_page.records()[0].event(), Some(&events[6]));
}

#[test]
fn concurrent_exact_replay_has_one_sequence_and_changed_reuse_conflicts() {
    let directory = TestDirectory::new("concurrent-replay");
    let event = Arc::new(failed_event('7', 1_800_000_003_000, repository_scope()));
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    let stores = (0..2)
        .map(|_| AuditStore::open(directory.path()).expect("open concurrent audit store"))
        .collect::<Vec<_>>();
    for mut store in stores {
        let barrier = Arc::clone(&barrier);
        let event = Arc::clone(&event);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            store.append(&event).expect("append concurrent exact event")
        }));
    }
    barrier.wait();
    let records = handles
        .into_iter()
        .map(|handle| handle.join().expect("join concurrent audit writer"))
        .collect::<Vec<_>>();
    assert_eq!(records[0], records[1]);
    assert_eq!(records[0].sequence(), 1);

    let mut store = AuditStore::open(directory.path()).expect("reopen audit store");
    let changed = AuditEvent::failed(
        event.event_id().clone(),
        event.occurred_at_millis(),
        AuditActor::System(SystemActorId(id("sys", '0'))),
        repository_scope(),
        RequestId(id("req", '7')),
        AuditAction::policy("policy.evaluate").expect("canonical changed action"),
        AuditState::unchanged(None).expect("unchanged changed-reuse state"),
        AuditOrigin::local("control-plane").expect("canonical changed origin"),
        AuditSubject::new(),
        "different-result",
        AuditRetention::Indefinite,
    )
    .expect("valid changed-reuse event");
    let error = store
        .append(&changed)
        .expect_err("changed event-id reuse must conflict");
    assert_eq!(error.kind(), AuditErrorKind::RequestConflict);
    let page = store
        .read(&repository_scope().into_access(), 0, 100, 1_800_000_004_000)
        .expect("read one concurrent result");
    assert_eq!(page.records().len(), 1);
    assert_eq!(page.records()[0], records[0]);
}

#[test]
fn payload_or_chain_head_tampering_fails_closed() {
    let directory = TestDirectory::new("tamper");
    let event = failed_event('8', 1_800_000_005_000, repository_scope());
    let store = {
        let mut store = AuditStore::open(directory.path()).expect("open audit store");
        store.append(&event).expect("append tamper fixture");
        store
    };
    let connection = rusqlite::Connection::open(store.database_path()).expect("inspect audit db");
    connection
        .execute(
            "UPDATE audit_payloads SET payload = X'7B7D' WHERE sequence = 1",
            [],
        )
        .expect("tamper audit payload through raw SQLite");
    let error = store
        .read(&repository_scope().into_access(), 0, 100, 1_800_000_006_000)
        .expect_err("tampered payload must fail closed");
    assert_eq!(error.kind(), AuditErrorKind::Corrupt);
    drop(connection);
    drop(store);

    let directory = TestDirectory::new("head-tamper");
    let event = failed_event('9', 1_800_000_007_000, repository_scope());
    let mut store = {
        let mut store = AuditStore::open(directory.path()).expect("open head audit store");
        store.append(&event).expect("append head fixture");
        store
    };
    let connection = rusqlite::Connection::open(store.database_path()).expect("inspect head db");
    connection
        .execute(
            "UPDATE audit_chain_heads SET last_digest = \
             'sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
            [],
        )
        .expect("tamper mutable chain head through raw SQLite");
    drop(connection);
    let append_error = store
        .append(&failed_event('A', 1_800_000_007_001, repository_scope()))
        .expect_err("append must not extend a chain whose head changed");
    assert_eq!(append_error.kind(), AuditErrorKind::Corrupt);
    let connection = rusqlite::Connection::open(store.database_path()).expect("inspect head db");
    let event_count = connection
        .query_row("SELECT COUNT(*) FROM audit_events", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count headers after rejected append");
    assert_eq!(event_count, 1);
    drop(connection);
    let error = store
        .verify_organization(event.scope().organization_id())
        .expect_err("tampered chain head must fail closed");
    assert_eq!(error.kind(), AuditErrorKind::Corrupt);
}

#[test]
fn model_invocation_keeps_only_bounded_identity_usage_and_digests() {
    let directory = TestDirectory::new("model-summary");
    let summary =
        AuditModelInvocation::try_new("openai", "gpt-5.6", digest('1'), digest('2'), 12_345, 678)
            .expect("canonical model invocation summary");
    let event = AuditEvent::succeeded(
        AuditEventId::try_new(id("aud", 'A')).expect("canonical model event id"),
        1_800_000_008_000,
        AuditActor::System(SystemActorId(id("sys", 'B'))),
        repository_scope(),
        RequestId(id("req", 'C')),
        AuditAction::model_invocation(summary).expect("canonical model action"),
        AuditState::unchanged(Some(digest('3'))).expect("unchanged model state"),
        AuditOrigin::local("control-plane").expect("canonical local origin"),
        AuditSubject::new()
            .with_delivery(DeliveryId(id("dlv", 'D')))
            .with_product_session(ProductSessionId(id("psn", 'E'))),
        "completed",
        AuditRetention::UntilMillis(1_900_000_008_000),
    )
    .expect("valid model audit event");

    let encoded = serde_json::to_value(&event).expect("encode model audit event");
    let encoded_text = encoded.to_string();
    assert!(encoded_text.contains("openai"));
    assert!(encoded_text.contains("gpt-5.6"));
    assert!(encoded_text.contains(&digest('1').0));
    assert!(encoded_text.contains(&digest('2').0));
    for forbidden in [
        "prompt",
        "credential",
        "authorization",
        "secret",
        "raw_request",
        "raw_response",
    ] {
        assert!(!encoded_text.contains(forbidden));
    }

    let mut store = AuditStore::open(directory.path()).expect("open model audit store");
    store.append(&event).expect("append model audit event");
    let database = fs::read(store.database_path()).expect("read model audit database");
    assert!(
        !database
            .windows(b"fixture raw prompt".len())
            .any(|window| window == b"fixture raw prompt")
    );
    store
        .verify_organization(event.scope().organization_id())
        .expect("verify model audit chain");
}

#[test]
fn payload_deletion_without_an_immutable_retention_tombstone_is_corruption() {
    let directory = TestDirectory::new("retention-tamper");
    let event = AuditEvent::failed(
        AuditEventId::try_new(id("aud", 'C')).expect("canonical retention event id"),
        1_800_000_009_000,
        AuditActor::System(SystemActorId(id("sys", 'D'))),
        repository_scope(),
        RequestId(id("req", 'E')),
        AuditAction::worker_lease("lease.expired").expect("canonical retention action"),
        AuditState::unchanged(None).expect("unchanged retention state"),
        AuditOrigin::local("control-plane").expect("canonical retention origin"),
        AuditSubject::new().with_lease(LeaseId(id("lse", 'F'))),
        "lease-expired",
        AuditRetention::UntilMillis(1_900_000_009_000),
    )
    .expect("valid retention event");
    let store = {
        let mut store = AuditStore::open(directory.path()).expect("open audit store");
        store.append(&event).expect("append retention event");
        store
    };
    let connection = rusqlite::Connection::open(store.database_path()).expect("inspect audit db");
    connection
        .execute("DELETE FROM audit_payloads WHERE sequence = 1", [])
        .expect("simulate unauthorized payload deletion");
    let error = store
        .verify_organization(event.scope().organization_id())
        .expect_err("missing retention tombstone must fail chain verification");
    assert_eq!(error.kind(), AuditErrorKind::Corrupt);
}

#[test]
fn concurrent_distinct_events_form_one_gapless_organization_chain() {
    let directory = TestDirectory::new("concurrent-order");
    let barrier = Arc::new(Barrier::new(9));
    let mut handles = Vec::new();
    let stores = (0..8)
        .map(|_| AuditStore::open(directory.path()).expect("open concurrent audit writer"))
        .collect::<Vec<_>>();
    for ((offset, tail), mut store) in ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H']
        .into_iter()
        .enumerate()
        .zip(stores)
    {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let event = failed_event(
                tail,
                1_800_000_010_000 + u64::try_from(offset).expect("small fixture offset"),
                repository_scope(),
            );
            barrier.wait();
            store.append(&event).expect("append distinct audit event")
        }));
    }
    barrier.wait();
    let mut sequences = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("join distinct audit writer")
                .sequence()
        })
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    assert_eq!(sequences, (1_u64..=8).collect::<Vec<_>>());

    let store = AuditStore::open(directory.path()).expect("reopen ordered audit store");
    let page = store
        .read(&repository_scope().into_access(), 0, 100, 1_800_000_011_000)
        .expect("read concurrent organization chain");
    assert_eq!(page.records().len(), 8);
    for pair in page.records().windows(2) {
        assert_eq!(pair[1].sequence(), pair[0].sequence() + 1);
        assert_eq!(pair[1].previous_digest(), Some(pair[0].event_digest()));
    }
    store
        .verify_organization(repository_scope().organization_id())
        .expect("verify gapless concurrent organization chain");
}

#[test]
fn every_audit_category_and_actor_shape_uses_closed_structured_values() {
    let directory = TestDirectory::new("closed-categories");
    let actions = [
        (
            "command",
            AuditActionKind::Command,
            AuditAction::command("delivery.advance").expect("command action"),
        ),
        (
            "approval",
            AuditActionKind::Approval,
            AuditAction::approval("publication.approve").expect("approval action"),
        ),
        (
            "policy",
            AuditActionKind::Policy,
            AuditAction::policy("repository.write").expect("policy action"),
        ),
        (
            "worker_lease",
            AuditActionKind::WorkerLease,
            AuditAction::worker_lease("lease.renew").expect("lease action"),
        ),
        (
            "model_invocation",
            AuditActionKind::ModelInvocation,
            AuditAction::model_invocation(
                AuditModelInvocation::try_new(
                    "openai",
                    "gpt-5.6",
                    digest('4'),
                    digest('5'),
                    10,
                    20,
                )
                .expect("model summary"),
            )
            .expect("model action"),
        ),
        (
            "delivery_state",
            AuditActionKind::DeliveryState,
            AuditAction::delivery_state("delivery.ready").expect("delivery action"),
        ),
        (
            "publication",
            AuditActionKind::Publication,
            AuditAction::publication("pull-request.create").expect("publication action"),
        ),
    ];
    let actors = [
        AuditActor::User(UserId(id("usr", '6'))),
        AuditActor::ServiceAccount(ServiceAccountId(id("svc", '7'))),
        AuditActor::System(SystemActorId(id("sys", '8'))),
    ];
    let tails = ['J', 'K', 'M', 'N', 'P', 'Q', 'R'];
    let mut store = AuditStore::open(directory.path()).expect("open category audit store");
    for (index, ((expected_kind, expected_typed_kind, action), tail)) in
        actions.into_iter().zip(tails).enumerate()
    {
        let event = AuditEvent::succeeded(
            AuditEventId::try_new(id("aud", tail)).expect("category event id"),
            1_800_000_012_000 + u64::try_from(index).expect("small category offset"),
            actors[index % actors.len()].clone(),
            repository_scope(),
            RequestId(id("req", tail)),
            action,
            AuditState::unchanged(Some(digest('6'))).expect("unchanged category state"),
            AuditOrigin::local("control-plane").expect("category origin"),
            AuditSubject::new(),
            "completed",
            AuditRetention::Indefinite,
        )
        .expect("valid category event");
        assert_eq!(event.action().kind(), expected_typed_kind);
        let encoded = serde_json::to_value(&event).expect("encode category event");
        assert_eq!(encoded["action"]["kind"], expected_kind);
        store.append(&event).expect("append category event");
    }
    store
        .verify_organization(repository_scope().organization_id())
        .expect("verify all audit action categories");
}

#[test]
fn deserialized_unsafe_event_is_rejected_before_any_write() {
    let directory = TestDirectory::new("deserialized-invalid");
    let event = failed_event('S', 1_800_000_013_000, repository_scope());
    let mut encoded = serde_json::to_value(event).expect("encode valid audit fixture");
    encoded["result_code"] = serde_json::Value::String("raw prompt with spaces".to_owned());
    let unsafe_event: AuditEvent =
        serde_json::from_value(encoded).expect("closed shape remains deserializable");

    let mut store = AuditStore::open(directory.path()).expect("open invalid audit store");
    let error = store
        .append(&unsafe_event)
        .expect_err("deserialization must not bypass audit validation");
    assert_eq!(error.kind(), AuditErrorKind::InvalidInput);
    let page = store
        .read(&repository_scope().into_access(), 0, 100, 1_800_000_014_000)
        .expect("read empty audit store after rejected input");
    assert!(page.records().is_empty());
}
