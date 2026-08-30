use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, StageRunId, WorkspaceId,
};
use winwincode_storage::{
    ExecutionJobRecord, ExecutionJobState, ExecutionQueueScope, SchedulerCancellationTarget,
    SchedulerCandidate, SchedulerPolicy, SchedulerPriority, SchedulerRetryDecision,
    SchedulerRetryPolicy, SchedulerWeights, plan_scheduler_cancellation, scheduler_retry_decision,
};

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn scope(organization: u64, project: u64, product_session: u64) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", organization)),
        workspace_id: WorkspaceId(id("wsp", organization)),
        project_id: ProjectId(id("prj", project)),
        repository_id: RepositoryId(id("rep", project)),
        product_session_id: ProductSessionId(id("psn", product_session)),
        delivery_id: Some(DeliveryId(id("dlv", product_session))),
    }
}

fn record(
    job: u64,
    scope: ExecutionQueueScope,
    state: ExecutionJobState,
    attempt: u64,
    dependencies: &[u64],
) -> ExecutionJobRecord {
    ExecutionJobRecord {
        scope,
        job_id: ExecutionJobId(id("job", job)),
        submission_request_id: RequestId(id("req", job)),
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        dispatch_payload: format!(r#"{{"jobId":"{}"}}"#, id("job", job)).into_bytes(),
        state,
        attempt,
        revision: 1,
        dependencies: dependencies
            .iter()
            .map(|dependency| ExecutionJobId(id("job", *dependency)))
            .collect(),
        stage_run_id: Some(StageRunId(id("run", job))),
        submitted_at: Instant("2027-02-15T08:00:00.000Z".into()),
        updated_at: Instant("2027-02-15T08:00:00.000Z".into()),
        cancellation: None,
    }
}

struct Fixture {
    record: ExecutionJobRecord,
    stage_run_id: StageRunId,
    priority: SchedulerPriority,
    enqueued_at_tick: u64,
    eligible_at_tick: u64,
    weights: SchedulerWeights,
}

impl Fixture {
    fn new(
        job: u64,
        scope: ExecutionQueueScope,
        priority: SchedulerPriority,
        enqueued_at_tick: u64,
        weights: SchedulerWeights,
    ) -> Self {
        Self {
            record: record(job, scope, ExecutionJobState::Queued, 1, &[]),
            stage_run_id: StageRunId(id("run", job)),
            priority,
            enqueued_at_tick,
            eligible_at_tick: enqueued_at_tick,
            weights,
        }
    }

    fn candidate(&self) -> SchedulerCandidate<'_> {
        SchedulerCandidate {
            record: &self.record,
            stage_run_id: Some(&self.stage_run_id),
            priority: self.priority,
            enqueued_at_tick: self.enqueued_at_tick,
            eligible_at_tick: self.eligible_at_tick,
            weights: self.weights,
        }
    }
}

fn candidates(fixtures: &[Fixture]) -> Vec<SchedulerCandidate<'_>> {
    fixtures.iter().map(Fixture::candidate).collect()
}

#[test]
fn explicit_priority_wins_until_starvation_protection_promotes_old_work() {
    let mut fixtures = vec![
        Fixture::new(
            1,
            scope(1, 1, 1),
            SchedulerPriority::Low,
            0,
            SchedulerWeights::EQUAL,
        ),
        Fixture::new(
            2,
            scope(2, 2, 2),
            SchedulerPriority::Critical,
            90,
            SchedulerWeights::EQUAL,
        ),
    ];
    let mut scheduler = SchedulerPolicy::new(100);
    let selected = scheduler
        .select(99, &candidates(&fixtures))
        .expect("priority selection")
        .expect("dispatch");
    assert_eq!(selected.job_id, ExecutionJobId(id("job", 2)));
    assert!(scheduler.release_stage_run(
        selected.stage_run_id.as_ref().expect("Delivery StageRun"),
        &selected.job_id
    ));

    fixtures[1].eligible_at_tick = 200;
    let aged = scheduler
        .select(101, &candidates(&fixtures))
        .expect("aged selection")
        .expect("dispatch");
    assert_eq!(aged.job_id, ExecutionJobId(id("job", 1)));
}

