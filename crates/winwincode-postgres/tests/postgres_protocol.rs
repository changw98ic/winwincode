// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_backup::{
    BackupCaptureCoordinator, BackupComponentKind, BackupComponentSnapshot, BackupId,
    BackupSnapshotRequest, BackupSnapshotSource, BackupSnapshotSourceError,
};
use winwincode_domain::{OrganizationId, RequestId, Sha256Digest};
use winwincode_postgres::{
    POSTGRES_SCHEMA_VERSION, PostgresCommitPlan, PostgresError, PostgresErrorKind,
    PostgresMigrationPlan, PostgresMigrationReceipt, PostgresProtocolPort, PostgresSnapshotExport,
    PostgresStorage, PostgresTransactionStage,
};
use winwincode_storage::{
    AggregateJournalKey, CommitReceipt, LoadedAggregateJournal, NewOutboxEvent, OutboxEvent,
    PendingAuditEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    StateCommit, StateMutation, StateRevisionGuard, StorageErrorKind, StoredState,
};

const COMMAND_DIGEST_A: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMAND_DIGEST_B: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BACKUP_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReceiptKey {
    scope: Vec<u8>,
    actor: Vec<u8>,
    request_id: String,
}

#[derive(Clone)]
struct FakeOutboxRow {
    scope: Vec<u8>,
    event: OutboxEvent,
    published: bool,
}

#[derive(Clone)]
struct FakeAuditRow {
    scope: Vec<u8>,
    identity: ReceiptKey,
    event: PendingAuditEvent,
    persisted: bool,
}

#[derive(Clone, Default)]
struct FakeDatabase {
    version: u64,
    plan_digest: Option<Sha256Digest>,
    schema_digest: Option<Sha256Digest>,
    states: BTreeMap<(Vec<u8>, String), StoredState>,
    receipts: BTreeMap<ReceiptKey, CommitReceipt>,
    outbox: Vec<FakeOutboxRow>,
    audit: Vec<FakeAuditRow>,
    next_sequence: u64,
    fail_migration: bool,
    fail_stage: Option<PostgresTransactionStage>,
    last_trace: Vec<PostgresTransactionStage>,
    closed: bool,
}

struct FakePostgresProtocol {
    database: Arc<Mutex<FakeDatabase>>,
}

impl FakePostgresProtocol {
    fn new(database: Arc<Mutex<FakeDatabase>>) -> Self {
        Self { database }
    }

    fn database(&self) -> Result<std::sync::MutexGuard<'_, FakeDatabase>, PostgresError> {
        self.database
            .lock()
            .map_err(|_| PostgresError::new(PostgresErrorKind::Unavailable))
    }
}

impl PostgresProtocolPort for FakePostgresProtocol {
    fn migrate(
        &mut self,
        plan: &PostgresMigrationPlan,
    ) -> Result<PostgresMigrationReceipt, PostgresError> {
        let mut database = self.database()?;
        if database.closed {
            return Err(PostgresError::new(PostgresErrorKind::Closed));
        }
        if std::mem::take(&mut database.fail_migration) {
            return Err(PostgresError::new(PostgresErrorKind::Unavailable));
        }
        if database.version != 0
            && (database.version != POSTGRES_SCHEMA_VERSION
                || database.plan_digest.as_ref() != Some(plan.digest()))
        {
            return Err(PostgresError::new(PostgresErrorKind::MigrationConflict));
        }
        let schema_digest = digest(
            &plan
                .migrations()
                .iter()
                .flat_map(|migration| migration.sql().as_bytes())
                .copied()
                .collect::<Vec<_>>(),
        );
        database.version = POSTGRES_SCHEMA_VERSION;
        database.plan_digest = Some(plan.digest().clone());
        database.schema_digest = Some(schema_digest.clone());
        PostgresMigrationReceipt::try_new(database.version, plan.digest().clone(), schema_digest)
    }

