// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use sha2::{Digest as _, Sha256};
use winwincode_change_batch::{
    ChangeBatchMutationStatus, ChangeBatchPolicy, LocalNoFollowFileSystem, derive_delta_digest,
    execute_prepared_change_batch, prepare_change_batch,
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId,
    RepositoryId, SessionIdentity, Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::validation_config::resolve_validation_profile;
use winwincode_execution_port::{
    change_batch_identity::derive_change_batch_id,
    generated::{
        AppliedFileOperation, AppliedFileSummary, ArtifactReference, ChangeBatchIdentity,
        ChangeBatchProgressEvent, ChangeBatchProgressState, ChangeBatchProposal,
        ChangeBatchProposalDisposition, ChangeBatchProposalEvent, ChangeBatchReceipt,
        ChangeBatchReceiptStatus, NormalizerReceipt, NormalizerReceiptStatus,
        ValidationCheckStatus, ValidationCheckSummary, ValidationProfileName,
        ValidationProfileSelection, ValidationProfileSelectionReasonCode, ValidationReceipt,
        ValidationReceiptStatus, ValidationSelectionSource,
    },
};
use winwincode_worker::change_batch_journal::{
    ActiveBatchState, ChangeBatchExecutionPhase, ChangeBatchJournal, ChangeBatchJournalErrorCode,
    ChangeBatchRecoveryState, FileStateFingerprint, JournalRetention,
    MAX_CHANGE_BATCH_PREIMAGE_BYTES, ObservationGateResult, RollbackPreimage,
};
use winwincode_worker::workspace_phase::{PhaseProcessReceipt, PhaseProcessStatus};

static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const BASE_TREE: &str = "git-tree:0123456789abcdef0123456789abcdef01234567";
const RESULT_TREE: &str = "git-tree:1123456789abcdef0123456789abcdef01234567";

fn revision(value: &str) -> WorkspaceRevision {
    WorkspaceRevision(value.to_owned())
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-change-batch-journal-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create journal fixture");
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

fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn plan_digest(event: &ChangeBatchProposalEvent) -> Sha256Digest {
    prepare_change_batch(event, ChangeBatchPolicy::default())
        .expect("prepare fixture plan")
        .plan_digest()
        .clone()
}

fn proposal_event() -> ChangeBatchProposalEvent {
    let patch = "*** Begin Patch\n*** Add File: delegated.txt\n+fixture\n*** End Patch\n";
    let patch_digest = digest(patch.as_bytes());
    let run_key = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let turn_id = "turn-1";
    let batch_id = derive_change_batch_id(run_key, turn_id, None, &patch_digest)
        .expect("derive batch fixture");
    ChangeBatchProposalEvent {
        identity: ChangeBatchIdentity {
            attempt: 1,
            batch_id,
            call_id: None,
            fencing_token: FencingToken("1".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
            lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
            patch_digest,
            repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
            run_key: run_key.to_owned(),
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
                product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
                stage_run_id: None,
                worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
            },
            turn_id: turn_id.to_owned(),
            workspace_revision: revision(BASE_TREE),
        },
        occurred_at: Instant("2026-09-01T00:00:00.000Z".to_owned()),
        proposal: ChangeBatchProposal {
            acceptance_criteria_ids: vec!["criterion-1".to_owned()],
            disposition: ChangeBatchProposalDisposition::Final,
            patch: patch.to_owned(),
            schema_version: 1,
            validation_profile: ValidationProfileName::Changed,
        },
    }
}

fn update_proposal_event() -> ChangeBatchProposalEvent {
    let mut event = proposal_event();
    let patch =
        "*** Begin Patch\n*** Update File: delegated.txt\n@@\n-before\n+fixture\n*** End Patch\n";
    let patch_digest = digest(patch.as_bytes());
    event.identity.batch_id = derive_change_batch_id(
        &event.identity.run_key,
        &event.identity.turn_id,
        None,
        &patch_digest,
    )
    .expect("derive update batch fixture");
    event.identity.patch_digest = patch_digest;
    patch.clone_into(&mut event.proposal.patch);
    event
}

fn now(second: u8) -> Instant {
    Instant(format!("2026-09-01T00:00:{second:02}.000Z"))
}

fn existing_preimage(before: &[u8], after: &[u8]) -> RollbackPreimage {
    RollbackPreimage {
        ordinal: 0,
        path: "delegated.txt".to_owned(),
        operation: "update".to_owned(),
        before_bytes: Some(before.to_vec()),
        before_mode: Some("100644".to_owned()),
        after: FileStateFingerprint {
            exists: true,
            digest: Some(digest(after)),
            mode: Some("100644".to_owned()),
        },
    }
}

fn progress(
    identity: &ChangeBatchIdentity,
    sequence: i64,
    state: ChangeBatchProgressState,
) -> ChangeBatchProgressEvent {
    ChangeBatchProgressEvent {
        artifact_refs: Vec::new(),
        identity: identity.clone(),
        occurred_at: now(u8::try_from(sequence).expect("small sequence")),
        sequence,
        state,
        summary: "bounded ChangeBatch progress".to_owned(),
    }
}

fn applied_file() -> AppliedFileSummary {
    AppliedFileSummary {
        after_sha256: Some(digest(b"fixture\n")),
        before_sha256: None,
        bytes_after: 8,
        bytes_before: 0,
        mode_after: Some("644".to_owned()),
        mode_before: None,
        move_path: None,
        operation: AppliedFileOperation::Create,
        path: "delegated.txt".to_owned(),
    }
}

#[test]
fn intent_is_compare_and_set_and_rejects_authority_tampering() {
    let root = TestDirectory::new("intent");
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    let event = proposal_event();
    let plan = plan_digest(&event);
    assert_eq!(
        journal
            .retain_intent(&event, &revision(BASE_TREE), &plan, &now(0))
            .unwrap(),
        JournalRetention::Inserted
    );
    assert_eq!(
        journal
            .retain_intent(&event, &revision(BASE_TREE), &plan, &now(0))
            .unwrap(),
        JournalRetention::Replay
    );

    let mut changed = event.clone();
    changed.identity.session_identity.worker_session_id =
        WorkerSessionId("wsn_11111111111111111111111111".to_owned());
    assert_eq!(
        journal
            .retain_intent(&changed, &revision(BASE_TREE), &plan, &now(0))
            .unwrap_err()
            .code(),
        ChangeBatchJournalErrorCode::Conflict
    );
    assert_eq!(
        journal
            .retain_intent(
                &event,
                &revision(BASE_TREE),
                &digest(b"foreign plan"),
                &now(0)
            )
            .unwrap_err()
            .code(),
        ChangeBatchJournalErrorCode::Invalid
    );
    assert_eq!(
        journal
            .retain_intent(&event, &revision(RESULT_TREE), &plan, &now(0),)
            .unwrap_err()
            .code(),
        ChangeBatchJournalErrorCode::Conflict
    );
}

#[test]
fn preimages_are_durable_idempotent_and_bounded_before_apply() {
    let root = TestDirectory::new("preimages");
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    let event = proposal_event();
    journal
        .retain_intent(&event, &revision(BASE_TREE), &plan_digest(&event), &now(0))
        .unwrap();
    let preimage = existing_preimage(b"before\n", b"after\n");
    assert_eq!(
        journal
            .retain_preimages(
                &event.identity.batch_id,
                std::slice::from_ref(&preimage),
                &now(1),
            )
            .unwrap(),
        JournalRetention::Inserted
    );
    assert_eq!(
        journal
            .retain_preimages(&event.identity.batch_id, &[preimage], &now(1))
            .unwrap(),
        JournalRetention::Replay
    );
    assert_eq!(
        journal.read_preimage(&event.identity.batch_id, 0).unwrap(),
        b"before\n"
    );
    assert_eq!(
        journal
            .load(&event.identity.batch_id)
            .unwrap()
            .unwrap()
            .phase,
        ChangeBatchExecutionPhase::PreimagesReady
    );

    let overflow_root = TestDirectory::new("capacity");
    let mut overflow = ChangeBatchJournal::open(overflow_root.path()).expect("open overflow");
    overflow
        .retain_intent(&event, &revision(BASE_TREE), &plan_digest(&event), &now(0))
        .unwrap();
    let too_large = RollbackPreimage {
        before_bytes: Some(vec![
            0_u8;
            usize::try_from(MAX_CHANGE_BATCH_PREIMAGE_BYTES + 1)
                .expect("fixture fits usize")
        ]),
        ..existing_preimage(b"before\n", b"after\n")
    };
    assert_eq!(
        overflow
            .retain_preimages(&event.identity.batch_id, &[too_large], &now(1))
            .unwrap_err()
            .code(),
        ChangeBatchJournalErrorCode::Capacity
    );
    assert_eq!(
        overflow
            .load(&event.identity.batch_id)
            .unwrap()
            .unwrap()
            .phase,
        ChangeBatchExecutionPhase::IntentRetained
    );
}

#[tokio::test]
async fn canonical_mutation_port_persists_preimages_before_the_real_write() {
    let root = TestDirectory::new("mutation-port");
    let workspace = root.path().join("workspace");
    let journal_root = root.path().join("journal");
    fs::create_dir(&workspace).expect("create mutation workspace");
    fs::write(workspace.join("delegated.txt"), b"before\n").expect("write mutation preimage");
    let mut journal = ChangeBatchJournal::open(&journal_root).expect("open journal");
    let event = update_proposal_event();
    let plan = prepare_change_batch(&event, ChangeBatchPolicy::default()).expect("prepare plan");
    journal
        .retain_intent(&event, &revision(BASE_TREE), plan.plan_digest(), &now(0))
        .expect("retain intent");

    let outcome = execute_prepared_change_batch(
        &plan,
        &workspace,
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("execute exact local mutation");

    assert_eq!(outcome.status(), ChangeBatchMutationStatus::Applied);
    assert_eq!(
        fs::read(workspace.join("delegated.txt")).expect("read applied file"),
        b"fixture\n"
    );
    let blobs = fs::read_dir(journal_root.join("preimages"))
        .expect("read preimage blobs")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect preimage blobs");
    assert_eq!(blobs.len(), 1);
    drop(journal);
    let journal = ChangeBatchJournal::open(&journal_root).expect("restart journal");
    assert_eq!(
        journal
            .load(&event.identity.batch_id)
            .expect("load execution")
            .expect("execution exists")
            .phase,
        ChangeBatchExecutionPhase::PreimagesReady
    );
    fs::remove_file(blobs[0].path()).expect("remove retained blob");
    assert_eq!(
        journal
            .load(&event.identity.batch_id)
            .expect_err("missing blob must fail closed")
            .code(),
        ChangeBatchJournalErrorCode::Corrupt
    );
}

fn applying_journal(label: &str) -> (TestDirectory, ChangeBatchJournal, ChangeBatchProposalEvent) {
    let root = TestDirectory::new(label);
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    let event = proposal_event();
    journal
        .retain_intent(&event, &revision(BASE_TREE), &plan_digest(&event), &now(0))
        .unwrap();
    journal
        .retain_preimages(
            &event.identity.batch_id,
            &[existing_preimage(b"before\n", b"after\n")],
            &now(1),
        )
        .unwrap();
    journal
        .transition(
            &event.identity.batch_id,
            ChangeBatchExecutionPhase::PreimagesReady,
            ChangeBatchExecutionPhase::Applying,
            &now(2),
        )
        .unwrap();
    (root, journal, event)
}

#[test]
fn interrupted_operation_uses_before_after_other_recovery_split() {
    let (_before_root, mut before, event) = applying_journal("before");
    let before_state = FileStateFingerprint {
        exists: true,
        digest: Some(digest(b"before\n")),
        mode: Some("100644".to_owned()),
    };
    assert_eq!(
        before
            .reconcile_interrupted_operation(&event.identity.batch_id, 0, &before_state, &now(3))
            .unwrap(),
        ChangeBatchRecoveryState::Before
    );
    assert_eq!(
        before
            .load(&event.identity.batch_id)
            .unwrap()
            .unwrap()
            .next_operation,
        0
    );

    let (after_root, after, event) = applying_journal("after");
    drop(after);
    let mut after = ChangeBatchJournal::open(after_root.path()).expect("restart applying journal");
    let after_state = FileStateFingerprint {
        exists: true,
        digest: Some(digest(b"after\n")),
        mode: Some("100644".to_owned()),
    };
    assert_eq!(
        after
            .reconcile_interrupted_operation(&event.identity.batch_id, 0, &after_state, &now(3))
            .unwrap(),
        ChangeBatchRecoveryState::After
    );
    assert_eq!(
        after
            .load(&event.identity.batch_id)
            .unwrap()
            .unwrap()
            .next_operation,
        1
    );
    assert_eq!(
        after
            .read_preimage(&event.identity.batch_id, 0)
            .expect("restart reads exact preimage"),
        b"before\n"
    );

    let (_other_root, mut other, event) = applying_journal("other");
    let other_state = FileStateFingerprint {
        exists: true,
        digest: Some(digest(b"foreign\n")),
        mode: Some("100644".to_owned()),
    };
    assert_eq!(
        other
            .reconcile_interrupted_operation(&event.identity.batch_id, 0, &other_state, &now(3))
            .unwrap(),
        ChangeBatchRecoveryState::Other
    );
    assert_eq!(
        other.load(&event.identity.batch_id).unwrap().unwrap().phase,
        ChangeBatchExecutionPhase::RollbackRequired
    );
}

#[test]
fn progress_and_receipt_outboxes_replay_until_exact_acknowledgement() {
    let root = TestDirectory::new("outbox");
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    let event = proposal_event();
    journal
        .retain_intent(&event, &revision(BASE_TREE), &plan_digest(&event), &now(0))
        .unwrap();
    let proposed = progress(&event.identity, 1, ChangeBatchProgressState::Proposed);
    let authorized = progress(&event.identity, 2, ChangeBatchProgressState::Authorized);
    assert_eq!(
        journal.append_progress(&proposed).unwrap(),
        JournalRetention::Inserted
    );
    assert_eq!(
        journal.append_progress(&proposed).unwrap(),
        JournalRetention::Replay
    );
    assert_eq!(
        journal.append_progress(&authorized).unwrap(),
        JournalRetention::Inserted
    );
    let skipped = progress(&event.identity, 4, ChangeBatchProgressState::Applied);
    assert_eq!(
        journal.append_progress(&skipped).unwrap_err().code(),
        ChangeBatchJournalErrorCode::Conflict
    );
    drop(journal);
    let mut journal = ChangeBatchJournal::open(root.path()).expect("restart outbox journal");
    assert_eq!(
        journal.pending_progress(&event.identity.batch_id).unwrap(),
        vec![proposed.clone(), authorized]
    );
    journal
        .acknowledge_progress(&event.identity.batch_id, proposed.sequence)
        .unwrap();
    assert_eq!(
        journal
            .pending_progress(&event.identity.batch_id)
            .unwrap()
            .len(),
        1
    );

    let apply_started = progress(&event.identity, 3, ChangeBatchProgressState::ApplyStarted);
    journal.append_progress(&apply_started).unwrap();
    let receipt = ChangeBatchReceipt {
        artifact_ref: None,
        base_revision: revision(BASE_TREE),
        delta_digest: Some(derive_delta_digest(&[applied_file()]).expect("exact delta")),
        delta_exact: true,
        files: vec![applied_file()],
        identity: event.identity.clone(),
        normalizer: None,
        observation: None,
        result_revision: Some(revision(RESULT_TREE)),
        status: ChangeBatchReceiptStatus::Applied,
        validation: None,
    };
    let applied = progress(&event.identity, 4, ChangeBatchProgressState::Applied);
    assert_eq!(
        journal
            .retain_final_progress_and_receipt(&applied, &receipt, &now(4))
            .unwrap(),
        JournalRetention::Inserted
    );
    assert_eq!(
        journal
            .retain_final_progress_and_receipt(&applied, &receipt, &now(4))
            .unwrap(),
        JournalRetention::Replay
    );
    let mut changed_receipt = receipt.clone();
    changed_receipt.artifact_ref = Some(ArtifactReference {
        artifact_id: ArtifactId("art_11111111111111111111111111".to_owned()),
        digest: digest(b"changed receipt evidence"),
    });
    assert_eq!(
        journal
            .retain_final_progress_and_receipt(&applied, &changed_receipt, &now(4))
            .unwrap_err()
            .code(),
        ChangeBatchJournalErrorCode::Conflict
    );
    assert_eq!(
        journal.pending_receipt(&event.identity.batch_id).unwrap(),
        Some(receipt.clone())
    );
    journal
        .acknowledge_receipt(&event.identity.batch_id)
        .unwrap();
    assert_eq!(
        journal.pending_receipt(&event.identity.batch_id).unwrap(),
        None
    );
    assert_eq!(
        journal
            .load(&event.identity.batch_id)
            .unwrap()
            .unwrap()
            .receipt,
        Some(receipt)
    );
}

#[test]
fn schema_v1_v2_and_v3_migrate_once_and_v4_open_does_not_repair_missing_tables() {
    let migrated_root = TestDirectory::new("schema-v1");
    drop(ChangeBatchJournal::open(migrated_root.path()).expect("create journal schema"));
    let database = migrated_root.path().join("change-batch.sqlite3");
    let connection = Connection::open(&database).expect("open migration fixture");
    connection
        .execute_batch(
            "DROP TABLE change_batch_workspace_barrier;
             PRAGMA user_version = 1;",
        )
        .expect("downgrade fixture to v1");
    drop(connection);

    drop(ChangeBatchJournal::open(migrated_root.path()).expect("migrate v1 journal"));
    let connection = Connection::open(&database).expect("inspect migrated journal");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 4);
    let barrier_exists: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'change_batch_workspace_barrier'",
            [],
            |row| row.get(0),
        )
        .expect("read migrated barrier table");
    assert_eq!(barrier_exists, 1);
    drop(connection);

    let v2_root = TestDirectory::new("schema-v2");
    drop(ChangeBatchJournal::open(v2_root.path()).expect("create v4 journal"));
    let v2_database = v2_root.path().join("change-batch.sqlite3");
    let connection = Connection::open(&v2_database).expect("open v2 fixture");
    connection
        .execute_batch(
            "DROP TABLE change_batch_phase_command;
             DROP TABLE change_batch_phase_run;
             PRAGMA user_version = 2;",
        )
        .expect("downgrade fixture to v2");
    drop(connection);
    drop(ChangeBatchJournal::open(v2_root.path()).expect("migrate v2 journal"));

    let v3_root = TestDirectory::new("schema-v3");
    drop(ChangeBatchJournal::open(v3_root.path()).expect("create v4 journal"));
    let v3_database = v3_root.path().join("change-batch.sqlite3");
    let connection = Connection::open(&v3_database).expect("open v3 fixture");
    connection
        .execute_batch(
            "DROP TABLE change_batch_diagnostic_evaluation;
             DROP TABLE change_batch_diagnostic_baseline;
             ALTER TABLE change_batch_phase_command
               DROP COLUMN diagnostic_parse_failed;
             ALTER TABLE change_batch_phase_command
               DROP COLUMN diagnostic_batch_json;
             PRAGMA user_version = 3;",
        )
        .expect("downgrade fixture to v3");
    drop(connection);
    drop(ChangeBatchJournal::open(v3_root.path()).expect("migrate v3 journal"));
    let connection = Connection::open(&v3_database).expect("inspect migrated v3 journal");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated v3 version");
    assert_eq!(version, 4);
    drop(connection);

    let corrupt_root = TestDirectory::new("schema-v4-missing");
    drop(ChangeBatchJournal::open(corrupt_root.path()).expect("create v4 journal"));
    let corrupt_database = corrupt_root.path().join("change-batch.sqlite3");
    let connection = Connection::open(&corrupt_database).expect("open corrupt fixture");
    connection
        .execute("DROP TABLE change_batch_workspace_barrier", [])
        .expect("remove required v4 table");
    drop(connection);
    assert_eq!(
        ChangeBatchJournal::open(corrupt_root.path())
            .expect_err("normal v4 open must not recreate a missing table")
            .code(),
        ChangeBatchJournalErrorCode::Corrupt
    );

    let old_restore_root = TestDirectory::new("schema-v3-old-restore-key");
    drop(ChangeBatchJournal::open(old_restore_root.path()).expect("create strict v4 journal"));
    let old_restore_database = old_restore_root.path().join("change-batch.sqlite3");
    let connection = Connection::open(&old_restore_database).expect("open old restore fixture");
    connection
        .execute_batch(
            "DROP TABLE change_batch_workspace_restore_intent;
             CREATE TABLE change_batch_workspace_restore_intent (
               workspace_id TEXT NOT NULL,
               expected_current TEXT NOT NULL,
               target_revision TEXT NOT NULL,
               PRIMARY KEY (workspace_id, expected_current)
             );",
        )
        .expect("install obsolete restore key");
    drop(connection);
    assert_eq!(
        ChangeBatchJournal::open(old_restore_root.path())
            .expect_err("normal v4 open rejects the obsolete restore key")
            .code(),
        ChangeBatchJournalErrorCode::Corrupt
    );
}

#[test]
fn phase_selection_commands_and_revision_receipts_are_durable_compare_and_set() {
    let root = TestDirectory::new("phase-journal");
    let workspace_id = "phase-workspace";
    let event = proposal_event();
    let base = revision(BASE_TREE);
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    journal
        .retain_workspace_barrier(workspace_id, &base, &now(1))
        .expect("retain barrier");
    journal
        .retain_claimed_intent(workspace_id, &event, &base, &plan_digest(&event), &now(2))
        .expect("retain claimed intent");
    let selection = explicit_phase_selection();
    assert_eq!(
        journal
            .retain_phase_selection(workspace_id, &event.identity.batch_id, &selection, &now(3),)
            .expect("retain selection"),
        JournalRetention::Inserted
    );
    assert_eq!(
        journal
            .retain_phase_selection(workspace_id, &event.identity.batch_id, &selection, &now(3),)
            .expect("replay selection"),
        JournalRetention::Replay
    );
    let formatter = phase_process_receipt("python-format", PhaseProcessStatus::Passed);
    journal
        .retain_phase_command_receipt(&event.identity.batch_id, 0, &formatter, &now(4))
        .expect("retain formatter result");
    assert_eq!(
        journal
            .retain_phase_command_receipt(&event.identity.batch_id, 0, &formatter, &now(4))
            .expect("replay formatter result"),
        JournalRetention::Replay
    );
    let mut changed = formatter.clone();
    changed.stdout_digest = digest(b"changed output");
    assert_eq!(
        journal
            .retain_phase_command_receipt(&event.identity.batch_id, 0, &changed, &now(4))
            .expect_err("changed formatter replay")
            .code(),
        ChangeBatchJournalErrorCode::Conflict
    );
    let normalizer = NormalizerReceipt {
        artifact_refs: Vec::new(),
        base_revision: base.clone(),
        changed_file_digests: Vec::new(),
        result_revision: Some(base.clone()),
        status: NormalizerReceiptStatus::Unchanged,
    };
    journal
        .retain_normalizer_receipt(
            &event.identity.batch_id,
            &normalizer,
            1,
            &base,
            Some(&base),
            &now(5),
        )
        .expect("retain normalizer receipt");
    let check = phase_process_receipt("python-check", PhaseProcessStatus::Passed);
    journal
        .retain_phase_command_receipt(&event.identity.batch_id, 1, &check, &now(6))
        .expect("retain validation result");
    let validation = ValidationReceipt {
        artifact_refs: Vec::new(),
        base_revision: base.clone(),
        checks: vec![ValidationCheckSummary {
            diagnostic_digest: Some(digest(b"validation diagnostic")),
            name: "python-check".to_owned(),
            status: ValidationCheckStatus::Passed,
            summary: "check passed".to_owned(),
        }],
        duration_millis: 1,
        profile: ValidationProfileName::Changed,
        result_revision: Some(base.clone()),
        status: ValidationReceiptStatus::Passed,
    };
    journal
        .retain_validation_receipt(
            &event.identity.batch_id,
            &validation,
            2,
            &base,
            Some(&base),
            &now(7),
        )
        .expect("retain validation receipt");
    drop(journal);

    let journal = ChangeBatchJournal::open(root.path()).expect("reopen journal");
    let record = journal
        .phase_record(&event.identity.batch_id)
        .expect("load phase record")
        .expect("phase record");
    assert_eq!(record.selection, selection);
    assert_eq!(record.command_receipts, [formatter, check]);
    assert_eq!(record.normalizer_receipt, Some(normalizer));
    assert_eq!(record.validation_receipt, Some(validation));
}

#[test]
fn automatic_profile_suggestion_is_durable_but_has_no_execution_cursor() {
    let root = TestDirectory::new("phase-advisory");
    let workspace_id = "phase-advisory-workspace";
    let event = proposal_event();
    let base = revision(BASE_TREE);
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    journal
        .retain_workspace_barrier(workspace_id, &base, &now(1))
        .expect("retain barrier");
    journal
        .retain_claimed_intent(workspace_id, &event, &base, &plan_digest(&event), &now(2))
        .expect("retain claimed intent");
    let advisory = resolve_validation_profile(None, "changed", &["README.md".to_owned()])
        .expect("canonical advisory");
    assert!(!advisory.executable);
    assert!(advisory.command_ids.is_empty());
    journal
        .retain_phase_selection(workspace_id, &event.identity.batch_id, &advisory, &now(3))
        .expect("retain advisory");
    let record = journal
        .phase_record(&event.identity.batch_id)
        .expect("read advisory")
        .expect("advisory record");
    assert_eq!(record.selection, advisory);
    assert!(record.command_receipts.is_empty());
    assert!(record.normalizer_receipt.is_none());
    assert!(record.validation_receipt.is_none());
}

fn explicit_phase_selection() -> ValidationProfileSelection {
    ValidationProfileSelection {
        changed_paths_digest: digest(b"delegated.txt"),
        command_ids: vec!["python-format".to_owned(), "python-check".to_owned()],
        configuration_digest: Some(digest(b"validation config")),
        executable: true,
        profile: ValidationProfileName::Changed,
        reason_code: ValidationProfileSelectionReasonCode::ExplicitProfile,
        source: ValidationSelectionSource::ExplicitConfiguration,
    }
}

fn phase_process_receipt(name: &str, status: PhaseProcessStatus) -> PhaseProcessReceipt {
    PhaseProcessReceipt {
        name: name.to_owned(),
        status,
        exit_code: Some(0),
        stdout_digest: digest(b"stdout"),
        stderr_digest: digest(b"stderr"),
        stdout_artifact_ref: None,
        stderr_artifact_ref: None,
        output_bytes: 12,
        duration_millis: 1,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn workspace_barrier_serializes_batches_and_accepts_only_exact_checkpoint() {
    let root = TestDirectory::new("workspace-barrier");
    let mut journal = ChangeBatchJournal::open(root.path()).expect("open journal");
    let workspace_id = "workspace-fixture";
    let base = revision(BASE_TREE);
    let result = revision(RESULT_TREE);
    let event = proposal_event();
    let files = vec![applied_file()];
    let delta = derive_delta_digest(&files).expect("exact delta");

    assert_eq!(
        journal
            .retain_workspace_barrier(workspace_id, &base, &now(0))
            .unwrap(),
        JournalRetention::Inserted
    );
    assert_eq!(
        journal
            .retain_claimed_intent(workspace_id, &event, &base, &plan_digest(&event), &now(1),)
            .unwrap(),
        JournalRetention::Inserted
    );

    let mut other = event.clone();
    other.identity.turn_id = "turn-2".to_owned();
    other.identity.batch_id = derive_change_batch_id(
        &other.identity.run_key,
        &other.identity.turn_id,
        None,
        &other.identity.patch_digest,
    )
    .expect("derive second batch");
    assert_eq!(
        journal
            .retain_claimed_intent(workspace_id, &other, &base, &plan_digest(&other), &now(2),)
            .unwrap_err()
            .code(),
        ChangeBatchJournalErrorCode::Conflict
    );
    assert!(journal.load(&other.identity.batch_id).unwrap().is_none());

    for (sequence, state) in [
        (1, ChangeBatchProgressState::Proposed),
        (2, ChangeBatchProgressState::Authorized),
        (3, ChangeBatchProgressState::ApplyStarted),
    ] {
        journal
            .append_progress(&progress(&event.identity, sequence, state))
            .unwrap();
    }
    journal
        .transition_workspace_batch(
            workspace_id,
            &event.identity.batch_id,
            ActiveBatchState::Applying,
            ActiveBatchState::CheckpointPending,
            &now(3),
        )
        .unwrap();
    let receipt = ChangeBatchReceipt {
        artifact_ref: None,
        base_revision: base.clone(),
        delta_digest: Some(delta.clone()),
        delta_exact: true,
        files,
        identity: event.identity.clone(),
        normalizer: None,
        observation: None,
        result_revision: Some(result.clone()),
        status: ChangeBatchReceiptStatus::Applied,
        validation: None,
    };
    journal
        .retain_applied_checkpoint(
            workspace_id,
            &progress(&event.identity, 4, ChangeBatchProgressState::Applied),
            &receipt,
            &now(4),
        )
        .unwrap();
    for (sequence, state, expected, next) in [
        (
            5,
            ChangeBatchProgressState::ValidationStarted,
            ActiveBatchState::Checkpointed,
            ActiveBatchState::ValidationPending,
        ),
        (
            6,
            ChangeBatchProgressState::ValidationCompleted,
            ActiveBatchState::ValidationPending,
            ActiveBatchState::ValidationPending,
        ),
        (
            7,
            ChangeBatchProgressState::ObservationRequested,
            ActiveBatchState::ValidationPending,
            ActiveBatchState::ObservationPending,
        ),
        (
            8,
            ChangeBatchProgressState::ObservationCompleted,
            ActiveBatchState::ObservationPending,
            ActiveBatchState::ObservationPending,
        ),
    ] {
        journal
            .retain_workspace_progress(
                workspace_id,
                &progress(&event.identity, sequence, state),
                expected,
                next,
            )
            .unwrap();
    }
    let accepted_progress = progress(&event.identity, 9, ChangeBatchProgressState::Accepted);

    assert_eq!(
        journal
            .accept_observed_checkpoint(workspace_id, &accepted_progress, &base, &delta, &now(7),)
            .unwrap(),
        ObservationGateResult::Stale
    );
    let before_accept = journal.workspace_barrier(workspace_id).unwrap().unwrap();
    assert_eq!(before_accept.state, ActiveBatchState::ObservationPending);
    assert_eq!(before_accept.accepted_revision, base);

    assert_eq!(
        journal
            .accept_observed_checkpoint(workspace_id, &accepted_progress, &result, &delta, &now(8),)
            .unwrap(),
        ObservationGateResult::Accepted
    );
    let accepted = journal.workspace_barrier(workspace_id).unwrap().unwrap();
    assert_eq!(accepted.state, ActiveBatchState::Accepted);
    assert_eq!(accepted.accepted_revision, result);
    assert!(accepted.active_batch_id.is_none());

    other.identity.workspace_revision = accepted.accepted_revision.clone();
    assert_eq!(
        journal
            .retain_claimed_intent(
                workspace_id,
                &other,
                &accepted.accepted_revision,
                &plan_digest(&other),
                &now(9),
            )
            .unwrap(),
        JournalRetention::Inserted
    );
}
