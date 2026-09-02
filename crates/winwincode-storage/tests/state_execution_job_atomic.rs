use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_domain::{
    ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, WorkspaceId,
};
use winwincode_storage::{
    ExecutionJobSubmission, ExecutionQueueScope, NewOutboxEvent, ProductStateStorage,
    PublicEventScope, ReceiptActorKey, ReceiptIdentity, SqliteStorage, StateCommit,
    StorageErrorKind, receipt_scope_key,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-state-execution-job-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn scope(seed: u64) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        product_session_id: ProductSessionId(id("psn", seed)),
        delivery_id: None,
    }
}

fn commit(seed: u64) -> StateCommit {
    let scope = scope(seed);
    StateCommit::new(
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(format!("actor-{seed}").into_bytes()).expect("actor key"),
            receipt_scope_key(&PublicEventScope::Repository {
                organization_id: scope.organization_id,
                workspace_id: scope.workspace_id,
                project_id: scope.project_id,
                repository_id: scope.repository_id,
            })
            .expect("scope key"),
            RequestId(id("req", seed)),
        )
        .expect("receipt identity"),
        Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        format!("product-session-catalog-{seed}"),
        0,
        format!(r#"{{"revision":1,"seed":{seed}}}"#).into_bytes(),
        vec![NewOutboxEvent::internal(
            format!("event-{seed}"),
            "product-session.changed",
            format!(r#"{{"seed":{seed}}}"#).into_bytes(),
        )],
    )
}

fn submission(seed: u64) -> ExecutionJobSubmission {
    ExecutionJobSubmission {
        scope: scope(seed),
        job_id: ExecutionJobId(id("job", seed)),
        request_id: RequestId(id("req", seed)),
        payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        dispatch_payload: format!(r#"{{"jobId":"{}"}}"#, id("job", seed)).into_bytes(),
        attempt: 1,
        dependencies: Vec::new(),
        stage_run_id: None,
        submitted_at: Instant("2027-08-28T10:00:00.000Z".into()),
    }
}

#[test]
fn state_and_execution_job_commit_once_and_replay_together_after_restart() {
    let root = temporary_directory("restart");
    let state = commit(1);
    let job = submission(1);
    let mut storage = SqliteStorage::open(&root).expect("storage open");

    let first = storage
        .commit_with_execution_job(&state, &job)
        .expect("atomic commit");
    assert!(!first.state.idempotent_replay);
    assert!(!first.execution_job.replayed);
    drop(storage);

    let mut restarted = SqliteStorage::open(&root).expect("storage restart");
    let replay = restarted
        .commit_with_execution_job(&state, &job)
        .expect("atomic replay");
    assert!(replay.state.idempotent_replay);
    assert!(replay.execution_job.replayed);
    assert_eq!(replay.execution_job.job, first.execution_job.job);

    let queued = restarted
        .execution_queue()
        .expect("queue open")
        .list_jobs(&job.scope, &[], None, 10)
        .expect("queue page");
    assert_eq!(queued.jobs, vec![first.execution_job.job]);
    drop(restarted);
    fs::remove_dir_all(root).expect("directory cleanup");
}

#[test]
fn changed_replay_and_preexisting_job_roll_back_the_product_state() {
    let root = temporary_directory("rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let original_state = commit(2);
    let original_job = submission(2);
    storage
        .commit_with_execution_job(&original_state, &original_job)
        .expect("atomic commit");

    let mut changed_job = original_job.clone();
    changed_job.dispatch_payload.push(b' ');
    let conflict = storage
        .commit_with_execution_job(&original_state, &changed_job)
        .expect_err("changed replay rejected");
    assert_eq!(conflict.kind(), StorageErrorKind::RequestConflict);

    let preexisting_job = submission(3);
    storage
        .execution_queue()
        .expect("queue open")
        .submit(&preexisting_job)
        .expect("independent job submit");
    let state = commit(4);
    let mut duplicate_job = preexisting_job;
    duplicate_job.request_id = RequestId(id("req", 4));
    let duplicate = storage
        .commit_with_execution_job(&state, &duplicate_job)
        .expect_err("duplicate job rejected");
    assert_eq!(duplicate.kind(), StorageErrorKind::InvalidInput);
    assert!(
        storage
            .load_state("product-session-catalog-4")
            .expect("state query")
            .is_none()
    );
    assert!(
        storage
            .load_receipt(&state.receipt_identity, &state.command_digest)
            .expect("receipt query")
            .is_none()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory cleanup");
}

#[test]
fn replay_never_repairs_a_state_receipt_that_has_no_execution_job() {
    let root = temporary_directory("partial-replay");
    let state = commit(5);
    let job = submission(5);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    storage.commit(&state).expect("state-only historical write");

    let missing = storage
        .commit_with_execution_job(&state, &job)
        .expect_err("partial replay rejected");
    assert_eq!(missing.kind(), StorageErrorKind::RequestReplayMissing);
    let jobs = storage
        .execution_queue()
        .expect("queue open")
        .list_jobs(&job.scope, &[], None, 10)
        .expect("queue page");
    assert!(jobs.jobs.is_empty());

    drop(storage);
    fs::remove_dir_all(root).expect("directory cleanup");
}

#[test]
fn concurrent_exact_commands_produce_one_write_and_one_replay() {
    let root = temporary_directory("concurrent");
    let state = commit(6);
    let job = submission(6);
    let bootstrap = SqliteStorage::open(&root).expect("storage bootstrap");
    drop(bootstrap);
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let state = state.clone();
            let job = job.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(root).expect("storage open");
                barrier.wait();
                storage
                    .commit_with_execution_job(&state, &job)
                    .expect("concurrent atomic commit")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| !receipt.state.idempotent_replay)
            .count(),
        1
    );
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| receipt.execution_job.replayed)
            .count(),
        1
    );

    let mut storage = SqliteStorage::open(&root).expect("storage restart");
    let jobs = storage
        .execution_queue()
        .expect("queue open")
        .list_jobs(&job.scope, &[], None, 10)
        .expect("queue page");
    assert_eq!(jobs.jobs.len(), 1);
    drop(storage);
    fs::remove_dir_all(root).expect("directory cleanup");
}