    fn commit(&mut self, plan: &PostgresCommitPlan) -> Result<CommitReceipt, PostgresError> {
        let mut database = self.database()?;
        if database.closed {
            return Err(PostgresError::new(PostgresErrorKind::Closed));
        }
        let key = receipt_key(&plan.commit().receipt_identity);
        if let Some(receipt) = database.receipts.get(&key) {
            if receipt.command_digest != plan.commit().command_digest {
                return Err(PostgresError::new(PostgresErrorKind::RequestConflict));
            }
            let mut replay = receipt.clone();
            replay.idempotent_replay = true;
            return Ok(replay);
        }
        if plan.commit().receipt_replay_required() {
            return Err(PostgresError::new(PostgresErrorKind::RequestReplayMissing));
        }
        let failure = database.fail_stage.take();
        let mut working = database.clone();
        working.fail_stage = None;
        let result = apply_commit(&mut working, plan, failure);
        match result {
            Ok(receipt) => {
                *database = working;
                Ok(receipt)
            }
            Err(error) => {
                database.last_trace = working.last_trace;
                Err(error)
            }
        }
    }

    fn load_receipt(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        identity: &ReceiptIdentity,
    ) -> Result<Option<CommitReceipt>, PostgresError> {
        require_scope(tenant_scope, identity)?;
        Ok(self
            .database()?
            .receipts
            .get(&receipt_key(identity))
            .cloned())
    }

    fn load_state(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        stream_id: &str,
    ) -> Result<Option<StoredState>, PostgresError> {
        Ok(self
            .database()?
            .states
            .get(&(tenant_scope.as_bytes().to_vec(), stream_id.to_owned()))
            .cloned())
    }

    fn load_journal(
        &mut self,
        _tenant_scope: &ReceiptScopeKey,
        _aggregate_type: &str,
        _aggregate_id: &str,
    ) -> Result<Option<LoadedAggregateJournal>, PostgresError> {
        Ok(None)
    }

    fn pending_events(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
    ) -> Result<Vec<OutboxEvent>, PostgresError> {
        Ok(self
            .database()?
            .outbox
            .iter()
            .filter(|row| row.scope == tenant_scope.as_bytes() && !row.published)
            .map(|row| row.event.clone())
            .collect())
    }

    fn mark_published(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        event_id: &str,
    ) -> Result<(), PostgresError> {
        let mut database = self.database()?;
        let Some(row) = database
            .outbox
            .iter_mut()
            .find(|row| row.scope == tenant_scope.as_bytes() && row.event.event_id == event_id)
        else {
            return Err(PostgresError::new(PostgresErrorKind::InvalidInput));
        };
        row.published = true;
        Ok(())
    }

    fn load_pending_audit_event(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        identity: &ReceiptIdentity,
    ) -> Result<Option<PendingAuditEvent>, PostgresError> {
        require_scope(tenant_scope, identity)?;
        let key = receipt_key(identity);
        Ok(self
            .database()?
            .audit
            .iter()
            .find(|row| row.identity == key)
            .map(|row| row.event.clone()))
    }

    fn pending_audit_events(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
    ) -> Result<Vec<PendingAuditEvent>, PostgresError> {
        Ok(self
            .database()?
            .audit
            .iter()
            .filter(|row| row.scope == tenant_scope.as_bytes() && !row.persisted)
            .map(|row| row.event.clone())
            .collect())
    }

    fn mark_audit_event_persisted(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        event_id: &str,
    ) -> Result<(), PostgresError> {
        let mut database = self.database()?;
        let Some(row) = database
            .audit
            .iter_mut()
            .find(|row| row.scope == tenant_scope.as_bytes() && row.event.event_id() == event_id)
        else {
            return Err(PostgresError::new(PostgresErrorKind::InvalidInput));
        };
        row.persisted = true;
        Ok(())
    }

    fn export_snapshot(
        &mut self,
        kind: BackupComponentKind,
        _scope: &AuditScope,
        consistency_cut_digest: &Sha256Digest,
    ) -> Result<PostgresSnapshotExport, PostgresError> {
        let database = self.database()?;
        let checkpoint = backup_digest(consistency_cut_digest.0.as_bytes());
        let content = backup_digest(format!("{kind:?}:{}", consistency_cut_digest.0).as_bytes());
        let (record_count, byte_count) = snapshot_counts(&database, kind);
        PostgresSnapshotExport::try_new(checkpoint, content, record_count, byte_count)
    }

