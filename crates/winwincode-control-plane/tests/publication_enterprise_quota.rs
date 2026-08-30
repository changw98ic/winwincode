// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    DurableEnterpriseQuotaAdmission, PublicationEnterpriseQuotaSaga,
    PublicationEnterpriseUsageReconciler,
};
use winwincode_domain::Instant;
use winwincode_publication::{
    PublicationOperation, PublicationOperationKind, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation, PublicationResourceFact,
    PublicationResourceKind,
    test_support::{
        current_policy_coordinator, current_publication_fixture, current_publication_operations,
    },
};
use winwincode_storage::{
    EnterpriseQuotaBoundary, EnterpriseQuotaLimits, EnterpriseQuotaPolicy,
    EnterpriseQuotaReservationState, ProductStateStorage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
enum LookupOutcome {
    Absent,
    Conflict,
    Unknown,
}

#[derive(Clone, Copy)]
enum ApplyOutcome {
    RemoteWrite,
    NoRemoteWrite,
    Rejected,
}

struct Provider {
    lookup_outcome: LookupOutcome,
    apply_outcome: ApplyOutcome,
    lookups: u64,
    applies: u64,
}

impl Provider {
    const fn new(lookup_outcome: LookupOutcome, apply_outcome: ApplyOutcome) -> Self {
        Self {
            lookup_outcome,
            apply_outcome,
            lookups: 0,
            applies: 0,
        }
    }
}

impl PublicationPort for Provider {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        self.lookups += 1;
        Ok(match self.lookup_outcome {
            LookupOutcome::Absent => PublicationPortObservation::absent(operation),
            LookupOutcome::Conflict => {
                PublicationPortObservation::conflict(operation, "provider-conflict")
            }
            LookupOutcome::Unknown => {
                PublicationPortObservation::unknown(operation, "provider-unknown")
            }
        })
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        self.applies += 1;
        let resource = (operation.kind() == PublicationOperationKind::PullRequest).then(|| {
            PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                42,
            )
            .expect("canonical pull-request resource")
        });
        Ok(match self.apply_outcome {
            ApplyOutcome::RemoteWrite => {
                PublicationPortMutation::applied(operation, resource, true)
            }
            ApplyOutcome::NoRemoteWrite => {
                PublicationPortMutation::applied(operation, resource, false)
            }
            ApplyOutcome::Rejected => {
                PublicationPortMutation::rejected(operation, "provider-rejected")
            }
        })
    }
}

#[test]
fn publication_quota_reserves_before_provider_work_and_usage_projection_settles_once() {
    let directory = temporary_directory("projection");
    let fixture = current_publication_fixture();
    let operations = current_publication_operations();
    let mut state = SqliteStorage::open(&directory).expect("open publication state");
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&directory).expect("open quota connection"),
    );
    let mut provider = Provider::new(LookupOutcome::Absent, ApplyOutcome::RemoteWrite);

    let requests = {
        let mut guarded = PublicationEnterpriseQuotaSaga::new(
            &mut quota,
            &mut provider,
            fixture.attribution(),
            fixture.publication_id(),
            quota_time(),
        );
        let requests = operations
            .iter()
            .map(|operation| guarded.reservation_request(operation))
            .collect::<Vec<_>>();
        current_policy_coordinator(&mut state, &mut guarded)
            .publish(
                fixture.publish_context(),
                fixture.publish_command(),
                fixture.authorization(),
            )
            .expect("persist publication intent");
        current_policy_coordinator(&mut state, &mut guarded)
            .resume(fixture.publication_id(), fixture.resume_time_millis())
            .expect("apply provider operations through quota guard");
        requests
    };
    assert_eq!((provider.lookups, provider.applies), (4, 4));
    quota.close().expect("close quota connection");

    let first = PublicationEnterpriseUsageReconciler::new(&mut state)
        .reconcile_publication_page(None, 200)
        .expect("project immutable Publication sources");
    assert_eq!((first.source_entries, first.inserted_entries), (4, 4));
    assert!(first.next.is_none());
    for request in &requests {
        let reservation = state
            .enterprise_quota_ledger()
            .expect("open quota ledger")
            .load_reservation(&request.reservation_id)
            .expect("load reservation")
            .expect("reservation exists");
        assert_eq!(reservation.state, EnterpriseQuotaReservationState::Settled);
        assert_eq!(reservation.source_seal, request.source_seal);
        assert_eq!(reservation.attribution, request.attribution);
    }

    Box::new(state).close().expect("close state before restart");
    let mut restarted = SqliteStorage::open(&directory).expect("restart state");
    let replay = PublicationEnterpriseUsageReconciler::new(&mut restarted)
        .reconcile_publication_page(None, 200)
        .expect("replay immutable Publication sources");
    assert_eq!(
        (
            replay.source_entries,
            replay.inserted_entries,
            replay.replayed_entries
        ),
        (4, 0, 4)
    );
    Box::new(restarted).close().expect("close restarted state");
    fs::remove_dir_all(directory).expect("remove fixture");
}

