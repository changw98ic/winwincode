use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, RepositoryScope, Scope, UserActor,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DeliveryVerdictCommitError, EventPublishError,
    EventPublisher, OutboxEvent,
};
use winwincode_delivery::{
    application::{
        attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
        verdict::{
            SubmitVerdictFacts,
            test_support::{VerdictFixture, VerdictFixtureOutcome, verdict_fixture},
        },
    },
    domain::{Delivery, DeliveryStatus, DeliveryTaskStatus},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryQueryPort, DeliveryStore, JournalBackendError,
        JournalEntryState, JournalRecordBytes, LoadedDeliveryJournal, ResolveDeliveryAttention,
    },
};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-verdict-atomic-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn fixture(seed: u64, outcome: VerdictFixtureOutcome) -> VerdictFixture {
    verdict_fixture(&DeliveryId(canonical_id("dlv", seed)), outcome)
}

fn verdict_command(seed: u64, fixture: &VerdictFixture) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: CommandName::DeliverySubmitVerdict,
        expected_revision: Revision(
            i64::try_from(fixture.delivery.revision()).expect("fixture revision range"),
        ),
        payload: serde_json::json!({
            "deliveryId": fixture.delivery.id().0,
            "candidateDigest": fixture.candidate.candidate_ref()
                .strip_prefix("git-candidate:")
                .expect("candidate digest prefix"),
        }),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(RepositoryScope {
            kind: winwincode_api::generated::RepositoryScopeKind::Repository,
            organization_id: OrganizationId(canonical_id("org", seed)),
            workspace_id: WorkspaceId(canonical_id("wsp", seed)),
            project_id: ProjectId(canonical_id("prj", seed)),
            repository_id: RepositoryId(canonical_id("rep", seed)),
        }),
    }
}

fn verdict_facts(fixture: &VerdictFixture, produced_at_millis: u64) -> SubmitVerdictFacts<'_> {
    SubmitVerdictFacts {
        expected_revision: fixture.delivery.revision(),
        candidate: &fixture.candidate,
        verification: &fixture.verification,
        evidence: &fixture.evidence,
        produced_at_millis,
    }
}

struct CapturingJournal {
    loaded: Option<LoadedDeliveryJournal>,
    publication: Mutex<Option<AtomicPublication>>,
}

impl Default for CapturingJournal {
    fn default() -> Self {
        Self {
            loaded: None,
            publication: Mutex::new(None),
        }
    }
}

impl CapturingJournal {
    fn loaded(journal: winwincode_storage::LoadedAggregateJournal) -> Self {
        Self {
            loaded: Some(LoadedDeliveryJournal {
                manifest: journal.manifest,
                records: journal
                    .records
                    .into_iter()
                    .map(|record| JournalRecordBytes {
                        sequence: record.sequence,
                        state: JournalEntryState::Published,
                        digest: record.digest,
                        bytes: record.payload,
                    })
                    .collect(),
            }),
            publication: Mutex::new(None),
        }
    }
}

impl DeliveryJournalPort for CapturingJournal {
    fn load(
        &self,
        _delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        Ok(self.loaded.clone())
    }

    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        *self.publication.lock().expect("publication lock") = Some(publication);
        Ok(())
    }
}

fn seed_delivery(root: &PathBuf, delivery: &Delivery) {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-verdict-journal".into()),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal publication");
    let publication = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("seed publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = publication
    else {
        panic!("seed must create the Delivery journal");
    };
    let publication = AggregateJournalPublication::Create {
        key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
        manifest,
        first_record: AggregateJournalRecord::new(
            first_record.sequence,
            first_record.digest,
            first_record.bytes,
        ),
    };
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    let receipt = storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"seed-actor".to_vec()).expect("seed actor"),
                    ReceiptScopeKey::from_encoded(b"seed-scope".to_vec()).expect("seed scope"),
                    RequestId("seed-verdict-state".into()),
                )
                .expect("seed identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("seed Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    "seed-verdict-event",
                    "delivery.seeded",
                    b"seed".to_vec(),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed transaction");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed event acknowledgement");
    Box::new(storage).close().expect("seed storage close");
}