    fn close(&mut self) -> Result<(), PostgresError> {
        self.database()?.closed = true;
        Ok(())
    }
}

fn apply_commit(
    database: &mut FakeDatabase,
    plan: &PostgresCommitPlan,
    failure: Option<PostgresTransactionStage>,
) -> Result<CommitReceipt, PostgresError> {
    database.last_trace.clear();
    for stage in PostgresCommitPlan::stages() {
        database.last_trace.push(stage);
        if failure == Some(stage) {
            return Err(PostgresError::new(PostgresErrorKind::Unavailable));
        }
        apply_stage(database, plan, stage)?;
    }
    database
        .receipts
        .get(&receipt_key(&plan.commit().receipt_identity))
        .cloned()
        .ok_or_else(|| PostgresError::new(PostgresErrorKind::CorruptData))
}

fn apply_stage(
    database: &mut FakeDatabase,
    plan: &PostgresCommitPlan,
    stage: PostgresTransactionStage,
) -> Result<(), PostgresError> {
    let commit = plan.commit();
    match stage {
        PostgresTransactionStage::ReceiptLookup
        | PostgresTransactionStage::AggregateJournal
        | PostgresTransactionStage::Commit => Ok(()),
        PostgresTransactionStage::RevisionGuards => validate_revisions(database, plan),
        PostgresTransactionStage::CanonicalState => {
            write_states(database, plan);
            Ok(())
        }
        PostgresTransactionStage::CommandReceipt => {
            database.receipts.insert(
                receipt_key(&commit.receipt_identity),
                CommitReceipt {
                    receipt_identity: commit.receipt_identity.clone(),
                    command_digest: commit.command_digest.clone(),
                    stream_id: commit.stream_id.clone(),
                    revision: commit.expected_revision + 1,
                    events: Vec::new(),
                    idempotent_replay: false,
                },
            );
            Ok(())
        }
        PostgresTransactionStage::AuditOutbox => {
            if let Some(event) = commit.pending_audit_event() {
                database.audit.push(FakeAuditRow {
                    scope: plan.tenant_scope().as_bytes().to_vec(),
                    identity: receipt_key(&commit.receipt_identity),
                    event: event.clone(),
                    persisted: false,
                });
            }
            Ok(())
        }
        PostgresTransactionStage::PublicOutbox => append_outbox(database, plan),
    }
}

fn validate_revisions(
    database: &FakeDatabase,
    plan: &PostgresCommitPlan,
) -> Result<(), PostgresError> {
    let commit = plan.commit();
    let scope = plan.tenant_scope().as_bytes();
    let actual = revision(database, scope, &commit.stream_id);
    if actual != commit.expected_revision {
        return Err(PostgresError::revision_conflict(actual));
    }
    for guard in commit.state_guards() {
        let actual = revision(database, scope, guard.stream_id());
        if actual != guard.expected_revision() {
            return Err(PostgresError::revision_conflict(actual));
        }
    }
    for mutation in commit.state_mutations() {
        let actual = revision(database, scope, mutation.stream_id());
        if actual != mutation.expected_revision() {
            return Err(PostgresError::revision_conflict(actual));
        }
    }
    Ok(())
}

fn write_states(database: &mut FakeDatabase, plan: &PostgresCommitPlan) {
    let commit = plan.commit();
    let scope = plan.tenant_scope().as_bytes().to_vec();
    database.states.insert(
        (scope.clone(), commit.stream_id.clone()),
        StoredState {
            stream_id: commit.stream_id.clone(),
            revision: commit.expected_revision + 1,
            payload: commit.state.clone(),
        },
    );
    for mutation in commit.state_mutations() {
        database.states.insert(
            (scope.clone(), mutation.stream_id().to_owned()),
            StoredState {
                stream_id: mutation.stream_id().to_owned(),
                revision: mutation.expected_revision() + 1,
                payload: mutation.state().to_vec(),
            },
        );
    }
}