#[test]
fn terminal_no_write_outcomes_release_while_unknown_keeps_the_reservation_active() {
    assert_terminal_state(
        "lookup-conflict",
        LookupOutcome::Conflict,
        ApplyOutcome::RemoteWrite,
        EnterpriseQuotaReservationState::Released,
        1,
        0,
    );
    assert_terminal_state(
        "apply-no-remote-write",
        LookupOutcome::Absent,
        ApplyOutcome::NoRemoteWrite,
        EnterpriseQuotaReservationState::Released,
        1,
        1,
    );
    assert_terminal_state(
        "apply-rejected",
        LookupOutcome::Absent,
        ApplyOutcome::Rejected,
        EnterpriseQuotaReservationState::Released,
        1,
        1,
    );
    assert_terminal_state(
        "lookup-unknown",
        LookupOutcome::Unknown,
        ApplyOutcome::RemoteWrite,
        EnterpriseQuotaReservationState::Active,
        1,
        0,
    );
}

#[test]
fn enterprise_denial_returns_before_the_provider_lookup() {
    let directory = temporary_directory("denied");
    let fixture = current_publication_fixture();
    let operation = current_publication_operations()
        .into_iter()
        .next()
        .expect("first operation");
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&directory).expect("open quota connection"),
    );
    quota
        .put_policy(&EnterpriseQuotaPolicy {
            boundary: EnterpriseQuotaBoundary::Organization {
                organization_id: fixture.attribution().organization_id().clone(),
            },
            revision: 1,
            limits: EnterpriseQuotaLimits {
                max_concurrent: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
        })
        .expect("zero-concurrency policy");
    let mut provider = Provider::new(LookupOutcome::Absent, ApplyOutcome::RemoteWrite);
    let observation = PublicationEnterpriseQuotaSaga::new(
        &mut quota,
        &mut provider,
        fixture.attribution(),
        fixture.publication_id(),
        quota_time(),
    )
    .lookup(&operation)
    .expect("quota denial is a provider observation");
    assert!(matches!(
        observation,
        PublicationPortObservation::Conflict { .. }
    ));
    assert_eq!(provider.lookups, 0);
    quota.close().expect("close quota connection");
    fs::remove_dir_all(directory).expect("remove fixture");
}