fn advance_after_failed_verdict(root: &PathBuf, delivery_id: &DeliveryId) -> Delivery {
    let mut storage = SqliteStorage::open(root).expect("advance storage");
    let key = AggregateJournalKey::new("delivery", delivery_id.0.clone()).expect("journal key");
    let loaded = storage
        .load_journal(&key)
        .expect("load verdict journal")
        .expect("verdict journal");
    let capture = CapturingJournal::loaded(loaded);
    let store = DeliveryStore::borrowed(&capture);
    let failed = store
        .query(winwincode_delivery::store::DeliveryQuery::Get(
            delivery_id.clone(),
        ))
        .expect("failed verdict Delivery");
    let item = failed
        .snapshot()
        .attention_items
        .first()
        .expect("computed blocking Attention")
        .clone();
    let transition = resolve_attention(
        &failed,
        ResolveAttentionInput {
            expected_revision: failed.revision(),
            attention_item_id: item.id,
            stage_run_id: item.stage_run_id.expect("verification StageRun"),
            expected_context: item.context,
            actor: "delivery-reviewer".into(),
            decision: AttentionDecision::Resolved,
            resolution: "authorize bounded rework".into(),
            now_millis: failed.snapshot().updated_at_millis + 1,
        },
    )
    .expect("resolve computed verdict Attention");
    let resolved = store
        .execute(DeliveryCommand::ResolveAttention(Box::new(
            ResolveDeliveryAttention {
                request_id: RequestId("advance-after-verdict".into()),
                request_digest: "c".repeat(64),
                expected_revision: failed.revision(),
                transition,
            },
        )))
        .expect("append typed Attention resolution")
        .snapshot;
    let publication = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("Attention journal publication");
    let AtomicPublication::Append {
        expected_tail_sequence,
        expected_tail_digest,
        record,
        ..
    } = publication
    else {
        panic!("Attention resolution must append the Delivery journal");
    };
    let advance = StateCommit::new(
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(b"advance-actor".to_vec()).expect("advance actor"),
            ReceiptScopeKey::from_encoded(b"advance-scope".to_vec()).expect("advance scope"),
            RequestId("advance-after-verdict-state".into()),
        )
        .expect("advance identity"),
        Sha256Digest(format!("sha256:{}", "c".repeat(64))),
        format!("delivery:{}", delivery_id.0),
        failed.revision(),
        resolved.encode_json().expect("resolved Delivery JSON"),
        vec![NewOutboxEvent::internal(
            "attention-resolved-after-verdict",
            "delivery.attention.resolved",
            b"resolved".to_vec(),
        )],
    )
    .with_journal_publication(AggregateJournalPublication::Append {
        key,
        expected_tail_sequence,
        expected_tail_digest,
        record: AggregateJournalRecord::new(record.sequence, record.digest, record.bytes),
    });
    let receipt = storage.commit(&advance).expect("advance durable Delivery");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("advance event acknowledgement");
    Box::new(storage).close().expect("advance storage close");
    resolved
}

struct CapturingPublisher {
    events: Arc<Mutex<Vec<OutboxEvent>>>,
}

impl EventPublisher for CapturingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.events.lock().expect("event lock").push(event.clone());
        Ok(())
    }
}