fn append_outbox(
    database: &mut FakeDatabase,
    plan: &PostgresCommitPlan,
) -> Result<(), PostgresError> {
    let commit = plan.commit();
    let mut receipt_events = Vec::with_capacity(commit.events.len());
    for event in &commit.events {
        if event.projection_stream().is_some() || event.public_context().is_some() {
            return Err(PostgresError::new(PostgresErrorKind::Unavailable));
        }
        database.next_sequence += 1;
        let stored = OutboxEvent {
            sequence: database.next_sequence,
            event_id: event.event_id.clone(),
            topic: event.topic.clone(),
            payload: event.payload.clone(),
            projection_cursor: None,
            public_context: None,
        };
        receipt_events.push(stored.clone());
        database.outbox.push(FakeOutboxRow {
            scope: plan.tenant_scope().as_bytes().to_vec(),
            event: stored,
            published: false,
        });
    }
    let receipt = database
        .receipts
        .get_mut(&receipt_key(&commit.receipt_identity))
        .ok_or_else(|| PostgresError::new(PostgresErrorKind::CorruptData))?;
    receipt.events = receipt_events;
    Ok(())
}

fn revision(database: &FakeDatabase, scope: &[u8], stream_id: &str) -> u64 {
    database
        .states
        .get(&(scope.to_vec(), stream_id.to_owned()))
        .map_or(0, |state| state.revision)
}

fn receipt_key(identity: &ReceiptIdentity) -> ReceiptKey {
    ReceiptKey {
        scope: identity.scope_key().as_bytes().to_vec(),
        actor: identity.actor_key().as_bytes().to_vec(),
        request_id: identity.request_id().0.clone(),
    }
}

fn require_scope(
    tenant_scope: &ReceiptScopeKey,
    identity: &ReceiptIdentity,
) -> Result<(), PostgresError> {
    if tenant_scope != identity.scope_key() {
        return Err(PostgresError::new(PostgresErrorKind::InvalidInput));
    }
    Ok(())
}

fn snapshot_counts(database: &FakeDatabase, kind: BackupComponentKind) -> (u64, u64) {
    match kind {
        BackupComponentKind::DeliveryState
        | BackupComponentKind::LeaseRegistry
        | BackupComponentKind::UsageLedger
        | BackupComponentKind::ReferenceCatalog
        | BackupComponentKind::SecretReferences => {
            let count = database.states.len() as u64;
            let bytes = database
                .states
                .values()
                .map(|state| state.payload.len() as u64)
                .sum();
            (count, bytes)
        }
        BackupComponentKind::AuditLedger => {
            let count = database.audit.len() as u64;
            let bytes = database
                .audit
                .iter()
                .map(|row| row.event.payload().len() as u64)
                .sum();
            (count, bytes)
        }
        BackupComponentKind::ArtifactObjects => (0, 0),
    }
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    Sha256Digest(value)
}

fn backup_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", digest(bytes).0))
}

fn scope(value: u8) -> ReceiptScopeKey {
    ReceiptScopeKey::from_encoded(vec![value]).expect("scope")
}

fn identity(scope: &ReceiptScopeKey, actor: u8, request: &str) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(vec![actor]).expect("actor"),
        scope.clone(),
        RequestId(request.to_owned()),
    )
    .expect("identity")
}

fn commit(
    scope: &ReceiptScopeKey,
    actor: u8,
    request: &str,
    stream: &str,
    expected_revision: u64,
    state: &[u8],
    event_id: &str,
) -> StateCommit {
    StateCommit::new(
        identity(scope, actor, request),
        Sha256Digest(COMMAND_DIGEST_A.to_owned()),
        stream,
        expected_revision,
        state,
        vec![NewOutboxEvent::internal(event_id, "state.changed", state)],
    )
}

fn open(
    database: &Arc<Mutex<FakeDatabase>>,
    scope: &ReceiptScopeKey,
) -> PostgresStorage<FakePostgresProtocol> {
    PostgresStorage::try_open(
        FakePostgresProtocol::new(Arc::clone(database)),
        scope.clone(),
    )
    .expect("open PostgreSQL protocol")
}