#[test]
fn weighted_fair_share_is_enforced_at_each_scope_level() {
    fn selected_counts(fixtures: &[Fixture], count: usize) -> (usize, usize) {
        let mut scheduler = SchedulerPolicy::new(1_000);
        let candidates = candidates(fixtures);
        let mut first = 0;
        let mut second = 0;
        for _ in 0..count {
            let dispatch = scheduler
                .select(10, &candidates)
                .expect("fair selection")
                .expect("dispatch");
            if dispatch.job_id.0 < id("job", 100) {
                first += 1;
            } else {
                second += 1;
            }
        }
        (first, second)
    }

    let mut organizations = Vec::new();
    for job in 1..=6 {
        organizations.push(Fixture::new(
            job,
            scope(1, 1, 1),
            SchedulerPriority::Normal,
            0,
            SchedulerWeights {
                organization: 2,
                ..SchedulerWeights::EQUAL
            },
        ));
    }
    for job in 101..=103 {
        organizations.push(Fixture::new(
            job,
            scope(2, 2, 2),
            SchedulerPriority::Normal,
            0,
            SchedulerWeights::EQUAL,
        ));
    }
    assert_eq!(selected_counts(&organizations, 6), (4, 2));

    let mut projects = Vec::new();
    for job in 1..=6 {
        projects.push(Fixture::new(
            job,
            scope(3, 10, 10),
            SchedulerPriority::Normal,
            0,
            SchedulerWeights {
                project: 2,
                ..SchedulerWeights::EQUAL
            },
        ));
    }
    for job in 101..=103 {
        projects.push(Fixture::new(
            job,
            scope(3, 20, 20),
            SchedulerPriority::Normal,
            0,
            SchedulerWeights::EQUAL,
        ));
    }
    assert_eq!(selected_counts(&projects, 6), (4, 2));

    let mut product_sessions = Vec::new();
    for job in 1..=6 {
        product_sessions.push(Fixture::new(
            job,
            scope(4, 30, 30),
            SchedulerPriority::Normal,
            0,
            SchedulerWeights {
                product_session: 2,
                ..SchedulerWeights::EQUAL
            },
        ));
    }
    for job in 101..=103 {
        product_sessions.push(Fixture::new(
            job,
            scope(4, 30, 40),
            SchedulerPriority::Normal,
            0,
            SchedulerWeights::EQUAL,
        ));
    }
    assert_eq!(selected_counts(&product_sessions, 6), (4, 2));
}

#[test]
fn dependencies_release_only_after_successful_uncancelled_completion() {
    let parent_scope = scope(5, 5, 5);
    let mut parent = Fixture::new(
        50,
        parent_scope.clone(),
        SchedulerPriority::Normal,
        0,
        SchedulerWeights::EQUAL,
    );
    parent.record.state = ExecutionJobState::Running;
    let mut child = Fixture::new(
        51,
        parent_scope,
        SchedulerPriority::Critical,
        0,
        SchedulerWeights::EQUAL,
    );
    child.record.dependencies = vec![parent.record.job_id.clone()];
    let mut scheduler = SchedulerPolicy::new(100);
    assert!(
        scheduler
            .select(10, &candidates(&[parent, child]))
            .expect("blocked selection")
            .is_none()
    );

    let mut parent = Fixture::new(
        50,
        scope(5, 5, 5),
        SchedulerPriority::Normal,
        0,
        SchedulerWeights::EQUAL,
    );
    parent.record.state = ExecutionJobState::Completed;
    let mut child = Fixture::new(
        51,
        scope(5, 5, 5),
        SchedulerPriority::Critical,
        0,
        SchedulerWeights::EQUAL,
    );
    child.record.dependencies = vec![parent.record.job_id.clone()];
    let fixtures = [parent, child];
    let released = scheduler
        .select(10, &candidates(&fixtures))
        .expect("released selection")
        .expect("dispatch");
    assert_eq!(released.job_id, ExecutionJobId(id("job", 51)));
}

