use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, WorkspaceId,
};
use winwincode_storage::{
    ExecutionJobCancellationRequest, ExecutionJobPageCursor, ExecutionJobState,
    ExecutionJobSubmission, ExecutionJobTransitionRequest, ExecutionQueueScope, SqliteStorage,
    StorageErrorKind,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-execution-queue-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

#[derive(Clone)]
struct FrozenClock(Instant);

impl FrozenClock {
    fn at(second: u64) -> Self {
        Self(Instant(format!("2027-02-15T08:00:{second:02}.000Z")))
    }

    fn now(&self) -> Instant {
        self.0.clone()
    }
}

fn scope(seed: u64) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        product_session_id: ProductSessionId(id("psn", seed)),
        delivery_id: Some(DeliveryId(id("dlv", seed))),
    }
}

fn submission(
    scope: &ExecutionQueueScope,
    job: u64,
    request: u64,
    second: u64,
    dependencies: &[u64],
) -> ExecutionJobSubmission {
    ExecutionJobSubmission {
        scope: scope.clone(),
        job_id: ExecutionJobId(id("job", job)),
        request_id: RequestId(id("req", request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        dispatch_payload: format!(r#"{{"jobId":"{}"}}"#, id("job", job)).into_bytes(),
        attempt: 1,
        dependencies: dependencies
            .iter()
            .map(|dependency| ExecutionJobId(id("job", *dependency)))
            .collect(),
        stage_run_id: scope
            .delivery_id
            .as_ref()
            .map(|_| winwincode_domain::StageRunId(id("run", job))),
        submitted_at: FrozenClock::at(second).now(),
    }
}

fn transition(
    scope: &ExecutionQueueScope,
    job: u64,
    request: u64,
    expected_revision: u64,
    from: ExecutionJobState,
    to: ExecutionJobState,
    second: u64,
) -> ExecutionJobTransitionRequest {
    ExecutionJobTransitionRequest {
        scope: scope.clone(),
        job_id: ExecutionJobId(id("job", job)),
        request_id: RequestId(id("req", request)),
        expected_revision,
        from,
        to,
        occurred_at: FrozenClock::at(second).now(),
    }
}

fn cancellation(
    scope: &ExecutionQueueScope,
    job: u64,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> ExecutionJobCancellationRequest {
    ExecutionJobCancellationRequest {
        scope: scope.clone(),
        job_id: ExecutionJobId(id("job", job)),
        request_id: RequestId(id("req", request)),
        expected_revision,
        requested_at: FrozenClock::at(second).now(),
    }
}

#[test]
fn submission_is_durable_idempotent_and_never_duplicates_a_job() {
    let root = temporary_directory("submission");
    let queue_scope = scope(1);
    let request = submission(&queue_scope, 1, 1, 1, &[]);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut queue = storage.execution_queue().expect("queue open");

    let first = queue.submit(&request).expect("job submit");
    assert!(!first.replayed);
    assert_eq!(first.job.state, ExecutionJobState::Queued);
    assert_eq!(first.job.revision, 1);
    assert_eq!(first.job.submission_request_id, request.request_id);

    let replay = queue.submit(&request).expect("job replay");
    assert!(replay.replayed);
    assert_eq!(replay.job, first.job);

    let mut changed = request.clone();
    changed.dispatch_payload.push(b' ');
    let conflict = queue.submit(&changed).expect_err("changed replay conflict");
    assert_eq!(conflict.kind(), StorageErrorKind::RequestConflict);

    let mut duplicate_identity = request.clone();
    duplicate_identity.request_id = RequestId(id("req", 2));
    let duplicate = queue
        .submit(&duplicate_identity)
        .expect_err("duplicate job identity");
    assert_eq!(duplicate.kind(), StorageErrorKind::InvalidInput);

    let page = queue
        .list_jobs(&queue_scope, &[], None, 100)
        .expect("queue page");
    assert_eq!(page.jobs, vec![first.job]);
    assert!(page.next_cursor.is_none());
    assert!(
        queue
            .has_request(&queue_scope, &RequestId(id("req", 1)))
            .expect("receipt query")
    );
    assert!(
        !queue
            .has_request(&queue_scope, &RequestId(id("req", 2)))
            .expect("rejected request query")
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn transitions_are_atomic_replayable_and_survive_an_abrupt_restart() {
    let root = temporary_directory("transition-restart");
    let queue_scope = scope(2);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut queue = storage.execution_queue().expect("queue open");
    queue
        .submit(&submission(&queue_scope, 2, 10, 1, &[]))
        .expect("job submit");

    let illegal = transition(
        &queue_scope,
        2,
        11,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Running,
        2,
    );
    let error = queue.transition(&illegal).expect_err("illegal transition");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    assert!(
        !queue
            .has_request(&queue_scope, &illegal.request_id)
            .expect("illegal receipt check")
    );
    let unchanged = queue
        .load_job(&queue_scope, &illegal.job_id)
        .expect("job read")
        .expect("job");
    assert_eq!(unchanged.state, ExecutionJobState::Queued);
    assert_eq!(unchanged.revision, 1);
    assert_eq!(unchanged.updated_at, FrozenClock::at(1).now());

    let lease = transition(
        &queue_scope,
        2,
        12,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Leased,
        2,
    );
    let leased = queue.transition(&lease).expect("lease transition");
    assert_eq!(leased.job.state, ExecutionJobState::Leased);
    assert_eq!(leased.job.revision, 2);

    let running = queue
        .transition(&transition(
            &queue_scope,
            2,
            13,
            2,
            ExecutionJobState::Leased,
            ExecutionJobState::Running,
            3,
        ))
        .expect("running transition");
    assert_eq!(running.job.state, ExecutionJobState::Running);
    assert_eq!(running.job.revision, 3);

    let replay = queue.transition(&lease).expect("lease receipt replay");
    assert!(replay.replayed);
    assert_eq!(replay.job, leased.job);

    let mut changed_replay = lease.clone();
    changed_replay.to = ExecutionJobState::Failed;
    let conflict = queue
        .transition(&changed_replay)
        .expect_err("changed transition replay");
    assert_eq!(conflict.kind(), StorageErrorKind::RequestConflict);

    // Deliberately drop the live connection without the explicit close path.
    drop(storage);

    let mut restarted_storage = SqliteStorage::open(&root).expect("storage restart");
    let restarted_queue = restarted_storage.execution_queue().expect("queue restart");
    let restored = restarted_queue
        .load_job(&queue_scope, &ExecutionJobId(id("job", 2)))
        .expect("restored job read")
        .expect("restored job");
    assert_eq!(restored, running.job);

    drop(restarted_storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn cancellation_removes_queued_and_running_jobs_from_admission() {
    let root = temporary_directory("cancellation");
    let queue_scope = scope(3);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut queue = storage.execution_queue().expect("queue open");

    queue
        .submit(&submission(&queue_scope, 30, 30, 1, &[]))
        .expect("queued job submit");
    let queued_cancel = cancellation(&queue_scope, 30, 31, 1, 2);
    let cancelling = queue
        .request_cancellation(&queued_cancel)
        .expect("queued cancellation");
    assert_eq!(cancelling.job.state, ExecutionJobState::Cancelling);
    assert_eq!(
        cancelling
            .job
            .cancellation
            .as_ref()
            .expect("cancellation intent")
            .request_id,
        queued_cancel.request_id
    );
    assert!(
        queue
            .list_jobs(&queue_scope, &[ExecutionJobState::Queued], None, 100)
            .expect("queued page")
            .jobs
            .is_empty()
    );
    let cancel_replay = queue
        .request_cancellation(&queued_cancel)
        .expect("cancellation replay");
    assert!(cancel_replay.replayed);
    assert_eq!(cancel_replay.job, cancelling.job);

    let cancelled_terminal = queue
        .transition(&transition(
            &queue_scope,
            30,
            32,
            2,
            ExecutionJobState::Cancelling,
            ExecutionJobState::Completed,
            3,
        ))
        .expect("cancel completion");
    assert_eq!(cancelled_terminal.job.state, ExecutionJobState::Completed);
    assert!(cancelled_terminal.job.cancellation.is_some());

    queue
        .submit(&submission(&queue_scope, 31, 40, 4, &[]))
        .expect("running job submit");
    queue
        .transition(&transition(
            &queue_scope,
            31,
            41,
            1,
            ExecutionJobState::Queued,
            ExecutionJobState::Leased,
            5,
        ))
        .expect("lease running job");
    queue
        .transition(&transition(
            &queue_scope,
            31,
            42,
            2,
            ExecutionJobState::Leased,
            ExecutionJobState::Running,
            6,
        ))
        .expect("start running job");
    let running_cancelled = queue
        .request_cancellation(&cancellation(&queue_scope, 31, 43, 3, 7))
        .expect("running cancellation");
    assert_eq!(running_cancelled.job.state, ExecutionJobState::Cancelling);
    assert_eq!(running_cancelled.job.revision, 4);

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn dependency_admission_and_pagination_are_scope_isolated_and_deterministic() {
    let root = temporary_directory("dependency-page");
    let first_scope = scope(4);
    let second_scope = scope(5);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut queue = storage.execution_queue().expect("queue open");

    queue
        .submit(&submission(&first_scope, 50, 50, 1, &[]))
        .expect("dependency submit");
    queue
        .submit(&submission(&first_scope, 51, 51, 2, &[50]))
        .expect("dependent submit");
    queue
        .submit(&submission(&second_scope, 52, 52, 1, &[]))
        .expect("other scope submit");

    let blocked_request = transition(
        &first_scope,
        51,
        53,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Leased,
        3,
    );
    let blocked = queue
        .transition(&blocked_request)
        .expect_err("dependency blocks lease");
    assert_eq!(blocked.kind(), StorageErrorKind::InvalidInput);
    assert!(
        !queue
            .has_request(&first_scope, &blocked_request.request_id)
            .expect("blocked receipt check")
    );

    queue
        .transition(&transition(
            &first_scope,
            50,
            54,
            1,
            ExecutionJobState::Queued,
            ExecutionJobState::Leased,
            3,
        ))
        .expect("dependency lease");
    queue
        .transition(&transition(
            &first_scope,
            50,
            55,
            2,
            ExecutionJobState::Leased,
            ExecutionJobState::Running,
            4,
        ))
        .expect("dependency running");
    queue
        .transition(&transition(
            &first_scope,
            50,
            56,
            3,
            ExecutionJobState::Running,
            ExecutionJobState::Completed,
            5,
        ))
        .expect("dependency complete");
    let admitted = queue
        .transition(&blocked_request)
        .expect("dependent admitted after completion");
    assert_eq!(admitted.job.state, ExecutionJobState::Leased);

    let first_page = queue
        .list_jobs(&first_scope, &[], None, 1)
        .expect("first page");
    assert_eq!(first_page.jobs.len(), 1);
    assert_eq!(first_page.jobs[0].job_id, ExecutionJobId(id("job", 50)));
    let cursor = first_page.next_cursor.expect("next cursor");
    let second_page = queue
        .list_jobs(&first_scope, &[], Some(&cursor), 1)
        .expect("second page");
    assert_eq!(second_page.jobs.len(), 1);
    assert_eq!(second_page.jobs[0].job_id, ExecutionJobId(id("job", 51)));
    assert!(second_page.next_cursor.is_none());

    let other_page = queue
        .list_jobs(&second_scope, &[], None, 100)
        .expect("other scope page");
    assert_eq!(other_page.jobs.len(), 1);
    assert_eq!(other_page.jobs[0].job_id, ExecutionJobId(id("job", 52)));
    assert!(
        queue
            .load_job(&second_scope, &ExecutionJobId(id("job", 50)))
            .expect("cross-scope job lookup")
            .is_none()
    );

    let invalid_cursor = ExecutionJobPageCursor {
        submitted_at: FrozenClock::at(1).now(),
        job_id: ExecutionJobId("job_not-canonical".into()),
    };
    assert_eq!(
        queue
            .list_jobs(&first_scope, &[], Some(&invalid_cursor), 1)
            .expect_err("invalid cursor")
            .kind(),
        StorageErrorKind::InvalidInput
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