#[test]
fn migration_plan_is_postgres_specific_atomic_and_tenant_forced() {
    let plan = PostgresMigrationPlan::current().expect("plan");
    assert_eq!(plan.migrations().len(), 1);
    assert_eq!(plan.migrations()[0].version(), POSTGRES_SCHEMA_VERSION);
    let sql = plan.migrations()[0].sql();
    assert!(sql.contains("pg_advisory_xact_lock"));
    assert!(sql.contains("GENERATED ALWAYS AS IDENTITY"));
    assert!(sql.contains("FORCE ROW LEVEL SECURITY"));
    assert!(sql.contains("current_setting('winwincode.scope_key', true)"));
    assert!(!sql.to_ascii_lowercase().contains("sqlite"));

    let database = Arc::new(Mutex::new(FakeDatabase {
        fail_migration: true,
        ..FakeDatabase::default()
    }));
    let error =
        PostgresStorage::try_open(FakePostgresProtocol::new(Arc::clone(&database)), scope(1))
            .expect_err("migration failure");
    assert_eq!(error.kind(), PostgresErrorKind::Unavailable);
    assert_eq!(database.lock().expect("database").version, 0);

    let _storage = open(&database, &scope(1));
    let durable = database.lock().expect("database");
    assert_eq!(durable.version, POSTGRES_SCHEMA_VERSION);
    assert_eq!(durable.plan_digest.as_ref(), Some(plan.digest()));
}