#[test]
fn one_stage_run_never_has_two_active_dispatches() {
    let shared_stage = StageRunId(id("run", 70));
    let mut first = Fixture::new(
        70,
        scope(7, 7, 7),
        SchedulerPriority::Normal,
        0,
        SchedulerWeights::EQUAL,
    );
    first.stage_run_id = shared_stage.clone();
    first.record.stage_run_id = Some(shared_stage.clone());
    let mut second = Fixture::new(
        71,
        scope(7, 7, 7),
        SchedulerPriority::Normal,
        0,
        SchedulerWeights::EQUAL,
    );
    second.stage_run_id = shared_stage;
    second.record.stage_run_id = Some(second.stage_run_id.clone());
    let fixtures = [first, second];
    let candidates = candidates(&fixtures);
    let mut scheduler = SchedulerPolicy::new(100);

    let first_dispatch = scheduler
        .select(10, &candidates)
        .expect("first selection")
        .expect("first dispatch");
    assert_eq!(first_dispatch.job_id, ExecutionJobId(id("job", 70)));
    assert!(
        scheduler
            .select(10, &candidates)
            .expect("duplicate selection")
            .is_none()
    );
}

#[test]
fn parent_cancellation_propagates_through_dependency_descendants() {
    let first_scope = scope(8, 8, 8);
    let other_scope = scope(9, 9, 9);
    let parent = record(80, first_scope.clone(), ExecutionJobState::Queued, 1, &[]);
    let child = record(
        81,
        first_scope.clone(),
        ExecutionJobState::Running,
        1,
        &[80],
    );
    let grandchild = record(82, first_scope.clone(), ExecutionJobState::Queued, 1, &[81]);
    let completed = record(
        83,
        first_scope.clone(),
        ExecutionJobState::Completed,
        1,
        &[80],
    );
    let unrelated = record(90, other_scope, ExecutionJobState::Queued, 1, &[]);
    let records = [parent, child, grandchild, completed, unrelated];

    let cascade = plan_scheduler_cancellation(
        &SchedulerCancellationTarget::ExecutionJob(ExecutionJobId(id("job", 80))),
        &records,
    );
    assert_eq!(
        cascade.job_ids,
        vec![
            ExecutionJobId(id("job", 80)),
            ExecutionJobId(id("job", 81)),
            ExecutionJobId(id("job", 82)),
        ]
    );

    let session = plan_scheduler_cancellation(
        &SchedulerCancellationTarget::ProductSession {
            organization_id: first_scope.organization_id,
            project_id: first_scope.project_id,
            product_session_id: first_scope.product_session_id,
        },
        &records,
    );
    assert_eq!(session.job_ids, cascade.job_ids);
}

#[test]
fn retry_policy_has_capped_backoff_and_a_hard_attempt_limit() {
    let mut failed = record(95, scope(9, 9, 9), ExecutionJobState::Running, 1, &[]);
    let policy = SchedulerRetryPolicy {
        max_attempts: 3,
        initial_backoff_ticks: 10,
        max_backoff_ticks: 15,
    };
    assert_eq!(
        scheduler_retry_decision(&failed, true, 100, policy).expect("first retry"),
        SchedulerRetryDecision::Retry {
            next_attempt: 2,
            eligible_at_tick: 110,
        }
    );

    failed.attempt = 2;
    assert_eq!(
        scheduler_retry_decision(&failed, true, 100, policy).expect("capped retry"),
        SchedulerRetryDecision::Retry {
            next_attempt: 3,
            eligible_at_tick: 115,
        }
    );
    failed.attempt = 3;
    assert_eq!(
        scheduler_retry_decision(&failed, true, 100, policy).expect("exhausted retry"),
        SchedulerRetryDecision::Exhausted
    );
    assert_eq!(
        scheduler_retry_decision(&failed, false, 100, policy).expect("permanent failure"),
        SchedulerRetryDecision::PermanentFailure
    );
}