#[test]
fn verdict_commit_is_atomic_and_replay_returns_the_original_event_once() {
    let root = temporary_directory("commit-replay");
    let fixture = fixture(1, VerdictFixtureOutcome::Fail);
    let command = verdict_command(1, &fixture);
    seed_delivery(&root, &fixture.delivery);
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");

    let first = control_plane
        .commit_delivery_verdict(&command, verdict_facts(&fixture, 1_800_000_000_100))
        .expect("atomic verdict commit");
    let replay = control_plane
        .commit_delivery_verdict(&command, verdict_facts(&fixture, 1_800_000_000_101))
        .expect("scoped verdict replay");

    assert!(!first.idempotent_replay);
    assert!(replay.idempotent_replay);
    assert_eq!(first.revision, fixture.delivery.revision() + 1);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert_eq!(first.events.len(), 2);
    assert!(first.events[0].projection_cursor.is_none());
    assert_eq!(first.events[1].topic, "delivery.changed.v1");
    assert!(first.events[1].projection_cursor.is_some());
    assert_eq!(published.lock().expect("published events").len(), 2);
    let state = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("state read")
        .expect("committed state");
    let delivery = Delivery::decode_json(&state.payload).expect("committed Delivery");
    assert_eq!(delivery.snapshot().status, DeliveryStatus::NeedsAttention);
    assert_eq!(delivery.snapshot().evidence.len(), 2);
    assert!(delivery.snapshot().verdict.is_some());
    assert_eq!(delivery.snapshot().attention_items.len(), 1);
    assert!(delivery.snapshot().attention_items[0].blocking);

    control_plane.shutdown().expect("shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let facts: (i64, i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1), \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), \
               (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.verdict.submitted')",
            [fixture.delivery.id().0.as_str(), command.request_id.0.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("atomic fact counts");
    assert_eq!(facts, (2, 1, 1));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn durable_replay_uses_the_original_verdict_after_facts_and_current_revision_change() {
    let root = temporary_directory("historical-replay");
    let original = fixture(10, VerdictFixtureOutcome::Fail);
    let command = verdict_command(10, &original);
    seed_delivery(&root, &original.delivery);
    let first_published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&first_published),
        }),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_verdict(&command, verdict_facts(&original, 1_800_000_000_100))
        .expect("initial verdict commit");
    control_plane.shutdown().expect("initial shutdown");

    let advanced = advance_after_failed_verdict(&root, original.delivery.id());
    assert_eq!(advanced.revision(), first.revision + 1);
    assert_eq!(advanced.snapshot().status, DeliveryStatus::Reworking);

    let replacement = fixture(999, VerdictFixtureOutcome::Pass);
    let replay_published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&replay_published),
        }),
    )
    .expect("replay Control Plane start");
    let replay = control_plane
        .commit_delivery_verdict(&command, verdict_facts(&replacement, 1_900_000_000_000))
        .expect("durable receipt replay must not read replacement facts");

    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert!(replay_published.lock().expect("replay events").is_empty());
    let current = control_plane
        .load_state(&format!("delivery:{}", original.delivery.id().0))
        .expect("current state read")
        .expect("current state");
    assert_eq!(current.revision, advanced.revision());
    assert_eq!(
        Delivery::decode_json(&current.payload).expect("current Delivery"),
        advanced
    );

    control_plane.shutdown().expect("replay shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let facts: (i64, i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1), \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), \
               (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.verdict.submitted')",
            [
                original.delivery.id().0.as_str(),
                command.request_id.0.as_str(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("historical replay counts");
    assert_eq!(facts, (3, 1, 1));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn durable_replay_returns_original_verdict_before_broken_state_journal_or_replacement_facts() {
    let root = temporary_directory("receipt-before-broken-facts");
    let original = fixture(11, VerdictFixtureOutcome::Fail);
    let command = verdict_command(11, &original);
    seed_delivery(&root, &original.delivery);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_verdict(&command, verdict_facts(&original, 1_800_000_000_100))
        .expect("initial verdict commit");
    control_plane.shutdown().expect("initial shutdown");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption database");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = X'00' \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [original.delivery.id().0.as_str()],
        )
        .expect("corrupt current Delivery journal");
    connection
        .execute(
            "UPDATE product_state SET payload = X'00' WHERE stream_id = ?1",
            [format!("delivery:{}", original.delivery.id().0)],
        )
        .expect("corrupt current Delivery state");
    connection.close().expect("corruption close");

    let replacement = fixture(999, VerdictFixtureOutcome::Pass);
    let replayed_publications = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&replayed_publications),
        }),
    )
    .expect("replay Control Plane start");
    let replay = control_plane
        .commit_delivery_verdict(&command, verdict_facts(&replacement, 1_900_000_000_000))
        .expect(
            "durable receipt replay must precede current state, journal, and replacement facts",
        );

    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert!(
        replayed_publications
            .lock()
            .expect("replayed publications")
            .is_empty()
    );
    control_plane.shutdown().expect("replay shutdown");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let facts: (i64, i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1), \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), \
               (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.verdict.submitted')",
            [
                original.delivery.id().0.as_str(),
                command.request_id.0.as_str(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("receipt-first replay counts");
    assert_eq!(facts, (2, 1, 1));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn passing_verdict_enters_final_manual_delivery_review_state() {
    let root = temporary_directory("passing-review");
    let fixture = fixture(2, VerdictFixtureOutcome::Pass);
    let command = verdict_command(2, &fixture);
    seed_delivery(&root, &fixture.delivery);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");

    control_plane
        .commit_delivery_verdict(&command, verdict_facts(&fixture, 1_800_000_000_100))
        .expect("passing verdict commit");
    let state = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("state read")
        .expect("committed state");
    let delivery = Delivery::decode_json(&state.payload).expect("committed Delivery");
    assert_eq!(delivery.snapshot().status, DeliveryStatus::ReadyToDeliver);
    assert!(delivery.snapshot().attention_items.is_empty());
    assert!(
        delivery
            .snapshot()
            .tasks
            .iter()
            .all(|task| task.status == DeliveryTaskStatus::Completed)
    );

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn failure_at_each_atomic_member_rolls_back_verdict_and_event() {
    let failure_points = [
        (
            "state",
            "CREATE TRIGGER fail_verdict_state BEFORE UPDATE ON product_state \
             WHEN NEW.stream_id LIKE 'delivery:%' \
             BEGIN SELECT RAISE(ABORT, 'injected verdict state failure'); END;",
        ),
        (
            "journal",
            "CREATE TRIGGER fail_verdict_journal BEFORE INSERT ON aggregate_journal_records \
             WHEN NEW.sequence = 2 \
             BEGIN SELECT RAISE(ABORT, 'injected verdict journal failure'); END;",
        ),
        (
            "receipt",
            "CREATE TRIGGER fail_verdict_receipt BEFORE INSERT ON command_receipts \
             WHEN NEW.request_id LIKE 'req_%' \
             BEGIN SELECT RAISE(ABORT, 'injected verdict receipt failure'); END;",
        ),
        (
            "outbox",
            "CREATE TRIGGER fail_verdict_outbox BEFORE INSERT ON outbox \
             WHEN NEW.topic = 'delivery.verdict.submitted' \
             BEGIN SELECT RAISE(ABORT, 'injected verdict outbox failure'); END;",
        ),
    ];

    for (offset, (member, trigger)) in failure_points.into_iter().enumerate() {
        let seed = 30 + u64::try_from(offset).expect("failure index");
        let root = temporary_directory(member);
        let fixture = fixture(seed, VerdictFixtureOutcome::Fail);
        let command = verdict_command(seed, &fixture);
        seed_delivery(&root, &fixture.delivery);
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("failure injection database");
        connection.execute_batch(trigger).expect("failure trigger");
        connection.close().expect("injector close");
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .expect("Control Plane start");

        let error = control_plane
            .commit_delivery_verdict(&command, verdict_facts(&fixture, 1_800_000_000_100))
            .expect_err("injected pre-commit failure");
        assert!(matches!(error, DeliveryVerdictCommitError::Storage(_)));
        let state = control_plane
            .load_state(&format!("delivery:{}", fixture.delivery.id().0))
            .expect("state read")
            .expect("seed state");
        assert_eq!(state.revision, fixture.delivery.revision(), "{member}");
        assert_eq!(
            state.payload,
            fixture.delivery.encode_json().expect("seed Delivery"),
            "{member}"
        );
        control_plane.shutdown().expect("shutdown");

        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("inspection database");
        let counts: (i64, i64, i64) = connection
            .query_row(
                "SELECT \
                   (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1), \
                   (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), \
                   (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.verdict.submitted')",
                [fixture.delivery.id().0.as_str(), command.request_id.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("rollback fact counts");
        assert_eq!(counts, (1, 0, 0), "{member}");
        connection.close().expect("inspection close");
        fs::remove_dir_all(root).expect("database cleanup");
    }
}