#[test]
fn transaction_failures_rollback_every_member_and_exact_replay_is_original() {
    let database = Arc::new(Mutex::new(FakeDatabase::default()));
    let tenant = scope(1);
    let mut storage = open(&database, &tenant);
    for (index, stage) in PostgresCommitPlan::stages().into_iter().enumerate() {
        database.lock().expect("database").fail_stage = Some(stage);
        let failed = commit(
            &tenant,
            1,
            &format!("req-fail-{index}"),
            &format!("stream-fail-{index}"),
            0,
            b"not-durable",
            &format!("evt-fail-{index}"),
        )
        .with_pending_audit_event(
            PendingAuditEvent::new(format!("audit-fail-{index}"), b"audit").expect("audit"),
        );
        assert_eq!(
            storage
                .commit(&failed)
                .expect_err("injected failure")
                .kind(),
            StorageErrorKind::Adapter
        );
        let durable = database.lock().expect("database");
        assert!(durable.states.is_empty());
        assert!(durable.receipts.is_empty());
        assert!(durable.outbox.is_empty());
        assert!(durable.audit.is_empty());
    }

    let accepted = commit(&tenant, 1, "req-main", "stream-main", 0, b"v1", "evt-main")
        .with_state_guard(StateRevisionGuard::new("guard", 0).expect("guard"))
        .with_state_mutation(StateMutation::new("secondary", 0, b"secondary").expect("mutation"))
        .with_pending_audit_event(
            PendingAuditEvent::new("audit-main", b"audit-body").expect("audit"),
        );
    let receipt = storage.commit(&accepted).expect("commit");
    assert!(!receipt.idempotent_replay);
    assert_eq!(receipt.events[0].payload, b"v1");
    let replay = storage.commit(&accepted).expect("replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.events, receipt.events);
    assert_eq!(
        storage.pending_audit_events().expect("audit")[0].payload(),
        b"audit-body"
    );
    assert_eq!(
        storage
            .load_state("secondary")
            .expect("state")
            .expect("row")
            .revision,
        1
    );

    let mut changed = accepted.clone();
    changed.command_digest = Sha256Digest(COMMAND_DIGEST_B.to_owned());
    assert_eq!(
        storage
            .commit(&changed)
            .expect_err("changed request")
            .kind(),
        StorageErrorKind::RequestConflict
    );
    let stale = commit(
        &tenant,
        1,
        "req-stale",
        "stream-main",
        0,
        b"stale",
        "evt-stale",
    );
    assert_eq!(
        storage.commit(&stale).expect_err("stale revision").kind(),
        StorageErrorKind::RevisionConflict
    );
}

#[test]
fn tenant_isolation_restart_outbox_and_backup_use_one_durable_cut() {
    let database = Arc::new(Mutex::new(FakeDatabase::default()));
    let tenant_a = scope(1);
    let tenant_b = scope(2);
    let mut storage_a = open(&database, &tenant_a);
    let mut storage_b = open(&database, &tenant_b);
    storage_a
        .commit(&commit(
            &tenant_a,
            1,
            "same-request",
            "same-stream",
            0,
            b"tenant-a",
            "event-a",
        ))
        .expect("tenant A");
    storage_b
        .commit(&commit(
            &tenant_b,
            1,
            "same-request",
            "same-stream",
            0,
            b"tenant-b",
            "event-b",
        ))
        .expect("tenant B");
    assert_eq!(
        storage_a
            .load_state("same-stream")
            .expect("A")
            .expect("A row")
            .payload,
        b"tenant-a"
    );
    assert_eq!(
        storage_b
            .load_state("same-stream")
            .expect("B")
            .expect("B row")
            .payload,
        b"tenant-b"
    );
    let foreign = commit(
        &tenant_b,
        1,
        "foreign",
        "foreign",
        0,
        b"foreign",
        "foreign-event",
    );
    assert_eq!(
        storage_a
            .commit(&foreign)
            .expect_err("foreign scope")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    drop(storage_a);
    let mut restarted = open(&database, &tenant_a);
    assert_eq!(restarted.pending_events().expect("pending").len(), 1);
    restarted.mark_published("event-a").expect("publish");
    restarted.mark_published("event-a").expect("publish replay");
    assert!(restarted.pending_events().expect("empty").is_empty());

    let mut sources = postgres_sources(&restarted);
    let mut object = StaticObjectSource;
    let mut source_refs: Vec<&mut dyn BackupSnapshotSource> = sources
        .iter_mut()
        .map(|source| source as &mut dyn BackupSnapshotSource)
        .collect();
    source_refs.push(&mut object);
    let manifest = BackupCaptureCoordinator::capture(
        BackupId::try_new("bkp_01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("backup id"),
        AuditScope::organization(OrganizationId("org_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()))
            .expect("audit scope"),
        "cn-north-1",
        1,
        Sha256Digest(BACKUP_DIGEST.to_owned()),
        &mut source_refs,
    )
    .expect("manifest");
    assert_eq!(manifest.components().len(), 7);
    assert_eq!(
        manifest
            .components()
            .iter()
            .filter(|component| component.kind() == BackupComponentKind::ArtifactObjects)
            .count(),
        1
    );
    let diagnostics = format!("{restarted:?} {manifest:?}");
    assert!(!diagnostics.contains("postgres://"));
    assert!(!diagnostics.contains("password"));
}

fn postgres_sources(
    storage: &PostgresStorage<FakePostgresProtocol>,
) -> Vec<winwincode_postgres::PostgresBackupSnapshotSource<FakePostgresProtocol>> {
    [
        BackupComponentKind::DeliveryState,
        BackupComponentKind::AuditLedger,
        BackupComponentKind::LeaseRegistry,
        BackupComponentKind::UsageLedger,
        BackupComponentKind::ReferenceCatalog,
        BackupComponentKind::SecretReferences,
    ]
    .into_iter()
    .map(|kind| storage.backup_source(kind).expect("source"))
    .collect()
}

struct StaticObjectSource;

impl BackupSnapshotSource for StaticObjectSource {
    fn kind(&self) -> BackupComponentKind {
        BackupComponentKind::ArtifactObjects
    }

    fn snapshot(
        &mut self,
        request: &BackupSnapshotRequest,
    ) -> Result<BackupComponentSnapshot, BackupSnapshotSourceError> {
        BackupComponentSnapshot::try_new(
            BackupComponentKind::ArtifactObjects,
            request.scope().clone(),
            request.consistency_cut_digest().clone(),
            backup_digest(request.consistency_cut_digest().0.as_bytes()),
            backup_digest(b"objects"),
            0,
            0,
        )
        .map_err(|_| BackupSnapshotSourceError::new())
    }
}

#[test]
fn aggregate_journal_key_remains_the_canonical_storage_type() {
    let key = AggregateJournalKey::new("delivery", "del_01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .expect("journal key");
    assert_eq!(key.aggregate_type(), "delivery");
}