#[test]
fn unknown_restart_replays_the_durable_timestamp_and_rejects_a_changed_timestamp() {
    let directory = temporary_directory("unknown-restart-timestamp");
    let fixture = current_publication_fixture();
    let operation = current_publication_operations()
        .into_iter()
        .next()
        .expect("first operation");
    let requested_at = publication_approval_time();

    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&directory).expect("open initial quota connection"),
    );
    let mut unknown = Provider::new(LookupOutcome::Unknown, ApplyOutcome::RemoteWrite);
    let request = {
        let mut guarded = PublicationEnterpriseQuotaSaga::new(
            &mut quota,
            &mut unknown,
            fixture.attribution(),
            fixture.publication_id(),
            requested_at.clone(),
        );
        let request = guarded.reservation_request(&operation);
        assert!(matches!(
            guarded.lookup(&operation).expect("unknown provider result"),
            PublicationPortObservation::Unknown { .. }
        ));
        request
    };
    assert_eq!(unknown.lookups, 1);
    quota.close().expect("close initial quota connection");

    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&directory).expect("reopen quota after crash"),
    );
    let mut replay = Provider::new(LookupOutcome::Unknown, ApplyOutcome::RemoteWrite);
    assert!(matches!(
        PublicationEnterpriseQuotaSaga::new(
            &mut quota,
            &mut replay,
            fixture.attribution(),
            fixture.publication_id(),
            requested_at,
        )
        .lookup(&operation)
        .expect("exact durable reservation replay"),
        PublicationPortObservation::Unknown { .. }
    ));
    assert_eq!(replay.lookups, 1);

    let mut changed = Provider::new(LookupOutcome::Unknown, ApplyOutcome::RemoteWrite);
    assert!(matches!(
        PublicationEnterpriseQuotaSaga::new(
            &mut quota,
            &mut changed,
            fixture.attribution(),
            fixture.publication_id(),
            quota_time(),
        )
        .lookup(&operation)
        .expect("changed timestamp is fail-closed"),
        PublicationPortObservation::Unknown { .. }
    ));
    assert_eq!(changed.lookups, 0);
    quota.close().expect("close restarted quota connection");

    let mut inspector = SqliteStorage::open(&directory).expect("open quota inspector");
    let reservation = inspector
        .enterprise_quota_ledger()
        .expect("open quota ledger")
        .load_reservation(&request.reservation_id)
        .expect("load reservation")
        .expect("active reservation remains durable");
    assert_eq!(reservation.state, EnterpriseQuotaReservationState::Active);
    Box::new(inspector).close().expect("close quota inspector");
    fs::remove_dir_all(directory).expect("remove fixture");
}

fn assert_terminal_state(
    name: &str,
    lookup_outcome: LookupOutcome,
    apply_outcome: ApplyOutcome,
    expected_state: EnterpriseQuotaReservationState,
    expected_lookups: u64,
    expected_applies: u64,
) {
    let directory = temporary_directory(name);
    let fixture = current_publication_fixture();
    let operation = current_publication_operations()
        .into_iter()
        .next()
        .expect("first operation");
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&directory).expect("open quota connection"),
    );
    let mut provider = Provider::new(lookup_outcome, apply_outcome);
    let request = {
        let mut guarded = PublicationEnterpriseQuotaSaga::new(
            &mut quota,
            &mut provider,
            fixture.attribution(),
            fixture.publication_id(),
            quota_time(),
        );
        let request = guarded.reservation_request(&operation);
        let observation = guarded.lookup(&operation).expect("guarded lookup");
        if matches!(observation, PublicationPortObservation::Absent { .. }) {
            let _ = guarded.apply(&operation).expect("guarded apply");
        }
        request
    };
    assert_eq!(
        (provider.lookups, provider.applies),
        (expected_lookups, expected_applies)
    );
    quota.close().expect("close quota connection");
    let mut inspector = SqliteStorage::open(&directory).expect("open inspector");
    let reservation = inspector
        .enterprise_quota_ledger()
        .expect("open quota ledger")
        .load_reservation(&request.reservation_id)
        .expect("load reservation")
        .expect("reservation exists");
    assert_eq!(reservation.state, expected_state);
    Box::new(inspector).close().expect("close inspector");
    fs::remove_dir_all(directory).expect("remove fixture");
}

fn quota_time() -> Instant {
    Instant("1970-01-01T00:00:00.000Z".to_owned())
}

fn publication_approval_time() -> Instant {
    Instant("1970-01-01T00:00:01.000Z".to_owned())
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-publication-enterprise-quota-{name}-{}-{suffix}",
        std::process::id()
    ))
}
